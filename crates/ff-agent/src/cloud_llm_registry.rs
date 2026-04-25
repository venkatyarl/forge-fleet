//! Cloud LLM provider registry (schema V26).
//!
//! Historically this module loaded `config/cloud_llm_providers.toml` into
//! the `cloud_llm_providers` Postgres table. That file has been deleted —
//! the DB migration `SCHEMA_V35_RETIRE_CLOUD_LLM_PROVIDERS_TOML` now owns
//! the canonical seed set, and operator edits via SQL (or a future
//! `ff cloud-llm add`) are preserved across upgrades.
//!
//! The seeder ([`seed_from_toml`]) is kept as a no-op for call-site
//! compatibility; the read-side helpers ([`list_providers`],
//! [`find_for_model`]) continue to work against Postgres as before.
//!
//! The gateway (`ff-gateway::cloud_llm`) uses [`find_for_model`] at request
//! time to decide whether a `/v1/chat/completions` body should be forwarded
//! to a cloud provider (based on the `model` field's prefix) instead of the
//! Pulse-backed local router.
//!
//! Credentials are NEVER stored in this table — `secret_key` is an opaque
//! lookup key into `fleet_secrets` (schema V9).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row as SqlxRow;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CloudLlmError {
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
pub struct SeedReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProvidersFile {
    #[serde(default)]
    pub provider: Vec<ProviderEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderEntry {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
    pub auth_kind: String,
    pub secret_key: String,
    #[serde(default)]
    pub oauth_token_secret: Option<String>,
    #[serde(default)]
    pub oauth_token_url: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    pub model_prefix: String,
    #[serde(default = "default_request_format")]
    pub request_format: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_request_format() -> String {
    "openai_chat".to_string()
}
fn default_enabled() -> bool {
    true
}

/// Runtime row shape returned by [`list_providers`] / [`find_for_model`].
#[derive(Debug, Clone, Serialize)]
pub struct Provider {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
    pub auth_kind: String,
    pub secret_key: String,
    pub oauth_token_secret: Option<String>,
    pub oauth_token_url: Option<String>,
    pub oauth_client_id: Option<String>,
    pub model_prefix: String,
    pub request_format: String,
    pub enabled: bool,
}

/// Retired no-op seeder. The DB migration
/// `SCHEMA_V35_RETIRE_CLOUD_LLM_PROVIDERS_TOML` now owns the canonical
/// `cloud_llm_providers` seed set; this function is kept only so callers
/// that predate the retirement keep compiling.
///
/// Logs a single info line the first time it's called in a process.
pub async fn seed_from_toml(
    _pool: &PgPool,
    _toml_path: &Path,
) -> Result<SeedReport, CloudLlmError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "cloud_llm_providers: TOML seeder retired; canonical rows come from migration V35"
        );
    }
    Ok(SeedReport::default())
}

pub async fn list_providers(pool: &PgPool) -> Result<Vec<Provider>, CloudLlmError> {
    let rows = sqlx::query(
        "SELECT id, display_name, base_url, auth_kind, secret_key,
                oauth_token_secret, oauth_token_url, oauth_client_id,
                model_prefix, request_format, enabled
           FROM cloud_llm_providers ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_provider).collect())
}

/// Find the provider whose `model_prefix` best matches `model_id`, preferring
/// the longest matching prefix. Disabled providers are skipped.
pub async fn find_for_model(
    pool: &PgPool,
    model_id: &str,
) -> Result<Option<Provider>, CloudLlmError> {
    let rows = sqlx::query(
        "SELECT id, display_name, base_url, auth_kind, secret_key,
                oauth_token_secret, oauth_token_url, oauth_client_id,
                model_prefix, request_format, enabled
           FROM cloud_llm_providers WHERE enabled = true",
    )
    .fetch_all(pool)
    .await?;

    let mut best: Option<(usize, Provider)> = None;
    for r in rows {
        let p = row_to_provider(r);
        if model_id.starts_with(&p.model_prefix) {
            let len = p.model_prefix.len();
            if best.as_ref().map(|(l, _)| len > *l).unwrap_or(true) {
                best = Some((len, p));
            }
        }
    }
    Ok(best.map(|(_, p)| p))
}

fn row_to_provider(r: sqlx::postgres::PgRow) -> Provider {
    Provider {
        id: r.get("id"),
        display_name: r.get("display_name"),
        base_url: r.get("base_url"),
        auth_kind: r.get("auth_kind"),
        secret_key: r.get("secret_key"),
        oauth_token_secret: r.get("oauth_token_secret"),
        oauth_token_url: r.get("oauth_token_url"),
        oauth_client_id: r.get("oauth_client_id"),
        model_prefix: r.get("model_prefix"),
        request_format: r.get("request_format"),
        enabled: r.get("enabled"),
    }
}

/// Default path resolution for `config/cloud_llm_providers.toml`.
pub fn resolve_config_path() -> PathBuf {
    for candidate in [
        PathBuf::from("config/cloud_llm_providers.toml"),
        PathBuf::from("../config/cloud_llm_providers.toml"),
        PathBuf::from("../../config/cloud_llm_providers.toml"),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        for rel in [
            "taylorProjects/forge-fleet/config/cloud_llm_providers.toml",
            "projects/forge-fleet/config/cloud_llm_providers.toml",
        ] {
            let p = PathBuf::from(&home).join(rel);
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("config/cloud_llm_providers.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_toml_and_applies_defaults() {
        let toml_src = r#"
[[provider]]
id = "openai"
display_name = "OpenAI"
base_url = "https://api.openai.com/v1"
auth_kind = "api_key"
secret_key = "cloud.openai.api_key"
model_prefix = "openai/"
request_format = "openai_chat"

[[provider]]
id = "x"
display_name = "X"
base_url = "https://x"
auth_kind = "api_key"
secret_key = "cloud.x.key"
model_prefix = "x/"
"#;
        let doc: ProvidersFile = toml::from_str(toml_src).expect("parse");
        assert_eq!(doc.provider.len(), 2);
        assert_eq!(doc.provider[0].request_format, "openai_chat");
        assert!(doc.provider[0].enabled);
        // Defaults applied
        assert_eq!(doc.provider[1].request_format, "openai_chat");
        assert!(doc.provider[1].enabled);
    }
}
