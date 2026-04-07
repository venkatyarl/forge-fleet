use std::env;

use serde::Deserialize;

use crate::registry::BackendEndpoint;

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    pub backends: Vec<BackendEndpoint>,
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
}

impl ApiConfig {
    pub fn from_env() -> Self {
        let host = env::var("FF_API_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
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
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            host,
            port,
            backends,
        }
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn default_true() -> bool {
    true
}

fn default_http() -> String {
    "http".to_string()
}
