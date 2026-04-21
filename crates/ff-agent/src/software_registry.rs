//! Software registry loader (retired).
//!
//! Historically this module parsed `config/software.toml` and upserted
//! rows into the `software_registry` Postgres table. That file has been
//! deleted — the DB migration `SCHEMA_V28_SOFTWARE_REGISTRY_SEED` now
//! owns the canonical seed set, and operator edits via
//! `ff software add/remove` (or direct SQL) are preserved across
//! upgrades.
//!
//! The public API ([`seed_from_toml`], [`SeedReport`], and the
//! supporting `SoftwareFile` / `SoftwareEntry` types) is intentionally
//! preserved so any callers that predate the retirement keep compiling.
//! The seeder itself is now a no-op that logs once and returns an empty
//! [`SeedReport`]. Deletion of the types happens in a later cleanup pass.

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

/// Retired no-op seeder. The DB migration `SCHEMA_V28_SOFTWARE_REGISTRY_SEED`
/// now owns the canonical `software_registry` seed set; this function is
/// kept only so callers that predate the retirement (e.g.
/// `examples/seed_v14_registries.rs`) keep compiling.
///
/// Logs a single info line the first time it's called in a process.
pub async fn seed_from_toml(
    _pool: &PgPool,
    _toml_path: &Path,
) -> Result<SeedReport, SoftwareError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "software_registry: TOML seeder retired; canonical rows come from migration V28"
        );
    }
    Ok(SeedReport::default())
}

/// Convert a `toml::value::Table` to `serde_json::Value::Object(...)`.
/// Retained for backwards-compat tests; the main seeder path no longer
/// uses it (see [`seed_from_toml`] no-op stub).
#[allow(dead_code)]
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
