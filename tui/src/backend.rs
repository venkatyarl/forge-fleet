//! Backend attachment: which LLM the TUI talks to.
//!
//! Three families, matching the operator directive (2026-07-28):
//!   - **Endpoint**: any OpenAI-compatible server, local or remote
//!     (`--base-url http://host:port --model <name>`). This covers every local
//!     fleet deployment (GLM, Lucy, qwen3-coder…) directly.
//!   - **Router**: the fleet's local-first `InferenceRouter` — per-turn
//!     endpoint selection with automatic fleet failover ("make it go through
//!     the LLM router").
//!   - **CloudCli**: a vendor CLI (`claude` | `codex` | `kimi`) driven through
//!     `cli_executor` — the cloud attach. Single-shot per prompt in v1 (no
//!     tool loop, no streaming; the CLI owns its own tools).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ff_agent::agent_loop::{AgentSession, AgentSessionConfig};
use ff_agent::inference_router::InferenceRouter;

/// The currently attached backend.
#[derive(Clone)]
pub enum Backend {
    /// Any OpenAI-compatible endpoint.
    Endpoint { base_url: String, model: String },
    /// Fleet LLM router (local-first + failover).
    Router {
        router: Arc<InferenceRouter>,
        model: String,
    },
    /// Cloud vendor CLI.
    CloudCli { cli: String },
}

impl Backend {
    /// One-line label for the status bar.
    pub fn label(&self) -> String {
        match self {
            Backend::Endpoint { base_url, model } => format!("endpoint {model} @ {base_url}"),
            Backend::Router { model, .. } => format!("router {model} (fleet failover)"),
            Backend::CloudCli { cli } => format!("cloud {cli} (single-shot)"),
        }
    }

    /// Build an agent session for this backend. CloudCli does not use the
    /// agent loop — see [`Backend::is_agent`].
    pub fn agent_session(&self, working_dir: &Path) -> Option<AgentSession> {
        let mut config = AgentSessionConfig {
            working_dir: working_dir.to_path_buf(),
            auto_save: false,
            ..Default::default()
        };
        match self {
            Backend::Endpoint { base_url, model } => {
                config.llm_base_url = base_url.clone();
                config.model = model.clone();
            }
            Backend::Router { router, model } => {
                config.inference_router = Some(router.clone());
                config.model = model.clone();
            }
            Backend::CloudCli { .. } => return None,
        }
        Some(AgentSession::new(config))
    }

    pub fn is_agent(&self) -> bool {
        !matches!(self, Backend::CloudCli { .. })
    }
}

/// Parse a `--backend` / `/backend` spec into a [`Backend`].
///
/// Forms:
///   `router`                     → fleet InferenceRouter (needs the fleet config)
///   `local`                      → localhost default endpoint (http://localhost:55000)
///   `endpoint <url> [model]`     → explicit OpenAI-compatible server
///   `claude` | `codex` | `kimi`  → cloud CLI
pub async fn parse(spec: &str, default_model: &str) -> Result<Backend> {
    let mut parts = spec.split_whitespace();
    let kind = parts.next().unwrap_or("");
    match kind {
        "router" => {
            let config_path = fleet_config_path()?;
            let router = InferenceRouter::from_config(&config_path).await;
            Ok(Backend::Router {
                router: Arc::new(router),
                model: default_model.to_string(),
            })
        }
        "local" => Ok(Backend::Endpoint {
            base_url: "http://localhost:55000".into(),
            model: default_model.to_string(),
        }),
        "endpoint" => {
            let url = parts
                .next()
                .context("usage: /backend endpoint <base-url> [model]")?;
            let model = parts.next().unwrap_or(default_model);
            Ok(Backend::Endpoint {
                base_url: url.trim_end_matches('/').to_string(),
                model: model.to_string(),
            })
        }
        cli @ ("claude" | "codex" | "kimi") => Ok(Backend::CloudCli {
            cli: cli.to_string(),
        }),
        other => anyhow::bail!(
            "unknown backend '{other}' — use: router | local | endpoint <url> [model] | claude | codex | kimi"
        ),
    }
}

/// `~/.forgefleet/fleet.toml` (honouring FORGEFLEET_HOME), same resolution the
/// CLI uses for the router config.
fn fleet_config_path() -> Result<PathBuf> {
    let home = std::env::var("FORGEFLEET_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| dirs_home().map(|h| h.join(".forgefleet")))
        .context("resolve FORGEFLEET_HOME / ~/.forgefleet")?;
    Ok(home.join("fleet.toml"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_endpoint_forms() {
        let b = parse("endpoint http://192.168.5.112:55008 glm-4.5-air", "auto")
            .await
            .unwrap();
        match b {
            Backend::Endpoint { base_url, model } => {
                assert_eq!(base_url, "http://192.168.5.112:55008");
                assert_eq!(model, "glm-4.5-air");
            }
            other => panic!("expected endpoint, got {}", other.label()),
        }
        // Trailing slash is stripped; model defaults.
        let b = parse("endpoint http://localhost:55000/", "auto")
            .await
            .unwrap();
        match b {
            Backend::Endpoint { base_url, model } => {
                assert_eq!(base_url, "http://localhost:55000");
                assert_eq!(model, "auto");
            }
            other => panic!("expected endpoint, got {}", other.label()),
        }
    }

    #[tokio::test]
    async fn parses_cloud_clis_and_rejects_unknown() {
        for cli in ["claude", "codex", "kimi"] {
            let b = parse(cli, "auto").await.unwrap();
            assert!(matches!(b, Backend::CloudCli { .. }));
            assert!(!b.is_agent());
        }
        assert!(parse("gemini", "auto").await.is_err());
        assert!(parse("endpoint", "auto").await.is_err());
    }

    #[tokio::test]
    async fn local_alias_maps_to_localhost_default() {
        let b = parse("local", "auto").await.unwrap();
        match b {
            Backend::Endpoint { base_url, .. } => {
                assert_eq!(base_url, "http://localhost:55000")
            }
            other => panic!("expected endpoint, got {}", other.label()),
        }
    }
}
