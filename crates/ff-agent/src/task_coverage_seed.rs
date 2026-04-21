//! Seed `fleet_task_coverage` from `config/task_coverage.toml`.
//!
//! The CoverageGuard (see [`crate::coverage_guard`]) reads this table on
//! every tick. This seeder upserts every row from the TOML so operators
//! can edit the file and re-run `ff fleet task-coverage seed` to pick up
//! changes without hand-writing SQL.

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

/// Load `task_coverage.toml` and upsert every row into
/// `fleet_task_coverage`. Idempotent.
pub async fn seed_from_toml(
    pool: &PgPool,
    toml_path: &Path,
) -> Result<TaskCoverageSeedReport, TaskCoverageError> {
    let raw = std::fs::read_to_string(toml_path).map_err(|source| TaskCoverageError::Io {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let doc: TaskCoverageFile =
        toml::from_str(&raw).map_err(|source| TaskCoverageError::Toml {
            path: toml_path.to_path_buf(),
            source,
        })?;

    let mut report = TaskCoverageSeedReport {
        total: doc.tasks.len(),
        ..TaskCoverageSeedReport::default()
    };

    for entry in &doc.tasks {
        let preferred_json =
            serde_json::to_value(&entry.preferred_model_ids).unwrap_or_else(|_| serde_json::json!([]));

        // Detect insert vs update via xmax.
        let row: Option<(bool,)> = sqlx::query_as(
            "INSERT INTO fleet_task_coverage
                (task, min_models_loaded, preferred_model_ids, priority, notes, alias)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (task) DO UPDATE SET
                min_models_loaded   = EXCLUDED.min_models_loaded,
                preferred_model_ids = EXCLUDED.preferred_model_ids,
                priority            = EXCLUDED.priority,
                notes               = EXCLUDED.notes,
                alias               = EXCLUDED.alias
             WHERE fleet_task_coverage.min_models_loaded   IS DISTINCT FROM EXCLUDED.min_models_loaded
                OR fleet_task_coverage.preferred_model_ids IS DISTINCT FROM EXCLUDED.preferred_model_ids
                OR fleet_task_coverage.priority            IS DISTINCT FROM EXCLUDED.priority
                OR fleet_task_coverage.notes               IS DISTINCT FROM EXCLUDED.notes
                OR fleet_task_coverage.alias               IS DISTINCT FROM EXCLUDED.alias
             RETURNING (xmax = 0) AS inserted",
        )
        .bind(&entry.task)
        .bind(entry.min_models_loaded)
        .bind(&preferred_json)
        .bind(&entry.priority)
        .bind(entry.notes.as_deref())
        .bind(entry.alias.as_deref())
        .fetch_optional(pool)
        .await?;

        match row {
            Some((true,)) => report.inserted += 1,
            Some((false,)) => report.updated += 1,
            None => report.unchanged += 1,
        }
    }

    Ok(report)
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
