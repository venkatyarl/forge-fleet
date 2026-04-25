//! Model catalog (V14) loader (retired).
//!
//! Historically this module parsed `config/model_catalog.toml` and upserted
//! rows into the `model_catalog` Postgres table introduced in schema V14.
//! That file has been deleted — the DB migration
//! `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML` now owns the canonical seed set,
//! and operator edits via SQL (or a future `ff model add`) are preserved
//! across upgrades.
//!
//! The public API ([`seed_from_toml`], [`ModelSeedReport`], and the
//! supporting `ModelCatalogFile` / `ModelEntry` / `ModelVariant` types) is
//! intentionally preserved so any callers that predate the retirement
//! keep compiling. The seeder itself is now a no-op that logs once and
//! returns an empty [`ModelSeedReport`]. Read paths against `model_catalog`
//! continue to work against Postgres as before.
//!
//! The three columns owned by background loops (`upstream_latest_rev`,
//! `upstream_checked_at`, `benchmark_results`) still start NULL / `'{}'`
//! on first insert and are owned by those loops going forward.

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

/// Retired no-op seeder. The DB migration
/// `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML` now owns the canonical
/// `model_catalog` seed set; this function is kept only so callers
/// that predate the retirement keep compiling.
///
/// Logs a single info line the first time it's called in a process.
pub async fn seed_from_toml(
    _pool: &PgPool,
    _toml_path: &Path,
) -> Result<ModelSeedReport, ModelSeedError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "model_catalog: TOML seeder retired; canonical rows come from migration V39"
        );
    }
    Ok(ModelSeedReport::default())
}

/// Turn an empty string into `None`, otherwise pass through as `Some(&str)`.
/// Retained for the parser-shape tests below; the runtime seeder is
/// retired (see module docs + V39).
#[allow(dead_code)]
fn opt_str(s: &str) -> Option<&str> {
    if s.trim().is_empty() { None } else { Some(s) }
}

/// Parse an optional `YYYY-MM-DD` string into a `chrono::NaiveDate`.
/// Returns `None` for missing or malformed values.
#[allow(dead_code)]
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
        assert_eq!(
            coder.description.as_deref(),
            Some("Qwen3 MoE coding model.")
        );
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
