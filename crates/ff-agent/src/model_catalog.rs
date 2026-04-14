//! Model catalog loader.
//!
//! Parses `config/model_catalog.toml` into `ff_db::ModelCatalogRow` rows and
//! upserts them into the `fleet_model_catalog` Postgres table.

use std::path::{Path, PathBuf};

use ff_db::{pg_upsert_catalog, ModelCatalogRow};
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
    "/Users/venkat/taylorProjects/forge-fleet/config/model_catalog.toml";

/// Resolve catalog path, honoring the `FORGEFLEET_CATALOG` env override.
pub fn resolve_catalog_path() -> PathBuf {
    std::env::var("FORGEFLEET_CATALOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CATALOG_PATH))
}

/// Read the catalog TOML from `path` and convert each entry into a
/// `ff_db::ModelCatalogRow`.
pub fn load_catalog_file(path: &Path) -> Result<Vec<ModelCatalogRow>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
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

/// Load the catalog (honoring `$FORGEFLEET_CATALOG`) and upsert every row
/// into Postgres. Returns the number of rows synced.
pub async fn sync_catalog(pool: &PgPool) -> Result<usize, String> {
    let path = resolve_catalog_path();
    let rows = load_catalog_file(&path)?;
    let mut synced = 0usize;
    for row in &rows {
        pg_upsert_catalog(pool, row)
            .await
            .map_err(|e| format!("upsert {}: {}", row.id, e))?;
        synced += 1;
    }
    Ok(synced)
}
