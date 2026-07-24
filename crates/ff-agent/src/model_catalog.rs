//! Model catalog loader.
//!
//! Historically this module parsed `config/model_catalog.toml` into
//! `ff_db::ModelCatalogRow` rows and upserted them into the legacy
//! `fleet_model_catalog` Postgres table. The bulk catalog was retired to
//! Postgres — the canonical V14 seed lives in
//! `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML`, which populates the newer
//! `model_catalog` table.
//!
//! The TOML lane survives for two purposes: a dev override (point
//! `$FORGEFLEET_CATALOG` at any file) and the Autopilot-5 **watchlist
//! seeds** — `config/model_catalog.toml` was re-introduced carrying the
//! `watchlist = true` entries (mirrored by migration V252) so the
//! auto-download flag stays visible and editable in source. When the file
//! is present, [`sync_catalog`] replays it into `fleet_model_catalog`;
//! when absent it stays a no-op that logs once and returns 0.

use std::path::{Path, PathBuf};

use ff_db::{ModelCatalogRow, pg_upsert_catalog};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Top-level TOML document.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogFile {
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub models: Vec<CatalogModel>,
}

/// One `[[models]]` entry in the TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub family: String,
    pub parameters: String,
    pub tier: i32,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub gated: bool,
    #[serde(default)]
    pub preferred_workloads: Vec<String>,
    /// Autopilot-5: auto-download this model when a node has the disk + RAM
    /// headroom (gated models are never auto-downloaded even when flagged).
    #[serde(default)]
    pub watchlist: bool,
    /// SPDX-ish license id. The watchlist auto-download gate fails closed:
    /// a watchlisted entry without an allowlisted license is never fetched
    /// automatically, so seeds must set this explicitly.
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub variants: Vec<CatalogVariant>,
}

/// One `[[models.variants]]` entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogVariant {
    pub runtime: String,
    pub quant: String,
    pub hf_repo: String,
    #[serde(default)]
    pub size_gb: f64,
}

/// Default path to the catalog TOML, relative to the repository root.
pub const DEFAULT_CATALOG_PATH: &str =
    "/Users/venkat/projects/forge-fleet/config/model_catalog.toml";

/// Resolve catalog path: `FORGEFLEET_CATALOG` env override first, then the
/// historical absolute default, then the repo-relative
/// `config/model_catalog.toml` (so the checked-in watchlist seeds load on
/// nodes where the repo lives somewhere other than the historical Mac path).
pub fn resolve_catalog_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("FORGEFLEET_CATALOG") {
        return PathBuf::from(env_path);
    }
    let default = PathBuf::from(DEFAULT_CATALOG_PATH);
    if default.exists() {
        return default;
    }
    PathBuf::from("config/model_catalog.toml")
}

/// Retired no-op loader. If `path` does not exist (which is the normal
/// case post-V39) this returns an empty Vec; legacy callers that still
/// hand a TOML file get the old behaviour so local testing keeps working.
pub fn load_catalog_file(path: &Path) -> Result<Vec<ModelCatalogRow>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let doc: CatalogFile =
        toml::from_str(&raw).map_err(|e| format!("parse {}: {}", path.display(), e))?;

    let mut rows = Vec::with_capacity(doc.models.len());
    for m in doc.models {
        let variants = serde_json::to_value(&m.variants)
            .map_err(|e| format!("variants->json for {}: {}", m.id, e))?;
        let preferred_workloads = serde_json::to_value(&m.preferred_workloads)
            .map_err(|e| format!("preferred_workloads->json for {}: {}", m.id, e))?;
        // tool_calling is derived from the workloads tag (pg_upsert_catalog
        // re-derives it too, so this is belt-and-braces).
        let tool_calling = m.preferred_workloads.iter().any(|w| w == "tool_calling");
        rows.push(ModelCatalogRow {
            id: m.id,
            name: m.name,
            family: m.family,
            parameters: m.parameters,
            tier: m.tier,
            description: m.description,
            gated: m.gated,
            preferred_workloads,
            variants,
            tool_calling,
            watchlist: m.watchlist,
            license: m.license,
        });
    }
    Ok(rows)
}

/// Retired no-op catalog sync. The DB migration
/// `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML` now owns the canonical seed for
/// the V14 `model_catalog` table. This function is preserved for
/// call-site compatibility and logs once + returns 0.
///
/// If a TOML file still exists at the resolved path (local override via
/// `$FORGEFLEET_CATALOG` or an operator-written file), rows from it are
/// upserted into the legacy `fleet_model_catalog` table for development
/// convenience. Otherwise the function is a silent no-op.
pub async fn sync_catalog(pool: &PgPool) -> Result<usize, String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);

    let path = resolve_catalog_path();
    let rows = load_catalog_file(&path)?;

    if rows.is_empty() {
        if !LOGGED.swap(true, Ordering::Relaxed) {
            tracing::info!(
                "model_catalog.sync_catalog: TOML retired; canonical V14 rows come from migration V39"
            );
        }
        return Ok(0);
    }

    // Dev path: a TOML override exists — replay it into the legacy table.
    let mut synced = 0usize;
    for row in &rows {
        pg_upsert_catalog(pool, row)
            .await
            .map_err(|e| format!("upsert {}: {}", row.id, e))?;
        synced += 1;
    }
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checked-in Autopilot-5 watchlist seeds must parse and carry the
    /// flag through to the rows `sync_catalog` upserts (mirrors the V252
    /// migration seed — keep both in step).
    #[test]
    fn watchlist_seeds_file_parses_with_flag_set() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/model_catalog.toml");
        let rows = load_catalog_file(&path).expect("seeds TOML must parse");
        for id in ["apriel-1.5-15b", "qwen3-coder-next-80b"] {
            let row = rows
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("watchlist seed {id} missing from TOML"));
            assert!(row.watchlist, "{id} must be flagged watchlist");
            assert!(
                !row.gated,
                "{id} must be ungated (never auto-download gated)"
            );
            assert!(
                row.variants.as_array().is_some_and(|v| !v.is_empty()),
                "{id} needs at least one variant with a verified hf_repo"
            );
            // The auto-download gate fails closed on a missing license, so a
            // seed without one would be flagged yet never fetched.
            let license = row
                .license
                .as_deref()
                .unwrap_or_else(|| panic!("watchlist seed {id} must declare its license"));
            assert!(
                crate::watchlist_reconciler::license_allows_auto_download(license),
                "{id} license {license:?} must be in the auto-download allowlist"
            );
        }
    }

    /// `watchlist` defaults to false so pre-existing TOML entries and
    /// operator overrides keep their old meaning.
    #[test]
    fn watchlist_defaults_false_in_toml() {
        let doc: CatalogFile = toml::from_str(
            r#"
            [[models]]
            id = "plain"
            name = "Plain"
            family = "test"
            parameters = "1B"
            tier = 1
            "#,
        )
        .unwrap();
        assert!(!doc.models[0].watchlist);
    }
}
