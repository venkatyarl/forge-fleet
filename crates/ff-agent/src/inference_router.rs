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
    /// Model strength tier (1–4, higher = stronger: T1≈9B, T2≈30B, T3≈70B,
    /// T4≈235B+). Used as the secondary sort key for agent dispatch so a weak
    /// model (a T1 9B that stalls the tool loop) never shadows a genuinely
    /// agent-viable one. 0 means unknown (no DB row). See
    /// `project_fleet_agent_swarm_broken`.
    pub tier: i32,
    /// True for the node's own LLM (localhost)
    pub is_local: bool,
    /// Per-slot context window (`/props` `n_ctx`) when known. The SAME model is
    /// served at wildly different ctx across the fleet (qwen36: 4096 on one node,
    /// 32768 on another), so routing a code/agent task without considering this
    /// can land it on a 4K slot that overflows reading one file. `None` = not
    /// probed (treated as "unknown", not excluded). See
    /// `active_url_filtered`'s `min_ctx` preference.
    pub n_ctx: Option<i32>,
}

/// Probe a llama.cpp-style endpoint's `/props` for its per-slot `n_ctx`.
/// Returns `None` for non-llama servers (no `/props`) or any failure — callers
/// treat unknown ctx as "don't exclude" so liveness is never sacrificed.
async fn fetch_n_ctx(base_url: &str, client: &reqwest::Client) -> Option<i32> {
    let v: serde_json::Value = client
        .get(format!("{}/props", base_url.trim_end_matches('/')))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v.get("default_generation_settings")
        .and_then(|s| s.get("n_ctx"))
        .or_else(|| v.get("n_ctx"))
        .and_then(|n| n.as_i64())
        .filter(|&n| n > 0)
        .map(|n| n as i32)
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
///   3. Within the same capability tier, stronger models (higher `tier`) rank
///      first, then higher-capacity nodes — so agent dispatch never falls back
///      to a weak T1 model that stalls the tool loop.
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
        self.active_url_min_ctx(false, 0).await
    }

    /// Back-compat: tool-preference selection with no context floor.
    pub async fn active_url_filtered(&self, require_tools: bool) -> Option<String> {
        self.active_url_min_ctx(require_tools, 0).await
    }

    /// Select a healthy endpoint, preferring (in order): tool-capable **and**
    /// adequate context → tool-capable → adequate context → any — always
    /// falling back to liveness rather than `None`.
    ///
    /// `min_ctx > 0` makes a code/agent run avoid a small per-slot ctx (e.g. a
    /// 4096 qwen36 slot) that overflows reading one file, even though the model
    /// is otherwise capable. Endpoints with UNKNOWN `n_ctx` are NOT excluded —
    /// only a positive under-min signal de-prioritises them — so a fleet that
    /// never answered `/props` still routes.
    ///
    /// `require_tools = true` keeps a local non-tool model (e.g. gemma-4) from
    /// shadowing a remote tool-capable one. Local-first within each tier.
    pub async fn active_url_min_ctx(&self, require_tools: bool, min_ctx: i32) -> Option<String> {
        let state = self.failures.lock().await;
        let meets_ctx =
            |ep: &RouterEndpoint| min_ctx <= 0 || ep.n_ctx.map(|c| c >= min_ctx).unwrap_or(false);

        // Pass 1 (best): tool-capable AND adequate ctx.
        if require_tools && min_ctx > 0 {
            for ep in &self.endpoints {
                if ep.supports_tools
                    && meets_ctx(ep)
                    && !state.is_cooling_down(&ep.url, self.cooldown)
                {
                    debug!(label = %ep.label, url = %ep.url, n_ctx = ?ep.n_ctx, "InferenceRouter selected tool+ctx endpoint");
                    return Some(ep.url.clone());
                }
            }
        }
        // Pass 2: when tools are required, any healthy tool-capable endpoint
        // (still local-first within that tier).
        if require_tools {
            for ep in &self.endpoints {
                if ep.supports_tools && !state.is_cooling_down(&ep.url, self.cooldown) {
                    debug!(label = %ep.label, url = %ep.url, "InferenceRouter selected tool-capable endpoint");
                    return Some(ep.url.clone());
                }
            }
        }
        // Pass 3: adequate ctx (any), when a ctx floor was requested.
        if min_ctx > 0 {
            for ep in &self.endpoints {
                if meets_ctx(ep) && !state.is_cooling_down(&ep.url, self.cooldown) {
                    debug!(label = %ep.label, url = %ep.url, n_ctx = ?ep.n_ctx, "InferenceRouter selected adequate-ctx endpoint");
                    return Some(ep.url.clone());
                }
            }
        }
        // Final pass: any healthy endpoint (or the only pass when tools aren't
        // required). Keeps liveness when no tool-capable node is reachable.
        for ep in &self.endpoints {
            if !state.is_cooling_down(&ep.url, self.cooldown) {
                debug!(label = %ep.label, url = %ep.url, "InferenceRouter selected endpoint");
                return Some(ep.url.clone());
            }
        }
        // All endpoints are in cooldown — return the least-recently-failed one
        // so the caller can attempt a recovery request rather than giving up.
        // "Least-recently-failed" = the one whose last failure is furthest in
        // the past (largest time-since-failure, or never failed), since it is
        // the most likely to have recovered and its cooldown is closest to
        // expiring. Prefer a tool-capable endpoint when tools are required.
        let pool: Vec<&RouterEndpoint> = if require_tools {
            let tools: Vec<&RouterEndpoint> = self
                .endpoints
                .iter()
                .filter(|ep| ep.supports_tools)
                .collect();
            // No tool-capable endpoint at all — fall back to any.
            if tools.is_empty() {
                self.endpoints.iter().collect()
            } else {
                tools
            }
        } else {
            self.endpoints.iter().collect()
        };
        let since_failure: Vec<Option<Duration>> = pool
            .iter()
            .map(|ep| state.failed_at.get(&ep.url).map(|t| t.elapsed()))
            .collect();
        recovery_pick_idx(&since_failure).map(|i| {
            let ep = pool[i];
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
            let base = format!("http://127.0.0.1:{port}");
            let model_id = fetch_first_model_id(&base, client).await;
            let supports_tools = model_supports_tools(&model_id);
            let n_ctx = fetch_n_ctx(&base, client).await;
            local.push(RouterEndpoint {
                url: base,
                model_id,
                label: format!("local:{port}"),
                supports_tools,
                tier: 0, // unknown until the DB override below fills it in
                is_local: true,
                n_ctx,
            });
        }
    }

    // Sort local: tool-capable first (tier is still unknown here — the DB
    // override re-sorts by strength once the catalog loads).
    sort_endpoints(&mut local);

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

            // DB-first parity for LOCAL endpoints. The local probe above only
            // had the model-name heuristic to go on (it has no DB row in hand
            // at probe time). Now that we have the catalog, override each local
            // endpoint's tool-capability from its `preferred_workloads` tags —
            // the same source of truth the remote tier uses below. This keeps a
            // non-tool local model (e.g. gemma-4 on taylor) out of the
            // tool-capable tier even if the name heuristic is ever loosened.
            // See feedback_ff_supervise_llm_routing / feedback_gemma4_no_tools.
            let local_db: HashMap<u16, (bool, i32)> = models
                .iter()
                .filter(|m| {
                    nodes
                        .iter()
                        .any(|n| n.name == m.worker_name && is_local_node(&n.ip))
                })
                .map(|m| {
                    let st = workloads_tool_capable(&m.preferred_workloads)
                        || model_supports_tools(&m.id);
                    (m.port as u16, (st, m.tier))
                })
                .collect();
            apply_local_db_metadata(&mut local, &local_db);

            // Collect (ip, port, cores, supports_tools, tier, label, model_id)
            let mut candidates: Vec<(String, u16, i32, bool, i32, String, String)> = Vec::new();

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
                        true,         // assume capable if we don't know
                        ASSUMED_TIER, // assume a mid (agent-viable) tier too
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
                            m.tier,
                            format!("{}:{}", node.name, m.port),
                            m.id.clone(),
                        ));
                    }
                }
            }

            // Sort: tool-capable first, then strongest model (tier desc), then
            // by cpu_cores desc. The tier key is what stops a weak T1 9B on a
            // high-core node from out-ranking a T2/T3 coder — the documented
            // agent-swarm stall (project_fleet_agent_swarm_broken).
            candidates.sort_by(|a, b| b.3.cmp(&a.3).then(b.4.cmp(&a.4)).then(b.2.cmp(&a.2)));

            // Probe reachability (parallel, short timeout)
            let probe_futs: Vec<_> = candidates
                .iter()
                .map(|(ip, port, _, supports_tools, tier, label, model_id)| {
                    let ip = ip.clone();
                    let label = label.clone();
                    let model_id = model_id.clone();
                    let st = *supports_tools;
                    let tier = *tier;
                    let port = *port;
                    let client = client.clone();
                    async move {
                        if tcp_reachable(&ip, port).await {
                            let base = format!("http://{ip}:{port}");
                            // Probe /props for the per-slot n_ctx so routing can
                            // avoid landing a code task on a 4K slot of an
                            // otherwise-capable model.
                            let n_ctx = fetch_n_ctx(&base, &client).await;
                            Some(RouterEndpoint {
                                url: base,
                                model_id,
                                label,
                                supports_tools: st,
                                tier,
                                is_local: false,
                                n_ctx,
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

/// Tier assumed for a reachable node we have no catalog row for ("assume
/// capable if we don't know"). A mid (agent-viable) value so an unknown node
/// competes fairly with known T2 coders instead of being buried or favoured.
const ASSUMED_TIER: i32 = 2;

/// Order a group of endpoints for selection: tool-capable first, then strongest
/// model (`tier` desc). This is the shared comparator for the local and remote
/// groups so agent dispatch (`active_url_filtered(true)`) picks the strongest
/// tool-capable endpoint in the group rather than the first one probed. A weak
/// T1 9B therefore never shadows a T2/T3 coder. See
/// `project_fleet_agent_swarm_broken`.
fn sort_endpoints(eps: &mut [RouterEndpoint]) {
    eps.sort_by(|a, b| {
        b.supports_tools
            .cmp(&a.supports_tools)
            .then(b.tier.cmp(&a.tier))
    });
}

/// Override the local endpoints' tool-capability and strength tier from a
/// DB-derived port→(supports_tools, tier) map (DB-first). Endpoints whose port
/// isn't in the map keep their probe-time (name-heuristic) tool value and
/// unknown tier (0), so unsynced local servers still route. Re-sorts the group
/// (tool-capable first, then strongest) after applying. See
/// `feedback_ff_supervise_llm_routing.md`.
fn apply_local_db_metadata(local: &mut [RouterEndpoint], db_by_port: &HashMap<u16, (bool, i32)>) {
    for ep in local.iter_mut() {
        if let Some(port) = ep
            .url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            && let Some(&(st, tier)) = db_by_port.get(&port)
        {
            ep.supports_tools = st;
            ep.tier = tier;
        }
    }
    sort_endpoints(local);
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

/// All-cooling-down recovery: index of the endpoint that failed *longest ago*
/// (or never), which is the most likely to have recovered and whose cooldown is
/// closest to expiring. `since_failure[i]` is the time since endpoint `i` last
/// failed, or `None` if it has no recorded failure (treated as infinitely long
/// ago — i.e. most preferred). Returns `None` only for an empty slice.
fn recovery_pick_idx(since_failure: &[Option<Duration>]) -> Option<usize> {
    since_failure
        .iter()
        .enumerate()
        .max_by_key(|(_, since)| since.unwrap_or(Duration::MAX))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ep(label: &str, supports_tools: bool, is_local: bool) -> RouterEndpoint {
        ep_t(label, supports_tools, is_local, ASSUMED_TIER)
    }

    fn ep_t(label: &str, supports_tools: bool, is_local: bool, tier: i32) -> RouterEndpoint {
        RouterEndpoint {
            url: format!("http://{label}"),
            model_id: label.into(),
            label: label.into(),
            supports_tools,
            tier,
            is_local,
            n_ctx: None,
        }
    }

    fn ep_ctx(label: &str, supports_tools: bool, n_ctx: i32) -> RouterEndpoint {
        RouterEndpoint {
            n_ctx: Some(n_ctx),
            ..ep_t(label, supports_tools, false, ASSUMED_TIER)
        }
    }

    #[tokio::test]
    async fn min_ctx_skips_small_slots_for_code_runs() {
        // Same model served at different per-slot ctx: a code run (min_ctx=32K)
        // must NOT land on the 4K slot even though it's first / tool-capable.
        let router = InferenceRouter::new(vec![
            ep_ctx("veronica-4k", true, 4096),
            ep_ctx("lily-8k", true, 8192),
            ep_ctx("logan-32k", true, 32768),
        ]);
        // ctx-aware: picks the 32K endpoint (skips 4K/8K).
        assert_eq!(
            router.active_url_min_ctx(true, 32768).await.as_deref(),
            Some("http://logan-32k"),
        );
        // ctx-blind (min_ctx=0): old behaviour — first tool-capable wins (4K).
        assert_eq!(
            router.active_url_min_ctx(true, 0).await.as_deref(),
            Some("http://veronica-4k"),
        );
    }

    #[tokio::test]
    async fn min_ctx_keeps_liveness_when_none_meet_floor() {
        // No tool-capable endpoint meets the 32K floor → the ctx passes find
        // nothing, but the tool-capable liveness pass still returns one (better
        // a small slot than failing the run with None).
        let router = InferenceRouter::new(vec![
            ep_ctx("small-8k", true, 8192),
            ep_t("unknown-ctx", true, false, ASSUMED_TIER), // n_ctx None
        ]);
        let got = router.active_url_min_ctx(true, 32768).await;
        assert!(got.is_some(), "must keep liveness, got None");
        assert!(matches!(
            got.as_deref(),
            Some("http://small-8k") | Some("http://unknown-ctx")
        ));

        // Only a small slot exists → still liveness, not None.
        let only_small = InferenceRouter::new(vec![ep_ctx("small-8k", true, 8192)]);
        assert_eq!(
            only_small.active_url_min_ctx(true, 32768).await.as_deref(),
            Some("http://small-8k"),
        );
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

    #[test]
    fn local_db_override_demotes_name_heuristic_false_positive() {
        // Probe-time: local server on 55000 looked tool-capable by name
        // (the name heuristic's fallback), but the DB knows it's non-tool
        // (e.g. gemma-4 tagged chat-only). The DB override must demote it.
        let mut local = vec![ep("127.0.0.1:55000", true, true)];
        let mut by_port = HashMap::new();
        by_port.insert(55000u16, (false, 2));
        apply_local_db_metadata(&mut local, &by_port);
        assert!(!local[0].supports_tools);
    }

    #[test]
    fn local_db_override_resorts_tool_capable_first() {
        // Two local servers; the DB promotes 55001 to tool-capable. After the
        // override the tool-capable endpoint must sort ahead.
        let mut local = vec![
            ep("127.0.0.1:55000", false, true),
            ep("127.0.0.1:55001", false, true),
        ];
        let mut by_port = HashMap::new();
        by_port.insert(55001u16, (true, 2));
        apply_local_db_metadata(&mut local, &by_port);
        assert_eq!(local[0].label, "127.0.0.1:55001");
        assert!(local[0].supports_tools);
    }

    #[test]
    fn local_db_override_keeps_heuristic_when_port_absent() {
        // A local server with no matching DB row keeps its probe-time value,
        // so unsynced local servers still route.
        let mut local = vec![ep("127.0.0.1:55002", true, true)];
        let by_port = HashMap::new(); // empty → no override
        apply_local_db_metadata(&mut local, &by_port);
        assert!(local[0].supports_tools);
    }

    #[test]
    fn local_db_override_applies_tier_and_sorts_strongest_first() {
        // Both local servers are tool-capable; the DB tags 55001 as a stronger
        // T3 model and 55000 as a weak T1. After the override the strongest
        // tool-capable endpoint must sort ahead.
        let mut local = vec![
            ep_t("127.0.0.1:55000", true, true, 0),
            ep_t("127.0.0.1:55001", true, true, 0),
        ];
        let mut by_port = HashMap::new();
        by_port.insert(55000u16, (true, 1));
        by_port.insert(55001u16, (true, 3));
        apply_local_db_metadata(&mut local, &by_port);
        assert_eq!(local[0].label, "127.0.0.1:55001");
        assert_eq!(local[0].tier, 3);
    }

    #[test]
    fn sort_endpoints_prefers_stronger_tier_within_tool_tier() {
        // The documented agent-swarm stall: a weak T1 9B must NOT out-rank a
        // T2 coder. Both are tool-capable, so tier breaks the tie.
        let mut eps = vec![
            ep_t("james-9b", true, false, 1),
            ep_t("sophie-32b", true, false, 2),
            ep_t("taylor-gemma", false, false, 2), // non-tool, ranks last
        ];
        sort_endpoints(&mut eps);
        assert_eq!(eps[0].label, "sophie-32b");
        assert_eq!(eps[1].label, "james-9b");
        assert_eq!(eps[2].label, "taylor-gemma");
    }

    #[tokio::test]
    async fn agent_dispatch_picks_strongest_tool_capable_endpoint() {
        // End-to-end of the fix: given a sorted endpoint list (strongest
        // tool-capable first, as build_endpoint_list now produces), a
        // tool-requiring caller reaches the T2 coder, not the weak T1 9B.
        let mut eps = vec![
            ep_t("james-9b", true, false, 1),
            ep_t("sophie-32b", true, false, 2),
        ];
        sort_endpoints(&mut eps);
        let router = InferenceRouter::new(eps);
        assert_eq!(
            router.active_url_filtered(true).await.as_deref(),
            Some("http://sophie-32b")
        );
    }

    #[test]
    fn recovery_prefers_the_endpoint_that_failed_longest_ago() {
        // Regression: the all-cooling-down fallback used min_by_key, which
        // returned the *most* recently failed endpoint (still broken). It must
        // pick the one whose failure is furthest in the past.
        let since = [
            Some(Duration::from_secs(2)),  // just failed — worst choice
            Some(Duration::from_secs(59)), // failed long ago — most recovered
            Some(Duration::from_secs(10)),
        ];
        assert_eq!(recovery_pick_idx(&since), Some(1));
    }

    #[test]
    fn recovery_prefers_a_never_failed_endpoint() {
        // `None` (no recorded failure) is treated as infinitely long ago.
        let since = [
            Some(Duration::from_secs(59)),
            None,
            Some(Duration::from_secs(2)),
        ];
        assert_eq!(recovery_pick_idx(&since), Some(1));
    }

    #[test]
    fn recovery_empty_slice_is_none() {
        assert_eq!(recovery_pick_idx(&[]), None);
    }
}
