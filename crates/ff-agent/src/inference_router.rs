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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
        let endpoints = build_endpoint_list(config_path).await;
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
    pub fn active_url(&self) -> Option<String> {
        let state = self.failures.lock().unwrap();
        for ep in &self.endpoints {
            if !state.is_cooling_down(&ep.url, self.cooldown) {
                debug!(label = %ep.label, url = %ep.url, "InferenceRouter selected endpoint");
                return Some(ep.url.clone());
            }
        }
        // All endpoints are in cooldown — return the least-recently-failed one
        // so the caller can attempt a recovery request rather than giving up.
        self.endpoints
            .iter()
            .min_by_key(|ep| {
                state
                    .failed_at
                    .get(&ep.url)
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::MAX)
            })
            .map(|ep| {
                warn!(label = %ep.label, "all endpoints in cooldown — returning least-recently-failed");
                ep.url.clone()
            })
    }

    /// Mark an endpoint as failed. It will be skipped for `cooldown` seconds,
    /// then automatically reconsidered.
    pub fn report_failure(&self, url: &str) {
        let mut state = self.failures.lock().unwrap();
        state.mark_failed(url);
        warn!(
            url,
            cooldown_secs = self.cooldown.as_secs(),
            "endpoint marked as failed — will retry after cooldown"
        );
    }

    /// Mark an endpoint as healthy (clears any failure state).
    pub fn report_success(&self, url: &str) {
        let mut state = self.failures.lock().unwrap();
        state.clear(url);
    }

    /// Return a snapshot of all endpoints and their current health status.
    pub fn status(&self) -> Vec<(String, String, bool)> {
        let state = self.failures.lock().unwrap();
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
async fn build_endpoint_list(config_path: &Path) -> Vec<RouterEndpoint> {
    let mut local: Vec<RouterEndpoint> = Vec::new();
    let mut remote: Vec<RouterEndpoint> = Vec::new();

    // --- Local endpoints (this node's own LLM servers) ---
    // Probe ports 55000–55003; keep all that respond.
    for port in 55000u16..=55003 {
        if tcp_reachable("127.0.0.1", port).await {
            // Ask the server which model it's running so we can detect tool support.
            let model_id = fetch_first_model_id(&format!("http://127.0.0.1:{port}")).await;
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
    if let Ok(toml_str) = std::fs::read_to_string(config_path) {
        if let Ok(config) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str) {
            let db_url = config.database.url.trim().to_string();
            if !db_url.is_empty() {
                if let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
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

                        let node_models: Vec<_> =
                            models.iter().filter(|m| m.node_name == node.name).collect();
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
                                let fam = m.family.to_lowercase();
                                let id_lower = m.id.to_lowercase();
                                let name_lower = m.name.to_lowercase();
                                let is_gemma4 = fam.contains("gemma")
                                    && (id_lower.contains("gemma-4")
                                        || id_lower.contains("gemma4")
                                        || name_lower.contains("gemma-4")
                                        || name_lower.contains("gemma4"));
                                let supports_tools = fam.contains("qwen") || is_gemma4;
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
async fn fetch_first_model_id(base_url: &str) -> String {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    if let Ok(resp) = client.get(&url).send().await {
        if let Ok(body) = resp.text().await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(id) = v["data"][0]["id"].as_str() {
                    return id.to_string();
                }
            }
        }
    }
    "auto".into()
}

/// Heuristic: does this model ID suggest tool-calling support?
fn model_supports_tools(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    id.contains("qwen")
        || id.contains("gemma-4")
        || id.contains("gemma4")
        || id.contains("mistral")
        || id.contains("llama-3")
        || id == "auto"
}
