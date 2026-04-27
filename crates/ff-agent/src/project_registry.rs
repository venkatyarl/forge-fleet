//! Project registry loader.
//!
//! Historically this parsed `config/projects.toml` and upserted each entry
//! into the `projects` Postgres table (schema V15). The TOML has been
//! retired — migration `SCHEMA_V56_RETIRE_LAST_TOMLS_AND_CLI_BUILD` seeds
//! the canonical row set directly. The seeder is preserved as a compat
//! shim that no-ops cleanly when the file is absent so any predate
//! callers (`ff project seed`) keep compiling.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

/// Errors that can occur while seeding the project registry.
#[derive(Debug, Error)]
pub enum ProjectError {
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

/// Summary returned by [`seed_from_toml`].
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct ProjectSeedReport {
    /// Rows that did not previously exist.
    pub inserted: usize,
    /// Rows whose editable fields changed.
    pub updated: usize,
    /// Rows that matched the DB row exactly (no changes).
    pub unchanged: usize,
    /// Total rows processed from the TOML file.
    pub total: usize,
}

/// Top-level TOML shape: `[[project]]` array plus an optional version tag.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectFile {
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub project: Vec<ProjectEntry>,
}

/// One `[[project]]` entry from TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectEntry {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub compose_file: Option<String>,
    #[serde(default)]
    pub repo_url: Option<String>,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    #[serde(default)]
    pub target_computers: Vec<String>,
    #[serde(default)]
    pub health_endpoint: Option<String>,
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_branch() -> String {
    "main".to_string()
}
fn default_status() -> String {
    "active".to_string()
}

/// Read the project TOML from `path` and upsert every row into `projects`.
///
/// Uses `INSERT ... ON CONFLICT (id) DO UPDATE`. Columns owned by the GitHub
/// sync loop (`main_commit_*`, `main_last_synced_at`) are left alone.
pub async fn seed_from_toml(
    pool: &PgPool,
    toml_path: &Path,
) -> Result<ProjectSeedReport, ProjectError> {
    let raw = match std::fs::read_to_string(toml_path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    "projects: TOML seeder retired; canonical rows come from migration V56"
                );
            }
            return Ok(ProjectSeedReport::default());
        }
        Err(source) => {
            return Err(ProjectError::Io {
                path: toml_path.to_path_buf(),
                source,
            });
        }
    };

    let doc: ProjectFile = toml::from_str(&raw).map_err(|source| ProjectError::Toml {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let mut report = ProjectSeedReport {
        total: doc.project.len(),
        ..ProjectSeedReport::default()
    };

    for entry in &doc.project {
        let target_computers =
            serde_json::to_value(&entry.target_computers).map_err(|source| ProjectError::Json {
                id: entry.id.clone(),
                field: "target_computers",
                source,
            })?;

        // xmax = 0 on RETURNING means the row was INSERTed; non-zero means
        // the conflict branch ran. We also compute `changed` by comparing the
        // pre-image to the new values, so genuine no-op updates are bucketed
        // as `unchanged`.
        let row: Option<(bool, bool)> = sqlx::query_as(
            r#"
            WITH existing AS (
                SELECT
                    display_name,
                    compose_file,
                    repo_url,
                    default_branch,
                    target_computers,
                    health_endpoint,
                    status
                FROM projects
                WHERE id = $1
            ),
            upsert AS (
                INSERT INTO projects (
                    id,
                    display_name,
                    compose_file,
                    repo_url,
                    default_branch,
                    target_computers,
                    health_endpoint,
                    status
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (id) DO UPDATE SET
                    display_name     = EXCLUDED.display_name,
                    compose_file     = EXCLUDED.compose_file,
                    repo_url         = EXCLUDED.repo_url,
                    default_branch   = EXCLUDED.default_branch,
                    target_computers = EXCLUDED.target_computers,
                    health_endpoint  = EXCLUDED.health_endpoint,
                    status           = EXCLUDED.status
                RETURNING (xmax = 0) AS inserted
            )
            SELECT
                u.inserted,
                COALESCE(
                    e.display_name     IS DISTINCT FROM $2 OR
                    e.compose_file     IS DISTINCT FROM $3 OR
                    e.repo_url         IS DISTINCT FROM $4 OR
                    e.default_branch   IS DISTINCT FROM $5 OR
                    e.target_computers IS DISTINCT FROM $6 OR
                    e.health_endpoint  IS DISTINCT FROM $7 OR
                    e.status           IS DISTINCT FROM $8,
                    true
                ) AS changed
            FROM upsert u
            LEFT JOIN existing e ON TRUE
            "#,
        )
        .bind(&entry.id)
        .bind(&entry.display_name)
        .bind(entry.compose_file.as_deref())
        .bind(entry.repo_url.as_deref())
        .bind(&entry.default_branch)
        .bind(&target_computers)
        .bind(entry.health_endpoint.as_deref())
        .bind(&entry.status)
        .fetch_optional(pool)
        .await?;

        match row {
            Some((true, _)) => report.inserted += 1,
            Some((false, true)) => report.updated += 1,
            Some((false, false)) => report.unchanged += 1,
            None => report.updated += 1,
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
schema_version = "1"

[[project]]
id = "forge-fleet"
display_name = "ForgeFleet"
repo_url = "https://github.com/venkatyarl/forge-fleet"
default_branch = "main"
compose_file = "deploy/docker-compose.yml"
target_computers = ["taylor", "marcus"]

[[project]]
id = "hireflow360"
display_name = "HireFlow360"
repo_url = "https://github.com/venkatyarl/hireflow360"
default_branch = "main"
target_computers = ["taylor"]
"#;

    #[test]
    fn parses_sample_toml_into_expected_shape() {
        let doc: ProjectFile = toml::from_str(SAMPLE_TOML).expect("parse toml");
        assert_eq!(doc.schema_version.as_deref(), Some("1"));
        assert_eq!(doc.project.len(), 2);

        let ff = &doc.project[0];
        assert_eq!(ff.id, "forge-fleet");
        assert_eq!(ff.display_name, "ForgeFleet");
        assert_eq!(ff.default_branch, "main");
        assert_eq!(ff.target_computers, vec!["taylor", "marcus"]);
        assert_eq!(
            ff.compose_file.as_deref(),
            Some("deploy/docker-compose.yml")
        );

        let hf = &doc.project[1];
        assert_eq!(hf.id, "hireflow360");
        assert!(hf.compose_file.is_none());
        assert_eq!(hf.status, "active"); // default
    }
}
