//! Local-first inference router with fleet fallback.
//!
//! Each ForgeFleet node uses its own LLM first — just as Claude Code uses Claude.
//! If the local LLM is unavailable, the router tries other fleet nodes in
//! priority order (tool-capable first, then by capacity).
//!
//! Endpoints are marked as failed for a cooldown window (60s) before being
//! retried, so the router heals automatically when a node comes back online.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use tracing::{debug, info, warn};

/// A single fleet LLM endpoint available for inference.
#[derive(Debug, Clone)]
pub struct RouterEndpoint {
    /// HTTP base URL, e.g. "http://127.0.0.1:55000"
    pub url: String,
    /// Model ID to request (e.g. "auto" or full path for MLX)
    pub model_id: String,
    /// Human label for logs (e.g. "local", "marcus", "taylor-gemma")
    pub label: String,
    /// Whether this endpoint supports OpenAI-compatible tool calling
    pub supports_tools: bool,
    /// True for the node's own LLM (localhost)
    pub is_local: bool,
}

/// Thread-safe state for tracking failed endpoints.
#[derive(Debug, Default)]
struct FailureState {
    /// url → time it was last marked as failed
    failed_at: HashMap<String, Instant>,
}

impl FailureState {
    /// True if the endpoint is currently in its failure cooldown.
    fn is_cooling_down(&self, url: &str, cooldown: Duration) -> bool {
        self.failed_at
            .get(url)
            .map(|t| t.elapsed() < cooldown)
            .unwrap_or(false)
    }

    fn mark_failed(&mut self, url: &str) {
        self.failed_at.insert(url.to_string(), Instant::now());
    }

    fn clear(&mut self, url: &str) {
        self.failed_at.remove(url);
    }
}

/// Routes LLM inference requests with local-first priority and fleet fallback.
#[derive(Debug)]
///
/// Ordering contract:
///   1. Local endpoints (is_local=true) always come first.
///   2. Within each group, tool-calling capable models rank above non-tool ones.
///   3. Within the same capability tier, higher-capacity nodes come first.
pub struct InferenceRouter {
    /// Ordered list of endpoints (local first, then fleet by priority).
    endpoints: Vec<RouterEndpoint>,
    /// Per-endpoint failure state (thread-safe).
    failures: Arc<Mutex<FailureState>>,
    /// How long to skip a failed endpoint before retrying it.
    cooldown: Duration,
}

impl InferenceRouter {
    /// Create a router with an explicit ordered endpoint list.
    pub fn new(endpoints: Vec<RouterEndpoint>) -> Self {
        Self {
            endpoints,
            failures: Arc::new(Mutex::new(FailureState::default())),
            cooldown: Duration::from_secs(60),
        }
    }

    /// Create a router from DB config + local probing.
    ///
    /// Probes each candidate endpoint via TCP within a short timeout and
    /// builds the ordered list. Always puts localhost first if reachable.
    pub async fn from_config(config_path: &Path) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default();
        let endpoints = build_endpoint_list(config_path, &client).await;
        info!(
            count = endpoints.len(),
            "InferenceRouter built: {}",
            endpoints
                .iter()
                .map(|e| e.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Self::new(endpoints)
    }

    /// Return the URL of the best currently-healthy endpoint, or None if all
    /// are in their failure cooldown window.
    ///
    /// Always favours local over remote, and tool-capable over non-tool.
    /// Call `report_failure` when a request to the returned URL fails.
    pub async fn active_url(&self) -> Option<String> {
        self.active_url_filtered(false).await
    }

    /// Like [`active_url`], but when `require_tools` is set, prefer endpoints
    /// whose model supports OpenAI-compatible tool calling. Only falls back to
    /// a non-tool endpoint when no tool-capable one is reachable, so the caller
    /// still gets liveness rather than `None`.
    ///
    /// The agent loop uses `require_tools = true` so a local non-tool model
    /// (e.g. gemma-4) never shadows a remote tool-capable one — the documented
    /// "agent dispatched to gemma-4 hangs silently" foot-gun. Local-first
    /// ordering is preserved *within* the tool-capable tier.
    pub async fn active_url_filtered(&self, require_tools: bool) -> Option<String> {
        let state = self.failures.lock().await;
        // First pass: when tools are required, only a healthy tool-capable
        // endpoint qualifies (still local-first within that tier).
        if require_tools {
            for ep in &self.endpoints {
                if ep.supports_tools && !state.is_cooling_down(&ep.url, self.cooldown) {
                    debug!(label = %ep.label, url = %ep.url, "InferenceRouter selected tool-capable endpoint");
                    return Some(ep.url.clone());
                }
            }
        }
        // Second pass: any healthy endpoint (or the only pass when tools aren't
        // required). Keeps liveness when no tool-capable node is reachable.
        for ep in &self.endpoints {
            if !state.is_cooling_down(&ep.url, self.cooldown) {
                debug!(label = %ep.label, url = %ep.url, "InferenceRouter selected endpoint");
                return Some(ep.url.clone());
            }
        }
        // All endpoints are in cooldown — return the least-recently-failed one
        // so the caller can attempt a recovery request rather than giving up.
        // Prefer a tool-capable endpoint here too when tools are required.
        self.endpoints
            .iter()
            .filter(|ep| !require_tools || ep.supports_tools)
            .min_by_key(|ep| {
                state
                    .failed_at
                    .get(&ep.url)
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::MAX)
            })
            .or_else(|| {
                // No tool-capable endpoint at all — fall back to any.
                self.endpoints.iter().min_by_key(|ep| {
                    state
                        .failed_at
                        .get(&ep.url)
                        .map(|t| t.elapsed())
                        .unwrap_or(Duration::MAX)
                })
            })
            .map(|ep| {
                warn!(label = %ep.label, "all endpoints in cooldown — returning least-recently-failed");
                ep.url.clone()
            })
    }

    /// Mark an endpoint as failed. It will be skipped for `cooldown` seconds,
    /// then automatically reconsidered.
    pub async fn report_failure(&self, url: &str) {
        let mut state = self.failures.lock().await;
        state.mark_failed(url);
        warn!(
            url,
            cooldown_secs = self.cooldown.as_secs(),
            "endpoint marked as failed — will retry after cooldown"
        );
    }

    /// Mark an endpoint as healthy (clears any failure state).
    pub async fn report_success(&self, url: &str) {
        let mut state = self.failures.lock().await;
        state.clear(url);
    }

    /// Return a snapshot of all endpoints and their current health status.
    pub async fn status(&self) -> Vec<(String, String, bool)> {
        let state = self.failures.lock().await;
        self.endpoints
            .iter()
            .map(|ep| {
                let healthy = !state.is_cooling_down(&ep.url, self.cooldown);
                (ep.label.clone(), ep.url.clone(), healthy)
            })
            .collect()
    }

    /// Number of endpoints in the router.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// True if there are no endpoints.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Endpoint discovery
// ---------------------------------------------------------------------------

/// Build the ordered endpoint list: local first, then fleet from Postgres.
async fn build_endpoint_list(config_path: &Path, client: &reqwest::Client) -> Vec<RouterEndpoint> {
    let mut local: Vec<RouterEndpoint> = Vec::new();
    let mut remote: Vec<RouterEndpoint> = Vec::new();

    // --- Local endpoints (this node's own LLM servers) ---
    // Probe ports 55000–55003; keep all that respond.
    for port in 55000u16..=55003 {
        if tcp_reachable("127.0.0.1", port).await {
            // Ask the server which model it's running so we can detect tool support.
            let model_id = fetch_first_model_id(&format!("http://127.0.0.1:{port}"), client).await;
            let supports_tools = model_supports_tools(&model_id);
            local.push(RouterEndpoint {
                url: format!("http://127.0.0.1:{port}"),
                model_id,
                label: format!("local:{port}"),
                supports_tools,
                is_local: true,
            });
        }
    }

    // Sort local: tool-capable first
    local.sort_by(|a, b| b.supports_tools.cmp(&a.supports_tools));

    // --- Remote endpoints from Postgres ---
    if let Ok(toml_str) = tokio::fs::read_to_string(config_path).await
        && let Ok(config) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str)
    {
        let db_url = config.database.url.trim().to_string();
        if !db_url.is_empty()
            && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(3))
                .connect(&db_url)
                .await
        {
            let nodes = ff_db::pg_list_nodes(&pool).await.unwrap_or_default();
            let models = ff_db::pg_list_models(&pool).await.unwrap_or_default();

            // Collect (ip, port, cores, supports_tools, label, model_id)
            let mut candidates: Vec<(String, u16, i32, bool, String, String)> = Vec::new();

            for node in &nodes {
                // Skip if this is the local node (already covered above)
                if is_local_node(&node.ip) {
                    continue;
                }

                let node_models: Vec<_> = models
                    .iter()
                    .filter(|m| m.worker_name == node.name)
                    .collect();
                if node_models.is_empty() {
                    candidates.push((
                        node.ip.clone(),
                        55000,
                        node.cpu_cores,
                        true, // assume capable if we don't know
                        node.name.clone(),
                        "auto".into(),
                    ));
                } else {
                    for m in node_models {
                        // DB-first: trust the `preferred_workloads` tags
                        // (synced from the catalog's `tool_calling` flag). Only
                        // fall back to the model-name heuristic when the DB is
                        // silent, so unsynced rows still route. This is what
                        // keeps gemma-4 — tagged non-tool in the catalog — out
                        // of the tool-capable tier.
                        let supports_tools = workloads_tool_capable(&m.preferred_workloads)
                            || model_supports_tools(&m.id);
                        candidates.push((
                            node.ip.clone(),
                            m.port as u16,
                            node.cpu_cores,
                            supports_tools,
                            format!("{}:{}", node.name, m.port),
                            m.id.clone(),
                        ));
                    }
                }
            }

            // Sort: tool-capable first, then by cpu_cores desc
            candidates.sort_by(|a, b| b.3.cmp(&a.3).then(b.2.cmp(&a.2)));

            // Probe reachability (parallel, short timeout)
            let probe_futs: Vec<_> = candidates
                .iter()
                .map(|(ip, port, _, supports_tools, label, model_id)| {
                    let ip = ip.clone();
                    let label = label.clone();
                    let model_id = model_id.clone();
                    let st = *supports_tools;
                    let port = *port;
                    async move {
                        if tcp_reachable(&ip, port).await {
                            Some(RouterEndpoint {
                                url: format!("http://{ip}:{port}"),
                                model_id,
                                label,
                                supports_tools: st,
                                is_local: false,
                            })
                        } else {
                            None
                        }
                    }
                })
                .collect();

            let results = futures::future::join_all(probe_futs).await;
            for ep in results.into_iter().flatten() {
                remote.push(ep);
            }
        }
    }

    // Final order: local tool-capable → local non-tool → remote tool-capable → remote non-tool
    let mut all = local;
    all.extend(remote);
    all
}

/// True if an IP address refers to the local machine.
fn is_local_node(ip: &str) -> bool {
    ip == "127.0.0.1" || ip == "::1" || ip == "localhost" || {
        // Check if this is one of our own network IPs
        use std::net::ToSocketAddrs;
        if let Ok(addrs) = format!("{ip}:0").to_socket_addrs() {
            addrs.into_iter().any(|a| a.ip().is_loopback())
        } else {
            false
        }
    }
}

/// TCP reachability probe with a 300ms timeout.
async fn tcp_reachable(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    tokio::time::timeout(
        Duration::from_millis(300),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Query /v1/models and return the first model ID, or "auto" on error.
async fn fetch_first_model_id(base_url: &str, client: &reqwest::Client) -> String {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    if let Ok(resp) = client.get(&url).send().await
        && let Ok(body) = resp.text().await
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
        && let Some(id) = v["data"][0]["id"].as_str()
    {
        return id.to_string();
    }
    "auto".into()
}

/// True if a model's `preferred_workloads` JSONB array tags it as agent /
/// tool-calling capable. This is the DB-first source of truth (the tags are
/// synced from the catalog's `tool_calling` flag); see the `agent` /
/// `tool_calling` synonym cluster in `ff_db`.
fn workloads_tool_capable(workloads: &serde_json::Value) -> bool {
    workloads
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|v| matches!(v.as_str(), Some("tool_calling") | Some("agent")))
        })
        .unwrap_or(false)
}

/// Name-based fallback heuristic for when the DB has no workload tags (e.g. a
/// local server we only know by `/v1/models` id). Deliberately excludes
/// gemma-4: per `feedback_gemma4_no_tools`, gemma-4 MLX does not reliably
/// tool-call, and routing an agent there hangs it silently.
fn model_supports_tools(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    id.contains("qwen") || id.contains("mistral") || id.contains("llama-3") || id == "auto"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ep(label: &str, supports_tools: bool, is_local: bool) -> RouterEndpoint {
        RouterEndpoint {
            url: format!("http://{label}"),
            model_id: label.into(),
            label: label.into(),
            supports_tools,
            is_local,
        }
    }

    #[test]
    fn gemma4_is_not_tool_capable_by_name() {
        // The documented foot-gun: gemma-4 must NOT be flagged tool-capable.
        assert!(!model_supports_tools("gemma-4-27b"));
        assert!(!model_supports_tools("gemma4"));
        // While genuinely tool-capable families still pass.
        assert!(model_supports_tools("qwen3-coder-30b"));
        assert!(model_supports_tools("mistral-large"));
        assert!(model_supports_tools("llama-3.1-70b"));
        assert!(model_supports_tools("auto"));
    }

    #[test]
    fn workloads_drive_tool_capability() {
        assert!(workloads_tool_capable(&json!(["chat", "tool_calling"])));
        assert!(workloads_tool_capable(&json!(["agent"])));
        assert!(!workloads_tool_capable(&json!(["chat", "summarize"])));
        assert!(!workloads_tool_capable(&json!([])));
        assert!(!workloads_tool_capable(&json!(null)));
    }

    #[tokio::test]
    async fn require_tools_skips_local_non_tool_endpoint() {
        // Local gemma-4 (non-tool) listed first, remote qwen (tool) second —
        // exactly the live taylor layout that used to hang the agent.
        let router = InferenceRouter::new(vec![
            ep("local-gemma", false, true),
            ep("remote-qwen", true, false),
        ]);
        // Tool-requiring caller must reach the remote tool-capable node.
        assert_eq!(
            router.active_url_filtered(true).await.as_deref(),
            Some("http://remote-qwen")
        );
        // Non-tool caller keeps the old local-first behavior.
        assert_eq!(
            router.active_url_filtered(false).await.as_deref(),
            Some("http://local-gemma")
        );
    }

    #[tokio::test]
    async fn require_tools_falls_back_when_none_tool_capable() {
        // No tool-capable endpoint anywhere — liveness wins; return something.
        let router = InferenceRouter::new(vec![ep("local-gemma", false, true)]);
        assert_eq!(
            router.active_url_filtered(true).await.as_deref(),
            Some("http://local-gemma")
        );
    }

    #[tokio::test]
    async fn require_tools_prefers_local_tool_capable() {
        // A local tool-capable node still beats a remote one (local-first
        // within the tool-capable tier).
        let router = InferenceRouter::new(vec![
            ep("local-qwen", true, true),
            ep("remote-qwen", true, false),
        ]);
        assert_eq!(
            router.active_url_filtered(true).await.as_deref(),
            Some("http://local-qwen")
        );
    }
}
