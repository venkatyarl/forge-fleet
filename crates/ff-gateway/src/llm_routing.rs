//! Pulse-v2-backed LLM request routing.
//!
//! This module replaces the older `inference_router` logic for the new
//! `/fleet/chat/completions` (and optionally `/v2/chat/completions`) endpoints.
//! Instead of consulting the ff-api `BackendRegistry`, it reads live LLM-server
//! state directly from Redis via [`ff_pulse::reader::PulseReader`], so any
//! fleet node that is currently beating with an active+healthy LLM server is
//! immediately routable — no explicit backend configuration required.
//!
//! Key differences from `proxy_chat_completions`:
//! - Source of truth is Redis Pulse beats (ephemeral fleet state), not a
//!   statically configured registry.
//! - Model-name matching is **case-insensitive prefix** match against each
//!   server's reported `model.id`. That way a request for `Qwen3-Coder-30B-A3B`
//!   can land on a server whose model id is `Qwen3-Coder-30B-A3B-Q4_K_M`
//!   (llama.cpp) or `qwen3-coder-30b-a3b:latest` (ollama).
//! - Candidate selection breaks ties by lowest `queue_depth`, then highest
//!   `tokens_per_sec_last_min`.
//! - When no candidate is found we report the list of loaded models fleet-wide
//!   so the caller sees what they *could* have asked for.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{RwLock, watch};
use tokio::task::JoinHandle;

use ff_pulse::beat_v2::{LlmServer, PulseBeatV2};
use ff_pulse::reader::{PulseError, PulseReader};

/// Errors returned by [`route_completion`].
#[derive(Debug, Error)]
pub enum LlmRoutingError {
    #[error("pulse: {0}")]
    Pulse(#[from] PulseError),

    #[error("missing `model` field on request")]
    MissingModel,

    /// No active+healthy LLM server in the fleet matches the requested model.
    #[error("no server has model '{requested}' loaded")]
    NoMatch {
        requested: String,
        available: Vec<String>,
    },

    #[error("upstream request failed: {0}")]
    Upstream(#[from] reqwest::Error),

    #[error("upstream timed out after {0:?}")]
    Timeout(Duration),
}

// ─── Session affinity (KV-cache / prefix-cache aware routing) ───────────────

/// How long a session→node mapping stays valid without a follow-up request.
const SESSION_AFFINITY_TTL: Duration = Duration::from_secs(300);
/// Hard cap on the number of affinity entries to prevent unbounded growth.
const SESSION_AFFINITY_MAX_ENTRIES: usize = 10_000;
/// If the preferred node has a queue_depth higher than this we route
/// elsewhere to avoid overloading a hot node, even at the cost of a
/// prefix-cache miss.
const SESSION_AFFINITY_QUEUE_THRESHOLD: i32 = 10;

// ─── Circuit breaker (failure-aware routing) ────────────────────────────────

/// Rolling window for failure counting.
const CIRCUIT_WINDOW: Duration = Duration::from_secs(60);
/// How many failures within [`CIRCUIT_WINDOW`] trip the breaker.
const CIRCUIT_THRESHOLD: usize = 3;
/// How long the breaker stays open before allowing probe traffic.
const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct FailureWindow {
    failures: Vec<Instant>,
}

impl FailureWindow {
    fn new() -> Self {
        Self {
            failures: Vec::with_capacity(CIRCUIT_THRESHOLD + 1),
        }
    }

    fn record(&mut self) {
        self.failures.push(Instant::now());
        self.prune();
    }

    fn prune(&mut self) {
        self.failures.retain(|t| t.elapsed() < CIRCUIT_WINDOW);
    }

    fn count_recent(&self) -> usize {
        self.failures
            .iter()
            .filter(|t| t.elapsed() < CIRCUIT_WINDOW)
            .count()
    }

    fn is_open(&self) -> bool {
        self.count_recent() >= CIRCUIT_THRESHOLD
    }

    fn last_failure(&self) -> Option<Instant> {
        self.failures.last().copied()
    }

    /// True if the breaker is open AND still within the cooldown period.
    fn is_open_and_cooling(&self) -> bool {
        if !self.is_open() {
            return false;
        }
        self.last_failure()
            .map(|t| t.elapsed() < CIRCUIT_COOLDOWN)
            .unwrap_or(false)
    }
}

/// Per-node circuit breaker for the LLM router.
///
/// When an upstream inference server starts failing (connection refused,
/// timeout, 5xx) the breaker temporarily blocks further traffic to that
/// node even if its Pulse heartbeat still reports healthy. This prevents
/// a half-dead node from absorbing requests and failing them.
#[derive(Clone)]
pub struct CircuitBreaker {
    inner: Arc<DashMap<String, FailureWindow>>,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Record a failure for `computer`.
    pub fn record_failure(&self, computer: &str) {
        self.inner
            .entry(computer.to_string())
            .or_insert_with(FailureWindow::new)
            .record();
        tracing::debug!(
            computer = %computer,
            "pulse: circuit breaker recorded failure"
        );
    }

    /// Returns `true` if `computer` should be avoided (breaker is open and
    /// still cooling down).
    pub fn is_open(&self, computer: &str) -> bool {
        self.inner
            .get(computer)
            .map(|w| w.is_open_and_cooling())
            .unwrap_or(false)
    }

    /// Diagnostic view: every tracked node and its current failure count.
    pub fn snapshot(&self) -> Vec<(String, usize, bool)> {
        self.inner
            .iter()
            .map(|e| {
                let w = e.value();
                let open = w.is_open_and_cooling();
                (e.key().clone(), w.count_recent(), open)
            })
            .collect()
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct AffinityEntry {
    pub computer: String,
    pub model_id: String,
    pub last_used: Instant,
}

/// In-memory conversation→node affinity cache.
///
/// When a multi-turn conversation hits the gateway, routing follow-up
/// turns to the same node that handled the previous turn yields a
/// prefix-cache hit (the KV cache for the prior context is already
/// resident). This dramatically reduces latency for chat sessions.
///
/// The cache is keyed by a session identifier extracted from the request
/// body (see [`extract_session_key`]). Entries auto-expire after
/// [`SESSION_AFFINITY_TTL`] of inactivity and are evicted when the
/// capacity exceeds [`SESSION_AFFINITY_MAX_ENTRIES`].
#[derive(Clone)]
pub struct SessionAffinityCache {
    inner: Arc<DashMap<String, AffinityEntry>>,
}

impl SessionAffinityCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Look up an affinity entry. Returns `None` if expired or missing.
    pub fn get(&self, key: &str) -> Option<AffinityEntry> {
        let entry = self.inner.get(key)?;
        if entry.last_used.elapsed() > SESSION_AFFINITY_TTL {
            drop(entry);
            self.inner.remove(key);
            return None;
        }
        Some(entry.clone())
    }

    /// Store (or refresh) an affinity mapping.
    pub fn set(&self, key: String, computer: String, model_id: String) {
        if self.inner.len() > SESSION_AFFINITY_MAX_ENTRIES {
            // Simple eviction: drop the first entry we can grab.
            if let Some(first) = self.inner.iter().next() {
                let k = first.key().clone();
                drop(first);
                self.inner.remove(&k);
            }
        }
        self.inner.insert(
            key,
            AffinityEntry {
                computer,
                model_id,
                last_used: Instant::now(),
            },
        );
    }

    /// Bump the `last_used` timestamp for an existing entry.
    pub fn refresh(&self, key: &str) {
        if let Some(mut entry) = self.inner.get_mut(key) {
            entry.last_used = Instant::now();
        }
    }

    /// Current number of tracked affinity entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when no affinity entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for SessionAffinityCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Derive a stable session key from the request body.
///
/// Priority:
/// 1. Top-level `session_id` field (ForgeFleet extension).
/// 2. Hash of `model` + first user message content (standard OpenAI-style
///    chat where the full history is sent each turn).
fn extract_session_key(body: &Value) -> Option<String> {
    // 1. Explicit session identifier.
    if let Some(id) = body.get("session_id").and_then(|v| v.as_str()) {
        return Some(format!("sess:{id}"));
    }

    // 2. Derived from model + first user message.
    let model = body.get("model").and_then(|v| v.as_str())?;
    let messages = body.get("messages")?.as_array()?;
    let first_user_content = messages
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (model, first_user_content).hash(&mut hasher);
    Some(format!("derived:{:x}", hasher.finish()))
}

/// A resolved routing decision — which server was picked for this request.
#[derive(Debug, Clone)]
pub struct RoutedServer {
    pub computer: String,
    pub endpoint: String,
    pub runtime: String,
    pub model_id: String,
    pub queue_depth: i32,
}

/// Result of a streaming route request. Holds the raw upstream [`reqwest::Response`]
/// plus routing metadata so the caller can proxy the stream and decorate headers.
#[derive(Debug)]
pub struct StreamRouteResult {
    pub upstream: reqwest::Response,
    pub routed: RoutedServer,
    pub cached: bool,
}

/// Diagnostic snapshot of the Pulse router's internal state.
#[derive(Debug, Clone, Serialize)]
pub struct RouterDiagnostics {
    pub session_affinity_entries: usize,
    pub circuit_breaker_nodes: Vec<(String, usize, bool)>,
}

/// Pulse-backed LLM router. Wraps a [`PulseReader`] and a reusable reqwest
/// client so upstream connections pool across many requests.
#[derive(Clone)]
pub struct PulseLlmRouter {
    reader: std::sync::Arc<PulseReader>,
    http: Client,
    upstream_timeout: Duration,
    session_cache: Option<Arc<SessionAffinityCache>>,
    circuit_breaker: Option<Arc<CircuitBreaker>>,
}

impl PulseLlmRouter {
    /// Construct a new router pointed at `redis_url`.
    ///
    /// The Redis URL usually comes from `$FORGEFLEET_REDIS_URL`; callers
    /// should respect that convention.
    pub fn new(redis_url: &str) -> Result<Self, LlmRoutingError> {
        let reader = PulseReader::new(redis_url)?;
        let http = Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("build reqwest client");
        Ok(Self {
            reader: std::sync::Arc::new(reader),
            http,
            upstream_timeout: Duration::from_secs(120),
            session_cache: None,
            circuit_breaker: None,
        })
    }

    /// Attach a [`SessionAffinityCache`] so follow-up turns in the same
    /// conversation are preferentially routed back to the node that handled
    /// the previous turn, yielding prefix-cache / KV-cache hits.
    pub fn with_session_affinity(self, cache: Arc<SessionAffinityCache>) -> Self {
        Self {
            session_cache: Some(cache),
            ..self
        }
    }

    /// Attach a [`CircuitBreaker`] so nodes that repeatedly fail upstream
    /// requests are temporarily excluded from routing even when their
    /// heartbeat is still reporting healthy.
    pub fn with_circuit_breaker(self, cb: Arc<CircuitBreaker>) -> Self {
        Self {
            circuit_breaker: Some(cb),
            ..self
        }
    }

    /// Snapshot of router-internal state for diagnostics.
    pub fn diagnostics(&self) -> RouterDiagnostics {
        RouterDiagnostics {
            session_affinity_entries: self.session_cache.as_ref().map(|c| c.len()).unwrap_or(0),
            circuit_breaker_nodes: self
                .circuit_breaker
                .as_ref()
                .map(|cb| cb.snapshot())
                .unwrap_or_default(),
        }
    }

    /// Return all raw Pulse beats (including offline/unhealthy nodes).
    /// Thin wrapper over the internal `PulseReader`.
    pub async fn all_beats(&self) -> Result<Vec<PulseBeatV2>, LlmRoutingError> {
        self.reader.all_beats().await.map_err(Into::into)
    }

    /// Return the circuit breaker, if one was wired.
    pub fn circuit_breaker(&self) -> Option<Arc<CircuitBreaker>> {
        self.circuit_breaker.clone()
    }

    /// Collect every active+healthy LLM server paired with the beat it
    /// came from, so callers can look up the node's primary IP for
    /// cross-host routing.
    async fn collect_active(&self) -> Result<Vec<(PulseBeatV2, LlmServer)>, LlmRoutingError> {
        let beats = self.reader.all_beats().await?;
        let mut out = Vec::new();
        for b in beats {
            if b.going_offline {
                continue;
            }
            for s in &b.llm_servers {
                if s.status == "active" && s.is_healthy {
                    out.push((b.clone(), s.clone()));
                }
            }
        }
        Ok(out)
    }

    /// Return every active+healthy LLM server in the fleet, in a shape
    /// convenient for the `/v1/fleet/servers` debug endpoint.
    pub async fn list_servers(&self) -> Result<Vec<Value>, LlmRoutingError> {
        let raw = self.collect_active().await?;
        Ok(raw
            .into_iter()
            .map(|(beat, s)| {
                let routed_endpoint = rewrite_endpoint(&s.endpoint, &beat.network.primary_ip);
                let load_score = compute_load_score(&beat, &s);
                json!({
                    "computer": beat.computer_name,
                    "endpoint": routed_endpoint,
                    "endpoint_raw": s.endpoint,
                    "primary_ip": beat.network.primary_ip,
                    "runtime": s.runtime,
                    "model": s.model.id,
                    "healthy": s.is_healthy,
                    "status": s.status,
                    "queue_depth": s.queue_depth,
                    "active_requests": s.active_requests,
                    "tokens_per_sec_last_min": s.tokens_per_sec_last_min,
                    "gpu_pct": beat.load.gpu_pct,
                    "gpu_memory_used_gb": s.gpu_memory_used_gb,
                    "kv_cache_gb": s.memory_used.kv_cache_gb,
                    "load_score": load_score,
                    "circuit_open": self.circuit_breaker.as_ref().map(|cb| cb.is_open(&beat.computer_name)).unwrap_or(false),
                })
            })
            .collect())
    }

    /// Pick the best candidate for `requested_model` using:
    ///   1. Normalize both the requested name and each server's `model.id`.
    ///      Normalization strips Ollama-style tags (`foo:14b` → `foo`),
    ///      `.gguf` extensions, common quantization suffixes
    ///      (`-q4_k_m`, `-q8_0`, `-bf16`, etc.), and folds underscores to
    ///      dashes, lowercased.
    ///   2. Prefer exact post-normalization match.
    ///   3. Otherwise accept prefix match in either direction.
    ///   4. Tie-break by lowest `queue_depth`, then highest
    ///      `tokens_per_sec_last_min`.
    ///   5. Exact matches always rank ahead of prefix matches.
    ///
    /// Returns `(computer_name, primary_ip, LlmServer)` when found.
    pub async fn pick_server(
        &self,
        requested_model: &str,
    ) -> Result<Option<(String, String, LlmServer)>, LlmRoutingError> {
        let requested_raw = requested_model.to_ascii_lowercase();
        let requested_norm = normalize_model_id(requested_model);
        let all = self.collect_active().await?;

        // Match rank, lower = better:
        //   0 = raw case-insensitive exact (preserves Ollama tag like `:14b`)
        //   1 = normalized exact (tag/quant stripped both sides)
        //   2 = normalized prefix match in either direction
        let mut candidates: Vec<(u8, PulseBeatV2, LlmServer)> = all
            .into_iter()
            .filter(|(b, _)| {
                // Skip nodes that are currently circuit-opened.
                if let Some(ref cb) = self.circuit_breaker
                    && cb.is_open(&b.computer_name)
                {
                    tracing::debug!(
                        computer = %b.computer_name,
                        "pulse: skipping circuit-opened node"
                    );
                    return false;
                }
                true
            })
            .filter_map(|(b, s)| {
                let id_raw = s.model.id.to_ascii_lowercase();
                let id_norm = normalize_model_id(&s.model.id);
                if id_raw == requested_raw {
                    Some((0u8, b, s))
                } else if id_norm == requested_norm {
                    Some((1u8, b, s))
                } else if id_norm.starts_with(&requested_norm)
                    || requested_norm.starts_with(&id_norm)
                {
                    Some((2u8, b, s))
                } else {
                    None
                }
            })
            .collect();

        // Primary: best match rank.
        // Secondary: composite load score (lower = better).
        // Tertiary: highest tokens/sec_last_min.
        candidates.sort_by(|(a_rank, a_beat, a), (b_rank, b_beat, b)| {
            a_rank
                .cmp(b_rank)
                .then_with(|| {
                    let a_score = compute_load_score(a_beat, a);
                    let b_score = compute_load_score(b_beat, b);
                    a_score
                        .partial_cmp(&b_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    b.tokens_per_sec_last_min
                        .partial_cmp(&a.tokens_per_sec_last_min)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        Ok(candidates
            .into_iter()
            .next()
            .map(|(_, b, s)| (b.computer_name, b.network.primary_ip, s)))
    }

    /// Pool-alias aware variant of [`pick_server`]. When `requested_model`
    /// matches `fleet_task_coverage.alias` (schema V27), the alias is
    /// expanded to the pool's `preferred_model_ids` and we pick the
    /// lowest-load live endpoint serving any member. Otherwise returns
    /// `None` so the caller falls back to the normal matcher.
    ///
    /// The beat-side primary_ip is looked up via a full-beat scan after the
    /// reader returns its pick; this keeps the reader pure (no beat→ip
    /// join inside ff-pulse).
    pub async fn pick_server_with_pools(
        &self,
        pg: &sqlx::PgPool,
        requested_model: &str,
    ) -> Result<Option<(String, String, LlmServer)>, LlmRoutingError> {
        let picked = self
            .reader
            .pick_llm_server_for_with_pools(pg, requested_model)
            .await?;
        let Some((computer, server)) = picked else {
            return Ok(None);
        };
        // Recover primary_ip from the beat (reader returns the computer name
        // but not the IP; a single extra scan here is fine because alias
        // routing is the uncommon path).
        let beats = self.reader.all_beats().await?;
        let primary_ip = beats
            .iter()
            .find(|b| b.computer_name == computer)
            .map(|b| b.network.primary_ip.clone())
            .unwrap_or_default();
        Ok(Some((computer, primary_ip, server)))
    }

    /// Shared helper: pick a server, rewrite the body, and build the upstream URL.
    ///
    /// Returns `(RoutedServer, url, modified_body)` on success. Does **not**
    /// send the request — callers decide whether to downgrade streaming, proxy
    /// as SSE, etc.
    async fn resolve_target(
        &self,
        mut body: Value,
        cache: Option<&LlmRoutingCache>,
        pg: Option<&sqlx::PgPool>,
    ) -> Result<(RoutedServer, String, Value), LlmRoutingError> {
        let requested_model = body
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or(LlmRoutingError::MissingModel)?;

        // ── Session affinity (prefix-cache / KV-cache aware) ─────────────
        // If this conversation was recently routed to a healthy node that
        // still has the model loaded and isn't overloaded, prefer it.
        let mut affinity_pick: Option<(String, String, LlmServer)> = None;
        if let Some(ref session_cache) = self.session_cache
            && let Some(session_key) = extract_session_key(&body)
            && let Some(entry) = session_cache.get(&session_key)
        {
            if let Ok(Some(beat)) = self.reader.latest_beat(&entry.computer).await
                && !beat.going_offline
            {
                for s in &beat.llm_servers {
                    if s.status == "active"
                        && s.is_healthy
                        && normalize_model_id(&s.model.id) == normalize_model_id(&entry.model_id)
                        && s.queue_depth <= SESSION_AFFINITY_QUEUE_THRESHOLD
                    {
                        affinity_pick = Some((
                            beat.computer_name.clone(),
                            beat.network.primary_ip.clone(),
                            s.clone(),
                        ));
                        ff_observability::metrics::PULSE_ROUTER_AFFINITY_HITS_TOTAL
                            .with_label_values(&[&requested_model])
                            .inc();
                        tracing::debug!(
                            session = %session_key,
                            computer = %entry.computer,
                            model = %entry.model_id,
                            queue_depth = s.queue_depth,
                            "pulse: session affinity hit"
                        );
                        break;
                    }
                }
            }
            if affinity_pick.is_none() {
                tracing::debug!(
                    session = %session_key,
                    computer = %entry.computer,
                    "pulse: session affinity stale (node offline or model unloaded)"
                );
            }
        }

        // 1. Pool-alias expansion (optional).
        let pool_pick = match pg {
            Some(pool) => self
                .pick_server_with_pools(pool, &requested_model)
                .await
                .unwrap_or(None),
            None => None,
        };

        // 2. Cache-first pick for the normal path, unless affinity won.
        let picked = if let Some(t) = affinity_pick {
            Some(t)
        } else {
            match pool_pick {
                Some(t) => Some(t),
                None => match cache {
                    Some(c) => c.pick(&requested_model).await,
                    None => self.pick_server(&requested_model).await?,
                },
            }
        };

        let Some((computer, primary_ip, server)) = picked else {
            let all = self.reader.list_llm_servers().await?;
            let available: Vec<String> = all.into_iter().map(|(_, s)| s.model.id).collect();
            return Err(LlmRoutingError::NoMatch {
                requested: requested_model,
                available,
            });
        };

        // Rewrite body.model to the concrete backend model id.
        if requested_model != server.model.id {
            body["model"] = Value::String(server.model.id.clone());
        }
        apply_qwen3_max_tokens_floor(&mut body, &server.model.id);

        let rewritten_endpoint = rewrite_endpoint(&server.endpoint, &primary_ip);
        let url = if rewritten_endpoint.contains("/chat/completions") {
            rewritten_endpoint.clone()
        } else {
            let base = rewritten_endpoint.trim_end_matches('/');
            format!("{base}/v1/chat/completions")
        };

        let routed = RoutedServer {
            computer,
            endpoint: rewritten_endpoint,
            runtime: server.runtime.clone(),
            model_id: server.model.id.clone(),
            queue_depth: server.queue_depth,
        };

        Ok((routed, url, body))
    }

    /// Record (or refresh) a session→node affinity mapping after a
    /// successful routing decision.
    fn record_affinity(&self, session_key: Option<String>, routed: &RoutedServer) {
        if let (Some(session_cache), Some(key)) = (self.session_cache.as_ref(), session_key) {
            session_cache.set(
                key.clone(),
                routed.computer.clone(),
                routed.model_id.clone(),
            );
            tracing::debug!(
                session = %key,
                computer = %routed.computer,
                model = %routed.model_id,
                "pulse: session affinity recorded"
            );
        }
    }

    /// Record a failure against `computer` for circuit-breaking purposes.
    fn record_failure(&self, computer: &str) {
        if let Some(ref cb) = self.circuit_breaker {
            cb.record_failure(computer);
            ff_observability::metrics::PULSE_CIRCUIT_BREAKER_TRIPS_TOTAL
                .with_label_values(&[computer])
                .inc();
        }
    }

    /// Full end-to-end non-streaming route.
    ///
    /// If the request body has `"stream": true` it is downgraded to
    /// non-streaming transparently. For true streaming proxying use
    /// [`Self::route_completion_streaming`].
    pub async fn route_completion(&self, body: &Value) -> Result<Value, LlmRoutingError> {
        self.route_completion_cached(body, None, None).await
    }

    /// Like [`route_completion`] but consults `cache` first and falls through
    /// to a live `pick_server` call only on miss.
    ///
    /// Accepts the body by reference. The body is only cloned once we've
    /// confirmed it has a `model` field — callers that pass clearly-invalid
    /// payloads pay no allocation. On success / NoMatch / upstream-error, the
    /// caller still owns the original body for any fallback path.
    pub async fn route_completion_cached(
        &self,
        body: &Value,
        cache: Option<&LlmRoutingCache>,
        pg: Option<&sqlx::PgPool>,
    ) -> Result<Value, LlmRoutingError> {
        // Cheap pre-check: bail out before cloning if the payload can't be routed.
        body.get("model")
            .and_then(|v| v.as_str())
            .ok_or(LlmRoutingError::MissingModel)?;

        // From here on we mutate (stream downgrade + model rewrite) so clone once.
        let mut body = body.clone();
        if body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
            body["stream"] = Value::Bool(false);
        }

        let session_key = extract_session_key(&body);
        let (routed, url, body) = self.resolve_target(body, cache, pg).await?;
        self.record_affinity(session_key, &routed);

        tracing::debug!(
            computer = %routed.computer,
            endpoint = %routed.endpoint,
            runtime = %routed.runtime,
            model_id = %routed.model_id,
            queue_depth = routed.queue_depth,
            cached = cache.is_some(),
            "pulse: proxying chat completion (non-streaming)"
        );

        let fut = self.http.post(&url).json(&body).send();
        let resp = match tokio::time::timeout(self.upstream_timeout, fut).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                self.record_failure(&routed.computer);
                ff_observability::metrics::PULSE_ROUTER_REQUESTS_TOTAL
                    .with_label_values(&[&routed.model_id, &routed.computer, "upstream_error"])
                    .inc();
                return Err(e.into());
            }
            Err(_) => {
                self.record_failure(&routed.computer);
                ff_observability::metrics::PULSE_ROUTER_REQUESTS_TOTAL
                    .with_label_values(&[&routed.model_id, &routed.computer, "timeout"])
                    .inc();
                return Err(LlmRoutingError::Timeout(self.upstream_timeout));
            }
        };

        if resp.status().is_server_error() {
            self.record_failure(&routed.computer);
        }

        let status = resp.status();
        let mut v: Value = resp.json().await?;

        if v.is_object() {
            v["_forgefleet_route"] = json!({
                "computer": routed.computer,
                "endpoint": routed.endpoint,
                "runtime": routed.runtime,
                "upstream_status": status.as_u16(),
                "cached": cache.is_some(),
            });
        }

        let result_label = if status.is_success() {
            "success"
        } else {
            "upstream_error"
        };
        ff_observability::metrics::PULSE_ROUTER_REQUESTS_TOTAL
            .with_label_values(&[&routed.model_id, &routed.computer, result_label])
            .inc();

        Ok(v)
    }

    /// Streaming variant of [`route_completion_cached`].
    ///
    /// Preserves `"stream": true` in the upstream request and returns the raw
    /// [`reqwest::Response`] so the caller can proxy SSE chunks back to the
    /// client. Routing metadata is returned alongside the response so the
    /// caller can inject `X-ForgeFleet-*` headers if desired.
    ///
    /// Accepts the body by reference; see [`Self::route_completion_cached`].
    pub async fn route_completion_streaming(
        &self,
        body: &Value,
        cache: Option<&LlmRoutingCache>,
        pg: Option<&sqlx::PgPool>,
    ) -> Result<StreamRouteResult, LlmRoutingError> {
        // Cheap pre-check before cloning.
        body.get("model")
            .and_then(|v| v.as_str())
            .ok_or(LlmRoutingError::MissingModel)?;

        let body = body.clone();
        let session_key = extract_session_key(&body);
        let (routed, url, body) = self.resolve_target(body, cache, pg).await?;
        self.record_affinity(session_key, &routed);

        tracing::debug!(
            computer = %routed.computer,
            endpoint = %routed.endpoint,
            runtime = %routed.runtime,
            model_id = %routed.model_id,
            queue_depth = routed.queue_depth,
            cached = cache.is_some(),
            "pulse: proxying chat completion (streaming)"
        );

        let fut = self.http.post(&url).json(&body).send();
        let upstream = match tokio::time::timeout(self.upstream_timeout, fut).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                self.record_failure(&routed.computer);
                ff_observability::metrics::PULSE_ROUTER_REQUESTS_TOTAL
                    .with_label_values(&[&routed.model_id, &routed.computer, "upstream_error"])
                    .inc();
                return Err(e.into());
            }
            Err(_) => {
                self.record_failure(&routed.computer);
                ff_observability::metrics::PULSE_ROUTER_REQUESTS_TOTAL
                    .with_label_values(&[&routed.model_id, &routed.computer, "timeout"])
                    .inc();
                return Err(LlmRoutingError::Timeout(self.upstream_timeout));
            }
        };

        let status = upstream.status();
        if status.is_server_error() {
            self.record_failure(&routed.computer);
        }

        let result_label = if status.is_success() {
            "success"
        } else {
            "upstream_error"
        };
        ff_observability::metrics::PULSE_ROUTER_REQUESTS_TOTAL
            .with_label_values(&[&routed.model_id, &routed.computer, result_label])
            .inc();

        Ok(StreamRouteResult {
            upstream,
            routed,
            cached: cache.is_some(),
        })
    }
}

/// Composite load score for candidate ranking.
///
/// Lower = better (less loaded). Combines queue depth, active requests,
/// GPU memory pressure, GPU utilization, and KV-cache size into a single
/// scalar so the router can preferentially send traffic to the least
/// stressed inference server.
///
/// The weights are heuristic and biased toward:
/// - queue_depth (1.0×) — most direct signal of load
/// - active_requests (0.5×) — overlapping work
/// - GPU memory used (1.0× per 8 GB) — caps at +10
/// - GPU utilization (1.0× per 10%%) — caps at +5
/// - KV cache size (1.0× per 4 GB) — caps at +5
fn compute_load_score(beat: &PulseBeatV2, server: &LlmServer) -> f64 {
    let mut score = server.queue_depth as f64;
    score += server.active_requests as f64 * 0.5;

    if let Some(gpu_mem) = server.gpu_memory_used_gb {
        score += (gpu_mem / 8.0).min(10.0);
    }

    if beat.load.gpu_pct > 0.0 {
        score += (beat.load.gpu_pct / 10.0).min(5.0);
    }

    score += (server.memory_used.kv_cache_gb / 4.0).min(5.0);

    score
}

/// Normalize a model identifier so heterogeneous fleet-reported model IDs
/// can be matched against user-supplied model names.
///
/// Handles (at least):
/// - Ollama tags: `qwen2.5-coder:14b` → `qwen2.5-coder`
/// - GGUF files: `Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf` →
///   `qwen3-coder-30b-a3b-instruct`
/// - Mixed case + underscore separators → lowercased, dashed
/// - Common llama.cpp/HF quantization suffixes are stripped so a bare
///   family name (`qwen3-coder-30b-a3b`) prefix-matches the richer id.
pub(crate) fn normalize_model_id(raw: &str) -> String {
    // Lowercase first.
    let mut s = raw.to_ascii_lowercase();

    // Path-component: keep only the final segment (for HF repo-style ids
    // like `Qwen/Qwen3-Coder-30B-A3B`).
    if let Some(idx) = s.rfind('/') {
        s = s[idx + 1..].to_string();
    }

    // Drop anything after a colon (Ollama tag — `:14b`, `:latest`).
    if let Some(idx) = s.find(':') {
        s.truncate(idx);
    }

    // Strip trailing `.gguf` / `.bin` / `.safetensors` extension.
    for ext in [".gguf", ".bin", ".safetensors"] {
        if s.ends_with(ext) {
            s.truncate(s.len() - ext.len());
            break;
        }
    }

    // Normalize separators: underscores → dashes, collapse runs of dashes.
    s = s.replace('_', "-");
    while s.contains("--") {
        s = s.replace("--", "-");
    }

    // Strip common quantization / precision suffixes if trailing.
    // Order matters: longer suffixes first so we don't leave a stray dash.
    let quant_suffixes: &[&str] = &[
        "-q2-k", "-q3-k-s", "-q3-k-m", "-q3-k-l", "-q4-0", "-q4-1", "-q4-k-s", "-q4-k-m", "-q5-0",
        "-q5-1", "-q5-k-s", "-q5-k-m", "-q6-k", "-q8-0", "-bf16", "-fp16", "-fp8", "-f16", "-f32",
        "-int8", "-int4", "-awq", "-gptq",
    ];
    // Strip repeatedly — a filename may carry more than one precision tag.
    loop {
        let mut changed = false;
        for sfx in quant_suffixes {
            if s.ends_with(sfx) {
                s.truncate(s.len() - sfx.len());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Trim leading/trailing dashes left over from stripping.
    s.trim_matches('-').to_string()
}

/// Minimum `max_tokens` for qwen3-family models running in thinking mode
/// (issue #94). Qwen3 / Qwen3-Coder / Qwen3-Omni / Qwen3-VL / Qwen3.5 /
/// Qwen3.6 always emit a `<think>` block that burns 300-800 tokens before
/// any visible content. llama.cpp's `enable_thinking=false` / `/no_think`
/// directives are currently non-functional (GH #13189, #20182, #20409),
/// so callers that pass `max_tokens < 1024` silently get empty `content`.
pub(crate) const QWEN3_MAX_TOKENS_FLOOR: u64 = 1024;

/// Apply the qwen3 thinking-mode `max_tokens` floor against `body` when the
/// resolved backend model name contains `qwen3`. Idempotent; safe to call
/// before AND after pool-alias expansion (issue #94).
///
/// No-op when `body` isn't an object, when the resolved model isn't qwen3,
/// or when the caller already supplied `max_tokens >= QWEN3_MAX_TOKENS_FLOOR`.
pub(crate) fn apply_qwen3_max_tokens_floor(body: &mut Value, resolved_model_id: &str) {
    if !resolved_model_id.to_ascii_lowercase().contains("qwen3") {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let current = obj.get("max_tokens").and_then(|v| v.as_u64());
    if current
        .map(|n| n >= QWEN3_MAX_TOKENS_FLOOR)
        .unwrap_or(false)
    {
        return;
    }
    let old = current
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unset".to_string());
    obj.insert("max_tokens".to_string(), json!(QWEN3_MAX_TOKENS_FLOOR));
    tracing::debug!(
        resolved_model = %resolved_model_id,
        old = %old,
        new = QWEN3_MAX_TOKENS_FLOOR,
        "qwen3 thinking-mode max_tokens floor re-applied after pool expansion"
    );
}

/// Replace `127.0.0.1` / `localhost` / `0.0.0.0` in an endpoint URL with
/// the node's reachable `primary_ip`. If `primary_ip` is empty, returns
/// the original endpoint unchanged.
fn rewrite_endpoint(endpoint: &str, primary_ip: &str) -> String {
    if primary_ip.is_empty() {
        return endpoint.to_string();
    }
    let loopbacks = ["127.0.0.1", "localhost", "0.0.0.0"];
    for lb in loopbacks {
        // Look for `://loopback` (scheme-relative) to avoid accidentally
        // rewriting path components that happen to contain the string.
        let needle = format!("://{lb}");
        if let Some(idx) = endpoint.find(&needle) {
            let before = &endpoint[..idx + 3]; // include "://"
            let after = &endpoint[idx + needle.len()..];
            return format!("{before}{primary_ip}{after}");
        }
    }
    endpoint.to_string()
}

/// Shape an [`LlmRoutingError`] into a (status, json) tuple for axum handlers.
pub fn error_to_response(err: LlmRoutingError) -> (u16, Value) {
    match err {
        LlmRoutingError::MissingModel => (
            400,
            json!({"error": {"message": "missing `model` field", "type": "invalid_request_error"}}),
        ),
        LlmRoutingError::NoMatch {
            requested,
            available,
        } => (
            404,
            json!({"error": {
                "message": format!("no server has model '{}' loaded", requested),
                "type": "model_not_loaded",
                "available": available,
            }}),
        ),
        LlmRoutingError::Timeout(d) => (
            504,
            json!({"error": {
                "message": format!("upstream timed out after {}s", d.as_secs()),
                "type": "upstream_timeout",
            }}),
        ),
        LlmRoutingError::Upstream(e) => (
            502,
            json!({"error": {
                "message": format!("upstream request failed: {}", e),
                "type": "upstream_error",
            }}),
        ),
        LlmRoutingError::Pulse(e) => (
            503,
            json!({"error": {
                "message": format!("pulse reader unavailable: {}", e),
                "type": "pulse_unavailable",
            }}),
        ),
    }
}

// ─── Routing cache with background warmer ────────────────────────────────
//
// `LlmRoutingCache` wraps a `PulseLlmRouter` and maintains a map of
// normalized-model-id → pre-computed pick result. A background task
// ("warmer") refreshes the cache every ~15s by enumerating currently-loaded
// models in the fleet and re-running `pick_server` for each. At request time
// the gateway does an O(1) HashMap lookup instead of a SCAN + all-beats
// decode + candidate sort, dropping routing overhead from tens of
// milliseconds to sub-millisecond.
//
// Cache keys are normalized via `normalize_model_id`, so a request for
// `qwen2.5-coder:7b` and the server-reported id `Qwen2.5-Coder-7B-Instruct`
// both hit the same slot.

/// Redis pub/sub channel that the warmer listens on for immediate
/// cache-invalidation triggers (issue #98). The CLI publishes on this
/// channel whenever `fleet_task_coverage` is mutated.
pub const CHANNEL_ROUTING_INVALIDATE: &str = "routing:invalidate";

/// How often the warmer re-runs `pick_server` for every known model id.
const WARMER_INTERVAL: Duration = Duration::from_secs(15);
/// Entries older than this are evicted from the cache (i.e. not seen for
/// ~4 warmer ticks). A miss will transparently fall through to the live
/// router and re-populate on next tick.
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct CachedEntry {
    /// (computer_name, primary_ip, LlmServer) — the full tuple
    /// `PulseLlmRouter::pick_server` normally returns.
    computer: String,
    primary_ip: String,
    server: LlmServer,
    refreshed_at: Instant,
}

/// Pre-computed pick cache in front of [`PulseLlmRouter`].
///
/// Construct with [`LlmRoutingCache::new`], spawn the background warmer with
/// [`LlmRoutingCache::spawn_warmer`], and query with [`LlmRoutingCache::pick`].
/// The gateway's `/v1/chat/completions` handler should prefer `pick` over
/// calling `router.pick_server` directly for hot-path routing.
pub struct LlmRoutingCache {
    router: Arc<PulseLlmRouter>,
    cache: Arc<RwLock<HashMap<String, CachedEntry>>>,
}

impl LlmRoutingCache {
    pub fn new(router: Arc<PulseLlmRouter>) -> Self {
        Self {
            router,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Spawn the background warmer loop. It ticks every `WARMER_INTERVAL`
    /// and exits when `shutdown` flips to `true`.
    ///
    /// In addition to the periodic tick, this also spawns a Redis pub/sub
    /// subscriber on channel `routing:invalidate` (see [`CHANNEL_ROUTING_INVALIDATE`]).
    /// Whenever an operator writes to `fleet_task_coverage` the CLI publishes
    /// on that channel, causing the warmer to run an immediate tick instead
    /// of waiting up to `WARMER_INTERVAL` seconds (issue #98).
    ///
    /// `redis_url` is used for the pub/sub listener. If `None`, or on any
    /// pub/sub error, the warmer silently degrades to the periodic-only path.
    pub fn spawn_warmer(&self, shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        // Read the same env ff-gateway already uses for the pulse router so
        // operators don't have to configure two URLs.
        let redis_url = std::env::var("FORGEFLEET_REDIS_URL").ok();
        self.spawn_warmer_with_redis(shutdown, redis_url)
    }

    /// Variant of [`spawn_warmer`] with an explicit Redis URL for pub/sub.
    /// Used mainly for tests; production callers should use [`spawn_warmer`].
    pub fn spawn_warmer_with_redis(
        &self,
        mut shutdown: watch::Receiver<bool>,
        redis_url: Option<String>,
    ) -> JoinHandle<()> {
        let router = self.router.clone();
        let cache = self.cache.clone();

        // Channel that the pub/sub listener uses to wake the warmer. Bounded
        // to 1 — extra pokes coalesce because a single tick serves them all.
        let (poke_tx, mut poke_rx) = tokio::sync::mpsc::channel::<()>(1);

        // Spawn the pub/sub listener as a sibling task. It owns its own
        // reconnect loop so a dropped Redis connection doesn't wedge the
        // warmer — the warmer still ticks on the 15s interval.
        let subscriber_handle = if let Some(url) = redis_url {
            Some(tokio::spawn(invalidate_subscriber(url, poke_tx.clone())))
        } else {
            tracing::debug!(
                "llm routing cache: FORGEFLEET_REDIS_URL not set; skipping pub/sub invalidation listener"
            );
            None
        };

        tokio::spawn(async move {
            // Run once immediately so the cache is warm by the time the
            // first request lands.
            if let Err(e) = warmer_tick(&router, &cache).await {
                tracing::warn!(error = %e, "llm routing cache: initial warmer tick failed");
            }
            let mut ticker = tokio::time::interval(WARMER_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately; absorb it since we just ran.
            ticker.tick().await;
            // Rate-limit the poke-driven path: a runaway publisher (observed
            // 2026-04-29 — 550 invalidations/sec on the leader's redis,
            // 34M log lines accumulated, task_runner CPU-starved) must not
            // be able to flood the warmer. Each warmer_tick re-reads the
            // whole fleet; a 2-second floor between pokes loses zero info
            // (every tick after the first sees the same authoritative state)
            // and trims runaway loads to ≤ 30/min.
            const POKE_RATE_LIMIT: Duration = Duration::from_secs(2);
            let mut last_poke_tick = std::time::Instant::now()
                .checked_sub(POKE_RATE_LIMIT)
                .unwrap_or_else(std::time::Instant::now);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = warmer_tick(&router, &cache).await {
                            tracing::warn!(error = %e, "llm routing cache: warmer tick failed");
                        }
                    }
                    maybe_poke = poke_rx.recv() => {
                        if maybe_poke.is_none() {
                            // Channel closed (sender dropped). Stop listening for
                            // pokes and let the periodic ticker carry on alone.
                            tracing::debug!("llm routing cache: poke channel closed; disabling pub/sub-driven ticks");
                            break;
                        }
                        let elapsed = last_poke_tick.elapsed();
                        if elapsed < POKE_RATE_LIMIT {
                            // Drop — the periodic ticker will catch this within WARMER_INTERVAL.
                            tracing::trace!(
                                elapsed_ms = elapsed.as_millis() as u64,
                                "llm routing cache: poke rate-limited (within 2s of last)"
                            );
                            continue;
                        }
                        last_poke_tick = std::time::Instant::now();
                        // Demoted from info to debug — the immediate-tick log
                        // line was producing 550/sec spam under storm conditions.
                        tracing::debug!(
                            "llm routing cache: immediate tick triggered by routing:invalidate"
                        );
                        if let Err(e) = warmer_tick(&router, &cache).await {
                            tracing::warn!(error = %e, "llm routing cache: invalidation-triggered tick failed");
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            tracing::debug!("llm routing cache warmer shutting down");
                            break;
                        }
                    }
                }
            }
            if let Some(h) = subscriber_handle {
                h.abort();
            }
        })
    }

    /// Look up a cached pick. Falls through to a live `pick_server` call on
    /// miss (or if the entry is stale) and populates the cache with the
    /// result. Returns `None` only if the live router also has no match.
    ///
    /// Return shape matches `PulseLlmRouter::pick_server`:
    /// `(computer_name, primary_ip, LlmServer)`.
    pub async fn pick(&self, model_id: &str) -> Option<(String, String, LlmServer)> {
        let key = normalize_model_id(model_id);

        // Fast path: read lock, hit, fresh.
        {
            let guard = self.cache.read().await;
            if let Some(entry) = guard.get(&key)
                && entry.refreshed_at.elapsed() < CACHE_TTL
            {
                ff_observability::metrics::PULSE_ROUTER_CACHE_HITS_TOTAL
                    .with_label_values(&[model_id])
                    .inc();
                return Some((
                    entry.computer.clone(),
                    entry.primary_ip.clone(),
                    entry.server.clone(),
                ));
            }
        }

        // Slow path: miss or stale — ask the live router and populate.
        match self.router.pick_server(model_id).await {
            Ok(Some((computer, primary_ip, server))) => {
                let entry = CachedEntry {
                    computer: computer.clone(),
                    primary_ip: primary_ip.clone(),
                    server: server.clone(),
                    refreshed_at: Instant::now(),
                };
                let mut guard = self.cache.write().await;
                guard.insert(key, entry);
                Some((computer, primary_ip, server))
            }
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(error = %e, model = %model_id, "live pick_server failed in cache fallback");
                None
            }
        }
    }

    /// Test/diagnostics helper: current cache size.
    #[allow(dead_code)]
    pub async fn len(&self) -> usize {
        self.cache.read().await.len()
    }

    /// Test/diagnostics helper: true when the cache has no entries.
    #[allow(dead_code)]
    pub async fn is_empty(&self) -> bool {
        self.cache.read().await.is_empty()
    }
}

/// One warmer pass:
/// 1. List every active+healthy LLM server currently beating.
/// 2. Collect the unique set of model ids they report.
/// 3. For each, call `pick_server` and refresh the cache entry.
/// 4. Evict entries older than `CACHE_TTL`.
async fn warmer_tick(
    router: &Arc<PulseLlmRouter>,
    cache: &Arc<RwLock<HashMap<String, CachedEntry>>>,
) -> Result<(), LlmRoutingError> {
    let servers = router.list_servers().await?;
    let mut seen_models: HashSet<String> = HashSet::new();
    for s in &servers {
        if let Some(m) = s.get("model").and_then(|v| v.as_str()) {
            seen_models.insert(m.to_string());
        }
    }

    let now = Instant::now();
    let mut refreshed: HashMap<String, CachedEntry> = HashMap::new();
    for model_id in &seen_models {
        match router.pick_server(model_id).await {
            Ok(Some((computer, primary_ip, server))) => {
                let key = normalize_model_id(model_id);
                refreshed.insert(
                    key,
                    CachedEntry {
                        computer,
                        primary_ip,
                        server,
                        refreshed_at: now,
                    },
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, model = %model_id, "warmer: pick_server failed");
            }
        }
    }

    // Merge: new entries overwrite, stale entries (not refreshed and older
    // than TTL) are dropped.
    let mut guard = cache.write().await;
    for (k, v) in refreshed {
        guard.insert(k, v);
    }
    guard.retain(|_, entry| entry.refreshed_at.elapsed() < CACHE_TTL);

    tracing::debug!(
        models_seen = seen_models.len(),
        cache_size = guard.len(),
        "llm routing cache: warmer tick complete"
    );
    Ok(())
}

/// Subscribe to [`CHANNEL_ROUTING_INVALIDATE`] and poke `tx` on every message.
/// Retries forever with a 5s backoff on connection errors so a transient
/// Redis outage doesn't permanently disable fast-path invalidation.
async fn invalidate_subscriber(redis_url: String, tx: tokio::sync::mpsc::Sender<()>) {
    loop {
        match run_invalidate_subscriber_once(&redis_url, &tx).await {
            Ok(()) => {
                // Clean exit only happens when the subscriber stream drops.
                tracing::debug!(
                    "llm routing cache: routing:invalidate subscriber stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    redis_url = %redis_url,
                    "llm routing cache: routing:invalidate subscriber failed; retrying in 5s"
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        // Exit the retry loop when the poke channel is dropped (warmer task
        // itself has shut down).
        if tx.is_closed() {
            tracing::debug!(
                "llm routing cache: warmer dropped poke channel; stopping invalidate subscriber"
            );
            return;
        }
    }
}

async fn run_invalidate_subscriber_once(
    redis_url: &str,
    tx: &tokio::sync::mpsc::Sender<()>,
) -> Result<(), redis::RedisError> {
    use futures::StreamExt;
    let client = redis::Client::open(redis_url)?;
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(CHANNEL_ROUTING_INVALIDATE).await?;
    tracing::info!(
        channel = %CHANNEL_ROUTING_INVALIDATE,
        "llm routing cache: subscribed for cache-invalidation messages"
    );
    let mut msgs = pubsub.into_on_message();
    while let Some(msg) = msgs.next().await {
        let reason: String = msg.get_payload().unwrap_or_else(|_| "(no payload)".into());
        tracing::debug!(%reason, "routing:invalidate received");
        // try_send: coalesce bursts — the warmer runs one tick per wake
        // and that tick re-reads the whole fleet, so dropping extras is safe.
        let _ = tx.try_send(());
    }
    Ok(())
}

/// Publish a best-effort cache-invalidation message so every gateway's
/// warmer runs an immediate tick. Used by CLI code paths that write to
/// `fleet_task_coverage` (issue #98).
///
/// Errors are logged at `debug` and swallowed — operator workflows must
/// never fail because Redis is unreachable.
pub async fn publish_routing_invalidate(redis_url: &str, reason: &str) {
    match publish_routing_invalidate_impl(redis_url, reason).await {
        Ok(()) => {
            tracing::debug!(
                channel = %CHANNEL_ROUTING_INVALIDATE,
                %reason,
                "published routing:invalidate"
            );
        }
        Err(e) => {
            tracing::debug!(
                redis_url,
                error = %e,
                %reason,
                "routing:invalidate publish failed; gateway caches will refresh on next periodic tick"
            );
        }
    }
}

async fn publish_routing_invalidate_impl(
    redis_url: &str,
    reason: &str,
) -> Result<(), redis::RedisError> {
    use redis::AsyncCommands;
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    conn.publish::<_, _, ()>(CHANNEL_ROUTING_INVALIDATE, reason)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_shapes_no_match() {
        let (code, body) = error_to_response(LlmRoutingError::NoMatch {
            requested: "foo".into(),
            available: vec!["bar".into(), "baz".into()],
        });
        assert_eq!(code, 404);
        assert_eq!(
            body["error"]["message"].as_str().unwrap(),
            "no server has model 'foo' loaded"
        );
        let avail = body["error"]["available"].as_array().unwrap();
        assert_eq!(avail.len(), 2);
    }

    #[test]
    fn error_response_shapes_missing_model() {
        let (code, _body) = error_to_response(LlmRoutingError::MissingModel);
        assert_eq!(code, 400);
    }

    #[test]
    fn rewrite_endpoint_replaces_loopback() {
        assert_eq!(
            rewrite_endpoint("http://127.0.0.1:55000", "192.168.5.102"),
            "http://192.168.5.102:55000"
        );
        assert_eq!(
            rewrite_endpoint("http://localhost:11434/v1", "192.168.5.103"),
            "http://192.168.5.103:11434/v1"
        );
        assert_eq!(
            rewrite_endpoint("http://0.0.0.0:51001", "10.0.0.5"),
            "http://10.0.0.5:51001"
        );
    }

    #[test]
    fn rewrite_endpoint_leaves_other_hosts_alone() {
        assert_eq!(
            rewrite_endpoint("http://192.168.5.100:55000", "192.168.5.102"),
            "http://192.168.5.100:55000"
        );
    }

    #[test]
    fn rewrite_endpoint_empty_primary_ip_noop() {
        assert_eq!(
            rewrite_endpoint("http://127.0.0.1:55000", ""),
            "http://127.0.0.1:55000"
        );
    }

    #[test]
    fn normalize_strips_ollama_tag() {
        assert_eq!(normalize_model_id("qwen2.5-coder:14b"), "qwen2.5-coder");
        assert_eq!(normalize_model_id("qwen2.5-coder:latest"), "qwen2.5-coder");
        assert_eq!(normalize_model_id("Qwen2.5-Coder:14B"), "qwen2.5-coder");
    }

    #[test]
    fn normalize_strips_gguf_and_quant() {
        assert_eq!(
            normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf"),
            "qwen3-coder-30b-a3b-instruct"
        );
        assert_eq!(
            normalize_model_id("Qwen2.5-Coder-32B-Instruct-Q8_0.gguf"),
            "qwen2.5-coder-32b-instruct"
        );
    }

    #[test]
    fn normalize_prefix_match_bare_vs_tagged() {
        // Bare name vs ollama-tagged server: both normalize to the same stem.
        let bare = normalize_model_id("qwen2.5-coder");
        let tagged = normalize_model_id("qwen2.5-coder:14b");
        assert_eq!(bare, tagged);
        assert_eq!(bare, "qwen2.5-coder");
    }

    #[test]
    fn normalize_prefix_request_matches_richer_id() {
        // A user asks for `qwen3-coder-30b-a3b`, server has
        // `qwen3-coder-30b-a3b-instruct`. Post-normalize, prefix match holds.
        let requested = normalize_model_id("qwen3-coder-30b-a3b");
        let server = normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf");
        assert!(server.starts_with(&requested));
    }

    #[test]
    fn normalize_handles_hf_repo_path() {
        // HF-style `Owner/Repo` ids — keep last segment.
        assert_eq!(
            normalize_model_id("Qwen/Qwen3-Coder-30B-A3B-Instruct"),
            "qwen3-coder-30b-a3b-instruct"
        );
    }

    // ─── #94 — qwen3 max_tokens floor post pool-alias expansion ──────────

    #[test]
    fn qwen3_floor_raises_max_tokens_for_resolved_qwen3_model() {
        // Caller asked for pool alias "thinking" with max_tokens=512; after
        // expansion the concrete model is qwen3-35b-thinking. Floor should
        // bump max_tokens to QWEN3_MAX_TOKENS_FLOOR.
        let mut body = json!({ "model": "thinking", "max_tokens": 512 });
        apply_qwen3_max_tokens_floor(&mut body, "qwen3-35b-thinking");
        assert_eq!(body["max_tokens"].as_u64().unwrap(), QWEN3_MAX_TOKENS_FLOOR);
    }

    #[test]
    fn qwen3_floor_inserts_max_tokens_when_absent() {
        let mut body = json!({ "model": "coder" });
        apply_qwen3_max_tokens_floor(&mut body, "Qwen3-Coder-30B-A3B");
        assert_eq!(body["max_tokens"].as_u64().unwrap(), QWEN3_MAX_TOKENS_FLOOR);
    }

    #[test]
    fn qwen3_floor_preserves_caller_value_when_already_above_floor() {
        let mut body = json!({ "model": "thinking", "max_tokens": 8192 });
        apply_qwen3_max_tokens_floor(&mut body, "qwen3-35b-thinking");
        assert_eq!(body["max_tokens"].as_u64().unwrap(), 8192);
    }

    #[test]
    fn qwen3_floor_noop_for_non_qwen3_models() {
        // Non-qwen3 model — even with max_tokens=16, no floor applies.
        let mut body = json!({ "model": "coder", "max_tokens": 16 });
        apply_qwen3_max_tokens_floor(&mut body, "qwen2.5-coder-32b");
        assert_eq!(body["max_tokens"].as_u64().unwrap(), 16);
    }

    #[test]
    fn qwen3_floor_noop_on_non_object_body() {
        let mut body = json!([1, 2, 3]);
        apply_qwen3_max_tokens_floor(&mut body, "qwen3-35b-thinking");
        // Unchanged.
        assert_eq!(body, json!([1, 2, 3]));
    }
}
