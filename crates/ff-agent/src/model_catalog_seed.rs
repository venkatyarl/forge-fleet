//! Model catalog (V14) loader.
//!
//! Parses `config/model_catalog.toml` and upserts rows into the new
//! `model_catalog` Postgres table introduced in schema V14 (see
//! `SCHEMA_V14_COMPUTERS_AND_PORTFOLIO` in `ff-db::schema`).
//!
//! The legacy `fleet_model_catalog` table is still populated by
//! `ff_agent::model_catalog` — this module is the V14 replacement and
//! intentionally does not touch the old table.
//!
//! # Ownership
//!
//! Three columns are owned by background loops and MUST NOT be overwritten
//! by this seeder on updates:
//!
//!   - `upstream_latest_rev`      — set by the upstream revision checker
//!   - `upstream_checked_at`      — set by the upstream revision checker
//!   - `benchmark_results`        — set by the benchmarking/evaluation loop
//!
//! On first INSERT those columns are left at their schema defaults
//! (`NULL`, `NULL`, `'{}'`). On ON CONFLICT UPDATE they are explicitly
//! preserved via `COALESCE(model_catalog.col, EXCLUDED.col)`-style logic
//! but actually we just omit them from the UPDATE SET clause entirely.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

/// Errors that can occur while seeding `model_catalog`.
#[derive(Debug, Error)]
pub enum ModelSeedError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("failed to serialize field {field} for {id}: {source}")]
    Json {
        id: String,
        field: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Per-run summary returned by [`seed_from_toml`].
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct ModelSeedReport {
    /// Rows that did not previously exist.
    pub inserted: usize,
    /// Rows whose editable fields changed.
    pub updated: usize,
    /// Rows that matched the DB exactly (no editable changes).
    pub unchanged: usize,
    /// Rows dropped because the TOML entry was missing a valid `id`
    /// or otherwise malformed.
    pub skipped_invalid: usize,
    /// Total entries seen in the TOML file (including skipped).
    pub total: usize,
}

/// Top-level TOML document.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelCatalogFile {
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

/// One `[[models]]` entry in the TOML. All fields except `id`/`name`/`family`
/// are `#[serde(default)]` so partial/old entries still parse.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelEntry {
    // ─── Identity ───────────────────────────────────────────────────────
    pub id: String,
    pub name: String,
    pub family: String,
    pub parameters: String,

    // ─── Legacy / internal classification (not persisted verbatim) ─────
    // These live in the TOML but don't have a direct V14 column.
    // `tier` is not a column on `model_catalog`; `quality_tier` replaced
    // it.  We still accept it so parsing doesn't fail.
    pub tier: Option<i32>,
    pub description: Option<String>,
    pub gated: bool,
    pub preferred_workloads: Vec<String>,

    // ─── HF task / modality taxonomy ────────────────────────────────────
    pub tasks: Vec<String>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub languages: Vec<String>,

    // ─── Upstream tracking ──────────────────────────────────────────────
    pub upstream_source: Option<String>,
    pub upstream_id: Option<String>,

    // ─── Release / runtime metadata ─────────────────────────────────────
    /// Optional ISO-8601 date (YYYY-MM-DD). Stored as SQL DATE.
    pub release_date: Option<String>,
    pub architecture: Option<String>,
    pub license: Option<String>,
    pub quantization: Option<String>,
    pub file_size_gb: Option<f64>,
    pub context_window: Option<i32>,
    pub recommended_runtime: Vec<String>,

    // ─── Hardware requirements ──────────────────────────────────────────
    pub required_gpu_kind: Option<String>,
    pub min_vram_gb: Option<f64>,
    pub cpu_runnable: Option<bool>,

    // ─── Portfolio lifecycle ────────────────────────────────────────────
    pub quality_tier: Option<String>,
    pub lifecycle_status: Option<String>,
    pub replaced_by: Option<String>,
    pub retirement_reason: Option<String>,
    pub retirement_date: Option<String>,

    // ─── Bookkeeping ────────────────────────────────────────────────────
    pub added_by: Option<String>,
    pub notes: Option<String>,

    // ─── Sub-tables ─────────────────────────────────────────────────────
    /// `[[models.variants]]` sub-entries — stored inside `metadata.variants`
    /// since the V14 `model_catalog` table has no dedicated variants column.
    pub variants: Vec<ModelVariant>,
}

/// One `[[models.variants]]` entry.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelVariant {
    pub runtime: String,
    pub quant: String,
    pub hf_repo: String,
    pub size_gb: f64,
}

/// Read the TOML at `toml_path` and upsert every `[[models]]` entry into
/// the V14 `model_catalog` table.
///
/// The UPSERT updates every persisted column EXCEPT:
///
///   - `upstream_latest_rev`  — owned by the upstream-check loop
///   - `upstream_checked_at`  — owned by the upstream-check loop
///   - `benchmark_results`    — owned by the benchmark loop
///
/// Rows with an empty/whitespace `id` are skipped and counted in
/// `skipped_invalid`.
pub async fn seed_from_toml(
    pool: &PgPool,
    toml_path: &Path,
) -> Result<ModelSeedReport, ModelSeedError> {
    let raw = std::fs::read_to_string(toml_path).map_err(|source| ModelSeedError::Io {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let doc: ModelCatalogFile = toml::from_str(&raw).map_err(|source| ModelSeedError::Toml {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let mut report = ModelSeedReport {
        total: doc.models.len(),
        ..ModelSeedReport::default()
    };

    for entry in &doc.models {
        if entry.id.trim().is_empty() || entry.name.trim().is_empty() || entry.family.trim().is_empty()
        {
            report.skipped_invalid += 1;
            continue;
        }

        let tasks_json = serde_json::to_value(&entry.tasks).map_err(|source| ModelSeedError::Json {
            id: entry.id.clone(),
            field: "tasks",
            source,
        })?;
        let input_modalities_json =
            serde_json::to_value(&entry.input_modalities).map_err(|source| ModelSeedError::Json {
                id: entry.id.clone(),
                field: "input_modalities",
                source,
            })?;
        let output_modalities_json =
            serde_json::to_value(&entry.output_modalities).map_err(|source| ModelSeedError::Json {
                id: entry.id.clone(),
                field: "output_modalities",
                source,
            })?;
        let languages_json =
            serde_json::to_value(&entry.languages).map_err(|source| ModelSeedError::Json {
                id: entry.id.clone(),
                field: "languages",
                source,
            })?;
        let recommended_runtime_json = serde_json::to_value(&entry.recommended_runtime)
            .map_err(|source| ModelSeedError::Json {
                id: entry.id.clone(),
                field: "recommended_runtime",
                source,
            })?;

        // metadata retains fields that don't have a dedicated V14 column:
        //   parameters, tier, gated, preferred_workloads, description, variants
        let metadata = serde_json::json!({
            "parameters": entry.parameters,
            "tier": entry.tier,
            "description": entry.description,
            "gated": entry.gated,
            "preferred_workloads": entry.preferred_workloads,
            "variants": entry.variants,
        });

        // Parse optional SQL DATE fields. Malformed dates round-trip as NULL
        // rather than aborting the whole seed.
        let release_date = parse_optional_date(&entry.release_date);
        let retirement_date = parse_optional_date(&entry.retirement_date);

        let upstream_source = entry
            .upstream_source
            .clone()
            .unwrap_or_else(|| "huggingface".to_string());
        let quality_tier = entry
            .quality_tier
            .clone()
            .unwrap_or_else(|| "standard".to_string());
        let lifecycle_status = entry
            .lifecycle_status
            .clone()
            .unwrap_or_else(|| "active".to_string());
        let cpu_runnable = entry.cpu_runnable.unwrap_or(true);

        // Cast f64 -> f32 where needed for FLOAT columns; SQL DOUBLE
        // PRECISION is fine either way but sqlx will pick based on type.
        let file_size_gb = entry.file_size_gb.map(|v| v as f32);
        let min_vram_gb = entry.min_vram_gb.map(|v| v as f32);

        let row: Option<(bool, bool)> = sqlx::query_as(
            r#"
            WITH existing AS (
                SELECT
                    display_name,
                    family,
                    parameter_count,
                    architecture,
                    license,
                    tasks,
                    input_modalities,
                    output_modalities,
                    languages,
                    upstream_source,
                    upstream_id,
                    release_date,
                    quantization,
                    file_size_gb,
                    context_window,
                    recommended_runtime,
                    required_gpu_kind,
                    min_vram_gb,
                    cpu_runnable,
                    quality_tier,
                    lifecycle_status,
                    replaced_by,
                    retirement_reason,
                    retirement_date,
                    added_by,
                    notes,
                    metadata
                FROM model_catalog
                WHERE id = $1
            ),
            upsert AS (
                INSERT INTO model_catalog (
                    id,
                    display_name,
                    family,
                    parameter_count,
                    architecture,
                    license,
                    tasks,
                    input_modalities,
                    output_modalities,
                    languages,
                    upstream_source,
                    upstream_id,
                    release_date,
                    quantization,
                    file_size_gb,
                    context_window,
                    recommended_runtime,
                    required_gpu_kind,
                    min_vram_gb,
                    cpu_runnable,
                    quality_tier,
                    lifecycle_status,
                    replaced_by,
                    retirement_reason,
                    retirement_date,
                    added_by,
                    notes,
                    metadata
                )
                VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    $11, $12, $13, $14, $15, $16, $17, $18, $19, $20,
                    $21, $22, $23, $24, $25, $26, $27, $28
                )
                ON CONFLICT (id) DO UPDATE SET
                    display_name         = EXCLUDED.display_name,
                    family               = EXCLUDED.family,
                    parameter_count      = EXCLUDED.parameter_count,
                    architecture         = EXCLUDED.architecture,
                    license              = EXCLUDED.license,
                    tasks                = EXCLUDED.tasks,
                    input_modalities     = EXCLUDED.input_modalities,
                    output_modalities    = EXCLUDED.output_modalities,
                    languages            = EXCLUDED.languages,
                    upstream_source      = EXCLUDED.upstream_source,
                    upstream_id          = EXCLUDED.upstream_id,
                    release_date         = EXCLUDED.release_date,
                    quantization         = EXCLUDED.quantization,
                    file_size_gb         = EXCLUDED.file_size_gb,
                    context_window       = EXCLUDED.context_window,
                    recommended_runtime  = EXCLUDED.recommended_runtime,
                    required_gpu_kind    = EXCLUDED.required_gpu_kind,
                    min_vram_gb          = EXCLUDED.min_vram_gb,
                    cpu_runnable         = EXCLUDED.cpu_runnable,
                    quality_tier         = EXCLUDED.quality_tier,
                    lifecycle_status     = EXCLUDED.lifecycle_status,
                    replaced_by          = EXCLUDED.replaced_by,
                    retirement_reason    = EXCLUDED.retirement_reason,
                    retirement_date      = EXCLUDED.retirement_date,
                    added_by             = EXCLUDED.added_by,
                    notes                = EXCLUDED.notes,
                    metadata             = EXCLUDED.metadata
                    -- upstream_latest_rev, upstream_checked_at,
                    -- benchmark_results are intentionally NOT updated;
                    -- they are owned by scout / benchmark loops.
                RETURNING (xmax = 0) AS inserted
            )
            SELECT
                u.inserted,
                COALESCE(
                    e.display_name        IS DISTINCT FROM $2  OR
                    e.family              IS DISTINCT FROM $3  OR
                    e.parameter_count     IS DISTINCT FROM $4  OR
                    e.architecture        IS DISTINCT FROM $5  OR
                    e.license             IS DISTINCT FROM $6  OR
                    e.tasks               IS DISTINCT FROM $7  OR
                    e.input_modalities    IS DISTINCT FROM $8  OR
                    e.output_modalities   IS DISTINCT FROM $9  OR
                    e.languages           IS DISTINCT FROM $10 OR
                    e.upstream_source     IS DISTINCT FROM $11 OR
                    e.upstream_id         IS DISTINCT FROM $12 OR
                    e.release_date        IS DISTINCT FROM $13 OR
                    e.quantization        IS DISTINCT FROM $14 OR
                    e.file_size_gb        IS DISTINCT FROM $15 OR
                    e.context_window      IS DISTINCT FROM $16 OR
                    e.recommended_runtime IS DISTINCT FROM $17 OR
                    e.required_gpu_kind   IS DISTINCT FROM $18 OR
                    e.min_vram_gb         IS DISTINCT FROM $19 OR
                    e.cpu_runnable        IS DISTINCT FROM $20 OR
                    e.quality_tier        IS DISTINCT FROM $21 OR
                    e.lifecycle_status    IS DISTINCT FROM $22 OR
                    e.replaced_by         IS DISTINCT FROM $23 OR
                    e.retirement_reason   IS DISTINCT FROM $24 OR
                    e.retirement_date     IS DISTINCT FROM $25 OR
                    e.added_by            IS DISTINCT FROM $26 OR
                    e.notes               IS DISTINCT FROM $27 OR
                    e.metadata            IS DISTINCT FROM $28,
                    true
                ) AS changed
            FROM upsert u
            LEFT JOIN existing e ON TRUE
            "#,
        )
        .bind(&entry.id)                     // $1
        .bind(&entry.name)                   // $2  display_name
        .bind(&entry.family)                 // $3
        .bind(opt_str(&entry.parameters))    // $4  parameter_count
        .bind(entry.architecture.as_deref()) // $5
        .bind(entry.license.as_deref())      // $6
        .bind(&tasks_json)                   // $7
        .bind(&input_modalities_json)        // $8
        .bind(&output_modalities_json)       // $9
        .bind(&languages_json)               // $10
        .bind(&upstream_source)              // $11
        .bind(entry.upstream_id.as_deref())  // $12
        .bind(release_date)                  // $13
        .bind(entry.quantization.as_deref()) // $14
        .bind(file_size_gb)                  // $15
        .bind(entry.context_window)          // $16
        .bind(&recommended_runtime_json)     // $17
        .bind(entry.required_gpu_kind.as_deref()) // $18
        .bind(min_vram_gb)                   // $19
        .bind(cpu_runnable)                  // $20
        .bind(&quality_tier)                 // $21
        .bind(&lifecycle_status)             // $22
        .bind(entry.replaced_by.as_deref())  // $23
        .bind(entry.retirement_reason.as_deref()) // $24
        .bind(retirement_date)               // $25
        .bind(entry.added_by.as_deref())     // $26
        .bind(entry.notes.as_deref())        // $27
        .bind(&metadata)                     // $28
        .fetch_optional(pool)
        .await?;

        match row {
            Some((true, _)) => report.inserted += 1,
            Some((false, true)) => report.updated += 1,
            Some((false, false)) => report.unchanged += 1,
            None => {
                // Upsert always returns a row; defensive branch.
                report.updated += 1;
            }
        }
    }

    Ok(report)
}

/// Turn an empty string into `None`, otherwise pass through as `Some(&str)`.
fn opt_str(s: &str) -> Option<&str> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Parse an optional `YYYY-MM-DD` string into a `chrono::NaiveDate`.
/// Returns `None` for missing or malformed values.
fn parse_optional_date(s: &Option<String>) -> Option<chrono::NaiveDate> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
schema_version = "2"
updated = "2026-04-18"

[[models]]
id = "qwen3-coder-30b"
name = "Qwen3-Coder-30B-A3B-Instruct"
family = "qwen"
parameters = "30B"
tier = 2
description = "Qwen3 MoE coding model."
gated = false
preferred_workloads = ["code", "tool_calling"]
tasks = ["text-generation", "code"]
input_modalities = ["text"]
output_modalities = ["text"]
languages = ["en", "zh"]
upstream_source = "huggingface"
upstream_id = "Qwen/Qwen3-Coder-30B-A3B-Instruct"
quality_tier = "flagship"
lifecycle_status = "active"
min_vram_gb = 20
cpu_runnable = false
license = "apache-2.0"

  [[models.variants]]
  runtime = "llama.cpp"
  quant = "Q4_K_M"
  hf_repo = "Qwen/Qwen3-Coder-30B-A3B-Instruct-GGUF"
  size_gb = 17.0

  [[models.variants]]
  runtime = "mlx"
  quant = "4bit"
  hf_repo = "mlx-community/Qwen3-Coder-30B-A3B-Instruct-4bit"
  size_gb = 17.0

[[models]]
id = "gemma4-31b-it"
name = "Gemma 4 31B Instruct"
family = "gemma"
parameters = "31B"
tier = 2
gated = true
tasks = ["text-generation"]
input_modalities = ["text"]
output_modalities = ["text"]
upstream_id = "google/gemma-4-31b-it"
quality_tier = "flagship"
lifecycle_status = "active"
license = "gemma"
"#;

    #[test]
    fn parses_sample_toml_into_expected_shape() {
        let doc: ModelCatalogFile = toml::from_str(SAMPLE_TOML).expect("parse toml");
        assert_eq!(doc.models.len(), 2);

        let coder = &doc.models[0];
        assert_eq!(coder.id, "qwen3-coder-30b");
        assert_eq!(coder.name, "Qwen3-Coder-30B-A3B-Instruct");
        assert_eq!(coder.family, "qwen");
        assert_eq!(coder.parameters, "30B");
        assert_eq!(coder.tier, Some(2));
        assert_eq!(coder.description.as_deref(), Some("Qwen3 MoE coding model."));
        assert!(!coder.gated);
        assert_eq!(coder.preferred_workloads.len(), 2);
        assert_eq!(coder.tasks, vec!["text-generation", "code"]);
        assert_eq!(coder.input_modalities, vec!["text"]);
        assert_eq!(coder.output_modalities, vec!["text"]);
        assert_eq!(coder.languages, vec!["en", "zh"]);
        assert_eq!(coder.upstream_source.as_deref(), Some("huggingface"));
        assert_eq!(
            coder.upstream_id.as_deref(),
            Some("Qwen/Qwen3-Coder-30B-A3B-Instruct")
        );
        assert_eq!(coder.quality_tier.as_deref(), Some("flagship"));
        assert_eq!(coder.lifecycle_status.as_deref(), Some("active"));
        assert_eq!(coder.min_vram_gb, Some(20.0));
        assert_eq!(coder.cpu_runnable, Some(false));
        assert_eq!(coder.license.as_deref(), Some("apache-2.0"));
        assert_eq!(coder.variants.len(), 2);
        assert_eq!(coder.variants[0].runtime, "llama.cpp");
        assert_eq!(coder.variants[0].quant, "Q4_K_M");
        assert!((coder.variants[0].size_gb - 17.0).abs() < 1e-6);

        let gemma = &doc.models[1];
        assert_eq!(gemma.id, "gemma4-31b-it");
        assert!(gemma.gated);
        // Fields absent from this entry should be defaulted.
        assert!(gemma.languages.is_empty());
        assert!(gemma.description.is_none());
        assert!(gemma.variants.is_empty());
        assert_eq!(gemma.license.as_deref(), Some("gemma"));
    }

    #[test]
    fn parse_optional_date_handles_valid_and_invalid() {
        assert_eq!(
            parse_optional_date(&Some("2026-04-18".to_string())),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 4, 18).unwrap())
        );
        assert_eq!(parse_optional_date(&Some("".to_string())), None);
        assert_eq!(parse_optional_date(&Some("not-a-date".to_string())), None);
        assert_eq!(parse_optional_date(&None), None);
    }

    #[test]
    fn opt_str_strips_empty() {
        assert_eq!(opt_str(""), None);
        assert_eq!(opt_str("   "), None);
        assert_eq!(opt_str("30B"), Some("30B"));
    }
}
