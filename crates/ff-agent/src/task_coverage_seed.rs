//! Seed `fleet_task_coverage` (retired).
//!
//! Historically this module parsed `config/task_coverage.toml` and upserted
//! rows into the `fleet_task_coverage` Postgres table. That file has been
//! deleted — the DB migration `SCHEMA_V36_RETIRE_TASK_COVERAGE_TOML` now
//! owns the canonical seed set, and operator edits via SQL (or
//! `ff fleet task-coverage set`) are preserved across upgrades.
//!
//! The public API ([`seed_from_toml`], [`TaskCoverageSeedReport`], and the
//! supporting `TaskCoverageFile` / `TaskCoverageEntry` types) is
//! intentionally preserved so any callers that predate the retirement
//! keep compiling. The seeder itself is now a no-op that logs once and
//! returns an empty [`TaskCoverageSeedReport`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

/// Default path, relative to the repo root.
pub const DEFAULT_TASK_COVERAGE_PATH: &str =
    "/Users/venkat/projects/forge-fleet/config/task_coverage.toml";

/// Errors that can occur during seeding.
#[derive(Debug, Error)]
pub enum TaskCoverageError {
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

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Summary of one seeding run.
#[derive(Debug, Default, Clone, Serialize)]
pub struct TaskCoverageSeedReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub total: usize,
}

/// Top-level TOML document.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskCoverageFile {
    #[serde(default, rename = "task")]
    pub tasks: Vec<TaskCoverageEntry>,
}

/// One `[[task]]` entry in the TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskCoverageEntry {
    pub task: String,
    #[serde(default = "default_min_models_loaded")]
    pub min_models_loaded: i32,
    #[serde(default)]
    pub preferred_model_ids: Vec<String>,
    #[serde(default = "default_priority")]
    pub priority: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// Optional short handle that gateway clients can send as the `model`
    /// field to route to any member of this pool (schema V27).
    #[serde(default)]
    pub alias: Option<String>,
}

fn default_min_models_loaded() -> i32 {
    1
}

fn default_priority() -> String {
    "normal".to_string()
}

/// Resolve the coverage TOML path, honoring `$FORGEFLEET_TASK_COVERAGE`.
pub fn resolve_task_coverage_path() -> PathBuf {
    std::env::var("FORGEFLEET_TASK_COVERAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TASK_COVERAGE_PATH))
}

/// Retired no-op seeder. The DB migration
/// `SCHEMA_V36_RETIRE_TASK_COVERAGE_TOML` now owns the canonical
/// `fleet_task_coverage` seed set; this function is kept only so callers
/// that predate the retirement keep compiling.
///
/// Logs a single info line the first time it's called in a process.
pub async fn seed_from_toml(
    _pool: &PgPool,
    _toml_path: &Path,
) -> Result<TaskCoverageSeedReport, TaskCoverageError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "fleet_task_coverage: TOML seeder retired; canonical rows come from migration V36"
        );
    }
    Ok(TaskCoverageSeedReport::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_toml() {
        let txt = r#"
            [[task]]
            task = "text-generation"
            min_models_loaded = 1
            priority = "critical"

            [[task]]
            task = "feature-extraction"
            min_models_loaded = 1
            priority = "normal"
            preferred_model_ids = ["bge-large-en-v1.5"]
        "#;
        let doc: TaskCoverageFile = toml::from_str(txt).unwrap();
        assert_eq!(doc.tasks.len(), 2);
        assert_eq!(doc.tasks[0].task, "text-generation");
        assert_eq!(doc.tasks[0].priority, "critical");
        assert_eq!(doc.tasks[1].preferred_model_ids, vec!["bge-large-en-v1.5"]);
    }
}
