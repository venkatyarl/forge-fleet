//! Software registry loader.
//!
//! Parses `config/software.toml` into rows and upserts them into the
//! `software_registry` Postgres table (schema V14).
//!
//! The table has `latest_version` + `latest_version_at` columns which are
//! owned by the upstream-check loop — this loader NEVER writes those two
//! columns, only INSERTs them as NULL on first insert.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

/// Errors that can occur while seeding the software registry.
#[derive(Debug, Error)]
pub enum SoftwareError {
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
pub struct SeedReport {
    /// Rows that did not previously exist.
    pub inserted: usize,
    /// Rows whose editable fields changed.
    pub updated: usize,
    /// Rows that matched the DB row exactly (no changes).
    pub unchanged: usize,
    /// Total rows processed from the TOML file.
    pub total: usize,
}

/// Top-level TOML document: `[[software]]` array.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SoftwareFile {
    #[serde(default)]
    pub software: Vec<SoftwareEntry>,
}

/// One `[[software]]` entry in the TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SoftwareEntry {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    #[serde(default)]
    pub applies_to_os_family: Option<String>,
    /// Inline table describing how to detect the installed version.
    /// Serialized to JSON and stored in the `version_source` JSONB column.
    #[serde(default)]
    pub version_source: toml::value::Table,
    /// Inline table of shell commands keyed by platform.
    /// Serialized to JSON and stored in the `upgrade_playbook` JSONB column.
    #[serde(default)]
    pub upgrade_playbook: toml::value::Table,
    #[serde(default)]
    pub requires_restart: bool,
    #[serde(default)]
    pub requires_reboot: bool,
}

/// Read the software TOML from `path` and upsert every row into
/// `software_registry`. Returns a per-row summary.
///
/// The SQL uses `INSERT ... ON CONFLICT (id) DO UPDATE SET ...` and updates
/// every column EXCEPT `latest_version` and `latest_version_at`, which are
/// owned by the upstream-check loop.
pub async fn seed_from_toml(
    pool: &PgPool,
    toml_path: &Path,
) -> Result<SeedReport, SoftwareError> {
    let raw = std::fs::read_to_string(toml_path).map_err(|source| SoftwareError::Io {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let doc: SoftwareFile = toml::from_str(&raw).map_err(|source| SoftwareError::Toml {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let mut report = SeedReport {
        total: doc.software.len(),
        ..SeedReport::default()
    };

    for entry in &doc.software {
        let version_source =
            toml_table_to_json(&entry.version_source).map_err(|source| SoftwareError::Json {
                id: entry.id.clone(),
                field: "version_source",
                source,
            })?;

        let upgrade_playbook =
            toml_table_to_json(&entry.upgrade_playbook).map_err(|source| SoftwareError::Json {
                id: entry.id.clone(),
                field: "upgrade_playbook",
                source,
            })?;

        // Use xmax=0 trick: a brand-new row has xmax=0; an updated row has xmax != 0.
        // We also compare the pre-image to detect "no-op" updates and bucket them
        // into `unchanged`.
        let row: Option<(bool, bool)> = sqlx::query_as(
            r#"
            WITH existing AS (
                SELECT
                    display_name,
                    kind,
                    applies_to_os_family,
                    version_source,
                    upgrade_playbook,
                    requires_restart,
                    requires_reboot
                FROM software_registry
                WHERE id = $1
            ),
            upsert AS (
                INSERT INTO software_registry (
                    id,
                    display_name,
                    kind,
                    applies_to_os_family,
                    version_source,
                    upgrade_playbook,
                    requires_restart,
                    requires_reboot
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (id) DO UPDATE SET
                    display_name         = EXCLUDED.display_name,
                    kind                 = EXCLUDED.kind,
                    applies_to_os_family = EXCLUDED.applies_to_os_family,
                    version_source       = EXCLUDED.version_source,
                    upgrade_playbook     = EXCLUDED.upgrade_playbook,
                    requires_restart     = EXCLUDED.requires_restart,
                    requires_reboot      = EXCLUDED.requires_reboot
                RETURNING (xmax = 0) AS inserted
            )
            SELECT
                u.inserted,
                COALESCE(
                    e.display_name         IS DISTINCT FROM $2 OR
                    e.kind                 IS DISTINCT FROM $3 OR
                    e.applies_to_os_family IS DISTINCT FROM $4 OR
                    e.version_source       IS DISTINCT FROM $5 OR
                    e.upgrade_playbook     IS DISTINCT FROM $6 OR
                    e.requires_restart     IS DISTINCT FROM $7 OR
                    e.requires_reboot      IS DISTINCT FROM $8,
                    true
                ) AS changed
            FROM upsert u
            LEFT JOIN existing e ON TRUE
            "#,
        )
        .bind(&entry.id)
        .bind(&entry.display_name)
        .bind(&entry.kind)
        .bind(entry.applies_to_os_family.as_deref())
        .bind(&version_source)
        .bind(&upgrade_playbook)
        .bind(entry.requires_restart)
        .bind(entry.requires_reboot)
        .fetch_optional(pool)
        .await?;

        match row {
            Some((true, _)) => report.inserted += 1,
            Some((false, true)) => report.updated += 1,
            Some((false, false)) => report.unchanged += 1,
            None => {
                // Upsert always returns a row; this branch is defensive.
                report.updated += 1;
            }
        }
    }

    Ok(report)
}

/// Convert a `toml::value::Table` to `serde_json::Value::Object(...)`.
fn toml_table_to_json(
    table: &toml::value::Table,
) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
[[software]]
id = "ff"
display_name = "ForgeFleet CLI (ff)"
kind = "binary"
version_source = { method = "cmd", args = ["ff", "--version"], regex = "ff (\\S+)" }
requires_restart = false

[software.upgrade_playbook]
macos = "brew upgrade ff"
linux-ubuntu = "apt upgrade ff"

[[software]]
id = "os-macos"
display_name = "macOS"
kind = "os"
applies_to_os_family = "macos"
version_source = { method = "sw_vers" }
requires_restart = true
requires_reboot = true

[software.upgrade_playbook]
macos = "softwareupdate -ia"
"#;

    #[test]
    fn parses_sample_toml_into_expected_shape() {
        let doc: SoftwareFile = toml::from_str(SAMPLE_TOML).expect("parse toml");
        assert_eq!(doc.software.len(), 2);

        let ff = &doc.software[0];
        assert_eq!(ff.id, "ff");
        assert_eq!(ff.display_name, "ForgeFleet CLI (ff)");
        assert_eq!(ff.kind, "binary");
        assert!(ff.applies_to_os_family.is_none());
        assert!(!ff.requires_restart);
        assert!(!ff.requires_reboot);
        assert_eq!(
            ff.version_source.get("method").and_then(|v| v.as_str()),
            Some("cmd")
        );
        assert_eq!(
            ff.upgrade_playbook.get("macos").and_then(|v| v.as_str()),
            Some("brew upgrade ff")
        );

        let os = &doc.software[1];
        assert_eq!(os.id, "os-macos");
        assert_eq!(os.kind, "os");
        assert_eq!(os.applies_to_os_family.as_deref(), Some("macos"));
        assert!(os.requires_restart);
        assert!(os.requires_reboot);
    }

    #[test]
    fn version_source_and_playbook_round_trip_to_json() {
        let doc: SoftwareFile = toml::from_str(SAMPLE_TOML).expect("parse toml");
        let ff = &doc.software[0];

        let vs = toml_table_to_json(&ff.version_source).expect("vs to json");
        assert_eq!(vs.get("method").and_then(|v| v.as_str()), Some("cmd"));
        let args = vs.get("args").and_then(|v| v.as_array()).expect("args arr");
        assert_eq!(args.len(), 2);

        let up = toml_table_to_json(&ff.upgrade_playbook).expect("up to json");
        assert_eq!(
            up.get("linux-ubuntu").and_then(|v| v.as_str()),
            Some("apt upgrade ff")
        );
    }
}
