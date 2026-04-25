//! Model catalog loader (retired).
//!
//! Historically this module parsed `config/model_catalog.toml` into
//! `ff_db::ModelCatalogRow` rows and upserted them into the legacy
//! `fleet_model_catalog` Postgres table. That file has been deleted —
//! the canonical V14 seed now lives in
//! `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML`, which populates the newer
//! `model_catalog` table.
//!
//! The public API ([`sync_catalog`], [`load_catalog_file`],
//! [`CatalogFile`], [`CatalogModel`], [`CatalogVariant`]) is kept only
//! so any callers that predate the retirement keep compiling.
//! `sync_catalog` is now a no-op that logs once and returns 0.

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

/// Resolve catalog path, honoring the `FORGEFLEET_CATALOG` env override.
pub fn resolve_catalog_path() -> PathBuf {
    std::env::var("FORGEFLEET_CATALOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CATALOG_PATH))
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
