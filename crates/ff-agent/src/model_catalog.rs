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
//! `sync_catalog` performs one narrowly-scoped capability repair. It never
//! replays TOML or treats an operator-provided file as catalog authority.

use std::path::{Path, PathBuf};

use ff_db::ModelCatalogRow;
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
        });
    }
    Ok(rows)
}

/// Reconcile the one canonical capability currently missing from the live
/// catalog. Catalog rows and metadata remain migration/database owned: this
/// function deliberately does not read `FORGEFLEET_CATALOG` or replay TOML.
///
/// The guarded update fails closed for malformed workload values and preserves
/// every column other than `preferred_workloads`. A case-insensitive existing
/// `code` element makes the operation idempotent.
pub async fn sync_catalog(pool: &PgPool) -> Result<usize, String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);

    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "model_catalog.sync_catalog: TOML retired; reconciling constrained canonical capabilities"
        );
    }

    let result = sqlx::query(
        r#"
        UPDATE fleet_model_catalog
           SET preferred_workloads = preferred_workloads || '["code"]'::jsonb
         WHERE id = 'devstral-small-2-24b'
           AND jsonb_typeof(preferred_workloads) = 'array'
           AND NOT EXISTS (
               SELECT 1
                 FROM jsonb_array_elements_text(
                          CASE
                            WHEN jsonb_typeof(preferred_workloads) = 'array'
                            THEN preferred_workloads
                            ELSE '[]'::jsonb
                          END
                      ) AS workload(value)
                WHERE LOWER(workload.value) = 'code'
           )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|e| format!("reconcile devstral code capability: {e}"))?;

    Ok(result.rows_affected() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Row, postgres::PgPoolOptions};

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_catalog_repair_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn drop_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1 AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .expect("terminate temp-db connections");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
    }

    #[tokio::test]
    async fn sync_catalog_repairs_only_devstral_workloads_and_is_idempotent() {
        let Some((admin_url, db_url, db_name)) = temp_db_urls() else {
            return;
        };
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .expect("connect temp db");

        sqlx::raw_sql(
            "CREATE TABLE fleet_model_catalog (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 family TEXT NOT NULL,
                 parameters TEXT NOT NULL,
                 tier INT NOT NULL,
                 description TEXT,
                 gated BOOLEAN NOT NULL DEFAULT FALSE,
                 preferred_workloads JSONB NOT NULL DEFAULT '[]'::jsonb,
                 variants JSONB NOT NULL DEFAULT '[]'::jsonb,
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                 tool_calling BOOLEAN NOT NULL DEFAULT FALSE,
                 display_name TEXT,
                 tasks JSONB,
                 modalities JSONB,
                 benchmarks JSONB,
                 license TEXT,
                 lifecycle TEXT
             );
             INSERT INTO fleet_model_catalog
                 (id, name, family, parameters, tier, description, gated,
                  preferred_workloads, variants, updated_at, tool_calling,
                  display_name, tasks, modalities, benchmarks, license, lifecycle)
             VALUES
                 ('devstral-small-2-24b', 'Devstral', 'mistral', '24B', 2,
                  'sentinel description', TRUE, '[\"reasoning\",\"tool_calling\"]',
                  '[{\"runtime\":\"llama.cpp\"}]', '2026-01-02T03:04:05Z', TRUE,
                  'Devstral Display', '[\"text-generation\"]', '[\"text\"]',
                  '{\"score\":99}', 'apache-2.0', 'active'),
                 ('sentinel', 'Sentinel', 'test', '1B', 9, NULL, FALSE,
                  '[\"reasoning\"]', '[]', '2025-01-01T00:00:00Z', FALSE,
                  NULL, NULL, NULL, NULL, NULL, NULL);",
        )
        .execute(&pool)
        .await
        .expect("create exact live catalog mirror");

        let before: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(cat) - 'preferred_workloads'
               FROM fleet_model_catalog cat
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("snapshot non-workload columns");
        let sentinel_before: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(cat) FROM fleet_model_catalog cat WHERE id = 'sentinel'",
        )
        .fetch_one(&pool)
        .await
        .expect("snapshot sentinel row");

        assert_eq!(sync_catalog(&pool).await.expect("first repair"), 1);
        let row = sqlx::query(
            "SELECT preferred_workloads,
                    to_jsonb(cat) - 'preferred_workloads' AS other_columns
               FROM fleet_model_catalog cat
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("read repaired row");
        assert_eq!(
            row.get::<serde_json::Value, _>("preferred_workloads"),
            serde_json::json!(["reasoning", "tool_calling", "code"])
        );
        assert_eq!(row.get::<serde_json::Value, _>("other_columns"), before);
        assert_eq!(sync_catalog(&pool).await.expect("idempotent repair"), 0);
        let sentinel_after: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(cat) FROM fleet_model_catalog cat WHERE id = 'sentinel'",
        )
        .fetch_one(&pool)
        .await
        .expect("read sentinel row");
        assert_eq!(sentinel_after, sentinel_before);

        for malformed_or_present in [
            serde_json::json!(["CoDe"]),
            serde_json::json!("code"),
            serde_json::json!({"code": true}),
            serde_json::Value::Null,
        ] {
            sqlx::query(
                "UPDATE fleet_model_catalog SET preferred_workloads = $1
                  WHERE id = 'devstral-small-2-24b'",
            )
            .bind(&malformed_or_present)
            .execute(&pool)
            .await
            .expect("set workload fixture");
            assert_eq!(sync_catalog(&pool).await.expect("guarded repair"), 0);
            let after: serde_json::Value = sqlx::query_scalar(
                "SELECT preferred_workloads FROM fleet_model_catalog
                  WHERE id = 'devstral-small-2-24b'",
            )
            .fetch_one(&pool)
            .await
            .expect("read guarded fixture");
            assert_eq!(after, malformed_or_present);
        }

        let arbitrary_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fleet_model_catalog WHERE id = 'operator-toml-row'",
        )
        .fetch_one(&pool)
        .await
        .expect("count arbitrary rows");
        assert_eq!(arbitrary_rows, 0);

        drop_temp_db(admin, pool, &db_name).await;
    }
}
