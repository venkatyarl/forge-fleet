//! Alert policy seed loader (retired).
//!
//! Historically this module parsed `config/alert_policies.toml` and upserted
//! rows into the `alert_policies` Postgres table. That file has been deleted —
//! the DB migration `SCHEMA_V34_RETIRE_ALERT_POLICIES_TOML` now owns the
//! canonical seed set, and operator edits via `ff alerts policy add` (or
//! direct SQL) are preserved across upgrades.
//!
//! The public API ([`seed_from_toml`], [`AlertSeedReport`], and the
//! supporting `AlertPoliciesFile` / `AlertPolicyEntry` types) is
//! intentionally preserved so any callers that predate the retirement
//! keep compiling. The seeder itself is now a no-op that logs once and
//! returns an empty [`AlertSeedReport`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AlertSeedError {
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

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct AlertSeedReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AlertPoliciesFile {
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub policy: Vec<AlertPolicyEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AlertPolicyEntry {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub metric: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub scope_computer_id: Option<String>,
    pub condition: String,
    #[serde(default = "default_duration")]
    pub duration_secs: i32,
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: i32,
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_scope() -> String {
    "any_computer".into()
}
fn default_duration() -> i32 {
    300
}
fn default_severity() -> String {
    "warning".into()
}
fn default_cooldown() -> i32 {
    3600
}
fn default_channel() -> String {
    "telegram".into()
}
fn default_enabled() -> bool {
    true
}

/// Retired no-op seeder. The DB migration
/// `SCHEMA_V34_RETIRE_ALERT_POLICIES_TOML` now owns the canonical
/// `alert_policies` seed set; this function is kept only so callers that
/// predate the retirement keep compiling.
///
/// Logs a single info line the first time it's called in a process.
pub async fn seed_from_toml(
    _pool: &PgPool,
    _toml_path: &Path,
) -> Result<AlertSeedReport, AlertSeedError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "alert_policies: TOML seeder retired; canonical rows come from migration V34"
        );
    }
    Ok(AlertSeedReport::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
schema_version = "1"

[[policy]]
name = "computer_offline"
description = "Computer has been ODOWN for more than 5 minutes"
metric = "computer_status"
scope = "any_computer"
condition = "== 'odown'"
duration_secs = 300
severity = "critical"
cooldown_secs = 3600
channel = "telegram"

[[policy]]
name = "high_cpu"
metric = "cpu_pct"
condition = "> 90"
"#;

    #[test]
    fn parses_sample_with_defaults() {
        let doc: AlertPoliciesFile = toml::from_str(SAMPLE_TOML).expect("parse");
        assert_eq!(doc.schema_version.as_deref(), Some("1"));
        assert_eq!(doc.policy.len(), 2);

        let p1 = &doc.policy[0];
        assert_eq!(p1.name, "computer_offline");
        assert_eq!(p1.metric, "computer_status");
        assert_eq!(p1.severity, "critical");
        assert_eq!(p1.duration_secs, 300);
        assert!(p1.enabled);

        let p2 = &doc.policy[1];
        assert_eq!(p2.name, "high_cpu");
        assert_eq!(p2.metric, "cpu_pct");
        assert_eq!(p2.condition, "> 90");
        // Defaults applied:
        assert_eq!(p2.scope, "any_computer");
        assert_eq!(p2.duration_secs, 300);
        assert_eq!(p2.severity, "warning");
        assert_eq!(p2.cooldown_secs, 3600);
        assert_eq!(p2.channel, "telegram");
        assert!(p2.enabled);
    }
}
