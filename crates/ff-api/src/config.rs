use std::env;

use ff_security::auth::ApiKey;
use serde::Deserialize;

use crate::registry::BackendEndpoint;

#[derive(Clone)]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    pub backends: Vec<BackendEndpoint>,
    pub api_keys: Vec<ApiKey>,
    pub cors_allowed_origins: Vec<String>,
    pub database_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BackendEnvConfig {
    #[serde(default)]
    id: Option<String>,
    node: String,
    host: String,
    port: u16,
    model: String,
    tier: u8,
    #[serde(default = "default_true")]
    healthy: bool,
    #[serde(default)]
    busy: bool,
    #[serde(default = "default_http")]
    scheme: String,
    #[serde(default = "default_true")]
    is_local: bool,
    #[serde(default)]
    cost_per_1k_input: f64,
    #[serde(default)]
    cost_per_1k_output: f64,
}

impl ApiConfig {
    pub fn from_env() -> Self {
        let host = env::var("FF_API_HOST").unwrap_or_else(|_| default_host());
        let port = env::var("FF_API_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(4000);

        let backends = env::var("FF_API_BACKENDS")
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<BackendEnvConfig>>(&raw).ok())
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| BackendEndpoint {
                        id: entry.id.unwrap_or_else(|| {
                            format!("{}:{}:{}", entry.node, entry.model, entry.port)
                        }),
                        node: entry.node,
                        host: entry.host,
                        port: entry.port,
                        model: entry.model,
                        tier: entry.tier,
                        healthy: entry.healthy,
                        busy: entry.busy,
                        scheme: entry.scheme,
                        is_local: entry.is_local,
                        cost_per_1k_input: entry.cost_per_1k_input,
                        cost_per_1k_output: entry.cost_per_1k_output,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let api_keys = env::var("FF_API_KEYS_JSON")
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<ApiKey>>(&raw).ok())
            .unwrap_or_default();
        let cors_allowed_origins =
            parse_cors_origins(&env::var("FF_API_CORS_ORIGINS").unwrap_or_default());

        let database_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .unwrap_or_else(|_| default_database_url());

        Self {
            host,
            port,
            backends,
            api_keys,
            cors_allowed_origins,
            database_url,
        }
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn parse_cors_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty() && *origin != "*")
        .map(str::to_owned)
        .collect()
}

fn default_true() -> bool {
    true
}

fn default_http() -> String {
    "http".to_string()
}

fn default_database_url() -> String {
    "postgres://forgefleet:forgefleet@localhost/forgefleet".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_default_is_loopback() {
        assert_eq!(default_host(), "127.0.0.1");
    }

    #[test]
    fn cors_origins_are_an_exact_allowlist_without_wildcards() {
        assert_eq!(
            parse_cors_origins(" https://one.example,*,https://two.example "),
            vec!["https://one.example", "https://two.example"]
        );
        assert!(parse_cors_origins("").is_empty());
    }
}
