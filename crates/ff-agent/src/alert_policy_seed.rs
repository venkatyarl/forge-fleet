//! Alert policy seed loader.
//!
//! Parses `config/alert_policies.toml` and upserts rows into the `alert_policies`
//! Postgres table (schema V16). UPSERT key is `name` — operators can edit the
//! TOML and re-seed to adjust policies.

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

/// Read the TOML at `path` and upsert into `alert_policies`.
pub async fn seed_from_toml(
    pool: &PgPool,
    toml_path: &Path,
) -> Result<AlertSeedReport, AlertSeedError> {
    let raw = std::fs::read_to_string(toml_path).map_err(|source| AlertSeedError::Io {
        path: toml_path.to_path_buf(),
        source,
    })?;

    let doc: AlertPoliciesFile =
        toml::from_str(&raw).map_err(|source| AlertSeedError::Toml {
            path: toml_path.to_path_buf(),
            source,
        })?;

    let mut report = AlertSeedReport {
        total: doc.policy.len(),
        ..AlertSeedReport::default()
    };

    for entry in &doc.policy {
        let scope_uuid = entry
            .scope_computer_id
            .as_deref()
            .and_then(|s| uuid::Uuid::parse_str(s).ok());

        let row: Option<(bool, bool)> = sqlx::query_as(
            r#"
            WITH existing AS (
                SELECT description, metric, scope, scope_computer_id, condition,
                       duration_secs, severity, cooldown_secs, channel, enabled
                FROM alert_policies
                WHERE name = $1
            ),
            upsert AS (
                INSERT INTO alert_policies (
                    name, description, metric, scope, scope_computer_id,
                    condition, duration_secs, severity, cooldown_secs,
                    channel, enabled
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (name) DO UPDATE SET
                    description       = EXCLUDED.description,
                    metric            = EXCLUDED.metric,
                    scope             = EXCLUDED.scope,
                    scope_computer_id = EXCLUDED.scope_computer_id,
                    condition         = EXCLUDED.condition,
                    duration_secs     = EXCLUDED.duration_secs,
                    severity          = EXCLUDED.severity,
                    cooldown_secs     = EXCLUDED.cooldown_secs,
                    channel           = EXCLUDED.channel,
                    enabled           = EXCLUDED.enabled
                RETURNING (xmax = 0) AS inserted
            )
            SELECT u.inserted,
                COALESCE(
                    e.description       IS DISTINCT FROM $2  OR
                    e.metric            IS DISTINCT FROM $3  OR
                    e.scope             IS DISTINCT FROM $4  OR
                    e.scope_computer_id IS DISTINCT FROM $5  OR
                    e.condition         IS DISTINCT FROM $6  OR
                    e.duration_secs     IS DISTINCT FROM $7  OR
                    e.severity          IS DISTINCT FROM $8  OR
                    e.cooldown_secs     IS DISTINCT FROM $9  OR
                    e.channel           IS DISTINCT FROM $10 OR
                    e.enabled           IS DISTINCT FROM $11,
                    true
                ) AS changed
            FROM upsert u
            LEFT JOIN existing e ON TRUE
            "#,
        )
        .bind(&entry.name)
        .bind(entry.description.as_deref())
        .bind(&entry.metric)
        .bind(&entry.scope)
        .bind(scope_uuid)
        .bind(&entry.condition)
        .bind(entry.duration_secs)
        .bind(&entry.severity)
        .bind(entry.cooldown_secs)
        .bind(&entry.channel)
        .bind(entry.enabled)
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
