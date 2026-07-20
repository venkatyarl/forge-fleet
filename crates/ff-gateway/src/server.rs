use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::middleware;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message as WsMessage},
    http::{HeaderMap, Response, StatusCode, header},
    response::{Html, IntoResponse},
    routing::{delete, get, post},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::stream::StreamExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{debug, info, warn};
use uuid::Uuid;

use ff_agent::agent_loop::{AgentEvent, AgentOutcome, AgentSession, AgentSessionConfig};
use ff_api::openai_compat::{self, validate_request};
use ff_api::registry::BackendRegistry;
use ff_api::router::{ModelRouter, TierRouter, TierRouterConfig, TierTimeouts};
use ff_api::token_ledger::{CostTracker, FleetCostSummary, ModelCostStats, TokenUsageRecord};
use ff_api::types::ChatCompletionRequest;
use ff_db::{OperationalStore, RuntimeRegistryStore, queries};
use ff_discovery::health::HealthStatus;
use ff_discovery::{FleetComputer, NodeRegistry};
use ff_mcp::McpServer;
use ff_mcp::transport::HttpTransport;
use ff_observability::metrics::{
    init_prometheus_metrics, metrics_handler, prometheus_metrics_middleware,
};
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::{
    embed,
    llm_routing::{self, CircuitBreaker, LlmRoutingCache, PulseLlmRouter, SessionAffinityCache},
    message::{Channel, IncomingMessage, OutgoingMessage},
    router::{MessageRouter, RouteTarget},
    telegram::TelegramClient,
    webhook,
};

// ─── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub bind_addr: String,
    pub bot_aliases: Vec<String>,
    pub command_prefixes: Vec<char>,
    /// Fleet config for /api/config and fleet status.
    pub fleet_config: Option<ff_core::config::FleetConfig>,
    /// Backend registry shared with ff-api for LLM proxying.
    pub backend_registry: Option<Arc<BackendRegistry>>,
    /// Discovery registry shared with ff-discovery for fleet status.
    pub discovery_registry: Option<Arc<NodeRegistry>>,
    /// Path to fleet.toml config file (for config save/reload).
    pub config_path: Option<String>,
    /// Operational persistence store (SQLite or Postgres) for tasks/config/audit/etc.
    pub operational_store: Option<OperationalStore>,
    /// Runtime registry store (SQLite or Postgres transitional backend).
    pub runtime_registry: Option<RuntimeRegistryStore>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8787".to_string(),
            bot_aliases: vec!["forgefleet".to_string(), "taylor".to_string()],
            command_prefixes: vec!['/', '!'],
            fleet_config: None,
            backend_registry: None,
            discovery_registry: None,
            config_path: None,
            operational_store: None,
            runtime_registry: None,
        }
    }
}

// ─── State ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GatewayState {
    pub router: Arc<MessageRouter>,
    pub inbound_messages: Arc<DashMap<Uuid, IncomingMessage>>,
    pub outbound_messages: Arc<DashMap<Uuid, OutgoingMessage>>,
    pub web_clients: Arc<DashMap<Uuid, mpsc::Sender<WsMessage>>>,
    /// Fleet config (for /api/config endpoint).
    pub fleet_config: Option<Arc<tokio::sync::RwLock<ff_core::config::FleetConfig>>>,
    /// Absolute path to the active fleet.toml file.
    pub config_path: Option<String>,
    /// Backend registry for LLM proxying (shared with ff-api).
    pub api_registry: Option<Arc<BackendRegistry>>,
    /// Model router for tier-escalated LLM routing (backward compat).
    pub model_router: Option<Arc<ModelRouter>>,
    /// Smart tier-escalation router with health tracking and metrics.
    pub tier_router: Option<Arc<TierRouter>>,
    /// Pulse-backed LLM router (live Redis beats, preferred routing strategy).
    pub pulse_router: Option<Arc<PulseLlmRouter>>,
    /// Pre-computed pick cache in front of `pulse_router`, refreshed every
    /// ~15s by a background warmer. When set, `/v1/chat/completions` routes
    /// in sub-ms because no Redis SCAN is needed per request.
    pub pulse_cache: Option<Arc<LlmRoutingCache>>,
    /// HTTP client for upstream LLM requests.
    pub http_client: reqwest::Client,
    /// Discovery registry for fleet node status.
    pub discovery_registry: Option<Arc<NodeRegistry>>,
    /// Operational persistence store for metadata endpoints.
    pub operational_store: Option<OperationalStore>,
    /// Runtime registry persistence backend (SQLite/Postgres).
    pub runtime_registry: Option<RuntimeRegistryStore>,
    /// Lightweight in-memory updater state for dashboard contract compatibility.
    pub update_state: Arc<tokio::sync::RwLock<UpdateRolloutState>>,
    /// WebSocket hub for typed event broadcasting.
    pub ws_hub: crate::websocket::WsHub,
    /// Active agent sessions keyed by session UUID.
    pub agent_sessions: Arc<DashMap<Uuid, AgentSessionHandle>>,
    /// Token usage and cost tracker for LLM requests.
    pub cost_tracker: Arc<CostTracker>,
    /// Cancellation token for background tasks (flush, heartbeat, warmer).
    pub cancel_token: CancellationToken,
    /// Shutdown sender for the routing cache warmer.
    pub warmer_shutdown: Option<tokio::sync::watch::Sender<bool>>,
}

/// Handle to a running agent session managed by the gateway.
#[derive(Clone)]
pub struct AgentSessionHandle {
    pub cancel_token: CancellationToken,
    pub created_at: DateTime<Utc>,
    pub model: String,
    pub llm_base_url: String,
    pub status: Arc<std::sync::atomic::AtomicU8>,
}

impl AgentSessionHandle {
    /// 0 = running, 1 = done, 2 = error, 3 = cancelled
    pub fn status_str(&self) -> &'static str {
        match self.status.load(std::sync::atomic::Ordering::Relaxed) {
            0 => "running",
            1 => "done",
            2 => "error",
            3 => "cancelled",
            _ => "unknown",
        }
    }
}

impl GatewayState {
    pub fn new(router: MessageRouter) -> Self {
        Self {
            router: Arc::new(router),
            inbound_messages: Arc::new(DashMap::new()),
            outbound_messages: Arc::new(DashMap::new()),
            web_clients: Arc::new(DashMap::new()),
            fleet_config: None,
            config_path: None,
            api_registry: None,
            model_router: None,
            tier_router: None,
            pulse_router: None,
            pulse_cache: None,
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("build reqwest client"),
            discovery_registry: None,
            operational_store: None,
            runtime_registry: None,
            update_state: Arc::new(tokio::sync::RwLock::new(UpdateRolloutState::default())),
            ws_hub: crate::websocket::WsHub::new(),
            agent_sessions: Arc::new(DashMap::new()),
            cost_tracker: Arc::new(CostTracker::new()),
            cancel_token: CancellationToken::new(),
            warmer_shutdown: None,
        }
    }

    pub fn broadcast_json(&self, payload: Value) {
        let text = payload.to_string();
        let mut disconnected = Vec::new();

        for entry in self.web_clients.iter() {
            if entry
                .value()
                .try_send(WsMessage::Text(text.clone().into()))
                .is_err()
            {
                disconnected.push(*entry.key());
            }
        }

        for key in disconnected {
            self.web_clients.remove(&key);
        }
    }

    /// Prune oldest agent sessions when the map exceeds `max` entries.
    pub fn prune_agent_sessions(&self, max: usize) {
        while self.agent_sessions.len() > max {
            let oldest = self
                .agent_sessions
                .iter()
                .min_by_key(|e| e.value().created_at);
            if let Some(entry) = oldest {
                let id = *entry.key();
                drop(entry);
                self.agent_sessions.remove(&id);
            } else {
                break;
            }
        }
    }

    /// Prune oldest inbound/outbound messages when maps exceed `max` entries each.
    pub fn prune_messages(&self, max: usize) {
        while self.inbound_messages.len() > max {
            let oldest = self.inbound_messages.iter().next();
            if let Some(entry) = oldest {
                let id = *entry.key();
                drop(entry);
                self.inbound_messages.remove(&id);
            } else {
                break;
            }
        }
        while self.outbound_messages.len() > max {
            let oldest = self.outbound_messages.iter().next();
            if let Some(entry) = oldest {
                let id = *entry.key();
                drop(entry);
                self.outbound_messages.remove(&id);
            } else {
                break;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRolloutState {
    id: Option<String>,
    version: Option<String>,
    stage: Option<String>,
    progress: Option<u8>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

impl Default for UpdateRolloutState {
    fn default() -> Self {
        Self {
            id: None,
            version: None,
            stage: Some("idle".to_string()),
            progress: Some(0),
            started_at: None,
            completed_at: None,
        }
    }
}

// ─── Server ──────────────────────────────────────────────────────────────────

pub struct GatewayServer {
    config: GatewayConfig,
    state: Arc<GatewayState>,
    flush_handle: Option<tokio::task::JoinHandle<()>>,
}

impl GatewayServer {
    pub fn new(config: GatewayConfig) -> anyhow::Result<Self> {
        let router =
            MessageRouter::new(config.bot_aliases.clone(), config.command_prefixes.clone());
        let mut state = GatewayState::new(router);

        // Wire fleet config into state
        if let Some(fleet_cfg) = &config.fleet_config {
            state.fleet_config = Some(Arc::new(tokio::sync::RwLock::new(fleet_cfg.clone())));
        }
        state.config_path = config.config_path.clone();

        // Wire discovery registry
        if let Some(disc_reg) = &config.discovery_registry {
            state.discovery_registry = Some(disc_reg.clone());
        }

        // Wire operational persistence store
        if let Some(store) = &config.operational_store {
            state.operational_store = Some(store.clone());
        }

        // Wire runtime registry persistence backend
        if let Some(runtime_registry) = &config.runtime_registry {
            state.runtime_registry = Some(runtime_registry.clone());
        }

        // Wire API backend registry and model routers
        if let Some(api_reg) = &config.backend_registry {
            let model_router = Arc::new(ModelRouter::new(api_reg.clone()));

            // Build tier timeouts from fleet config, falling back to defaults
            let tier_timeouts = config
                .fleet_config
                .as_ref()
                .map(|fc| {
                    let t = &fc.llm.timeouts;
                    TierTimeouts {
                        tier1: Duration::from_secs(t.tier1.unwrap_or(30)),
                        tier2: Duration::from_secs(t.tier2.unwrap_or(60)),
                        tier3: Duration::from_secs(t.tier3.unwrap_or(120)),
                        tier4: Duration::from_secs(t.tier4.unwrap_or(300)),
                    }
                })
                .unwrap_or_default();

            let tier_config = TierRouterConfig {
                timeouts: tier_timeouts,
                ..Default::default()
            };
            let tier_router = Arc::new(TierRouter::new(api_reg.clone(), tier_config));

            state.api_registry = Some(api_reg.clone());
            state.model_router = Some(model_router);
            state.tier_router = Some(tier_router);
        }

        // ─── Pulse-backed LLM router (preferred for /v1/chat/completions) ───
        //
        // The Pulse router reads live Redis beats, so any fleet node that is
        // currently beating with an active+healthy LLM server becomes
        // immediately routable without explicit backend configuration.
        //
        // If Redis is unreachable, construction still succeeds but every
        // `route_completion` call will fail with a PulseError; the old
        // tier-router path then takes over as the fallback.
        let redis_url = std::env::var("FORGEFLEET_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:56379/".to_string());
        match PulseLlmRouter::new(&redis_url) {
            Ok(pr) => {
                let affinity = Arc::new(SessionAffinityCache::new());
                let breaker = Arc::new(CircuitBreaker::new());
                let router = pr
                    .with_session_affinity(affinity)
                    .with_circuit_breaker(breaker);
                let router_arc = Arc::new(router);
                // Build the routing cache + warmer. The warmer JoinHandle is
                // detached; it exits on a shutdown watch channel. We leak the
                // sender so the watch channel stays alive for the process
                // lifetime (gateway is a long-running daemon; abort happens
                // on process exit).
                let cache = Arc::new(LlmRoutingCache::new(router_arc.clone()));
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                cache.spawn_warmer(shutdown_rx);
                state.warmer_shutdown = Some(shutdown_tx);

                state.pulse_router = Some(router_arc);
                state.pulse_cache = Some(cache);
                info!(redis_url = %redis_url, "pulse-backed LLM router + routing cache + session affinity initialized");
            }
            Err(e) => {
                warn!(redis_url = %redis_url, error = %e, "failed to construct PulseLlmRouter; tier-router fallback only");
            }
        }

        // Build a proper HTTP client for upstream proxying. connect_timeout
        // mirrors PulseLlmRouter::new — without it, a stale beat to a dead
        // destination hangs 75s per request before the circuit breaker
        // gets enough hits to trip (GW.1, 2026-05-19).
        state.http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| {
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(120))
                    .build()
                    .expect("build reqwest client")
            });

        let state = Arc::new(state);

        // Spawn background token ledger flush task (every 5 minutes)
        let flush_state = state.clone();
        let flush_cancel = state.cancel_token.clone();
        let flush_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Some(pool) = flush_state
                            .operational_store
                            .as_ref()
                            .and_then(|os| os.pg_pool())
                        {
                            match flush_state.cost_tracker.flush_to_db(pool).await {
                                Ok(count) if count > 0 => {
                                    tracing::info!(records = count, "token ledger flushed to database");
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::warn!(error = %e, "token ledger flush failed");
                                }
                            }
                        }
                    }
                    _ = flush_cancel.cancelled() => {
                        tracing::debug!("token ledger flush task shutting down");
                        break;
                    }
                }
            }
        });

        // Background message-map trim. `prune_messages` runs inline on every
        // accepted message but only kicks in once the map already exceeds the
        // threshold — under bursty concurrent inserts both maps can briefly
        // overshoot. Trimming proactively every 30s smooths that out and
        // bounds memory growth even when traffic is steady.
        let trim_state = state.clone();
        let trim_cancel = state.cancel_token.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await; // skip the immediate first tick
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // Trim to 80% of the threshold so we have headroom
                        // before the next inline prune fires.
                        trim_state.prune_messages(8_000);
                    }
                    _ = trim_cancel.cancelled() => {
                        tracing::debug!("gateway message trim task shutting down");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            config,
            state,
            flush_handle: Some(flush_handle),
        })
    }

    pub fn app(&self) -> Router {
        build_router(self.state.clone())
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.config.bind_addr)
            .await
            .with_context(|| format!("failed to bind ff-gateway on {}", self.config.bind_addr))?;

        let cancel_token = self.state.cancel_token.clone();

        // Spawn WebSocket heartbeat pruning task (30s interval, 60s timeout).
        let heartbeat_handle = self.state.ws_hub.spawn_heartbeat_task(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(60),
            cancel_token.clone(),
        );

        info!(address = %listener.local_addr()?, "ff-gateway listening");
        axum::serve(listener, self.app())
            .with_graceful_shutdown(shutdown_signal(cancel_token.clone()))
            .await?;

        // Graceful shutdown: cancel background tasks and signal warmer.
        cancel_token.cancel();
        if let Some(tx) = &self.state.warmer_shutdown {
            let _ = tx.send(true);
        }
        heartbeat_handle.abort();
        if let Some(flush_handle) = self.flush_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), flush_handle).await;
        }
        info!("ff-gateway shutdown complete");
        Ok(())
    }

    pub fn shared_state(&self) -> Arc<GatewayState> {
        self.state.clone()
    }
}

async fn shutdown_signal(cancel: CancellationToken) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    let sigterm = async {
        #[cfg(unix)]
        {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install signal handler")
                .recv()
                .await;
        }
        #[cfg(not(unix))]
        {
            futures::future::pending::<()>().await;
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm => {}
        _ = cancel.cancelled() => {}
    }
    info!("shutdown signal received");
}

pub async fn run(config: GatewayConfig) -> anyhow::Result<()> {
    GatewayServer::new(config)?.run().await
}

// ─── Router ──────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<GatewayState>) -> Router {
    let mut app = Router::new()
        // Core gateway routes
        .route("/health", get(health))
        .route("/.well-known/forgefleet.json", get(well_known_forgefleet))
        .route("/ws", get(websocket_upgrade))
        .route("/api/messages", post(incoming_message_http))
        .route("/api/messages/raw", post(incoming_message_raw_http))
        .route("/api/send", post(outgoing_message_http))
        .route("/api/webhook", post(webhook::webhook_http_handler))
        .route("/api/webhooks/github", post(github_webhook_handler))
        .route("/embed/widget.js", get(embed::widget_js_handler))
        .route("/dashboard", get(dashboard))
        // ─── Fleet integration routes ────────────────────────────────
        .route("/api/fleet/status", get(fleet_status))
        .route("/api/status", get(fleet_status))
        .route("/api/jarvis/state", get(crate::jarvis_api::jarvis_state))
        .route("/api/jarvis/ask", post(crate::jarvis_api::jarvis_ask))
        .route("/jarvis", get(crate::jarvis_api::jarvis_hud))
        .route("/jarvis.html", get(crate::jarvis_api::jarvis_hud))
        .route(
            "/api/interactions",
            get(crate::interactions_api::list_interactions),
        )
        .route(
            "/api/interactions/summary",
            get(crate::interactions_api::interactions_summary),
        )
        .route("/console", get(crate::interactions_api::console_page))
        .route("/console.html", get(crate::interactions_api::console_page))
        .route("/api/fleet/enroll", post(fleet_enroll))
        .route("/api/fleet/heartbeat", post(fleet_heartbeat))
        .route("/api/fleet/llm-usage", get(fleet_llm_usage))
        // ─── Skills registry ─────────────────────────────────────────
        .route("/api/skills", get(crate::skills_api::list_skills))
        .route("/api/skills/{*id}", get(crate::skills_api::get_skill))
        // ─── Fleet Tool Registry (Phase 15a) ─────────────────────────
        .route("/api/tools", get(crate::tool_registry_api::list_tools))
        .route(
            "/api/tools/health",
            get(crate::tool_registry_api::tool_health),
        )
        .route(
            "/api/tools/register",
            post(crate::tool_registry_api::register_tools),
        )
        .route(
            "/api/tools/heartbeat",
            post(crate::tool_registry_api::tool_heartbeat),
        )
        .route(
            "/api/tools/usage",
            post(crate::tool_registry_api::record_tool_usage),
        )
        .route(
            "/api/tools/route",
            get(crate::tool_registry_api::route_tool),
        )
        .route(
            "/api/tools/search",
            get(crate::tool_registry_api::search_tools),
        )
        .route("/api/ledger/summary", get(ledger_summary))
        .route("/api/ledger/models", get(ledger_models))
        .route(
            "/api/ledger/budget",
            get(ledger_budget).post(ledger_budget_update),
        )
        .route("/api/ledger/flush", post(ledger_flush))
        .route("/api/ledger/records", get(ledger_records))
        .route("/api/ledger/health", get(ledger_health))
        .route("/api/voice/transcribe", post(crate::voice_api::transcribe))
        .route("/api/voice/speak", post(crate::voice_api::speak))
        // Onboarding (see crates/ff-gateway/src/onboard.rs + plan §§3–3h)
        .route(
            "/onboard/bootstrap.sh",
            get(crate::onboard::bootstrap_script),
        )
        .route(
            "/onboard/bootstrap.ps1",
            get(crate::onboard::bootstrap_script_ps1),
        )
        .route("/api/fleet/self-enroll", post(crate::onboard::self_enroll))
        .route(
            "/api/fleet/enrollment-progress",
            post(crate::onboard::enrollment_progress),
        )
        .route("/api/fleet/check-ip", get(crate::onboard::check_ip))
        .route("/api/fleet/check-tcp", get(crate::onboard::check_tcp))
        .route("/api/fleet/tooling", get(crate::onboard::get_fleet_tooling))
        // Virtual Brain API (see crates/ff-gateway/src/brain_api.rs)
        .route("/api/brain/threads", get(crate::brain_api::list_threads))
        .route("/api/brain/threads", post(crate::brain_api::create_thread))
        .route(
            "/api/brain/threads/{slug}/messages",
            get(crate::brain_api::thread_messages),
        )
        .route(
            "/api/brain/threads/{slug}/message",
            post(crate::brain_api::send_thread_message),
        )
        .route(
            "/api/brain/attach",
            post(crate::brain_api::attach_to_thread),
        )
        .route(
            "/api/brain/candidates",
            get(crate::brain_api::list_candidates),
        )
        .route(
            "/api/brain/candidates/{id}",
            post(crate::brain_api::update_candidate),
        )
        .route("/api/brain/graph", get(crate::brain_api::vault_graph))
        .route(
            "/api/brain/vault/search",
            get(crate::brain_api::vault_search),
        )
        .route(
            "/api/brain/reminders",
            get(crate::brain_api::list_reminders),
        )
        .route(
            "/api/brain/reminders",
            post(crate::brain_api::create_reminder),
        )
        .route("/api/brain/whoami", get(crate::brain_api::whoami))
        .route(
            "/api/brain/stack/{thread_slug}",
            get(crate::brain_api::stack_list),
        )
        .route(
            "/api/brain/stack/{thread_slug}/push",
            post(crate::brain_api::stack_push),
        )
        .route(
            "/api/brain/stack/{thread_slug}/pop",
            post(crate::brain_api::stack_pop),
        )
        .route(
            "/api/brain/backlog/{project}",
            get(crate::brain_api::backlog_list),
        )
        .route(
            "/api/brain/backlog/{project}/add",
            post(crate::brain_api::backlog_add),
        )
        .route(
            "/api/brain/backlog/{project}/complete",
            post(crate::brain_api::backlog_complete),
        )
        .route("/api/fleet/secret-peek", get(crate::onboard::secret_peek))
        .route("/api/fleet/mesh-check", get(crate::onboard::get_mesh_check))
        .route(
            "/api/fleet/verify-node",
            post(crate::onboard::post_verify_computer),
        )
        .route("/api/fleet/deferred", get(crate::onboard::list_deferred))
        .route(
            "/api/fleet/deferred/{id}/promote",
            post(crate::onboard::promote_deferred),
        )
        .route(
            "/api/transports/telegram/status",
            get(telegram_transport_status),
        )
        .route("/api/fleet/computers/{id}", get(fleet_worker_detail))
        // Legacy alias retained during the node→computer rename window.
        // Drop once dashboards + clients are confirmed on the new path.
        .route("/api/fleet/nodes/{id}", get(fleet_worker_detail))
        .route("/api/config", get(get_config).post(update_config))
        .route("/api/config/reload-status", get(config_reload_status))
        .route("/api/settings/runtime", get(settings_runtime))
        .route("/api/brain/status", get(brain_status))
        .route(
            "/api/brain/search",
            get(crate::brain_api::hybrid_search_handler),
        )
        .route("/api/audit/recent", get(audit_recent))
        .route("/api/audit/events", get(audit_recent))
        .route("/api/proxy/stats", get(proxy_stats))
        .route("/api/proxy/requests", get(proxy_requests))
        .route("/v1/proxy/stats", get(proxy_stats))
        .route("/v1/proxy/requests", get(proxy_requests))
        .route("/api/update/status", get(update_status))
        .route("/api/update/check", get(update_check))
        .route("/api/update/pause", post(update_pause))
        .route("/api/update/resume", post(update_resume))
        .route("/api/update/abort", post(update_abort))
        .route("/v1/chat/completions", post(proxy_chat_completions))
        .route(
            "/v1/orchestrate",
            post(crate::orchestrate::handle_orchestrate),
        )
        .route("/v1/models", get(list_models))
        .route("/api/models", get(list_models))
        .route("/v1/fleet/route", post(route_fleet_capability))
        .route("/v1/embeddings", post(proxy_embeddings))
        .route("/v1/tasks", post(crate::tasks::handle_task))
        .route(
            "/v1/tasks/{task_type}",
            post(crate::tasks::handle_task_from_path),
        )
        .route(
            "/v1/images/generations",
            post(crate::tasks::handle_image_generation),
        )
        .route(
            "/v1/audio/transcriptions",
            post(crate::tasks::handle_audio_transcription),
        )
        .route("/v1/internal/delegate", post(internal_delegate))
        .route("/v1/async/{ticket}", get(async_poll))
        // ─── Distributed tracing routes ──────────────────────────────
        .route("/api/traces/recent", get(traces_recent))
        // ─── Agent session routes ───────────────────────────────────
        .route("/api/agent/session", post(create_agent_session))
        .route(
            "/api/agent/session/{id}/message",
            post(agent_session_message),
        )
        .route("/api/agent/session/{id}/cancel", post(cancel_agent_session))
        .route("/api/agent/session/{id}/status", get(agent_session_status))
        .route("/api/agent/sessions", get(list_agent_sessions))
        .route("/api/agent/v54/session/{id}", get(get_v54_session))
        // ─── Pulse v2 dashboard routes ──────────────────────────────
        .route(
            "/api/fleet/computers",
            get(crate::pulse_api::list_computers),
        )
        .route("/api/fleet/members", get(crate::pulse_api::list_members))
        .route(
            "/api/fleet/leader",
            get(crate::pulse_api::get_leader).post(crate::pulse_api::post_leader),
        )
        .route("/api/fleet/health", get(crate::pulse_api::fleet_health))
        .route("/api/llm/servers", get(crate::pulse_api::llm_servers))
        .route("/api/router/diagnostics", get(router_diagnostics))
        .route(
            "/api/software/computers",
            get(crate::pulse_api::software_computers),
        )
        .route("/api/software/drift", get(crate::pulse_api::software_drift))
        .route("/api/projects", get(crate::pulse_api::list_projects))
        .route(
            "/api/projects/{id}/branches",
            get(crate::pulse_api::project_branches),
        )
        // Projects-first PM (V141): attach many GitHub repos + many local folders.
        .route(
            "/api/projects/{id}/repos",
            get(crate::pulse_api::list_project_repos).post(crate::pulse_api::add_project_repo),
        )
        .route(
            "/api/projects/repos/{repo_id}",
            delete(crate::pulse_api::delete_project_repo),
        )
        .route(
            "/api/projects/{id}/folders",
            get(crate::pulse_api::list_project_folders).post(crate::pulse_api::add_project_folder),
        )
        .route(
            "/api/projects/folders/{folder_id}",
            delete(crate::pulse_api::delete_project_folder),
        )
        .route("/api/pm/work-items", get(crate::pulse_api::list_work_items))
        .route(
            "/api/alerts/policies",
            get(crate::pulse_api::alert_policies),
        )
        .route("/api/alerts/events", get(crate::pulse_api::alert_events))
        .route(
            "/api/metrics/{computer}/history",
            get(crate::pulse_api::metrics_history),
        )
        .route("/api/ha/status", get(crate::pulse_api::ha_status))
        .route(
            "/api/docker/projects",
            get(crate::pulse_api::docker_projects),
        )
        .route("/api/events/stream", get(crate::pulse_api::events_stream))
        // ─── Chat management routes ─────────────────────────────────
        .route("/api/chats", get(list_chats).post(create_chat))
        .route("/api/chats/folders", get(list_chat_folders))
        .route("/api/chats/{id}", get(get_chat).delete(delete_chat));

    // Mount MCP HTTP transport under /mcp
    let mcp_server = McpServer::new();
    let mcp_transport = HttpTransport::new(mcp_server);
    app = app.merge(mcp_transport.router().with_state(()));

    // Mount Mission Control API routes.
    //
    if let Some(store) = state.operational_store.clone() {
        let mc_routes = ff_mc::operational_api::mc_router_operational(store);
        app = app.merge(mc_routes.with_state(()));
        info!("mission control API mounted at /api/mc/* (operational store backend)");
    } else {
        warn!("mission control API not mounted: no operational store available");
    }

    // Initialize Prometheus metrics (idempotent).
    init_prometheus_metrics();

    // CORS: restrict to known origins instead of permissive Any.
    let cors_origins: Vec<_> = std::env::var("FF_CORS_ORIGINS")
        .ok()
        .map(|s| s.split(',').map(|o| o.trim().to_string()).collect())
        .unwrap_or_else(|| {
            vec![
                "http://localhost:51002".to_string(),
                "http://localhost:5173".to_string(),
                "http://127.0.0.1:51002".to_string(),
                "http://127.0.0.1:5173".to_string(),
            ]
        });
    let cors = if cors_origins.iter().any(|o| o == "*") {
        CorsLayer::new().allow_origin(tower_http::cors::Any)
    } else {
        let origins: Vec<_> = cors_origins
            .iter()
            .filter_map(|o| o.parse::<header::HeaderValue>().ok())
            .collect();
        CorsLayer::new().allow_origin(origins)
    }
    .allow_methods([
        axum::http::Method::GET,
        axum::http::Method::POST,
        axum::http::Method::PUT,
        axum::http::Method::PATCH,
        axum::http::Method::DELETE,
        axum::http::Method::DELETE,
    ])
    .allow_headers(tower_http::cors::Any);

    app.route("/metrics", get(serve_prometheus_metrics))
        .fallback(crate::static_files::serve_dashboard)
        .layer(middleware::from_fn(crate::middleware::jwt_auth_middleware))
        .layer(middleware::from_fn(crate::middleware::trace_id_middleware))
        .layer(middleware::from_fn(prometheus_metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

// ─── Health ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    /// Git SHA (10-char) the RUNNING daemon was compiled from — baked at build
    /// time (ff-gateway/build.rs). Lets `curl <node>:PORT/health` answer "what
    /// code is this daemon actually executing?" without deriving it from
    /// process-start-time vs binary mtime. Distinct from `version` (the static
    /// crate version) and from `computer_software.installed_version` (which can
    /// momentarily reflect the on-disk binary after a deploy whose restart
    /// hasn't swapped the process yet — the exact gap this surfaces).
    build_sha: &'static str,
    uptime_epoch: i64,
    ws_clients: usize,
    inbound_buffered: usize,
    outbound_buffered: usize,
    telegram_transport: TelegramTransportStatus,
}

#[derive(Debug, Clone, Serialize, Default)]
struct TelegramTransportStatus {
    enabled: bool,
    running: bool,
    allowed_chat_ids: Vec<i64>,
    polling_interval_secs: Option<u64>,
    polling_timeout_secs: Option<u64>,
    media_download_dir: Option<String>,
    started_at: Option<String>,
    last_update_id: Option<i64>,
    last_message_at: Option<String>,
    last_error: Option<String>,
}

/// Runtime override for the daemon's build SHA, set once at startup by the
/// hosting binary (which has the always-fresh `env!("FF_GIT_SHA")`).
static RUNTIME_BUILD_SHA: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Inject the running daemon's git SHA. Call ONCE at startup from the binary
/// (`forgefleetd`) whose root build script keeps `FF_GIT_SHA` fresh. Preferred
/// over the compile-time `FF_GATEWAY_GIT_SHA` bake because ff-gateway's build
/// script does NOT re-run when only `main` advances (its `.git/HEAD` watch is a
/// branch ref that never changes), so the baked value goes stale across deploys
/// — which made /health report an old SHA while the process ran new code
/// (false "stale-daemon" in `ff fleet versions --live`, 2026-06-25).
pub fn set_runtime_build_sha(sha: impl Into<String>) {
    let _ = RUNTIME_BUILD_SHA.set(sha.into());
}

/// The build SHA to report: the runtime-injected value if set, else the
/// compile-time bake (which may be stale — see [`set_runtime_build_sha`]).
fn current_build_sha() -> &'static str {
    RUNTIME_BUILD_SHA
        .get()
        .map(String::as_str)
        .unwrap_or(env!("FF_GATEWAY_GIT_SHA"))
}

async fn health(State(state): State<Arc<GatewayState>>) -> Json<HealthResponse> {
    let telegram_transport = telegram_transport_snapshot(state.as_ref()).await;

    Json(HealthResponse {
        status: "ok",
        service: "ff-gateway",
        version: ff_core::VERSION,
        build_sha: current_build_sha(),
        uptime_epoch: Utc::now().timestamp(),
        ws_clients: state.web_clients.len(),
        inbound_buffered: state.inbound_messages.len(),
        outbound_buffered: state.outbound_messages.len(),
        telegram_transport,
    })
}

/// Discovery document at the standard RFC 8615 well-known path.
///
/// External agents (Codex, Claude Code, third-party CLIs) probe
/// `http://<host>:51002/.well-known/forgefleet.json` to confirm a ForgeFleet
/// leader is running and to auto-discover capabilities + endpoints without
/// operator configuration. Designed for the "any computer on the LAN can
/// find its ForgeFleet without hardcoding URLs" use case.
async fn well_known_forgefleet(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    let leader = match state.operational_store.as_ref().and_then(|os| os.pg_pool()) {
        Some(pool) => {
            sqlx::query_scalar::<_, String>("SELECT member_name FROM fleet_leader_state LIMIT 1")
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
        }
        None => None,
    };
    let fleet_size = match state.operational_store.as_ref().and_then(|os| os.pg_pool()) {
        Some(pool) => sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM computers")
            .fetch_one(pool)
            .await
            .ok(),
        None => None,
    };

    Json(json!({
        "service": "forgefleet",
        "version": ff_core::VERSION,
        "spec_version": "2026-04-21",
        "leader": leader,
        "fleet_size": fleet_size,
        "endpoints": {
            "health":       "/health",
            "status":       "/api/fleet/status",
            "onboard":      "/onboarding",
            "bootstrap_sh": "/onboard/bootstrap.sh",
            "bootstrap_ps1":"/onboard/bootstrap.ps1",
            "self_enroll":  "/api/fleet/self-enroll",
            "openai_chat":  "/v1/chat/completions",
            "openai_models":"/v1/models",
            "mcp":          "http://{host}:{port}/mcp",
            "websocket":    "/ws",
            "metrics":      "/metrics"
        },
        "capabilities": {
            "pulse_protocol_version": 2,
            "openai_compat":       true,
            "mcp":                 true,
            "agent_dispatch":      true,
            "supports_worktrees":  true
        },
        "docs": "https://github.com/venkatyarl/forge-fleet"
    }))
}

/// Verify GitHub webhook HMAC-SHA256 signature.
///
/// `signature` is the `x-hub-signature-256` header value (e.g. `sha256=abc123...`).
/// `secret` is the webhook secret configured in the GitHub repo settings.
fn verify_github_signature(body: &[u8], signature: &str, secret: &str) -> bool {
    let expected = signature.strip_prefix("sha256=").unwrap_or(signature);
    let expected = match hex::decode(expected) {
        Ok(b) => b,
        Err(_) => return false,
    };
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let result = mac.finalize().into_bytes();
    // Constant-time comparison to prevent timing attacks.
    if result.len() != expected.len() {
        return false;
    }
    use subtle::ConstantTimeEq;
    result.as_slice().ct_eq(&expected).into()
}

/// Map GitHub's `workflow_run` (status, conclusion) pair onto the
/// `project_ci_runs.status` enum.
fn map_workflow_run_status(gh_status: &str, conclusion: &str) -> &'static str {
    match (gh_status, conclusion) {
        ("queued", _) => "queued",
        ("in_progress", _) => "in_progress",
        ("completed", "success") => "success",
        ("completed", "failure") => "failure",
        ("completed", "cancelled") => "cancelled",
        ("completed", _) => "completed",
        _ => "unknown",
    }
}

/// Terminal CI statuses — a run that reached one of these is finished. GitHub
/// does NOT guarantee webhook delivery order and retries failed deliveries, so
/// a late/retried `queued`/`in_progress` event can arrive after `completed`.
/// We must never regress a terminal run back to a non-terminal status. NOTE:
/// this set MUST stay in sync with the `status IN (...)` list in the
/// `project_ci_runs` UPDATE guard below.
fn is_terminal_ci_status(status: &str) -> bool {
    matches!(status, "success" | "failure" | "cancelled" | "completed")
}

/// Parse the PR number out of a GitHub merge-group head_ref such as
/// `refs/heads/gh-readonly-queue/main/pr-123[-<sha>]`.
fn parse_merge_group_pr_number(head_ref: &str) -> Option<i64> {
    let suffix = head_ref.rsplit_once("/pr-")?.1;
    let digits: String = suffix.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Map GitHub CI status / conclusion onto `work_item_merge_queue.status`.
fn map_merge_train_ci_status(gh_status: &str, conclusion: &str) -> &'static str {
    match (gh_status, conclusion) {
        ("completed", "success") | ("completed", "skipped") => "mergeable",
        ("completed", "failure")
        | ("completed", "cancelled")
        | ("completed", "timed_out")
        | ("completed", "action_required") => "failed",
        ("in_progress", _) => "ci_running",
        ("queued" | "pending" | "waiting", _) => "queued",
        _ => "unknown",
    }
}

/// Resolve a GitHub `owner/repo` full name to a ForgeFleet `project_id`.
/// Prefers the explicit `project_repos` mapping, then falls back to the
/// legacy short-repo-name convention used by `projects.id`.
async fn resolve_project_id_for_repo(pool: &sqlx::PgPool, repo_full: &str) -> Option<String> {
    let github_url = format!("https://github.com/{}", repo_full);
    if let Ok(Some(row)) = sqlx::query(
        "SELECT project_id FROM project_repos \
            WHERE github_url = $1 \
            ORDER BY is_primary DESC, created_at ASC LIMIT 1",
    )
    .bind(&github_url)
    .fetch_optional(pool)
    .await
    {
        if let Ok(pid) = row.try_get::<String, _>("project_id") {
            return Some(pid);
        }
    }
    let short = repo_full.split('/').nth(1).unwrap_or(repo_full);
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1)")
        .bind(short)
        .fetch_one(pool)
        .await
        .unwrap_or(false);
    if exists {
        Some(short.to_string())
    } else {
        None
    }
}

/// Look up the merge-queue row and its work item for a PR number in a project.
async fn find_merge_queue_by_pr(
    pool: &sqlx::PgPool,
    project_id: &str,
    pr_number: i64,
) -> Result<Option<(Uuid, Uuid)>, sqlx::Error> {
    let pr_pattern = format!("%/pull/{}", pr_number);
    let row = sqlx::query(
        "SELECT id, work_item_id FROM work_item_merge_queue \
            WHERE project_id = $1 AND pr_url LIKE $2 LIMIT 1",
    )
    .bind(project_id)
    .bind(&pr_pattern)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.get("id"), r.get("work_item_id"))))
}

/// Look up the work item associated with a PR number in a project.
async fn find_work_item_by_pr(
    pool: &sqlx::PgPool,
    project_id: &str,
    pr_number: i64,
) -> Result<Option<Uuid>, sqlx::Error> {
    let pr_pattern = format!("%/pull/{}", pr_number);
    sqlx::query_scalar("SELECT id FROM work_items WHERE project_id = $1 AND pr_url LIKE $2 LIMIT 1")
        .bind(project_id)
        .bind(&pr_pattern)
        .fetch_optional(pool)
        .await
}

/// Mirror a CI status for a merge-train PR into `work_item_merge_queue`.
async fn update_merge_queue_status(
    pool: &sqlx::PgPool,
    project_id: &str,
    pr_number: i64,
    status: &str,
    conclusion: &str,
) -> Result<u64, sqlx::Error> {
    let pr_pattern = format!("%/pull/{}", pr_number);
    let failure_reason = if status == "failed" {
        Some(conclusion)
    } else {
        None
    };
    let result = sqlx::query(
        "UPDATE work_item_merge_queue \
            SET status = $1, \
                failed_at = CASE WHEN $1 = 'failed' THEN NOW() ELSE failed_at END, \
                failure_reason = COALESCE($2, failure_reason) \
          WHERE project_id = $3 AND pr_url LIKE $4",
    )
    .bind(status)
    .bind(failure_reason)
    .bind(project_id)
    .bind(&pr_pattern)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Handle a `merge_group` `checks_requested` event — a merge train has been
/// created for one or more PRs. Attach it to the matching work item merge
/// queue entry (creating the queue row if the agent dispatch hasn't yet).
async fn handle_train_creation(pool: &sqlx::PgPool, payload: &Value) -> (StatusCode, Json<Value>) {
    let repo_html_url = payload
        .pointer("/repository/html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let repo_full = payload
        .pointer("/repository/full_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown/unknown");
    let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let mg = payload.get("merge_group").cloned().unwrap_or(Value::Null);
    let head_ref = mg.get("head_ref").and_then(|v| v.as_str()).unwrap_or("");
    let head_sha = mg.get("head_sha").and_then(|v| v.as_str()).unwrap_or("");

    // Use the shared parser to normalize owner/repo.
    let (owner, repo) = ff_agent::project_github_sync::parse_owner_repo(repo_html_url)
        .unwrap_or_else(|| {
            let mut parts = repo_full.split('/');
            let o = parts.next().unwrap_or("unknown").to_string();
            let r = parts.next().unwrap_or("unknown").to_string();
            (o, r)
        });
    let repo_full = format!("{}/{}", owner, repo);

    let Some(project_id) = resolve_project_id_for_repo(pool, &repo_full).await else {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": "unknown project"})),
        );
    };

    let Some(pr_number) = parse_merge_group_pr_number(head_ref) else {
        return (
            StatusCode::ACCEPTED,
            Json(
                json!({"accepted": false, "reason": "could not parse PR number from merge_group head_ref"}),
            ),
        );
    };

    let pr_url = format!("https://github.com/{}/pull/{}", repo_full, pr_number);

    match find_merge_queue_by_pr(pool, &project_id, pr_number).await {
        Ok(Some((id, _))) => {
            let _ = sqlx::query(
                "UPDATE work_item_merge_queue \
                    SET status = 'ci_running', \
                        branch_name = $1, \
                        head_sha = $2, \
                        pr_url = $3, \
                        started_at = COALESCE(started_at, NOW()), \
                        failed_at = NULL, \
                        failure_reason = NULL \
                  WHERE id = $4",
            )
            .bind(head_ref)
            .bind(head_sha)
            .bind(&pr_url)
            .bind(id)
            .execute(pool)
            .await;
        }
        Ok(None) => match find_work_item_by_pr(pool, &project_id, pr_number).await {
            Ok(Some(work_item_id)) => {
                let _ = sqlx::query(
                    "INSERT INTO work_item_merge_queue \
                        (work_item_id, project_id, status, branch_name, pr_url, head_sha, started_at) \
                     VALUES ($1, $2, 'ci_running', $3, $4, $5, NOW()) \
                     ON CONFLICT (work_item_id) DO UPDATE \
                        SET status = 'ci_running', \
                            branch_name = EXCLUDED.branch_name, \
                            pr_url = EXCLUDED.pr_url, \
                            head_sha = EXCLUDED.head_sha, \
                            started_at = COALESCE(work_item_merge_queue.started_at, NOW()), \
                            failed_at = NULL, \
                            failure_reason = NULL",
                )
                .bind(work_item_id)
                .bind(&project_id)
                .bind(head_ref)
                .bind(&pr_url)
                .bind(head_sha)
                .execute(pool)
                .await;
            }
            Ok(None) => {
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({"accepted": false, "reason": "no matching work item for PR"})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({"accepted": false, "reason": format!("db error: {}", e)})),
                );
            }
        },
        Err(e) => {
            return (
                StatusCode::ACCEPTED,
                Json(json!({"accepted": false, "reason": format!("db error: {}", e)})),
            );
        }
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "accepted": true,
            "event": "merge_group",
            "action": action,
            "project": project_id,
            "pr": pr_number,
            "head_sha": head_sha
        })),
    )
}

/// Handle `check_run` events against a merge-group commit. These are the
/// per-check status updates for an in-flight train.
async fn handle_train_status(pool: &sqlx::PgPool, payload: &Value) -> (StatusCode, Json<Value>) {
    let repo_full = payload
        .pointer("/repository/full_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown/unknown");
    let cr = payload.get("check_run").cloned().unwrap_or(Value::Null);
    let gh_status = cr.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let conclusion = cr.get("conclusion").and_then(|v| v.as_str()).unwrap_or("");
    let head_sha = cr.get("head_sha").and_then(|v| v.as_str()).unwrap_or("");

    let status = map_merge_train_ci_status(gh_status, conclusion);
    if status == "unknown" {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": "unhandled check_run status/conclusion"})),
        );
    }

    let Some(project_id) = resolve_project_id_for_repo(pool, repo_full).await else {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": "unknown project"})),
        );
    };

    match sqlx::query(
        "SELECT id, work_item_id FROM work_item_merge_queue \
            WHERE project_id = $1 AND head_sha = $2 LIMIT 1",
    )
    .bind(&project_id)
    .bind(head_sha)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(r)) => {
            let id: Uuid = r.get("id");
            let failure_reason = if status == "failed" {
                Some(conclusion)
            } else {
                None
            };
            let _ = sqlx::query(
                "UPDATE work_item_merge_queue \
                    SET status = $1, \
                        failed_at = CASE WHEN $1 = 'failed' THEN NOW() ELSE failed_at END, \
                        failure_reason = COALESCE($2, failure_reason) \
                  WHERE id = $3",
            )
            .bind(status)
            .bind(failure_reason)
            .bind(id)
            .execute(pool)
            .await;
            (
                StatusCode::ACCEPTED,
                Json(json!({
                    "accepted": true,
                    "event": "check_run",
                    "project": project_id,
                    "status": status,
                    "head_sha": head_sha
                })),
            )
        }
        Ok(None) => (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": "no matching merge queue entry"})),
        ),
        Err(e) => (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": format!("db error: {}", e)})),
        ),
    }
}

/// Handle `merge_group` `destroyed` (or any terminal train completion) — the
/// train has either merged or been discarded. Mark the queue entry merged.
async fn handle_train_completion(
    pool: &sqlx::PgPool,
    payload: &Value,
) -> (StatusCode, Json<Value>) {
    let repo_full = payload
        .pointer("/repository/full_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown/unknown");
    let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let mg = payload.get("merge_group").cloned().unwrap_or(Value::Null);
    let head_ref = mg.get("head_ref").and_then(|v| v.as_str()).unwrap_or("");
    let head_sha = mg.get("head_sha").and_then(|v| v.as_str()).unwrap_or("");

    let Some(project_id) = resolve_project_id_for_repo(pool, repo_full).await else {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": "unknown project"})),
        );
    };

    let Some(pr_number) = parse_merge_group_pr_number(head_ref) else {
        return (
            StatusCode::ACCEPTED,
            Json(
                json!({"accepted": false, "reason": "could not parse PR number from merge_group head_ref"}),
            ),
        );
    };

    match find_merge_queue_by_pr(pool, &project_id, pr_number).await {
        Ok(Some((id, work_item_id))) => {
            match queries::pg_mark_merge_merged(pool, id, work_item_id).await {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "accepted": true,
                        "event": "merge_group",
                        "action": action,
                        "project": project_id,
                        "pr": pr_number,
                        "head_sha": head_sha,
                        "merged": true
                    })),
                ),
                Err(e) => (
                    StatusCode::ACCEPTED,
                    Json(json!({"accepted": false, "reason": format!("db error: {}", e)})),
                ),
            }
        }
        Ok(None) => (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": "no matching merge queue entry"})),
        ),
        Err(e) => (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": format!("db error: {}", e)})),
        ),
    }
}

/// GitHub webhook receiver — drops `workflow_run` + `check_run` events
/// into `project_ci_runs` so `ff project status <id>` + the dashboard
/// can show live CI state without polling the GitHub API.
///
/// It also listens for merge-train (`merge_group`) events and mirrors them
/// into `work_item_merge_queue` so the PM dashboard reflects queue state.
///
/// HMAC-SHA256 signature verification is enforced when `fleet_secrets.github_webhook_secret`
/// is configured. Unsigned webhooks are rejected in that case.
async fn github_webhook_handler(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> (StatusCode, Json<Value>) {
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Fetch webhook secret from fleet_secrets.
    let secret = if let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) {
        sqlx::query_scalar::<_, String>(
            "SELECT value FROM fleet_secrets WHERE key = 'github_webhook_secret' LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
    } else {
        None
    };

    if let Some(ref sec) = secret {
        if signature.is_empty() {
            warn!("github webhook rejected: signature header missing but secret is configured");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"accepted": false, "reason": "signature required"})),
            );
        }
        if !verify_github_signature(&body, &signature, sec) {
            warn!("github webhook rejected: HMAC signature mismatch");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"accepted": false, "reason": "invalid signature"})),
            );
        }
        debug!("github webhook signature verified");
    } else {
        warn!(
            "github webhook secret not configured — accepting unsigned webhook (configure fleet_secrets.github_webhook_secret to enable verification)"
        );
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"accepted": false, "reason": format!("invalid json: {}", e)})),
            );
        }
    };

    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"accepted": false, "reason": "no postgres pool"})),
        );
    };

    match event.as_str() {
        "workflow_run" => {
            // GH delivers workflow_run events on action=requested/in_progress/completed.
            let wr = payload.get("workflow_run").cloned().unwrap_or(Value::Null);
            let repo_full = payload
                .pointer("/repository/full_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown/unknown")
                .to_string();
            // project_id is the short repo name (matches `projects.id` convention).
            let project_id = repo_full
                .split('/')
                .nth(1)
                .unwrap_or(&repo_full)
                .to_string();
            let branch = wr
                .get("head_branch")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let commit = wr
                .get("head_sha")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let workflow_name = wr.get("name").and_then(|v| v.as_str()).map(str::to_string);
            let run_id = wr.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string());
            let run_url = wr
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let gh_status = wr
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let conclusion = wr.get("conclusion").and_then(|v| v.as_str()).unwrap_or("");
            let status = map_workflow_run_status(gh_status.as_str(), conclusion);
            let started_at = wr
                .get("run_started_at")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let completed_at = wr
                .get("updated_at")
                .and_then(|v| v.as_str())
                .filter(|_| gh_status == "completed")
                .map(str::to_string);
            let triggered_by = wr.get("event").and_then(|v| v.as_str()).map(str::to_string);

            // UPSERT by (project_id, run_id). Skip if the project row doesn't
            // exist yet — project_ci_runs.project_id FKs projects(id).
            let project_exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1)")
                    .bind(&project_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(false);
            if !project_exists {
                tracing::warn!(target: "gh_webhook", %project_id, %repo_full, "ignoring workflow_run for unknown project");
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({"accepted": false, "reason": "unknown project"})),
                );
            }

            let _ = sqlx::query(
                "INSERT INTO project_ci_runs
                    (project_id, branch_name, commit_sha, workflow_name, run_id, run_url,
                     status, started_at, completed_at, triggered_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9::timestamptz, $10)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&project_id)
            .bind(&branch)
            .bind(&commit)
            .bind(&workflow_name)
            .bind(&run_id)
            .bind(&run_url)
            .bind(status)
            .bind(&started_at)
            .bind(&completed_at)
            .bind(&triggered_by)
            .execute(pool)
            .await;

            // Update status if the row already existed (same run_id reappears on
            // subsequent action=in_progress / action=completed deliveries).
            // Guard against out-of-order / retried deliveries: never regress a
            // run that already reached a terminal status back to a non-terminal
            // one. The update applies when the NEW status is terminal ($5) OR the
            // stored status is still non-terminal. (GitHub does not order webhook
            // deliveries — see is_terminal_ci_status.)
            if run_id.is_some() {
                let _ = sqlx::query(
                    "UPDATE project_ci_runs
                        SET status = $1,
                            completed_at = COALESCE($2::timestamptz, completed_at)
                      WHERE project_id = $3 AND run_id = $4
                        AND ($5 OR status NOT IN ('success', 'failure', 'cancelled', 'completed'))",
                )
                .bind(status)
                .bind(&completed_at)
                .bind(&project_id)
                .bind(run_id.as_deref())
                .bind(is_terminal_ci_status(status))
                .execute(pool)
                .await;
            }

            // If this workflow_run is for a GitHub merge-train branch, mirror the
            // CI status into the matching `work_item_merge_queue` row.
            if branch.starts_with("gh-readonly-queue/") {
                if let Some(pr_number) = parse_merge_group_pr_number(&branch) {
                    let train_status = map_merge_train_ci_status(gh_status.as_str(), conclusion);
                    let _ = update_merge_queue_status(
                        pool,
                        &project_id,
                        pr_number,
                        train_status,
                        conclusion,
                    )
                    .await;
                }
            }

            (
                StatusCode::ACCEPTED,
                Json(
                    json!({"accepted": true, "event": "workflow_run", "project": project_id, "run_id": run_id, "status": status}),
                ),
            )
        }
        "merge_group" => {
            let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
            match action {
                "checks_requested" => handle_train_creation(pool, &payload).await,
                "destroyed" => handle_train_completion(pool, &payload).await,
                other => (
                    StatusCode::ACCEPTED,
                    Json(
                        json!({"accepted": false, "reason": format!("merge_group action '{}' not handled", other)}),
                    ),
                ),
            }
        }
        "check_run" => handle_train_status(pool, &payload).await,
        "ping" => (
            StatusCode::OK,
            Json(json!({"accepted": true, "event": "ping", "message": "pong"})),
        ),
        other => (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": false, "reason": format!("event '{other}' not handled")})),
        ),
    }
}

async fn telegram_transport_status(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    let snapshot = telegram_transport_snapshot(state.as_ref()).await;
    Json(json!({
        "status": "ok",
        "transport": "telegram",
        "telegram": snapshot,
    }))
}

async fn telegram_transport_snapshot(state: &GatewayState) -> TelegramTransportStatus {
    let mut snapshot = TelegramTransportStatus::default();

    if let Some(config) = &state.fleet_config {
        let config = config.read().await;
        if let Some(telegram) = config.transport.telegram.as_ref() {
            snapshot.enabled = telegram.enabled;
            snapshot.allowed_chat_ids = telegram.allowed_chat_ids.clone();
            snapshot.polling_interval_secs = Some(telegram.polling_interval_secs);
            snapshot.polling_timeout_secs = Some(telegram.polling_timeout_secs);
            snapshot.media_download_dir = telegram.media_download_dir.clone();
        }
    }

    if let Some(store) = &state.operational_store {
        let runtime_status = async {
            Ok::<_, ff_db::DbError>([
                (
                    "enabled",
                    store.config_get("transport.telegram.enabled").await?,
                ),
                (
                    "running",
                    store.config_get("transport.telegram.running").await?,
                ),
                (
                    "started_at",
                    store.config_get("transport.telegram.started_at").await?,
                ),
                (
                    "last_update_id",
                    store
                        .config_get("transport.telegram.last_update_id")
                        .await?,
                ),
                (
                    "last_message_at",
                    store
                        .config_get("transport.telegram.last_message_at")
                        .await?,
                ),
                (
                    "last_error",
                    store.config_get("transport.telegram.last_error").await?,
                ),
            ])
        }
        .await;

        if let Ok(entries) = runtime_status {
            for (key, value) in entries {
                match (key, value) {
                    ("enabled", Some(raw)) => {
                        snapshot.enabled = parse_bool_like(&raw).unwrap_or(snapshot.enabled)
                    }
                    ("running", Some(raw)) => {
                        snapshot.running = parse_bool_like(&raw).unwrap_or(false)
                    }
                    ("started_at", Some(raw)) => snapshot.started_at = Some(raw),
                    ("last_update_id", Some(raw)) => {
                        snapshot.last_update_id = raw.trim().parse::<i64>().ok()
                    }
                    ("last_message_at", Some(raw)) => snapshot.last_message_at = Some(raw),
                    ("last_error", Some(raw)) => {
                        let trimmed = raw.trim();
                        snapshot.last_error = if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    snapshot
}

fn parse_bool_like(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

// ─── Prometheus Metrics ──────────────────────────────────────────────────────

/// GET /metrics — Prometheus text exposition format.
async fn serve_prometheus_metrics() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        metrics_handler(),
    )
}

// ─── Recent Traces ──────────────────────────────────────────────────────────

/// GET /api/traces/recent — return the last 100 trace summaries from the
/// in-memory ring buffer.
async fn traces_recent(Query(params): Query<TracesRecentQuery>) -> Json<Value> {
    let limit = params.limit.unwrap_or(100).min(500);
    let traces = ff_observability::global_trace_store().recent(limit);
    Json(json!({
        "count": traces.len(),
        "traces": traces,
    }))
}

#[derive(Debug, Deserialize)]
struct TracesRecentQuery {
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AuditRecentQuery {
    limit: Option<u32>,
}

/// GET /api/audit/recent and /api/audit/events — dashboard audit feed.
async fn audit_recent(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<AuditRecentQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(store) = &state.operational_store else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "operational store not initialized", "type": "not_ready"}}),
            ),
        ));
    };

    let limit = params.limit.unwrap_or(100).clamp(1, 500);
    let rows = store
        .recent_audit_log(limit)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("audit query failed: {e}"), "type": "db_error"}})),
            )
        })?;

    let events: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let details = serde_json::from_str::<Value>(&row.details_json)
                .unwrap_or_else(|_| json!({"raw": row.details_json}));
            json!({
                "id": row.id.to_string(),
                "timestamp": row.timestamp,
                "actor": row.actor,
                "action": row.event_type,
                "target": row.target,
                "details": details,
                "node": row.worker_name,
            })
        })
        .collect();

    Ok(Json(json!({
        "events": events,
        "count": events.len(),
    })))
}

// ─── Fleet Status ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct FleetStatusSummary {
    total_nodes: usize,
    connected_nodes: usize,
    unhealthy_nodes: usize,
    reachable_computers: usize,
    unreachable_computers: usize,
    computer_reachability_unknown: usize,
    joined_daemons: usize,
    unjoined_daemons: usize,
    daemon_join_unknown: usize,
    enrolled_nodes: usize,
    seed_nodes: usize,
    model_count: usize,
    leader: String,
    gateway_version: String,
}

#[derive(Debug, Clone, Serialize)]
struct FleetReplicationView {
    mode: String,
    sequence: Option<u64>,
    health: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct FleetWorkloadView {
    status: String,
    source: String,
    active_tasks: Option<usize>,
    task_ids: Vec<String>,
    status_breakdown: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
struct FleetNodeHardwareView {
    discovered_at: String,
    last_seen: String,
    open_ports: Vec<u16>,
    cpu: String,
    ram: String,
    gpu: String,
}

#[derive(Debug, Clone, Serialize)]
struct FleetNodeMetricsView {
    latency_ms: Option<u128>,
    tcp_ok: Option<bool>,
    http_ok: Option<bool>,
    checked_at: Option<String>,
    active_tasks: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct FleetNodeModelView {
    id: String,
    name: String,
    node: Option<String>,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
struct FleetNodeView {
    id: String,
    name: String,
    hostname: Option<String>,
    ip: String,
    open_ports: Vec<u16>,
    status: String,
    health: String,
    role: String,
    leader_state: String,
    is_leader: bool,
    cpu: String,
    ram: String,
    cpu_cores: Option<u32>,
    memory_gib: Option<u64>,
    gpu: String,
    models_loaded: Vec<String>,
    models_loaded_state: String,
    source_kind: String,
    seeded_from_config: bool,
    runtime_enrolled: bool,
    computer_reachable: Option<bool>,
    daemon_joined: Option<bool>,
    runtime_provenance: Vec<String>,
    last_heartbeat: String,
    heartbeat_source: String,
    heartbeat_freshness: String,
    heartbeat_age_seconds: Option<i64>,
    service_version: String,
    replication_state: FleetReplicationView,
    current_workload: FleetWorkloadView,
    hardware: FleetNodeHardwareView,
    models: Vec<FleetNodeModelView>,
    metrics: FleetNodeMetricsView,
}

#[derive(Debug, Clone, Serialize)]
struct FleetStatusPayload {
    status: String,
    total_nodes: usize,
    summary: FleetStatusSummary,
    models: Vec<FleetNodeModelView>,
    nodes: Vec<FleetNodeView>,
    scanned_at: String,
}

#[derive(Debug, Clone, Default)]
struct DbNodeSnapshot {
    role: Option<String>,
    status: Option<String>,
    last_heartbeat: Option<String>,
    models: Vec<String>,
    service_version: Option<String>,
    replication_state: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct NodeWorkloadAggregate {
    active_tasks: usize,
    task_ids: Vec<String>,
    status_breakdown: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default)]
struct DbFleetSnapshot {
    nodes_by_name: HashMap<String, DbNodeSnapshot>,
    nodes_by_host: HashMap<String, DbNodeSnapshot>,
    workloads: HashMap<String, NodeWorkloadAggregate>,
    replication_sequence: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct ConfigNodeHints {
    role: Option<String>,
    cpu: Option<String>,
    ram: Option<String>,
    cpu_cores: Option<u32>,
    memory_gib: Option<u64>,
    gpu: Option<String>,
}

#[derive(Debug, Clone)]
struct HeartbeatView {
    value: String,
    source: String,
    freshness: String,
    age_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct FleetHeartbeatPayload {
    node_id: String,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    ips: Vec<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    heartbeat_at: Option<String>,
    #[serde(default)]
    resources: Value,
    #[serde(default)]
    services: Value,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    capabilities: Value,
    #[serde(default)]
    stale: Option<FleetHeartbeatStaleness>,
}

#[derive(Debug, Deserialize)]
struct FleetHeartbeatStaleness {
    degraded_after_secs: Option<i64>,
    offline_after_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct FleetEnrollPayload {
    node_id: String,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    ips: Vec<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    heartbeat_at: Option<String>,
    #[serde(default)]
    resources: Value,
    #[serde(default)]
    services: Value,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    capabilities: Value,
    #[serde(default)]
    service_version: Option<String>,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    stale: Option<FleetHeartbeatStaleness>,
}

fn extract_enrollment_token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(token) = headers
        .get("x-fleet-enrollment-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(token.to_string());
    }

    if let Some(token) = headers
        .get("x-enrollment-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(token.to_string());
    }

    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// GET /api/fleet/status — complete connected fleet operational status.
async fn fleet_status(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let payload = build_fleet_status_payload(&state).await?;
    let value = serde_json::to_value(payload).unwrap_or_else(|_| json!({"status": "error"}));
    Ok(Json(value))
}

/// GET /api/router/diagnostics — Pulse router internal state.
///
/// Exposes session affinity entry count and circuit breaker status
/// so operators can debug routing decisions without reading logs.
async fn router_diagnostics(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let diag = match state.pulse_router.as_ref() {
        Some(router) => router.diagnostics(),
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Pulse router not initialized"})),
            ));
        }
    };
    Ok(Json(json!({
        "session_affinity_entries": diag.session_affinity_entries,
        "circuit_breaker_nodes": diag.circuit_breaker_nodes,
    })))
}

/// GET /api/fleet/llm-usage — aggregate token + cost from the
/// existing `cloud_llm_usage` ledger. Optional filters via query
/// string: `?since=1h|24h|7d` (default 24h), `?provider=<id>`,
/// `?session_id=<id>`. Returns one row per provider with totals.
///
/// Foundation of Pillar 3 (token + billing accounting). The data is
/// already captured by `record_usage()` for every cloud_llm call —
/// this endpoint exposes it.
async fn fleet_llm_usage(
    State(state): State<Arc<GatewayState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"postgres not configured"})),
        ));
    };

    let since = params.get("since").map(String::as_str).unwrap_or("24h");
    let since_secs: i64 = match since {
        "1h" => 3600,
        "6h" => 6 * 3600,
        "12h" => 12 * 3600,
        "24h" | "1d" => 24 * 3600,
        "7d" => 7 * 24 * 3600,
        "30d" => 30 * 24 * 3600,
        _ => 24 * 3600,
    };

    let provider_filter = params.get("provider").cloned();
    let session_filter = params.get("session_id").cloned();

    let mut sql = String::from(
        "SELECT provider_id,
                COUNT(*)                                  AS call_count,
                COALESCE(SUM(tokens_input), 0)            AS total_in,
                COALESCE(SUM(tokens_output), 0)           AS total_out,
                COALESCE(SUM(cost_usd)::FLOAT8, 0.0)      AS total_cost_usd
           FROM cloud_llm_usage
          WHERE used_at > NOW() - make_interval(secs => $1::int)",
    );
    let mut bindings: Vec<String> = Vec::new();
    if let Some(p) = &provider_filter {
        bindings.push(p.clone());
        sql.push_str(&format!(" AND provider_id = ${}", bindings.len() + 1));
    }
    if let Some(s) = &session_filter {
        bindings.push(s.clone());
        sql.push_str(&format!(" AND session_id = ${}", bindings.len() + 1));
    }
    sql.push_str(" GROUP BY provider_id ORDER BY total_cost_usd DESC, total_in DESC");

    let mut q = sqlx::query(&sql).bind(since_secs as i32);
    for b in &bindings {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {e}")})),
        )
    })?;

    use sqlx::Row;
    let mut out = Vec::with_capacity(rows.len());
    let mut grand_in: i64 = 0;
    let mut grand_out: i64 = 0;
    let mut grand_calls: i64 = 0;
    let mut grand_cost = 0.0_f64;
    for r in rows {
        let provider_id: String = r.get("provider_id");
        let call_count: i64 = r.get("call_count");
        let total_in: i64 = r.get("total_in");
        let total_out: i64 = r.get("total_out");
        let cost_usd: f64 = r.get("total_cost_usd");
        grand_in += total_in;
        grand_out += total_out;
        grand_calls += call_count;
        grand_cost += cost_usd;
        out.push(json!({
            "provider_id": provider_id,
            "call_count": call_count,
            "tokens_input": total_in,
            "tokens_output": total_out,
            "tokens_total": total_in + total_out,
            "cost_usd": cost_usd,
        }));
    }

    Ok(Json(json!({
        "since": since,
        "since_secs": since_secs,
        "provider_filter": provider_filter,
        "session_filter": session_filter,
        "providers": out,
        "totals": {
            "call_count":     grand_calls,
            "tokens_input":   grand_in,
            "tokens_output":  grand_out,
            "tokens_total":   grand_in + grand_out,
            "cost_usd":       grand_cost,
        }
    })))
}

// ─── Token Ledger API endpoints ──────────────────────────────────────────────

/// GET /api/ledger/summary — Fleet-wide token usage and cost summary.
async fn ledger_summary(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<FleetCostSummary>, (StatusCode, Json<Value>)> {
    let summary = state.cost_tracker.summary().await;
    Ok(Json(summary))
}

/// GET /api/ledger/models — Per-model token usage and cost stats.
async fn ledger_models(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Vec<ModelCostStats>>, (StatusCode, Json<Value>)> {
    let stats = state.cost_tracker.model_stats();
    Ok(Json(stats))
}

/// GET /api/ledger/budget — Current budget configuration.
async fn ledger_budget(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let budget = state.cost_tracker.budget_config().await;
    Ok(Json(json!(budget)))
}

/// POST /api/ledger/budget — Update budget configuration.
async fn ledger_budget_update(
    State(state): State<Arc<GatewayState>>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let config = match serde_json::from_value::<ff_api::token_ledger::BudgetConfig>(payload) {
        Ok(c) => c,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid budget config: {e}")})),
            ));
        }
    };
    state.cost_tracker.set_budget(config.clone()).await;
    Ok(Json(json!({"status": "ok", "budget": config })))
}

/// POST /api/ledger/flush — Persist in-memory token ledger to Postgres.
async fn ledger_flush(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres not configured"})),
        ));
    };

    match state.cost_tracker.flush_to_db(pool).await {
        Ok(count) => Ok(Json(json!({"status": "ok", "records_flushed": count }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("flush failed: {e}")})),
        )),
    }
}

/// GET /api/ledger/records — Recent token usage records.
async fn ledger_records(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let day = params
        .get("day")
        .cloned()
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);

    let records = state.cost_tracker.daily_records(&day).await;
    let limited: Vec<_> = records.into_iter().rev().take(limit).collect();

    Ok(Json(json!({
        "day": day,
        "count": limited.len(),
        "records": limited,
    })))
}

/// GET /api/ledger/health — Health check for the token ledger subsystem.
async fn ledger_health(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let summary = state.cost_tracker.summary().await;
    let budget = state.cost_tracker.budget_config().await;
    let healthy = summary.budget_percent_used < 100.0 || !budget.enforce_budget;

    Ok(Json(json!({
        "status": if healthy { "ok" } else { "budget_exceeded" },
        "healthy": healthy,
        "daily_cost_usd": summary.daily_cost_usd,
        "daily_budget_usd": summary.daily_budget_usd,
        "budget_remaining_usd": summary.budget_remaining_usd,
        "budget_percent_used": summary.budget_percent_used,
        "total_requests": summary.total_requests,
        "total_cost_usd": summary.total_cost_usd,
    })))
}

/// GET /api/fleet/nodes/{id} — direct node detail endpoint used by dashboard.
async fn fleet_worker_detail(
    Path(id): Path<String>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let payload = build_fleet_status_payload(&state).await?;

    let Some(node) = payload.nodes.into_iter().find(|n| {
        n.id == id || n.name == id || n.hostname.as_deref() == Some(id.as_str()) || n.ip == id
    }) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": {"message": format!("node not found: {id}"), "type": "not_found"}}),
            ),
        ));
    };

    let value = serde_json::to_value(node).unwrap_or_else(|_| json!({}));
    Ok(Json(value))
}

fn normalize_role_for_runtime(raw: Option<String>) -> String {
    let normalized = raw
        .unwrap_or_else(|| "worker".to_string())
        .trim()
        .to_ascii_lowercase();

    match normalized.as_str() {
        "leader" | "worker" | "gateway" | "builder" => normalized,
        _ => "worker".to_string(),
    }
}

fn normalize_status_for_runtime(raw: Option<String>) -> String {
    match raw
        .unwrap_or_else(|| "online".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "online" | "healthy" | "ok" => "online".to_string(),
        "degraded" | "starting" | "maintenance" | "busy" => "degraded".to_string(),
        "offline" | "unreachable" | "down" => "offline".to_string(),
        _ => "unknown".to_string(),
    }
}

fn normalize_object(value: Value) -> Value {
    if value.is_object() { value } else { json!({}) }
}

fn normalize_array(value: Value) -> Value {
    if value.is_array() { value } else { json!([]) }
}

/// POST /api/fleet/enroll — trust-gated runtime node enrollment.
async fn fleet_enroll(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(payload): Json<FleetEnrollPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(runtime_registry) = &state.runtime_registry else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "runtime registry not initialized", "type": "not_ready"}}),
            ),
        ));
    };

    let Some(config_lock) = &state.fleet_config else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "fleet config not loaded", "type": "not_ready"}})),
        ));
    };

    let config = config_lock.read().await.clone();
    let enrollment = config.enrollment.clone();

    let policy = enrollment.enforcement_policy();
    if matches!(
        &policy,
        ff_core::config::EnrollmentEnforcement::MisconfiguredRequired
    ) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "enrollment shared secret is not configured", "type": "enrollment_not_configured"}}),
            ),
        ));
    }

    let node_id = payload.node_id.trim().to_string();
    if node_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "node_id is required", "type": "invalid_payload"}})),
        ));
    }

    let hostname = payload
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| node_id.clone());

    let mut ips = payload
        .ips
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if let Some(single_ip) = payload
        .ip
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        ips.push(single_ip.to_string());
    }
    ips.sort();
    ips.dedup();

    let normalized_allowed_roles = enrollment
        .allowed_roles
        .iter()
        .map(|role| normalize_role_for_runtime(Some(role.clone())))
        .collect::<HashSet<_>>();

    let requested_role = normalize_role_for_runtime(
        payload
            .role
            .clone()
            .or_else(|| enrollment.default_role.clone()),
    );

    if !normalized_allowed_roles.is_empty() && !normalized_allowed_roles.contains(&requested_role) {
        let capabilities = normalize_object(payload.capabilities.clone());
        let event = queries::FleetEnrollmentEventInsert {
            node_id: Some(node_id.clone()),
            hostname: Some(hostname.clone()),
            outcome: "rejected".to_string(),
            reason: Some(format!("requested role '{requested_role}' is not allowed")),
            role: Some(requested_role.clone()),
            service_version: payload.service_version.clone(),
            addresses_json: serde_json::to_string(&ips).unwrap_or_else(|_| "[]".to_string()),
            capabilities_json: serde_json::to_string(&capabilities)
                .unwrap_or_else(|_| "{}".to_string()),
            metadata_json: serde_json::to_string(&normalize_object(payload.metadata.clone()))
                .unwrap_or_else(|_| "{}".to_string()),
        };
        let _ = runtime_registry.insert_enrollment_event(&event).await;

        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({"error": {"message": "requested role is not allowed", "type": "role_not_allowed"}}),
            ),
        ));
    }

    let presented_token = payload
        .token
        .clone()
        .or_else(|| extract_enrollment_token_from_headers(&headers));

    match &policy {
        ff_core::config::EnrollmentEnforcement::Disabled => {
            tracing::warn!(
                endpoint = "/api/fleet/enroll",
                node = %node_id,
                "enrollment token check DISABLED (require_shared_secret=false) — accepting request without auth"
            );
        }
        ff_core::config::EnrollmentEnforcement::Required(expected) => {
            if presented_token.as_deref() != Some(expected.as_str()) {
                let capabilities = normalize_object(payload.capabilities.clone());
                let event = queries::FleetEnrollmentEventInsert {
                    node_id: Some(node_id.clone()),
                    hostname: Some(hostname.clone()),
                    outcome: "rejected".to_string(),
                    reason: Some("invalid enrollment token".to_string()),
                    role: Some(requested_role.clone()),
                    service_version: payload.service_version.clone(),
                    addresses_json: serde_json::to_string(&ips)
                        .unwrap_or_else(|_| "[]".to_string()),
                    capabilities_json: serde_json::to_string(&capabilities)
                        .unwrap_or_else(|_| "{}".to_string()),
                    metadata_json: serde_json::to_string(&normalize_object(
                        payload.metadata.clone(),
                    ))
                    .unwrap_or_else(|_| "{}".to_string()),
                };
                let _ = runtime_registry.insert_enrollment_event(&event).await;

                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(
                        json!({"error": {"message": "invalid enrollment token", "type": "unauthorized"}}),
                    ),
                ));
            }
        }
        ff_core::config::EnrollmentEnforcement::MisconfiguredRequired => {
            unreachable!("handled above")
        }
    }

    let reported_status = "online".to_string();
    let heartbeat_at = if let Some(raw) = payload.heartbeat_at {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        } else {
            chrono::DateTime::parse_from_rfc3339(trimmed)
                .map_err(|error| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": {"message": format!("invalid heartbeat_at timestamp: {error}"), "type": "invalid_payload"}})),
                    )
                })?
                .with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }
    } else {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };

    let degraded_after_secs = payload
        .stale
        .as_ref()
        .and_then(|stale| stale.degraded_after_secs)
        .unwrap_or(90)
        .max(1);
    let offline_after_secs = payload
        .stale
        .as_ref()
        .and_then(|stale| stale.offline_after_secs)
        .unwrap_or(180)
        .max(degraded_after_secs + 1);

    let resources = normalize_object(payload.resources);
    let services = normalize_array(payload.services);
    let capabilities = normalize_object(payload.capabilities);
    let metadata = normalize_object(payload.metadata);

    let mut models = payload
        .models
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();

    let heartbeat_row = queries::FleetNodeRuntimeHeartbeatRow {
        node_id: node_id.clone(),
        hostname: hostname.clone(),
        ips_json: serde_json::to_string(&ips).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize ips: {error}"), "type": "serialization_error"}})),
            )
        })?,
        role: requested_role.clone(),
        reported_status,
        last_heartbeat: heartbeat_at,
        resources_json: serde_json::to_string(&resources).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize resources: {error}"), "type": "serialization_error"}})),
            )
        })?,
        services_json: serde_json::to_string(&services).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize services: {error}"), "type": "serialization_error"}})),
            )
        })?,
        models_json: serde_json::to_string(&models).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize models: {error}"), "type": "serialization_error"}})),
            )
        })?,
        capabilities_json: serde_json::to_string(&capabilities).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize capabilities: {error}"), "type": "serialization_error"}})),
            )
        })?,
        stale_degraded_after_secs: degraded_after_secs,
        stale_offline_after_secs: offline_after_secs,
    };

    let event = queries::FleetEnrollmentEventInsert {
        node_id: Some(node_id.clone()),
        hostname: Some(hostname.clone()),
        outcome: "accepted".to_string(),
        reason: None,
        role: Some(requested_role.clone()),
        service_version: payload.service_version,
        addresses_json: serde_json::to_string(&ips).unwrap_or_else(|_| "[]".to_string()),
        capabilities_json: serde_json::to_string(&capabilities)
            .unwrap_or_else(|_| "{}".to_string()),
        metadata_json: serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string()),
    };

    let runtime_row = runtime_registry
        .upsert_runtime_with_enrollment(&heartbeat_row, &event)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to enroll node: {error}"), "type": "db_error"}})),
            )
        })?;

    let heartbeat_interval_secs = enrollment
        .heartbeat_interval_secs
        .unwrap_or(config.fleet.heartbeat_interval_secs)
        .max(1);

    Ok(Json(json!({
        "status": "ok",
        "enrollment": {
            "accepted": true,
            "node_id": runtime_row.node_id,
            "hostname": runtime_row.hostname,
            "assigned_role": requested_role,
            "derived_status": runtime_row.derived_status,
            "heartbeat_interval_secs": heartbeat_interval_secs,
            "heartbeat_endpoint": "/api/fleet/heartbeat"
        }
    })))
}

/// POST /api/fleet/heartbeat — persist live runtime node state.
async fn fleet_heartbeat(
    State(state): State<Arc<GatewayState>>,
    Json(payload): Json<FleetHeartbeatPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(runtime_registry) = &state.runtime_registry else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "runtime registry not initialized", "type": "not_ready"}}),
            ),
        ));
    };

    let node_id = payload.node_id.trim().to_string();
    if node_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "node_id is required", "type": "invalid_payload"}})),
        ));
    }

    let hostname = payload
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| node_id.clone());

    let mut ips = payload
        .ips
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if let Some(single_ip) = payload
        .ip
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        ips.push(single_ip.to_string());
    }
    ips.sort();
    ips.dedup();

    let role = normalize_role_for_runtime(payload.role);
    let reported_status = normalize_status_for_runtime(payload.status);

    let heartbeat_at = if let Some(raw) = payload.heartbeat_at {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        } else {
            chrono::DateTime::parse_from_rfc3339(trimmed)
                .map_err(|error| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": {"message": format!("invalid heartbeat_at timestamp: {error}"), "type": "invalid_payload"}})),
                    )
                })?
                .with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }
    } else {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };

    let degraded_after_secs = payload
        .stale
        .as_ref()
        .and_then(|stale| stale.degraded_after_secs)
        .unwrap_or(90)
        .max(1);
    let offline_after_secs = payload
        .stale
        .as_ref()
        .and_then(|stale| stale.offline_after_secs)
        .unwrap_or(180)
        .max(degraded_after_secs + 1);

    let resources = normalize_object(payload.resources);
    let services = normalize_array(payload.services);
    let capabilities = normalize_object(payload.capabilities);

    let mut models = payload
        .models
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();

    let heartbeat_row = queries::FleetNodeRuntimeHeartbeatRow {
        node_id: node_id.clone(),
        hostname: hostname.clone(),
        ips_json: serde_json::to_string(&ips).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize ips: {error}"), "type": "serialization_error"}})),
            )
        })?,
        role,
        reported_status,
        last_heartbeat: heartbeat_at,
        resources_json: serde_json::to_string(&resources).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize resources: {error}"), "type": "serialization_error"}})),
            )
        })?,
        services_json: serde_json::to_string(&services).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize services: {error}"), "type": "serialization_error"}})),
            )
        })?,
        models_json: serde_json::to_string(&models).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize models: {error}"), "type": "serialization_error"}})),
            )
        })?,
        capabilities_json: serde_json::to_string(&capabilities).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to serialize capabilities: {error}"), "type": "serialization_error"}})),
            )
        })?,
        stale_degraded_after_secs: degraded_after_secs,
        stale_offline_after_secs: offline_after_secs,
    };

    let runtime_row = runtime_registry
        .upsert_runtime(&heartbeat_row)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("failed to persist heartbeat: {error}"), "type": "db_error"}})),
            )
        })?;

    Ok(Json(json!({
        "status": "ok",
        "node_id": runtime_row.node_id,
        "hostname": runtime_row.hostname,
        "derived_status": runtime_row.derived_status,
        "heartbeat_age_seconds": runtime_row.heartbeat_age_secs,
        "last_heartbeat": runtime_row.last_heartbeat,
    })))
}

async fn build_fleet_status_payload(
    state: &GatewayState,
) -> Result<FleetStatusPayload, (StatusCode, Json<Value>)> {
    let nodes = state
        .discovery_registry
        .as_ref()
        .map(|registry| registry.list_nodes())
        .unwrap_or_default();

    let fleet_config = if let Some(cfg_lock) = &state.fleet_config {
        Some(cfg_lock.read().await.clone())
    } else {
        None
    };

    let db_snapshot = load_db_fleet_snapshot(state).await;

    // leader_hint reads the authoritative LeaderTick state directly from
    // Postgres `fleet_leader_state` — the same source the `/api/fleet/leader`
    // GET and `/.well-known/forgefleet` use. Deep-review #4: the old
    // discovery/TOML election that populated this via
    // discovery_registry.current_leader() ran concurrently with LeaderTick and
    // could leak a stale winner here, so it was removed. The DB runtime-snapshot
    // role=="leader" path can't surface it either (fleet_worker_runtime is empty
    // in practice — it would always yield None). One source of truth now.
    let leader_hint = match state.operational_store.as_ref().and_then(|os| os.pg_pool()) {
        Some(pool) => sqlx::query_scalar::<_, String>(
            "SELECT member_name FROM fleet_leader_state WHERE singleton_key = 'current'",
        )
        .fetch_optional(pool)
        .await
        .ok()
        .flatten(),
        None => None,
    };

    Ok(assemble_fleet_status_payload(
        nodes,
        leader_hint,
        fleet_config.as_ref(),
        db_snapshot.as_ref(),
        None,
    ))
}

fn assemble_fleet_status_payload(
    nodes: Vec<FleetComputer>,
    leader_hint: Option<String>,
    fleet_config: Option<&ff_core::config::FleetConfig>,
    db_snapshot: Option<&DbFleetSnapshot>,
    leader_sequence: Option<u64>,
) -> FleetStatusPayload {
    let (config_by_name, config_by_ip) = build_config_hints(fleet_config);

    let mut node_views = Vec::with_capacity(nodes.len());
    let mut seen_config_names = HashSet::new();
    let mut seen_ips = HashSet::new();

    for node in &nodes {
        let ip = node.ip.to_string();
        let config_hint = node
            .config_name
            .as_ref()
            .and_then(|name| config_by_name.get(name))
            .or_else(|| config_by_ip.get(&ip));

        let db_node = db_snapshot.and_then(|snapshot| {
            node.config_name
                .as_ref()
                .and_then(|name| snapshot.nodes_by_name.get(name))
                .or_else(|| snapshot.nodes_by_host.get(&ip))
        });

        let workload = db_snapshot.and_then(|snapshot| {
            node.config_name
                .as_ref()
                .and_then(|name| snapshot.workloads.get(name))
                .or_else(|| snapshot.workloads.get(&ip))
        });

        let view = build_fleet_worker_view(
            node,
            leader_hint.as_deref(),
            config_hint,
            db_node,
            workload,
            db_snapshot.is_some(),
            leader_sequence,
            db_snapshot.and_then(|snapshot| snapshot.replication_sequence),
        );

        if let Some(name) = &node.config_name {
            seen_config_names.insert(name.clone());
        }
        seen_ips.insert(ip);

        node_views.push(view);
    }

    // Preserve static fleet.toml seeds that are not represented in runtime registry.
    // Runtime entries always win if both sources refer to the same node.
    if let Some(config) = fleet_config {
        for (name, node_cfg) in &config.nodes {
            let ip = node_cfg.ip.trim();
            let has_runtime_entry =
                seen_config_names.contains(name) || (!ip.is_empty() && seen_ips.contains(ip));
            if has_runtime_entry {
                continue;
            }

            let view = build_seed_node_view(
                name,
                node_cfg,
                config.fleet.api_port,
                leader_hint.as_deref(),
                config_by_name.get(name),
                db_snapshot.and_then(|snapshot| {
                    snapshot
                        .nodes_by_name
                        .get(name)
                        .or_else(|| snapshot.nodes_by_host.get(ip))
                }),
                db_snapshot.is_some(),
            );

            if !ip.is_empty() {
                seen_ips.insert(ip.to_string());
            }
            seen_config_names.insert(name.clone());
            node_views.push(view);
        }
    }

    node_views.sort_by(|a, b| a.name.cmp(&b.name));

    let connected_nodes = node_views
        .iter()
        .filter(|node| node.status == "online" || node.status == "degraded")
        .count();

    let unhealthy_nodes = node_views
        .iter()
        .filter(|node| node.status != "online")
        .count();

    let reachable_computers = node_views
        .iter()
        .filter(|node| node.computer_reachable == Some(true))
        .count();
    let unreachable_computers = node_views
        .iter()
        .filter(|node| node.computer_reachable == Some(false))
        .count();
    let computer_reachability_unknown = node_views
        .iter()
        .filter(|node| node.computer_reachable.is_none())
        .count();
    let joined_daemons = node_views
        .iter()
        .filter(|node| node.daemon_joined == Some(true))
        .count();
    let unjoined_daemons = node_views
        .iter()
        .filter(|node| node.daemon_joined == Some(false))
        .count();
    let daemon_join_unknown = node_views
        .iter()
        .filter(|node| node.daemon_joined.is_none())
        .count();

    let enrolled_nodes = node_views
        .iter()
        .filter(|node| node.runtime_enrolled)
        .count();
    let seed_nodes = node_views
        .iter()
        .filter(|node| node.source_kind == "seed/static")
        .count();

    let mut model_ids = HashSet::new();
    for node in &node_views {
        for model in &node.models_loaded {
            model_ids.insert(model.clone());
        }
    }

    let mut model_rows = Vec::new();
    let mut seen_model_rows = HashSet::new();
    for node in &node_views {
        for model in &node.models {
            let key = format!("{}::{}", model.id, model.node.clone().unwrap_or_default());
            if seen_model_rows.insert(key) {
                model_rows.push(model.clone());
            }
        }
    }

    FleetStatusPayload {
        status: "ok".to_string(),
        total_nodes: node_views.len(),
        summary: FleetStatusSummary {
            total_nodes: node_views.len(),
            connected_nodes,
            unhealthy_nodes,
            reachable_computers,
            unreachable_computers,
            computer_reachability_unknown,
            joined_daemons,
            unjoined_daemons,
            daemon_join_unknown,
            enrolled_nodes,
            seed_nodes,
            model_count: model_ids.len(),
            leader: leader_hint.unwrap_or_else(|| "unknown".to_string()),
            gateway_version: ff_core::VERSION.to_string(),
        },
        models: model_rows,
        nodes: node_views,
        scanned_at: Utc::now().to_rfc3339(),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_fleet_worker_view(
    node: &FleetComputer,
    leader_hint: Option<&str>,
    config_hint: Option<&ConfigNodeHints>,
    db_node: Option<&DbNodeSnapshot>,
    workload: Option<&NodeWorkloadAggregate>,
    db_available: bool,
    leader_sequence: Option<u64>,
    local_replication_sequence: Option<u64>,
) -> FleetNodeView {
    let display_name = node
        .config_name
        .clone()
        .or(node.hostname.clone())
        .unwrap_or_else(|| node.ip.to_string());

    let seeded_from_config = node.config_name.is_some() || config_hint.is_some();
    let runtime_enrolled = is_runtime_enrolled(node, db_node);
    let computer_reachable = derive_computer_reachable(node.health.as_ref().map(|h| &h.status));
    let daemon_joined = derive_daemon_joined(db_available, db_node.is_some());
    let source_kind = if runtime_enrolled {
        "enrolled/live".to_string()
    } else if seeded_from_config {
        "seed/static".to_string()
    } else {
        "unknown".to_string()
    };
    let runtime_provenance =
        derive_runtime_provenance(node, db_node, seeded_from_config, runtime_enrolled);

    // Runtime telemetry (db/registry) wins over static config hints.
    let role = db_node
        .and_then(|db| db.role.clone())
        .or_else(|| config_hint.and_then(|hint| hint.role.clone()))
        .unwrap_or_else(|| "unknown".to_string());

    // A node is the leader if its live DB role says so, OR if it matches the
    // authoritative leader_hint by any of its names. The role check is the
    // robust path: leader_hint is the member_name from fleet_leader_state
    // (e.g. "taylor") and a registry-discovered node may carry no config_name/
    // hostname to match it against — but its db_node.role (now sourced from the
    // live fleet_workers table) does resolve to "leader".
    let is_leader = role == "leader"
        || role == "gateway"
        || leader_hint.is_some_and(|leader| {
            node.config_name.as_deref() == Some(leader)
                || node.hostname.as_deref() == Some(leader)
                || display_name == leader
        });

    let leader_state = if is_leader {
        "leader".to_string()
    } else if leader_hint.is_some() || role != "unknown" {
        "follower".to_string()
    } else {
        "unknown".to_string()
    };

    let status = derive_node_status(
        node.health.as_ref().map(|h| &h.status),
        db_node.and_then(|d| d.status.as_deref()),
    );

    let (cpu, ram, gpu) = derive_node_resources(node, config_hint);
    let (cpu_cores, memory_gib) = derive_node_capacity(node, config_hint);

    let (models_loaded, models_loaded_state) = derive_models_loaded(node, db_node);

    let heartbeat = derive_last_heartbeat(node, db_node, runtime_enrolled);

    let service_version = db_node
        .and_then(|db| db.service_version.clone())
        .unwrap_or_else(|| "unreported".to_string());

    let replication_state = build_replication_view(
        is_leader,
        leader_hint.is_some(),
        db_node.and_then(|db| db.replication_state.as_deref()),
        leader_sequence,
        local_replication_sequence,
    );

    let current_workload = build_workload_view(workload, db_available);

    let health = match status.as_str() {
        "online" => "healthy".to_string(),
        "degraded" => "degraded".to_string(),
        "offline" => "offline".to_string(),
        _ => "unknown".to_string(),
    };

    let model_rows = if !node.models.is_empty() {
        node.models
            .iter()
            .map(|model| FleetNodeModelView {
                id: model.id.clone(),
                name: model.id.clone(),
                node: model.owned_by.clone().or_else(|| node.config_name.clone()),
                status: if status == "online" {
                    "healthy".to_string()
                } else {
                    status.clone()
                },
            })
            .collect()
    } else {
        models_loaded
            .iter()
            .map(|id| FleetNodeModelView {
                id: id.clone(),
                name: id.clone(),
                node: node.config_name.clone(),
                status: if status == "unknown" {
                    "unknown".to_string()
                } else {
                    "reported".to_string()
                },
            })
            .collect()
    };

    FleetNodeView {
        id: node.id.to_string(),
        name: display_name,
        hostname: node.hostname.clone(),
        ip: node.ip.to_string(),
        open_ports: node.open_ports.clone(),
        status: status.clone(),
        health,
        role,
        leader_state,
        is_leader,
        cpu: cpu.clone(),
        ram: ram.clone(),
        cpu_cores,
        memory_gib,
        gpu: gpu.clone(),
        models_loaded,
        models_loaded_state,
        source_kind,
        seeded_from_config,
        runtime_enrolled,
        computer_reachable,
        daemon_joined,
        runtime_provenance,
        last_heartbeat: heartbeat.value,
        heartbeat_source: heartbeat.source,
        heartbeat_freshness: heartbeat.freshness,
        heartbeat_age_seconds: heartbeat.age_seconds,
        service_version,
        replication_state,
        current_workload: current_workload.clone(),
        hardware: FleetNodeHardwareView {
            discovered_at: node.discovered_at.to_rfc3339(),
            last_seen: node.last_seen.to_rfc3339(),
            open_ports: node.open_ports.clone(),
            cpu,
            ram,
            gpu,
        },
        models: model_rows,
        metrics: FleetNodeMetricsView {
            latency_ms: node.health.as_ref().map(|h| h.latency_ms),
            tcp_ok: node.health.as_ref().map(|h| h.tcp_ok),
            http_ok: node.health.as_ref().and_then(|h| h.http_ok),
            checked_at: node.health.as_ref().map(|h| h.checked_at.to_rfc3339()),
            active_tasks: current_workload.active_tasks,
        },
    }
}

fn build_seed_node_view(
    name: &str,
    node_cfg: &ff_core::config::NodeConfig,
    default_api_port: u16,
    leader_hint: Option<&str>,
    config_hint: Option<&ConfigNodeHints>,
    db_node: Option<&DbNodeSnapshot>,
    db_available: bool,
) -> FleetNodeView {
    let role = config_hint
        .and_then(|hint| hint.role.clone())
        .unwrap_or_else(|| role_to_string(node_cfg.role));

    let is_leader = if let Some(leader) = leader_hint {
        leader == name || leader == node_cfg.ip
    } else {
        role == "leader" || role == "gateway"
    };

    let leader_state = if is_leader {
        "leader".to_string()
    } else if leader_hint.is_some() {
        "follower".to_string()
    } else {
        "unknown".to_string()
    };

    let cpu = config_hint
        .and_then(|hint| hint.cpu.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let ram = config_hint
        .and_then(|hint| hint.ram.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let gpu = config_hint
        .and_then(|hint| hint.gpu.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let port = node_cfg.port.unwrap_or(default_api_port);

    FleetNodeView {
        id: format!("seed::{name}"),
        name: name.to_string(),
        hostname: None,
        ip: node_cfg.ip.clone(),
        open_ports: vec![port],
        status: "unknown".to_string(),
        health: "unknown".to_string(),
        role,
        leader_state,
        is_leader,
        cpu: cpu.clone(),
        ram: ram.clone(),
        cpu_cores: config_hint.and_then(|hint| hint.cpu_cores),
        memory_gib: config_hint.and_then(|hint| hint.memory_gib),
        gpu: gpu.clone(),
        models_loaded: Vec::new(),
        models_loaded_state: "unreported".to_string(),
        source_kind: "seed/static".to_string(),
        seeded_from_config: true,
        runtime_enrolled: false,
        computer_reachable: None,
        daemon_joined: derive_daemon_joined(db_available, db_node.is_some()),
        runtime_provenance: vec!["fleet.toml.seed".to_string()],
        last_heartbeat: "unknown".to_string(),
        heartbeat_source: "unreported".to_string(),
        heartbeat_freshness: "unknown".to_string(),
        heartbeat_age_seconds: None,
        service_version: "unreported".to_string(),
        replication_state: FleetReplicationView {
            mode: "unknown".to_string(),
            sequence: None,
            health: "unreported".to_string(),
            detail: "seed node (static config) has no runtime replication telemetry".to_string(),
        },
        current_workload: FleetWorkloadView {
            status: "unreported".to_string(),
            source: "seed.static".to_string(),
            active_tasks: None,
            task_ids: Vec::new(),
            status_breakdown: BTreeMap::new(),
        },
        hardware: FleetNodeHardwareView {
            discovered_at: "unknown".to_string(),
            last_seen: "unknown".to_string(),
            open_ports: vec![port],
            cpu,
            ram,
            gpu,
        },
        models: Vec::new(),
        metrics: FleetNodeMetricsView {
            latency_ms: None,
            tcp_ok: None,
            http_ok: None,
            checked_at: None,
            active_tasks: None,
        },
    }
}

fn is_runtime_enrolled(node: &FleetComputer, db_node: Option<&DbNodeSnapshot>) -> bool {
    node.config_name.is_none()
        || node.health.is_some()
        || node.hardware.is_some()
        || !node.models.is_empty()
        || db_node.is_some()
}

fn derive_runtime_provenance(
    node: &FleetComputer,
    db_node: Option<&DbNodeSnapshot>,
    seeded_from_config: bool,
    runtime_enrolled: bool,
) -> Vec<String> {
    let mut provenance = Vec::new();

    if node.config_name.is_none() {
        provenance.push("registry.discovery".to_string());
    }
    if node.health.is_some() {
        provenance.push("registry.health".to_string());
    }
    if node.hardware.is_some() {
        provenance.push("registry.hardware".to_string());
    }
    if !node.models.is_empty() {
        provenance.push("registry.models".to_string());
    }
    if db_node.is_some() {
        provenance.push("db.node_snapshot".to_string());
    }

    if provenance.is_empty() {
        if seeded_from_config && !runtime_enrolled {
            provenance.push("fleet.toml.seed".to_string());
        } else {
            provenance.push("registry.node".to_string());
        }
    }

    provenance
}

fn derive_node_status(health: Option<&HealthStatus>, db_status: Option<&str>) -> String {
    if let Some(health) = health {
        return match health {
            HealthStatus::Healthy => "online",
            HealthStatus::Degraded => "degraded",
            HealthStatus::Unreachable => "offline",
        }
        .to_string();
    }

    match db_status.map(|status| status.trim().to_ascii_lowercase()) {
        Some(status) if status == "online" => "online".to_string(),
        Some(status) if status == "degraded" || status == "starting" || status == "maintenance" => {
            "degraded".to_string()
        }
        Some(status) if status == "offline" => "offline".to_string(),
        _ => "unknown".to_string(),
    }
}

fn derive_computer_reachable(health: Option<&HealthStatus>) -> Option<bool> {
    health.map(|status| !matches!(status, HealthStatus::Unreachable))
}

fn derive_daemon_joined(db_available: bool, node_present: bool) -> Option<bool> {
    db_available.then_some(node_present)
}

fn derive_node_resources(
    node: &FleetComputer,
    config_hint: Option<&ConfigNodeHints>,
) -> (String, String, String) {
    if let Some(hw) = &node.hardware {
        let cpu = format!("{} ({} cores)", hw.cpu.model, hw.cpu.logical_cores.max(1));
        let ram = if hw.memory.total_gb > 0.0 {
            format!("{:.1} GB", hw.memory.total_gb)
        } else {
            "unknown".to_string()
        };
        let gpu = if hw.gpu_devices.is_empty() {
            format!("{:?}", hw.gpu_type).to_ascii_lowercase()
        } else {
            hw.gpu_devices.join(", ")
        };

        return (cpu, ram, gpu);
    }

    let cpu = config_hint
        .and_then(|hint| hint.cpu.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let ram = config_hint
        .and_then(|hint| hint.ram.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let gpu = config_hint
        .and_then(|hint| hint.gpu.clone())
        .unwrap_or_else(|| "unknown".to_string());

    (cpu, ram, gpu)
}

fn derive_node_capacity(
    node: &FleetComputer,
    config_hint: Option<&ConfigNodeHints>,
) -> (Option<u32>, Option<u64>) {
    if let Some(hw) = &node.hardware {
        let cpu_cores = if hw.cpu.logical_cores > 0 {
            Some(hw.cpu.logical_cores as u32)
        } else {
            config_hint.and_then(|hint| hint.cpu_cores)
        };

        let memory_gib = if hw.memory.total_gb > 0.0 {
            Some(hw.memory.total_gb.round() as u64)
        } else {
            config_hint.and_then(|hint| hint.memory_gib)
        };

        return (cpu_cores, memory_gib);
    }

    (
        config_hint.and_then(|hint| hint.cpu_cores),
        config_hint.and_then(|hint| hint.memory_gib),
    )
}

fn derive_models_loaded(
    node: &FleetComputer,
    db_node: Option<&DbNodeSnapshot>,
) -> (Vec<String>, String) {
    let mut ids = Vec::new();

    for model in &node.models {
        if !model.id.trim().is_empty() {
            ids.push(model.id.clone());
        }
    }

    if ids.is_empty()
        && let Some(db_models) = db_node.map(|db| db.models.clone())
    {
        ids = db_models;
    }

    if ids.is_empty() {
        (Vec::new(), "unreported".to_string())
    } else {
        ids.sort();
        ids.dedup();
        (ids, "reported".to_string())
    }
}

fn derive_last_heartbeat(
    node: &FleetComputer,
    db_node: Option<&DbNodeSnapshot>,
    runtime_enrolled: bool,
) -> HeartbeatView {
    if let Some(checked_at) = node
        .health
        .as_ref()
        .map(|health| health.checked_at.to_rfc3339())
    {
        return heartbeat_view(checked_at, "registry.health");
    }

    if runtime_enrolled && node.last_seen.timestamp() > 0 {
        return heartbeat_view(node.last_seen.to_rfc3339(), "registry.last_seen");
    }

    if let Some(heartbeat) = db_node.and_then(|db| db.last_heartbeat.clone()) {
        return heartbeat_view(heartbeat, "db.last_heartbeat");
    }

    HeartbeatView {
        value: "unknown".to_string(),
        source: "unreported".to_string(),
        freshness: "unknown".to_string(),
        age_seconds: None,
    }
}

fn heartbeat_view(value: String, source: &str) -> HeartbeatView {
    let (freshness, age_seconds) = classify_heartbeat_freshness(&value);
    HeartbeatView {
        value,
        source: source.to_string(),
        freshness,
        age_seconds,
    }
}

fn classify_heartbeat_freshness(raw: &str) -> (String, Option<i64>) {
    let Some(timestamp) = parse_heartbeat_timestamp(raw) else {
        return ("unknown".to_string(), None);
    };

    let age_seconds = Utc::now()
        .signed_duration_since(timestamp)
        .num_seconds()
        .max(0);
    let freshness = if age_seconds <= 90 {
        "fresh"
    } else if age_seconds <= 300 {
        "stale"
    } else {
        "expired"
    };

    (freshness.to_string(), Some(age_seconds))
}

fn parse_heartbeat_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let value = raw.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("unknown") {
        return None;
    }

    if let Ok(ts) = DateTime::parse_from_rfc3339(value) {
        return Some(ts.with_timezone(&Utc));
    }

    if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(ts, Utc));
    }

    None
}

fn build_replication_view(
    is_leader: bool,
    leader_known: bool,
    db_replication_state: Option<&str>,
    leader_sequence: Option<u64>,
    local_sequence: Option<u64>,
) -> FleetReplicationView {
    if is_leader {
        let sequence = leader_sequence.or(local_sequence);
        return FleetReplicationView {
            mode: "leader".to_string(),
            sequence,
            health: if sequence.is_some() {
                "healthy".to_string()
            } else {
                "unreported".to_string()
            },
            detail: db_replication_state
                .map(str::to_string)
                .unwrap_or_else(|| "leader snapshot coordinator status unreported".to_string()),
        };
    }

    if leader_known {
        return FleetReplicationView {
            mode: "follower".to_string(),
            sequence: local_sequence,
            health: if local_sequence.is_some() {
                "unknown".to_string()
            } else {
                "unreported".to_string()
            },
            detail: db_replication_state.map(str::to_string).unwrap_or_else(|| {
                "follower replication telemetry unavailable (sequence only)".to_string()
            }),
        };
    }

    FleetReplicationView {
        mode: "unknown".to_string(),
        sequence: local_sequence,
        health: "unknown".to_string(),
        detail: db_replication_state
            .map(str::to_string)
            .unwrap_or_else(|| "replication role unknown".to_string()),
    }
}

fn build_workload_view(
    workload: Option<&NodeWorkloadAggregate>,
    db_available: bool,
) -> FleetWorkloadView {
    match (db_available, workload) {
        (true, Some(entry)) => FleetWorkloadView {
            status: if entry.active_tasks > 0 {
                "active".to_string()
            } else {
                "idle".to_string()
            },
            source: "db.tasks".to_string(),
            active_tasks: Some(entry.active_tasks),
            task_ids: entry.task_ids.clone(),
            status_breakdown: entry.status_breakdown.clone(),
        },
        (true, None) => FleetWorkloadView {
            status: "idle".to_string(),
            source: "db.tasks".to_string(),
            active_tasks: Some(0),
            task_ids: Vec::new(),
            status_breakdown: BTreeMap::new(),
        },
        (false, _) => FleetWorkloadView {
            status: "unreported".to_string(),
            source: "db_unavailable".to_string(),
            active_tasks: None,
            task_ids: Vec::new(),
            status_breakdown: BTreeMap::new(),
        },
    }
}

fn build_config_hints(
    fleet_config: Option<&ff_core::config::FleetConfig>,
) -> (
    HashMap<String, ConfigNodeHints>,
    HashMap<String, ConfigNodeHints>,
) {
    let Some(config) = fleet_config else {
        return (HashMap::new(), HashMap::new());
    };

    let mut by_name = HashMap::new();
    let mut by_ip = HashMap::new();

    for (name, node_cfg) in &config.nodes {
        let cpu = node_cfg
            .effective_cpu_cores()
            .map(|cores| format!("{cores} cores (configured)"));
        let ram = node_cfg
            .effective_ram_gb()
            .map(|gb| format!("{gb} GB (configured)"));

        let gpu = node_cfg
            .resources
            .as_ref()
            .and_then(|res| res.vram_gb)
            .map(|vram| format!("{vram} GB VRAM (configured)"))
            .or_else(|| {
                if node_cfg.models.is_empty() {
                    None
                } else {
                    Some("configured".to_string())
                }
            });

        let hint = ConfigNodeHints {
            role: Some(role_to_string(node_cfg.role)),
            cpu,
            ram,
            cpu_cores: node_cfg.effective_cpu_cores(),
            memory_gib: node_cfg.effective_ram_gb(),
            gpu,
        };

        by_name.insert(name.clone(), hint.clone());
        if !node_cfg.ip.trim().is_empty() {
            by_ip.insert(node_cfg.ip.clone(), hint);
        }
    }

    (by_name, by_ip)
}

fn role_to_string(role: ff_core::Role) -> String {
    match role {
        ff_core::Role::Leader => "leader",
        ff_core::Role::Worker => "worker",
        ff_core::Role::Gateway => "gateway",
        ff_core::Role::Builder => "builder",
    }
    .to_string()
}

fn extract_json_string(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .map(|raw| raw.trim())
        .find(|raw| !raw.is_empty())
        .map(str::to_string)
}

fn parse_db_node_snapshot(row: &queries::WorkerRow) -> DbNodeSnapshot {
    let parsed_hardware = serde_json::from_str::<Value>(&row.hardware_json).ok();
    let models = serde_json::from_str::<Vec<String>>(&row.models_json).unwrap_or_default();

    let service_version = parsed_hardware.as_ref().and_then(|value| {
        extract_json_string(
            value,
            &[
                "/service_version",
                "/serviceVersion",
                "/service/version",
                "/build/version",
                "/version",
            ],
        )
    });

    let replication_state = parsed_hardware.as_ref().and_then(|value| {
        extract_json_string(
            value,
            &[
                "/replication_state",
                "/replication/state",
                "/replication/health",
            ],
        )
    });

    DbNodeSnapshot {
        role: Some(row.role.trim().to_ascii_lowercase()),
        status: Some(row.status.trim().to_ascii_lowercase()),
        last_heartbeat: row.last_heartbeat.clone(),
        models,
        service_version,
        replication_state,
    }
}

fn aggregate_workloads(
    statuses: &[(&str, Vec<queries::TaskRow>)],
) -> HashMap<String, NodeWorkloadAggregate> {
    let mut workloads = HashMap::<String, NodeWorkloadAggregate>::new();

    for (status, tasks) in statuses {
        for task in tasks {
            let Some(node) = task
                .assigned_node
                .as_deref()
                .map(str::trim)
                .filter(|node| !node.is_empty())
            else {
                continue;
            };

            let entry = workloads.entry(node.to_string()).or_default();
            entry.active_tasks += 1;
            entry.task_ids.push(task.id.clone());
            *entry
                .status_breakdown
                .entry((*status).to_string())
                .or_insert(0) += 1;
        }
    }

    for workload in workloads.values_mut() {
        workload.task_ids.sort();
        workload.task_ids.dedup();
    }

    workloads
}

async fn load_db_fleet_snapshot(state: &GatewayState) -> Option<DbFleetSnapshot> {
    let mut snapshot = DbFleetSnapshot::default();
    let mut has_any_data = false;

    if let Some(store) = &state.operational_store {
        let node_rows = store.list_nodes().await;
        let active_statuses = ["claimed", "in_progress", "running", "review"];

        match node_rows {
            Ok(node_rows) => {
                let mut nodes_by_name = HashMap::new();
                let mut nodes_by_host = HashMap::new();

                for row in &node_rows {
                    let parsed = parse_db_node_snapshot(row);
                    nodes_by_name.insert(row.name.clone(), parsed.clone());
                    nodes_by_host.insert(row.host.clone(), parsed);
                }

                let mut status_rows = Vec::new();
                let mut status_query_failed = false;
                for status in active_statuses {
                    match store.list_tasks_by_status(status).await {
                        Ok(rows) => status_rows.push((status, rows)),
                        Err(err) => {
                            status_query_failed = true;
                            warn!(status, error = %err, "failed to load task status rows from operational store");
                            break;
                        }
                    }
                }

                let replication_sequence = match store.config_get("replication.sequence").await {
                    Ok(Some(raw)) => raw.trim().parse::<u64>().ok(),
                    Ok(None) => None,
                    Err(err) => {
                        warn!(error = %err, "failed to read replication sequence from operational store");
                        None
                    }
                };

                if !status_query_failed {
                    snapshot.nodes_by_name = nodes_by_name;
                    snapshot.nodes_by_host = nodes_by_host;
                    snapshot.workloads = aggregate_workloads(&status_rows);
                    snapshot.replication_sequence = replication_sequence;
                    has_any_data = true;
                }
            }
            Err(err) => {
                warn!(error = %err, "failed to load fleet metadata from operational store");
            }
        }
    }

    if has_any_data { Some(snapshot) } else { None }
}

// ─── Config Endpoints ────────────────────────────────────────────────────────

/// GET /api/config — return current fleet config as JSON.
async fn get_config(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(config_lock) = &state.fleet_config else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "fleet config not loaded", "type": "not_ready"}})),
        ));
    };

    let config = config_lock.read().await;
    let config_json = serde_json::to_value(&*config).unwrap_or(Value::Null);
    let content = toml::to_string_pretty(&*config).unwrap_or_default();

    Ok(Json(json!({
        "status": "ok",
        "config": config_json,
        "content": content,
    })))
}

/// POST /api/config — update fleet config (full replacement) and trigger reload.
/// Requires admin token via `X-Admin-Token` header (set `FF_ADMIN_TOKEN` env).
async fn update_config(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // RBAC: require admin token for config mutation.
    let admin_token = std::env::var("FF_ADMIN_TOKEN").unwrap_or_default();
    let provided = headers
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !admin_token.is_empty() && provided != admin_token {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": {"message": "admin token required", "type": "forbidden"}})),
        ));
    }

    let Some(config_lock) = &state.fleet_config else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "fleet config not loaded", "type": "not_ready"}})),
        ));
    };

    let new_config: ff_core::config::FleetConfig = if let Some(content) =
        payload.get("content").and_then(|v| v.as_str())
    {
        toml::from_str::<ff_core::config::FleetConfig>(content).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": e.to_string(), "type": "invalid_config_toml"}})),
            )
        })?
    } else {
        serde_json::from_value(payload).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": e.to_string(), "type": "invalid_config"}})),
            )
        })?
    };

    {
        let mut config = config_lock.write().await;
        *config = new_config;
    }

    info!("fleet config updated via API");

    Ok(Json(json!({
        "status": "ok",
        "message": "config updated",
    })))
}

/// GET /api/config/reload-status — compatibility endpoint used by dashboard.
async fn config_reload_status() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "message": "config in sync"
    }))
}

fn configured_secret_source(inline_configured: bool, resolved: bool) -> &'static str {
    if inline_configured {
        "fleet.toml"
    } else if resolved {
        "env"
    } else {
        "missing"
    }
}

fn url_scheme(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }

    trimmed
        .split_once("://")
        .map(|(scheme, _)| scheme.to_string())
}

/// GET /api/settings/runtime — operationally-safe runtime settings visibility.
///
/// This endpoint intentionally does not return secret values.
async fn settings_runtime(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    let config = if let Some(config_lock) = &state.fleet_config {
        Some(config_lock.read().await.clone())
    } else {
        None
    };

    let telegram_snapshot = telegram_transport_snapshot(state.as_ref()).await;

    let (runtime_config, enrollment, telegram, configured_external_db, bootstrap_visibility) =
        if let Some(cfg) = &config {
            let models_configured = cfg
                .nodes
                .values()
                .map(|node| node.models.len())
                .sum::<usize>()
                + cfg.models.len();

            let bootstrap_total = cfg.bootstrap_targets.len();
            let bootstrap_enrolled = cfg
                .bootstrap_targets
                .iter()
                .filter(|target| {
                    target.enrolled.unwrap_or_else(|| {
                        target
                            .status
                            .as_deref()
                            .map(|status| {
                                matches!(
                                    status.trim().to_ascii_lowercase().as_str(),
                                    "received" | "enrolled" | "ready" | "active" | "complete"
                                )
                            })
                            .unwrap_or(false)
                    })
                })
                .count();
            let bootstrap_ssh_reachable = cfg
                .bootstrap_targets
                .iter()
                .filter(|target| target.reachable_by_ssh.unwrap_or(false))
                .count();
            let bootstrap_manual_pending = cfg
                .bootstrap_targets
                .iter()
                .filter(|target| !target.required_manual_floor.is_empty())
                .count();

            let enrollment_inline = cfg
                .enrollment
                .shared_secret
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());
            let enrollment_resolved = cfg.enrollment.resolve_shared_secret().is_some();

            let telegram_cfg = cfg.transport.telegram.as_ref();
            let telegram_inline = telegram_cfg
                .and_then(|tg| tg.bot_token.as_deref())
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());
            let telegram_resolved = telegram_cfg
                .and_then(|tg| tg.resolve_bot_token())
                .is_some_and(|value| !value.trim().is_empty());

            (
                json!({
                    "loaded": true,
                    "config_path": state.config_path.clone(),
                    "fleet_name": cfg.fleet.name.clone(),
                    "api_port": cfg.fleet.api_port,
                    "heartbeat_interval_secs": cfg.fleet.heartbeat_interval_secs,
                    "heartbeat_timeout_secs": cfg.fleet.heartbeat_timeout_secs,
                    "nodes_configured": cfg.nodes.len(),
                    "models_configured": models_configured,
                    "bootstrap_targets_configured": bootstrap_total,
                    "loops": {
                        "evolution": cfg.loops.evolution.enabled,
                        "updater": cfg.loops.updater.enabled,
                        "self_heal": cfg.loops.self_heal.enabled,
                        "mcp_federation": cfg.loops.mcp_federation.enabled,
                    }
                }),
                json!({
                    "default_role": cfg.enrollment.default_role.clone(),
                    "allowed_roles": cfg.enrollment.allowed_roles.clone(),
                    "token": {
                        "configured_inline": enrollment_inline,
                        "env_var": cfg.enrollment.shared_secret_env.clone(),
                        "resolved": enrollment_resolved,
                        "source": configured_secret_source(enrollment_inline, enrollment_resolved),
                        "editable_in_dashboard": false,
                    }
                }),
                json!({
                    "configured": telegram_cfg.is_some(),
                    "enabled": telegram_cfg.is_some_and(|tg| tg.enabled),
                    "allowed_chat_ids": telegram_snapshot.allowed_chat_ids.len(),
                    "polling_interval_secs": telegram_snapshot.polling_interval_secs,
                    "polling_timeout_secs": telegram_snapshot.polling_timeout_secs,
                    "runtime": {
                        "running": telegram_snapshot.running,
                        "started_at": telegram_snapshot.started_at,
                        "last_update_id": telegram_snapshot.last_update_id,
                        "last_message_at": telegram_snapshot.last_message_at,
                        "last_error": telegram_snapshot.last_error,
                    },
                    "token": {
                        "configured_inline": telegram_inline,
                        "env_var": telegram_cfg.map(|tg| tg.bot_token_env.clone()).unwrap_or_else(|| "FORGEFLEET_TELEGRAM_BOT_TOKEN".to_string()),
                        "resolved": telegram_resolved,
                        "source": configured_secret_source(telegram_inline, telegram_resolved),
                        "editable_in_dashboard": false,
                    }
                }),
                json!({
                    "mode": cfg.database.mode.as_str(),
                    "url_present": !cfg.database.url.trim().is_empty(),
                    "url_scheme": url_scheme(&cfg.database.url),
                    "host": cfg.database.host.clone(),
                    "port": cfg.database.port,
                    "name": cfg.database.name.clone(),
                }),
                json!({
                    "summary": {
                        "total_targets": bootstrap_total,
                        "enrolled_targets": bootstrap_enrolled,
                        "ssh_reachable_targets": bootstrap_ssh_reachable,
                        "manual_steps_pending": bootstrap_manual_pending,
                    },
                    "targets": cfg.bootstrap_targets.iter().map(|target| {
                        json!({
                            "name": target.name,
                            "status": target.status,
                            "os": target.os,
                            "hardware": target.hardware,
                            "reachable_by_ssh": target.reachable_by_ssh,
                            "enrolled": target.enrolled,
                            "required_manual_floor": target.required_manual_floor,
                            "notes": target.notes,
                        })
                    }).collect::<Vec<_>>()
                }),
            )
        } else {
            (
                json!({
                    "loaded": false,
                    "config_path": state.config_path.clone(),
                }),
                json!({
                    "token": {
                        "configured_inline": false,
                        "env_var": "FORGEFLEET_ENROLLMENT_TOKEN",
                        "resolved": false,
                        "source": "missing",
                        "editable_in_dashboard": false,
                    }
                }),
                json!({
                    "configured": false,
                    "enabled": telegram_snapshot.enabled,
                    "allowed_chat_ids": telegram_snapshot.allowed_chat_ids.len(),
                    "polling_interval_secs": telegram_snapshot.polling_interval_secs,
                    "polling_timeout_secs": telegram_snapshot.polling_timeout_secs,
                    "runtime": {
                        "running": telegram_snapshot.running,
                        "started_at": telegram_snapshot.started_at,
                        "last_update_id": telegram_snapshot.last_update_id,
                        "last_message_at": telegram_snapshot.last_message_at,
                        "last_error": telegram_snapshot.last_error,
                    },
                    "token": {
                        "configured_inline": false,
                        "env_var": "FORGEFLEET_TELEGRAM_BOT_TOKEN",
                        "resolved": false,
                        "source": "missing",
                        "editable_in_dashboard": false,
                    }
                }),
                json!({}),
                json!({
                    "summary": {
                        "total_targets": 0,
                        "enrolled_targets": 0,
                        "ssh_reachable_targets": 0,
                        "manual_steps_pending": 0,
                    },
                    "targets": [],
                }),
            )
        };

    let db_status = if let Some(store) = &state.operational_store {
        let backend = store.backend_label();
        let ping = store.health_probe().await;

        match ping {
            Ok((ping_ok, kv_count)) => {
                json!({
                    "active_mode": backend,
                    "status": if ping_ok { "ready" } else { "degraded" },
                    "postgres": {
                        "config_kv_entries": kv_count,
                    }
                })
            }
            Err(err) => json!({
                "active_mode": backend,
                "status": "error",
                "error": err.to_string(),
            }),
        }
    } else {
        json!({
            "active_mode": "none",
            "status": "unavailable",
        })
    };

    Json(json!({
        "status": "ok",
        "runtime_config": runtime_config,
        "enrollment": enrollment,
        "telegram": telegram,
        "bootstrap": bootstrap_visibility,
        "database": db_status,
        "configured_external_database": configured_external_db,
        "guidance": {
            "secrets_editable_in_dashboard": false,
            "workflow": [
                "Edit secrets in fleet.toml or environment variables on the host machine.",
                "Restart forgefleet gateway after changing enrollment/telegram token values.",
                "Re-open this page and verify token source + runtime status are healthy."
            ],
            "activation": [
                "Confirm fleet.toml was loaded from the expected config path.",
                "Verify enrollment token resolves and default role policy is correct.",
                "Verify Telegram transport is enabled with a resolved bot token source.",
                "Confirm operational store reports status=ready for the active backend."
            ],
            "onboarding": [
                "Use Config Editor for non-secret runtime updates before worker enrollment.",
                "Enroll new nodes with the same enrollment secret source shown in Settings.",
                "Validate heartbeat + transport activity after first worker joins.",
                "Confirm runtime registry counts increase and remain healthy."
            ],
            "troubleshooting": [
                "If enrollment token source is missing, set FORGEFLEET_ENROLLMENT_TOKEN or update fleet.toml then restart.",
                "If Telegram runtime is not running, verify token source and allowed chat IDs, then restart gateway.",
                "If database status is degraded/error, validate backend connectivity and inspect gateway logs."
            ]
        }
    }))
}

// ─── LLM Proxy ───────────────────────────────────────────────────────────────

/// POST /v1/chat/completions — proxy to fleet LLM servers.
///
/// Routing strategy (new, preferred):
/// 1. Try the Pulse-backed router (`PulseLlmRouter`) — this reads live Redis
///    beats so any active+healthy LLM server in the fleet is routable
///    immediately, no explicit backend configuration required.
/// 2. If Pulse finds no match (or Pulse itself is unreachable), fall back to
///    the legacy tier-escalation router based on `BackendRegistry`.
///
/// Legacy tier/model-router path preserved verbatim below the Pulse attempt
/// for backward compatibility.
async fn proxy_chat_completions(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<AsyncQuery>,
    headers: HeaderMap,
    Json(mut raw_payload): Json<Value>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    // GW.2 trace breadcrumbs (kept permanently at debug level so they're
    // available when a future operator runs RUST_LOG=ff_gateway=debug,
    // but silent by default). Originally diagnosed the worker-gateway
    // hang where `FORGEFLEET_REDIS_URL` defaulted to localhost on
    // workers, causing pulse beats reads to block forever on connect.
    // The fix (env via systemd unit / launchd plist) is in the
    // operator runbook; the breadcrumbs stay so the same chain can be
    // re-traced if any future routing-layer change regresses.
    let _trace_model = raw_payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    debug!(model = %_trace_model, "GW.2: chat handler entered");

    // ── Session affinity hint from header ────────────────────────────
    // Clients may pass X-ForgeFleet-Session to explicitly pin a
    // conversation to a specific affinity key. This overrides the
    // body-derived key in extract_session_key.
    if let Some(session_header) = headers
        .get("X-ForgeFleet-Session")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        && let Some(obj) = raw_payload.as_object_mut()
    {
        obj.insert("session_id".to_string(), json!(session_header));
    }

    // ── Async mode (P1.6) ────────────────────────────────────────────
    //
    // When ?async=true, enqueue the request as a fleet_task and return
    // a ticket immediately. The caller polls /v1/async/{ticket} or
    // receives a webhook when complete.
    if query.async_mode == Some(true)
        && let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool())
    {
        let model = raw_payload
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let summary = format!("async chat completion ({model})");
        let task_id: Uuid = sqlx::query_scalar(
            r#"
                INSERT INTO fleet_tasks (task_type, summary, payload, priority, status, created_at)
                VALUES ('async_chat', $1, $2, 50, 'pending', NOW())
                RETURNING id
                "#,
        )
        .bind(&summary)
        .bind(&raw_payload)
        .fetch_one(pool)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("db error: {e}")})),
            )
        })?;

        let ticket = json!({
            "ticket": task_id,
            "status": "pending",
            "poll_url": format!("/v1/async/{task_id}"),
        });
        return Ok(Json(ticket).into_response());
    }

    debug!(model = %_trace_model, "GW.2: pre-cloud_llm");
    // ── Cloud-LLM routing (first pass) ───────────────────────────────
    //
    // If the `model` field matches a row in `cloud_llm_providers`
    // (schema V26) we forward the request off-fleet to the provider's
    // public API (OpenAI/Anthropic/Moonshot/Google). This only fires
    // when we have a Postgres pool available; otherwise we quietly
    // skip straight to Pulse.
    if let Some(store) = state.operational_store.as_ref()
        && let Some(pool) = store.pg_pool()
        && let Some(model) = raw_payload.get("model").and_then(|v| v.as_str())
        && let Some(result) = crate::cloud_llm::try_route_to_cloud(
            pool,
            model,
            &raw_payload,
            None,
            &state.http_client,
        )
        .await
    {
        match result {
            Ok(resp) => return Ok(resp),
            Err(resp) => return Ok(resp),
        }
    }

    // ── Qwen3 thinking-mode max_tokens floor ─────────────────────────
    //
    // Qwen3-family models (Qwen3, Qwen3-Coder, Qwen3-Omni, Qwen3-VL,
    // Qwen3.5, Qwen3.6, …) always emit a `<think>` block that burns
    // 300-800 tokens before any visible content. llama.cpp's
    // `enable_thinking=false` / `/no_think` directives are currently
    // non-functional (GH #13189, #20182, #20409), so callers that pass
    // `max_tokens < 1024` silently get empty `content`. Floor it here.
    // Cloud-routed requests have already returned above; this only
    // affects local fleet inference.
    if raw_payload.is_object() {
        let model_is_qwen3 = raw_payload
            .get("model")
            .and_then(|v| v.as_str())
            .map(|m| m.to_ascii_lowercase().contains("qwen3"))
            .unwrap_or(false);
        if model_is_qwen3 {
            let obj = raw_payload.as_object_mut().expect("checked is_object");
            let current = obj.get("max_tokens").and_then(|v| v.as_u64());
            if current.map(|n| n < 1024).unwrap_or(true) {
                let old = current
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unset".to_string());
                let model = obj
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                obj.insert("max_tokens".to_string(), json!(1024u64));
                tracing::debug!(
                    model = %model,
                    old = %old,
                    new = 1024,
                    "qwen3 thinking-mode max_tokens floor applied"
                );
            }
        }
    } else {
        warn!("chat completion payload is not a JSON object; skipping qwen3 max_tokens floor");
    }

    debug!(model = %_trace_model, "GW.2: pre-pulse-router");
    // ── Pulse-first routing ──────────────────────────────────────────
    //
    // We try the Pulse router first. If it successfully picks a server
    // and the upstream call returns *something* (success or a 4xx from
    // the inference server itself), we return that. Only if Pulse cannot
    // find a matching server OR its upstream call fails outright do we
    // fall through to the legacy tier-router path.
    if let Some(pulse) = state.pulse_router.clone() {
        debug!(model = %_trace_model, "GW.2: pulse-router cloned");
        let cache_ref = state.pulse_cache.as_deref();
        // Hand the Pulse router the PG pool so it can expand pool aliases
        // (`fleet_task_coverage.alias`, schema V27) before doing the
        // normal exact/prefix model-id match.
        let pg_ref = state.operational_store.as_ref().and_then(|s| s.pg_pool());
        let is_streaming = raw_payload
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        debug!(model = %_trace_model, has_cache = cache_ref.is_some(), has_pg = pg_ref.is_some(), is_streaming, "GW.2: pre-route-completion-call");

        if is_streaming {
            match pulse
                .route_completion_streaming(&raw_payload, cache_ref, pg_ref)
                .await
            {
                Ok(result) => {
                    let status = result.upstream.status();
                    let content_type = result
                        .upstream
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .cloned()
                        .unwrap_or_else(|| {
                            header::HeaderValue::from_static("text/event-stream; charset=utf-8")
                        });
                    let byte_stream = result.upstream.bytes_stream().map(|chunk| {
                        chunk.map_err(|e| {
                            std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string())
                        })
                    });
                    return Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, content_type)
                        .header(header::CACHE_CONTROL, "no-cache")
                        .header(header::CONNECTION, "keep-alive")
                        .header("X-Accel-Buffering", "no")
                        .header("X-ForgeFleet-Computer", result.routed.computer)
                        .header("X-ForgeFleet-Model", result.routed.model_id)
                        .header("X-ForgeFleet-Runtime", result.routed.runtime)
                        .header(
                            "X-ForgeFleet-QueueDepth",
                            result.routed.queue_depth.to_string(),
                        )
                        .body(Body::from_stream(byte_stream))
                        .map_err(|e| (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({"error": {"message": e.to_string(), "type": "upstream_error"}})),
                        ));
                }
                Err(llm_routing::LlmRoutingError::NoMatch { .. }) => {
                    debug!(
                        "pulse found no matching server for streaming; trying tier-router fallback"
                    );
                }
                Err(llm_routing::LlmRoutingError::MissingModel) => {
                    let (code, body) =
                        llm_routing::error_to_response(llm_routing::LlmRoutingError::MissingModel);
                    return Err((
                        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
                        Json(body),
                    ));
                }
                Err(e) => {
                    warn!(error = %e, "pulse streaming routing failed; falling back to tier-router");
                }
            }
        } else {
            let started = std::time::Instant::now();
            match pulse
                .route_completion_cached(&raw_payload, cache_ref, pg_ref)
                .await
            {
                Ok(value) => {
                    record_pulse_usage(&state, &value).await;
                    // Interaction-log capture (V121 ff_interactions): the
                    // Pulse router is the primary path for every routed
                    // /v1/chat/completions turn, so without this hook the
                    // training corpus records nothing from gateway traffic.
                    if let Some(pool) = pg_ref {
                        let latency_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
                        let rec = build_router_interaction(
                            last_user_message_text(&raw_payload),
                            &value,
                            latency_ms,
                        );
                        spawn_interaction_capture(pool.clone(), rec);
                    }
                    return Ok(Json(value).into_response());
                }
                Err(llm_routing::LlmRoutingError::NoMatch { .. }) => {
                    debug!("pulse found no matching server; trying tier-router fallback");
                }
                Err(llm_routing::LlmRoutingError::MissingModel) => {
                    let (code, body) =
                        llm_routing::error_to_response(llm_routing::LlmRoutingError::MissingModel);
                    return Err((
                        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
                        Json(body),
                    ));
                }
                Err(e) => {
                    warn!(error = %e, "pulse routing failed; falling back to tier-router");
                }
            }
        }
    }

    // ── Legacy tier-router fallback ──────────────────────────────────
    // Re-parse the raw payload into the typed ChatCompletionRequest the
    // legacy path expects. Any schema-level error becomes a 400 here.
    let payload: ChatCompletionRequest = match serde_json::from_value(raw_payload) {
        Ok(p) => p,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {
                    "message": format!("invalid chat completion request: {e}"),
                    "type": "invalid_request_error"
                }})),
            ));
        }
    };

    // ── Validate request ─────────────────────────────────────────────
    if let Err(msg) = validate_request(&payload) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": msg, "type": "invalid_request_error"}})),
        ));
    }

    // ── Prefer TierRouter, fall back to ModelRouter ──────────────────
    let tier_router = state.tier_router.as_ref();
    let model_router = state.model_router.as_ref();

    if tier_router.is_none() && model_router.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "LLM backend not configured", "type": "not_ready"}})),
        ));
    }

    let streaming = payload.stream.unwrap_or(false);

    // ── TierRouter path (preferred) ──────────────────────────────────
    if let Some(tier_router) = tier_router {
        let mut escalation_chain = tier_router
            .route_with_escalation(&payload.model, None, None)
            .await;

        // If no backend matched, try to auto-load the model on this node from
        // the fleet_model_library table. This makes `ff model download <x>`
        // followed by a chat request with `model: "<x>"` Just Work — the
        // router will spawn the inference server on demand.
        if escalation_chain.is_empty()
            && let (Some(store), Some(registry)) = (
                state.operational_store.as_ref(),
                state.api_registry.as_ref(),
            )
            && let Some(pool) = store.pg_pool()
        {
            match ff_api::autoload::ensure_deployed(pool, &payload.model).await {
                Ok(url) => {
                    if let Some((host, port)) = parse_autoload_url(&url) {
                        let endpoint = ff_api::registry::BackendEndpoint {
                            id: format!("autoload-{}-{}", payload.model, port),
                            node: "local".to_string(),
                            host,
                            port,
                            model: payload.model.clone(),
                            tier: 2,
                            healthy: true,
                            busy: false,
                            scheme: "http".to_string(),
                            is_local: true,
                            cost_per_1k_input: 0.0,
                            cost_per_1k_output: 0.0,
                        };
                        registry.add_endpoint(endpoint).await;
                        info!(model = %payload.model, %url, "autoloaded model for chat request");
                        escalation_chain = tier_router
                            .route_with_escalation(&payload.model, None, None)
                            .await;
                    }
                }
                Err(e) => {
                    warn!(model = %payload.model, error = %e, "autoload failed");
                }
            }
        }

        if escalation_chain.is_empty() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": {
                    "message": format!("no healthy backend for model '{}'", payload.model),
                    "type": "backend_unavailable"
                }})),
            ));
        }

        let mut last_error = None::<String>;

        for (tier, backends) in &escalation_chain {
            let timeout = tier_router.timeout_for_tier(*tier);

            for backend in backends {
                let url = ff_core::url::normalize_chat_completions_url(&backend.base_url());
                debug!(
                    backend = %backend.id,
                    tier = %tier,
                    timeout_secs = %timeout.as_secs(),
                    %url,
                    "proxying chat completion (tier escalation)"
                );

                let start = std::time::Instant::now();

                let result = tokio::time::timeout(
                    timeout,
                    state.http_client.post(&url).json(&payload).send(),
                )
                .await;

                match result {
                    Ok(Ok(upstream)) => {
                        let status = upstream.status();
                        let latency = start.elapsed();

                        // 4xx — client error, don't retry
                        if status.is_client_error()
                            && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                        {
                            tier_router.record_success(&backend.id, latency);
                            if streaming {
                                return openai_compat::passthrough_streaming_response(upstream)
                                    .await
                                    .map_err(|e| (
                                        StatusCode::BAD_GATEWAY,
                                        Json(json!({"error": {"message": e, "type": "upstream_error"}})),
                                    ));
                            }
                            // Non-streaming: record usage then passthrough
                            let bytes = upstream.bytes().await.unwrap_or_default();
                            let bytes = record_usage_from_response(
                                &state,
                                backend,
                                &payload,
                                bytes,
                                latency.as_millis() as u64,
                            )
                            .await;
                            let mut response = Response::builder().status(status);
                            response = response.header(header::CONTENT_TYPE, "application/json");
                            return response.body(Body::from(bytes))
                                .map_err(|e| (
                                    StatusCode::BAD_GATEWAY,
                                    Json(json!({"error": {"message": e.to_string(), "type": "upstream_error"}})),
                                ));
                        }

                        // 5xx / 429 / 503 — retryable
                        if status.is_server_error()
                            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        {
                            tier_router.record_failure(&backend.id, latency);
                            last_error = Some(format!(
                                "{} responded {} (tier {})",
                                backend.id, status, tier
                            ));
                            warn!(
                                backend = %backend.id,
                                tier = %tier,
                                status = %status,
                                "backend returned retryable error, trying next"
                            );
                            continue;
                        }

                        // 2xx success
                        tier_router.record_success(&backend.id, latency);

                        if streaming {
                            return openai_compat::passthrough_streaming_response(upstream)
                                .await
                                .map_err(|e| (
                                    StatusCode::BAD_GATEWAY,
                                    Json(json!({"error": {"message": e, "type": "upstream_error"}})),
                                ));
                        }
                        // Non-streaming: record usage then passthrough
                        let bytes = upstream.bytes().await.unwrap_or_default();
                        let bytes = record_usage_from_response(
                            &state,
                            backend,
                            &payload,
                            bytes,
                            latency.as_millis() as u64,
                        )
                        .await;
                        let mut response = Response::builder().status(status);
                        response = response.header(header::CONTENT_TYPE, "application/json");
                        return response.body(Body::from(bytes))
                            .map_err(|e| (
                                StatusCode::BAD_GATEWAY,
                                Json(json!({"error": {"message": e.to_string(), "type": "upstream_error"}})),
                            ));
                    }
                    Ok(Err(err)) => {
                        let latency = start.elapsed();
                        tier_router.record_failure(&backend.id, latency);
                        warn!(
                            backend = %backend.id,
                            tier = %tier,
                            %err,
                            "upstream request failed; trying next backend"
                        );
                        last_error = Some(format!(
                            "{} request failed (tier {}): {}",
                            backend.id, tier, err
                        ));
                    }
                    Err(_elapsed) => {
                        let latency = start.elapsed();
                        tier_router.record_failure(&backend.id, latency);
                        warn!(
                            backend = %backend.id,
                            tier = %tier,
                            timeout_secs = %timeout.as_secs(),
                            "request timed out; trying next backend"
                        );
                        last_error = Some(format!(
                            "{} timed out after {}s (tier {})",
                            backend.id,
                            timeout.as_secs(),
                            tier
                        ));
                    }
                }
            }
        }

        return Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {
                "message": last_error.unwrap_or_else(|| "all backends failed across all tiers".to_string()),
                "type": "upstream_error"
            }})),
        ));
    }

    // ── ModelRouter fallback path (backward compat) ──────────────────
    let model_router = model_router.unwrap();
    let route_chain = model_router.route_chain(&payload.model).await;

    if route_chain.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {
                "message": format!("no healthy backend for model '{}'", payload.model),
                "type": "backend_unavailable"
            }})),
        ));
    }

    let mut last_error = None::<String>;

    for backend in &route_chain {
        let url = ff_core::url::normalize_chat_completions_url(&backend.base_url());
        debug!(backend = %backend.id, %url, "proxying chat completion request (legacy)");

        match state.http_client.post(&url).json(&payload).send().await {
            Ok(upstream) => {
                let status = upstream.status();

                if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                {
                    last_error = Some(format!("{} responded {} (busy)", backend.id, status));
                    continue;
                }

                if streaming {
                    return openai_compat::passthrough_streaming_response(upstream)
                        .await
                        .map_err(|e| {
                            (
                                StatusCode::BAD_GATEWAY,
                                Json(json!({"error": {"message": e, "type": "upstream_error"}})),
                            )
                        });
                }
                // Non-streaming: record usage
                let status = upstream.status();
                let bytes = upstream.bytes().await.unwrap_or_default();
                let latency_ms = 0u64; // Legacy path doesn't track latency
                let bytes =
                    record_usage_from_response(&state, backend, &payload, bytes, latency_ms).await;
                let mut response = Response::builder().status(status);
                response = response.header(header::CONTENT_TYPE, "application/json");
                return response.body(Body::from(bytes)).map_err(|e| {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(
                            json!({"error": {"message": e.to_string(), "type": "upstream_error"}}),
                        ),
                    )
                });
            }
            Err(err) => {
                warn!(backend = %backend.id, %err, "upstream request failed; trying fallback");
                last_error = Some(format!("{} request failed: {}", backend.id, err));
            }
        }
    }

    Err((
        StatusCode::BAD_GATEWAY,
        Json(json!({"error": {
            "message": last_error.unwrap_or_else(|| "all backends failed".to_string()),
            "type": "upstream_error"
        }})),
    ))
}

/// Record token usage from a non-streaming upstream response.
async fn record_usage_from_response(
    state: &GatewayState,
    backend: &ff_api::registry::BackendEndpoint,
    payload: &ChatCompletionRequest,
    upstream_body: bytes::Bytes,
    latency_ms: u64,
) -> bytes::Bytes {
    // Try to parse usage from the response
    if let Ok(resp_json) = serde_json::from_slice::<Value>(&upstream_body) {
        let model = resp_json
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&backend.model)
            .to_string();
        let prompt_tokens = resp_json
            .get("usage")
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let completion_tokens = resp_json
            .get("usage")
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let _total_tokens = prompt_tokens + completion_tokens;

        let cost = if backend.is_local {
            0.0
        } else {
            backend.estimate_cost(prompt_tokens, completion_tokens)
        };

        let record = TokenUsageRecord::new(uuid::Uuid::new_v4().to_string(), &model, &backend.id)
            .with_tokens(prompt_tokens, completion_tokens)
            .with_cost(cost, backend.is_local)
            .with_latency(latency_ms);

        state.cost_tracker.record_usage(record).await;

        // Update Prometheus metrics
        ff_observability::metrics::LLM_TOKENS_TOTAL
            .with_label_values(&[&model, "prompt"])
            .inc_by(prompt_tokens as u64);
        ff_observability::metrics::LLM_TOKENS_TOTAL
            .with_label_values(&[&model, "completion"])
            .inc_by(completion_tokens as u64);
        ff_observability::metrics::LLM_COST_USD_TOTAL
            .with_label_values(&[&model, if backend.is_local { "true" } else { "false" }])
            .add(cost);

        // Interaction-log capture (V121 ff_interactions) for the legacy
        // tier/model-router fallback. Pulse-routed turns are captured in
        // proxy_chat_completions; the `choices` gate skips upstream error
        // bodies that flow through here on the 4xx passthrough path.
        if resp_json.get("choices").is_some()
            && let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool())
        {
            let request_text = payload
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| match &m.content {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let mut rec = build_router_interaction(
                request_text,
                &resp_json,
                latency_ms.min(i32::MAX as u64) as i32,
            );
            if rec.worker_name.is_none() {
                rec.worker_name = Some(backend.node.clone());
            }
            if rec.endpoint.is_none() {
                rec.endpoint = Some(backend.base_url());
            }
            spawn_interaction_capture(pool.clone(), rec);
        }
    }
    upstream_body
}

/// Record token usage from a Pulse-routed (local fleet) response.
///
/// Unlike the legacy tier-router path, Pulse responses carry routing
/// metadata under `_forgefleet_route` but the upstream `usage` block
/// is standard OpenAI shape. We parse it and update the cost tracker
/// and Prometheus counters so local fleet inference is visible in the
/// ledger alongside cloud provider usage.
async fn record_pulse_usage(state: &GatewayState, value: &Value) {
    let Some(usage) = value.get("usage") else {
        return;
    };
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let record = TokenUsageRecord::new(uuid::Uuid::new_v4().to_string(), &model, "pulse-local")
        .with_tokens(prompt_tokens, completion_tokens)
        .with_cost(0.0, true) // local inference has no external cost
        .with_latency(0); // latency not captured in this path yet

    state.cost_tracker.record_usage(record).await;

    ff_observability::metrics::LLM_TOKENS_TOTAL
        .with_label_values(&[&model, "prompt"])
        .inc_by(prompt_tokens as u64);
    ff_observability::metrics::LLM_TOKENS_TOTAL
        .with_label_values(&[&model, "completion"])
        .inc_by(completion_tokens as u64);
    ff_observability::metrics::LLM_COST_USD_TOTAL
        .with_label_values(&[&model, "true"])
        .add(0.0);
}

/// Last `role == "user"` message content from a raw OpenAI-style chat body.
/// String content is returned as-is; structured (array) content is serialized
/// so multimodal turns still land in the corpus.
fn last_user_message_text(body: &Value) -> String {
    body.get("messages")
        .and_then(|v| v.as_array())
        .and_then(|msgs| {
            msgs.iter()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        })
        .and_then(|m| m.get("content"))
        .map(|c| match c {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

/// Build the `ff_interactions` row (V121/V138) for one routed
/// /v1/chat/completions turn. Pure extraction — no I/O — so the mapping is
/// unit-testable without a database. `_forgefleet_route` (attached by the
/// Pulse router) supplies worker/endpoint attribution when present.
fn build_router_interaction(
    request_text: String,
    response: &Value,
    latency_ms: i32,
) -> ff_db::InteractionRecord {
    let usage = response.get("usage");
    let route = response.get("_forgefleet_route");
    ff_db::InteractionRecord {
        channel: "gateway-router".to_string(),
        request_text,
        route_decision: route.cloned().unwrap_or_else(|| json!({})),
        engine: response
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        response_text: response
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        tokens_in: usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        tokens_out: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        latency_ms: Some(latency_ms),
        outcome: "ok".to_string(),
        worker_name: route
            .and_then(|r| r.get("computer"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        endpoint: route
            .and_then(|r| r.get("endpoint"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ..Default::default()
    }
}

/// Fire-and-forget insert into `ff_interactions`. Never blocks the HTTP
/// response. Failures log at warn (not debug): a silent capture failure is
/// exactly how the corpus went dark for a month without anyone noticing.
fn spawn_interaction_capture(pool: sqlx::PgPool, rec: ff_db::InteractionRecord) {
    tokio::spawn(async move {
        if let Err(e) = ff_db::pg_record_interaction(&pool, &rec).await {
            warn!(error = %e, "gateway-router interaction capture failed");
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
//  P1.2 Inter-Node RPC  +  P1.6 Async Job Queue
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
struct AsyncQuery {
    async_mode: Option<bool>,
}

/// POST /v1/internal/delegate — accept a signed task from another fleet node.
///
/// The caller must include:
///   - `X-ForgeFleet-Signature`: hex HMAC-SHA256 of `{method}\n{path}\n{ts}\n{body}`
///   - `X-ForgeFleet-Timestamp`: unix seconds
///
/// Verified against `config.enrollment.resolve_shared_secret()`.
async fn internal_delegate(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let signature = headers
        .get("x-forgefleet-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let timestamp_str = headers
        .get("x-forgefleet-timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0");
    let timestamp: i64 = timestamp_str.parse().unwrap_or(0);

    let secret = if let Some(cfg_lock) = &state.fleet_config {
        let cfg = cfg_lock.read().await;
        cfg.enrollment.resolve_shared_secret().unwrap_or_default()
    } else {
        String::new()
    };

    if secret.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "inter-node auth not configured"})),
        ));
    }

    if !ff_security::computer_auth::is_request_fresh(timestamp, 300) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "request expired / replay detected"})),
        ));
    }

    if !ff_security::computer_auth::verify_signature(
        &secret,
        "POST",
        "/v1/internal/delegate",
        timestamp,
        &body,
        signature,
    ) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid signature"})),
        ));
    }

    let payload: Value = serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid json: {e}")})),
        )
    })?;

    let pool = match state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "postgres unavailable"})),
            ));
        }
    };

    let task_type = payload
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("delegate");
    let summary = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("delegated task");
    let task_payload = payload.get("payload").cloned().unwrap_or(json!({}));
    let priority = payload
        .get("priority")
        .and_then(|v| v.as_i64())
        .unwrap_or(50) as i32;
    let capabilities = payload.get("capabilities").cloned().unwrap_or(json!([]));

    let task_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (task_type, summary, payload, priority, requires_capability, status, created_at)
        VALUES ($1, $2, $3, $4, $5, 'pending', NOW())
        RETURNING id
        "#,
    )
    .bind(task_type)
    .bind(summary)
    .bind(&task_payload)
    .bind(priority)
    .bind(&capabilities)
    .fetch_one(pool)
    .await
    .map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("db error: {e}")})),
    ))?;

    Ok(Json(json!({
        "task_id": task_id,
        "status": "pending",
        "poll_url": format!("/v1/async/{task_id}"),
    })))
}

/// GET /v1/async/{ticket} — poll the status of an async job.
async fn async_poll(
    State(state): State<Arc<GatewayState>>,
    Path(ticket): Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = match state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "postgres unavailable"})),
            ));
        }
    };

    let row = sqlx::query(
        "SELECT status, result, error, progress_pct, progress_message, completed_at FROM fleet_tasks WHERE id = $1"
    )
    .bind(ticket)
    .fetch_optional(pool)
    .await
    .map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("db error: {e}")})),
    ))?;

    match row {
        Some(r) => {
            let status: String = r.get("status");
            let result: Option<Value> = r.get("result");
            let error: Option<String> = r.get("error");
            let progress_pct: Option<f32> = r.get("progress_pct");
            let progress_message: Option<String> = r.get("progress_message");
            let completed_at: Option<chrono::DateTime<Utc>> = r.get("completed_at");

            Ok(Json(json!({
                "ticket": ticket,
                "status": status,
                "result": result,
                "error": error,
                "progress_pct": progress_pct,
                "progress_message": progress_message,
                "completed_at": completed_at,
            })))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "ticket not found"})),
        )),
    }
}

/// GET /v1/models — list available models from the backend registry.
async fn list_models(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let now = Utc::now().timestamp();
    let mut model_map: std::collections::BTreeMap<String, (u8, bool, f64, f64)> =
        std::collections::BTreeMap::new();

    // 1) Static registry models
    if let Some(registry) = &state.api_registry {
        let models = registry.available_models().await;
        let endpoints = registry.all_endpoints().await;
        for (model, tier) in models {
            let model_eps: Vec<_> = endpoints.iter().filter(|e| e.model == model).collect();
            let is_local = model_eps.first().map(|e| e.is_local).unwrap_or(true);
            let cost_in = model_eps
                .first()
                .map(|e| e.cost_per_1k_input)
                .unwrap_or(0.0);
            let cost_out = model_eps
                .first()
                .map(|e| e.cost_per_1k_output)
                .unwrap_or(0.0);
            model_map.insert(model, (tier, is_local, cost_in, cost_out));
        }
    }

    // 2) Operational store models (Postgres)
    if let Some(store) = state.operational_store.as_ref()
        && let Some(pool) = store.pg_pool()
    {
        match sqlx::query("SELECT slug, tier FROM fleet_models")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => {
                for row in rows {
                    let slug: String = row.get("slug");
                    let tier: i32 = row.get("tier");
                    let is_local = !slug.starts_with("gpt")
                        && !slug.starts_with("claude")
                        && !slug.starts_with("gemini");
                    let entry = model_map
                        .entry(slug)
                        .or_insert((tier as u8, is_local, 0.0, 0.0));
                    entry.0 = entry.0.min(tier as u8);
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "list_models: fleet_models query failed");
            }
        }
    }

    let data: Vec<Value> = model_map
        .into_iter()
        .map(|(model, (tier, is_local, cost_in, cost_out))| {
            json!({
                "id": model,
                "object": "model",
                "created": now,
                "owned_by": "forgefleet",
                "tier": tier,
                "is_local": is_local,
                "cost_per_1k_input": cost_in,
                "cost_per_1k_output": cost_out,
            })
        })
        .collect();

    Ok(Json(json!({
        "object": "list",
        "data": data,
    })))
}

// ─── POST /v1/fleet/route — capability-based fleet routing ───────────────────

#[derive(Debug, Deserialize)]
struct RouteFleetRequest {
    /// Human-readable task description (for logging / reasoning).
    #[serde(default)]
    task: String,
    /// Required capabilities the chosen model must support, e.g.
    /// `["vision"]` or `["reasoning", "tool_calling"]`.
    required_capabilities: Vec<String>,
    /// Prefer a model running on the local node (same host as gateway).
    #[serde(default)]
    preferred_local: bool,
}

#[derive(Debug, Serialize)]
struct RouteFleetResponse {
    /// The resolved upstream URL for the chosen model.
    target: String,
    /// Node name hosting the chosen model.
    node: String,
    /// Model slug / identifier.
    model: String,
    /// Human-readable model name.
    model_name: String,
    /// Capabilities this model advertises.
    capabilities: Vec<String>,
    /// Whether the chosen endpoint is on the local node.
    is_local: bool,
    /// Why this endpoint was chosen.
    reason: String,
    /// Current queue depth on the chosen server (if known from pulse).
    queue_depth: Option<i32>,
    /// Tokens/sec served in the last minute (if known from pulse).
    tokens_per_sec: Option<f64>,
    /// Alternative candidates that also matched (for debugging).
    alternatives: Vec<AlternativeCandidate>,
}

#[derive(Debug, Serialize)]
struct AlternativeCandidate {
    node: String,
    model: String,
    target: String,
    reason_skipped: String,
}

/// POST /v1/fleet/route
///
/// Accepts a task description + required capabilities and returns the best
/// live fleet endpoint that can serve it.  Uses two sources of truth:
///   1. Postgres `fleet_models` × `fleet_workers` for capability metadata + health.
///   2. Redis Pulse beats for live server state (queue depth, throughput).
///
/// This is the primitive that project sidecars should call instead of
/// hard-coding node:port mappings.
async fn route_fleet_capability(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<RouteFleetRequest>,
) -> Result<Json<RouteFleetResponse>, (StatusCode, Json<Value>)> {
    let cap_set: HashSet<String> = req.required_capabilities.iter().cloned().collect();
    if cap_set.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "required_capabilities must not be empty"})),
        ));
    }

    // ── 1. Load catalog entries for capability enrichment ──
    // Query BOTH fleet_model_catalog (new, preferred) and model_catalog (legacy V39).
    let mut catalog_entries: HashMap<String, (String, i32, Value)> = HashMap::new();

    if let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        // 1a. New table
        match sqlx::query("SELECT id, name, tier, preferred_workloads FROM fleet_model_catalog")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => {
                for r in rows {
                    let id: String = r.get("id");
                    let name: String = r.get("name");
                    let tier: i32 = r.get("tier");
                    let pw: Value = r.get("preferred_workloads");
                    catalog_entries.insert(id, (name, tier, pw));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "route_fleet_capability: fleet_model_catalog query failed")
            }
        }

        // 1b. Legacy table (V39) — merge in anything missing
        match sqlx::query(
            "SELECT id, display_name, COALESCE((metadata->>'tier')::int, 2) as tier, metadata->>'preferred_workloads' as pw FROM model_catalog WHERE metadata->>'preferred_workloads' IS NOT NULL"
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                for r in rows {
                    let id: String = r.get("id");
                    if catalog_entries.contains_key(&id) {
                        continue;
                    }
                    let name: String = r.get("display_name");
                    let tier: i32 = r.get("tier");
                    let pw_raw: Option<String> = r.get("pw");
                    let pw = pw_raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Array(vec![]));
                    catalog_entries.insert(id, (name, tier, pw));
                }
            }
            Err(e) => tracing::warn!(error = %e, "route_fleet_capability: model_catalog query failed"),
        }
    }

    // ── 2. Fetch live Pulse servers ──
    let live_servers = match state.pulse_router.as_ref() {
        Some(router) => router.list_servers().await.unwrap_or_default(),
        None => Vec::new(),
    };

    // ── 3. Enrich live servers with catalog capabilities ──
    let mut candidates: Vec<(String, String, String, i32, String, Value, &Value)> = Vec::new();
    for s in &live_servers {
        let computer = s
            .get("computer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let endpoint = s
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let raw_model_id = s
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let healthy = s.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);
        if !healthy {
            continue;
        }

        // Normalize model id: basename for paths, lowercase for matching
        let model_id = std::path::Path::new(&raw_model_id)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&raw_model_id)
            .to_string();

        // Try exact catalog match first, then fuzzy prefix match
        let catalog_match: Option<&(String, i32, Value)> =
            catalog_entries.get(&model_id).or_else(|| {
                let model_lower = model_id.to_lowercase();
                catalog_entries
                    .iter()
                    .find(|(cat_id, _)| {
                        model_lower.contains(&cat_id.to_lowercase())
                            || cat_id.to_lowercase().contains(&model_lower)
                    })
                    .map(|(_, v)| v)
            });

        let (name, tier, pw): (String, i32, Value) = match catalog_match {
            Some((n, t, p)) => (n.clone(), *t, p.clone()),
            None => (model_id.clone(), 2, Value::Array(vec![])),
        };

        // Filter by required capabilities
        let has_all_caps = req.required_capabilities.iter().all(|cap| {
            pw.as_array()
                .map(|arr: &Vec<Value>| arr.iter().any(|v| v.as_str() == Some(cap)))
                .unwrap_or(false)
        });

        if has_all_caps {
            candidates.push((computer, model_id, name, tier, endpoint, pw, s));
        }
    }

    // ── 3. Score candidates ──
    //   Priority: exact local match > lower tier > lower queue depth > higher tps
    let local_hostname = tokio::task::spawn_blocking(|| {
        std::env::var("FF_NODE")
            .ok()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .or_else(|| {
                std::process::Command::new("hostname")
                    .arg("-s")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
            })
            .unwrap_or_default()
            .trim()
            .to_lowercase()
    })
    .await
    .unwrap_or_default();

    #[allow(clippy::type_complexity)]
    let mut scored: Vec<(
        i32,
        i32,
        i32,
        f64,
        String,
        String,
        String,
        String,
        Value,
        &Value,
    )> = Vec::new();
    let mut alternatives: Vec<AlternativeCandidate> = Vec::new();

    for (worker_name, slug, name, tier, endpoint, pw, beat) in &candidates {
        let is_local = worker_name.to_lowercase() == local_hostname
            || local_hostname.starts_with(&worker_name.to_lowercase())
            || worker_name.to_lowercase().starts_with(&local_hostname);
        let qd = beat
            .get("queue_depth")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        let tps = beat
            .get("tokens_per_sec_last_min")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        // Scoring tuple: (local_bonus, tier_asc, queue_asc, tps_desc)
        let local_bonus = if is_local && req.preferred_local {
            1
        } else {
            0
        };

        scored.push((
            local_bonus,
            *tier,
            qd,
            tps,
            slug.clone(),
            name.clone(),
            worker_name.clone(),
            endpoint.clone(),
            pw.clone(),
            *beat,
        ));
    }

    // Sort: local first, then lower tier, then lower queue, then higher tps
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0) // local bonus descending
            .then_with(|| a.1.cmp(&b.1)) // tier ascending
            .then_with(|| a.2.cmp(&b.2)) // queue depth ascending
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal)) // tps descending
    });

    if let Some((_, tier, qd, tps, slug, name, worker_name, endpoint, pw, _beat)) = scored.first() {
        let caps = pw
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let is_local = worker_name.to_lowercase() == local_hostname;
        let reason = if is_local && req.preferred_local {
            format!("local match, tier {tier}, queue_depth {qd}")
        } else {
            format!("fleet match, tier {tier}, queue_depth {qd}, tps {tps:.1}")
        };

        // Build alternatives list from remaining scored items + skipped items
        for (_, _, _, _, alt_slug, _, alt_node, alt_endpoint, _, _) in scored.iter().skip(1).take(5)
        {
            alternatives.push(AlternativeCandidate {
                node: alt_node.clone(),
                model: alt_slug.clone(),
                target: alt_endpoint.clone(),
                reason_skipped: "lower priority".to_string(),
            });
        }

        return Ok(Json(RouteFleetResponse {
            target: endpoint.clone(),
            node: worker_name.clone(),
            model: slug.clone(),
            model_name: name.clone(),
            capabilities: caps,
            is_local,
            reason,
            queue_depth: Some(*qd),
            tokens_per_sec: Some(*tps),
            alternatives,
        }));
    }

    // ── 4. No match ──
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "no healthy fleet endpoint matches the required capabilities",
            "required_capabilities": req.required_capabilities,
            "task": req.task,
            "alternatives_considered": alternatives.len(),
        })),
    ))
}

/// POST /v1/embeddings — OpenAI-compatible embedding proxy routed to fleet nodes
/// that advertise an `embeddings` capability.
async fn proxy_embeddings(
    State(state): State<Arc<GatewayState>>,
    Json(raw_payload): Json<Value>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    let requested_model = raw_payload
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);

    // ── 1. Load catalog entries with embedding capability ──
    let mut catalog_entries: HashMap<String, (String, i32, Value)> = HashMap::new();

    if let Some(pool) = state.operational_store.as_ref().and_then(|s| s.pg_pool()) {
        match sqlx::query("SELECT id, name, tier, preferred_workloads FROM fleet_model_catalog")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => {
                for r in rows {
                    let id: String = r.get("id");
                    let name: String = r.get("name");
                    let tier: i32 = r.get("tier");
                    let pw: Value = r.get("preferred_workloads");
                    catalog_entries.insert(id, (name, tier, pw));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "proxy_embeddings: fleet_model_catalog query failed")
            }
        }

        match sqlx::query(
            "SELECT id, display_name, COALESCE((metadata->>'tier')::int, 2) as tier, metadata->>'preferred_workloads' as pw FROM model_catalog WHERE metadata->>'preferred_workloads' IS NOT NULL"
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                for r in rows {
                    let id: String = r.get("id");
                    if catalog_entries.contains_key(&id) {
                        continue;
                    }
                    let name: String = r.get("display_name");
                    let tier: i32 = r.get("tier");
                    let pw_raw: Option<String> = r.get("pw");
                    let pw = pw_raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Array(vec![]));
                    catalog_entries.insert(id, (name, tier, pw));
                }
            }
            Err(e) => tracing::warn!(error = %e, "proxy_embeddings: model_catalog query failed"),
        }
    }

    // ── 2. Fetch live Pulse servers ──
    let live_servers = match state.pulse_router.as_ref() {
        Some(router) => router.list_servers().await.unwrap_or_default(),
        None => Vec::new(),
    };

    // ── 3. Find embedding-capable candidates ──
    let mut candidates: Vec<(String, String, String, i32, String, Value, &Value)> = Vec::new();
    for s in &live_servers {
        let computer = s
            .get("computer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let endpoint = s
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let raw_model_id = s
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let healthy = s.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);
        if !healthy {
            continue;
        }

        let model_id = std::path::Path::new(&raw_model_id)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&raw_model_id)
            .to_string();

        let catalog_match: Option<&(String, i32, Value)> =
            catalog_entries.get(&model_id).or_else(|| {
                let model_lower = model_id.to_lowercase();
                catalog_entries
                    .iter()
                    .find(|(cat_id, _)| {
                        model_lower.contains(&cat_id.to_lowercase())
                            || cat_id.to_lowercase().contains(&model_lower)
                    })
                    .map(|(_, v)| v)
            });

        let (name, tier, pw): (String, i32, Value) = match catalog_match {
            Some((n, t, p)) => (n.clone(), *t, p.clone()),
            None => (model_id.clone(), 2, Value::Array(vec![])),
        };

        // Must have embeddings capability
        let has_embedding_cap = pw
            .as_array()
            .map(|arr: &Vec<Value>| {
                arr.iter()
                    .any(|v| v.as_str() == Some("embeddings") || v.as_str() == Some("embedding"))
            })
            .unwrap_or(false);

        if !has_embedding_cap {
            continue;
        }

        // If a specific model was requested, filter by it
        if let Some(ref req_model) = requested_model {
            let req_lower = req_model.to_lowercase();
            let model_lower = model_id.to_lowercase();
            let name_lower = name.to_lowercase();
            if !model_lower.contains(&req_lower)
                && !name_lower.contains(&req_lower)
                && !req_lower.contains(&model_lower)
            {
                continue;
            }
        }

        candidates.push((computer, model_id, name, tier, endpoint, pw, s));
    }

    // ── 4. Score candidates (lower tier > lower queue depth > higher tps) ──
    let mut scored: Vec<(i32, i32, f64, String, String, String)> = Vec::new();
    for (_node_name, slug, name, tier, endpoint, _pw, beat) in &candidates {
        let qd = beat
            .get("queue_depth")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        let tps = beat
            .get("tokens_per_sec_last_min")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        scored.push((*tier, qd, tps, slug.clone(), name.clone(), endpoint.clone()));
    }

    scored.sort_by(|a, b| {
        a.0.cmp(&b.0) // tier ascending
            .then_with(|| a.1.cmp(&b.1)) // queue depth ascending
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)) // tps descending
    });

    // ── 5. Proxy to best candidate ──
    if let Some((_tier, _qd, _tps, slug, _name, endpoint)) = scored.first() {
        let url = format!("{endpoint}/v1/embeddings");
        debug!(model = %slug, %url, "proxying embeddings request");

        match state.http_client.post(&url).json(&raw_payload).send().await {
            Ok(upstream) => {
                let status = upstream.status();
                let bytes = upstream.bytes().await.unwrap_or_default();
                let mut response = Response::builder().status(status.as_u16());
                response = response.header(header::CONTENT_TYPE, "application/json");
                return response.body(Body::from(bytes)).map_err(|e| {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(
                            json!({"error": {"message": e.to_string(), "type": "upstream_error"}}),
                        ),
                    )
                });
            }
            Err(err) => {
                warn!(model = %slug, %err, "embeddings upstream request failed");
                return Err((
                    StatusCode::BAD_GATEWAY,
                    Json(
                        json!({"error": {"message": format!("embeddings upstream failed: {}", err), "type": "upstream_error"}}),
                    ),
                ));
            }
        }
    }

    // ── 6. No match ──
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "message": "no healthy fleet endpoint with embeddings capability",
                "type": "backend_unavailable",
            },
            "model": requested_model,
        })),
    ))
}

/// GET /api/proxy/stats (and /v1/proxy/stats) — dashboard-friendly proxy metrics.
async fn proxy_stats(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(tier_router) = &state.tier_router else {
        return Ok(Json(json!({
            "totalRequests": 0,
            "avgLatencyMs": 0.0,
            "errorRate": 0.0,
            "activeRoutes": 0,
        })));
    };

    let metrics = tier_router.all_metrics();
    let total_requests: u64 = metrics.iter().map(|m| m.request_count).sum();
    let total_errors: u64 = metrics.iter().map(|m| m.error_count).sum();
    let weighted_latency_sum: f64 = metrics
        .iter()
        .map(|m| m.avg_latency_ms * (m.request_count as f64))
        .sum();

    let avg_latency = if total_requests > 0 {
        weighted_latency_sum / (total_requests as f64)
    } else {
        0.0
    };

    let error_rate_pct = if total_requests > 0 {
        (total_errors as f64 / total_requests as f64) * 100.0
    } else {
        0.0
    };

    let active_routes = metrics.iter().filter(|m| m.request_count > 0).count();

    Ok(Json(json!({
        "totalRequests": total_requests,
        "avgLatencyMs": avg_latency,
        "errorRate": (error_rate_pct * 100.0).round() / 100.0,
        "activeRoutes": active_routes,
    })))
}

/// GET /api/proxy/requests (and /v1/proxy/requests) — lightweight recent request view.
async fn proxy_requests(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(tier_router) = &state.tier_router else {
        return Ok(Json(json!({ "requests": [] })));
    };

    let now = Utc::now().to_rfc3339();
    let mut requests: Vec<Value> = tier_router
        .all_metrics()
        .into_iter()
        .filter(|m| m.request_count > 0)
        .map(|m| {
            json!({
                "id": m.backend_id,
                "model": m.backend_id,
                "tier": null,
                "latencyMs": (m.avg_latency_ms * 100.0).round() / 100.0,
                "decision": "tier_router",
                "status": if m.error_count > 0 { "degraded" } else { "ok" },
                "timestamp": now,
                "requests": m.request_count,
                "successCount": m.success_count,
                "errorCount": m.error_count,
                "errorRate": (m.error_rate * 10000.0).round() / 100.0,
            })
        })
        .collect();

    requests.sort_by(|a, b| {
        let ar = a.get("requests").and_then(Value::as_u64).unwrap_or(0);
        let br = b.get("requests").and_then(Value::as_u64).unwrap_or(0);
        br.cmp(&ar)
    });

    Ok(Json(json!({ "requests": requests })))
}

// ─── Updates (dashboard contract compatibility) ─────────────────────────────

async fn update_status(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    let rollout = state.update_state.read().await.clone();
    Json(json!({
        "currentVersion": ff_core::VERSION,
        "rollout": {
            "id": rollout.id,
            "version": rollout.version,
            "stage": rollout.stage,
            "progress": rollout.progress,
            "startedAt": rollout.started_at,
            "completedAt": rollout.completed_at,
            "nodes": []
        },
        "history": [],
        "nodeVersions": []
    }))
}

async fn update_check() -> Json<Value> {
    Json(json!({
        "available": false,
        "currentVersion": ff_core::VERSION,
        "latestVersion": ff_core::VERSION,
        "releaseNotes": "No new release metadata source configured in gateway"
    }))
}

async fn update_pause(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    let mut rollout = state.update_state.write().await;
    rollout.stage = Some("paused".to_string());
    Json(json!({"status": "ok", "message": "rollout paused"}))
}

async fn update_resume(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    let mut rollout = state.update_state.write().await;
    rollout.stage = Some("rolling".to_string());
    if rollout.started_at.is_none() {
        rollout.started_at = Some(Utc::now().to_rfc3339());
    }
    Json(json!({"status": "ok", "message": "rollout resumed"}))
}

async fn update_abort(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    let mut rollout = state.update_state.write().await;
    rollout.stage = Some("aborted".to_string());
    rollout.completed_at = Some(Utc::now().to_rfc3339());
    Json(json!({"status": "ok", "message": "rollout aborted"}))
}

// ─── Dashboard ───────────────────────────────────────────────────────────────

async fn dashboard(State(state): State<Arc<GatewayState>>) -> Html<String> {
    let html = format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>ForgeFleet Gateway Dashboard</title>
    <style>
      :root {{ color-scheme: dark; }}
      body {{ font-family: Inter, system-ui, sans-serif; margin: 24px; background: #030712; color: #e5e7eb; }}
      .grid {{ display: grid; grid-template-columns: repeat(auto-fit,minmax(220px,1fr)); gap: 14px; margin-bottom: 16px; }}
      .card {{ border: 1px solid #1f2937; border-radius: 12px; padding: 12px; background: #0f172a; }}
      code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; color: #93c5fd; }}
      #stream {{ border: 1px solid #1f2937; border-radius: 12px; padding: 12px; min-height: 220px; max-height: 420px; overflow: auto; background: #0b1220; }}
      .row {{ margin-bottom: 8px; font-size: 13px; line-height: 1.4; }}
      .muted {{ color: #9ca3af; }}
    </style>
  </head>
  <body>
    <h1>ForgeFleet Gateway</h1>
    <div class="grid">
      <div class="card"><div class="muted">Active WebSocket clients</div><div id="ws">{ws_clients}</div></div>
      <div class="card"><div class="muted">Inbound buffered</div><div id="inbound">{inbound}</div></div>
      <div class="card"><div class="muted">Outbound buffered</div><div id="outbound">{outbound}</div></div>
      <div class="card"><div class="muted">Embed script</div><code>/embed/widget.js</code></div>
    </div>

    <h3>Live events</h3>
    <div id="stream"></div>

    <script>
      const stream = document.getElementById('stream');
      const protocol = location.protocol === 'https:' ? 'wss' : 'ws';
      const ws = new WebSocket(protocol + '://' + location.host + '/ws');
      ws.addEventListener('message', (event) => {{
        const row = document.createElement('div');
        row.className = 'row';
        row.textContent = event.data;
        stream.prepend(row);
      }});
    </script>
  </body>
</html>"#,
        ws_clients = state.web_clients.len(),
        inbound = state.inbound_messages.len(),
        outbound = state.outbound_messages.len(),
    );

    Html(html)
}

// ─── Message handling ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MessageQuery {
    channel: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct InboundAcceptedResponse {
    accepted: bool,
    id: String,
    channel: Channel,
    target: RouteTarget,
    reason: String,
}

async fn incoming_message_http(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<MessageQuery>,
    Json(payload): Json<Value>,
) -> Result<Json<InboundAcceptedResponse>, (StatusCode, Json<Value>)> {
    let fallback_channel = query
        .channel
        .as_deref()
        .and_then(|value| value.parse::<Channel>().ok());

    let incoming = if payload.get("channel").is_some() && payload.get("chat_id").is_some() {
        serde_json::from_value::<IncomingMessage>(payload).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": error.to_string(), "type": "invalid_payload"}})),
            )
        })?
    } else {
        webhook::normalize_payload(payload, fallback_channel).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": error.to_string(), "type": "invalid_webhook"}})),
            )
        })?
    };

    let accepted = process_incoming_message(state, incoming).await;
    Ok(Json(accepted))
}

async fn incoming_message_raw_http(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<MessageQuery>,
    body: Bytes,
) -> Result<Json<InboundAcceptedResponse>, (StatusCode, Json<Value>)> {
    let payload: Value = serde_json::from_slice(&body).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": error.to_string(), "type": "invalid_json"}})),
        )
    })?;

    incoming_message_http(State(state), Query(query), Json(payload)).await
}

async fn outgoing_message_http(
    State(state): State<Arc<GatewayState>>,
    Json(outgoing): Json<OutgoingMessage>,
) -> Json<Value> {
    state
        .outbound_messages
        .insert(outgoing.id, outgoing.clone());
    state.broadcast_json(json!({
        "type": "outgoing_message",
        "id": outgoing.id,
        "channel": outgoing.channel,
        "chat_id": outgoing.chat_id,
        "text": outgoing.text,
        "created_at": outgoing.created_at,
    }));

    let mut delivery = json!({
        "attempted": false,
        "status": "buffered_only"
    });

    if outgoing.channel == Channel::Telegram {
        delivery["attempted"] = json!(true);

        let telegram_cfg = if let Some(config) = &state.fleet_config {
            config.read().await.transport.telegram.clone()
        } else {
            None
        };

        match telegram_cfg {
            Some(cfg) if cfg.enabled => match outgoing.chat_id.trim().parse::<i64>() {
                Ok(chat_id) if !cfg.is_chat_allowed(chat_id) => {
                    delivery["status"] = json!("blocked_by_allowlist");
                    delivery["error"] = json!(format!(
                        "chat {} is not in telegram allowlist",
                        outgoing.chat_id
                    ));
                }
                Ok(_) => {
                    if let Some(token) = cfg.resolve_bot_token() {
                        match TelegramClient::new(token) {
                            Ok(client) => match client.send_message(&outgoing).await {
                                Ok(response) => {
                                    delivery["status"] = json!("sent");
                                    delivery["telegram_response"] = response;
                                }
                                Err(error) => {
                                    warn!(%error, "failed to send telegram outbound message");
                                    delivery["status"] = json!("send_failed");
                                    delivery["error"] = json!(error.to_string());
                                }
                            },
                            Err(error) => {
                                delivery["status"] = json!("client_init_failed");
                                delivery["error"] = json!(error.to_string());
                            }
                        }
                    } else {
                        delivery["status"] = json!("missing_token");
                        delivery["error"] = json!(format!(
                            "telegram bot token not configured (set {} or transport.telegram.bot_token)",
                            cfg.bot_token_env
                        ));
                    }
                }
                Err(_) => {
                    delivery["status"] = json!("invalid_chat_id");
                    delivery["error"] =
                        json!("telegram chat_id must be numeric when sending outbound messages");
                }
            },
            Some(_) => {
                delivery["status"] = json!("disabled");
                delivery["error"] = json!("transport.telegram.enabled is false");
            }
            None => {
                delivery["status"] = json!("not_configured");
                delivery["error"] = json!("transport.telegram config is missing");
            }
        }
    }

    Json(json!({
        "accepted": true,
        "id": outgoing.id,
        "delivery": delivery,
    }))
}

// ─── WebSocket ───────────────────────────────────────────────────────────────

async fn websocket_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket_session(state, socket))
}

async fn websocket_session(state: Arc<GatewayState>, mut socket: axum::extract::ws::WebSocket) {
    let session_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel::<WsMessage>(256);

    state.web_clients.insert(session_id, tx);
    info!(session = %session_id, "web client connected");

    loop {
        tokio::select! {
            outbound = rx.recv() => {
                match outbound {
                    Some(message) => {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Err(error) = handle_websocket_text(state.clone(), session_id, text.to_string()).await {
                            warn!(session = %session_id, %error, "failed to process websocket text message");
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        warn!(session = %session_id, %error, "websocket receive error");
                        break;
                    }
                }
            }
        }
    }

    state.web_clients.remove(&session_id);
    info!(session = %session_id, "web client disconnected");
}

#[derive(Debug, Deserialize)]
struct WebSocketInbound {
    text: String,
    chat_id: Option<String>,
    user_id: Option<String>,
    thread_id: Option<String>,
    #[serde(default)]
    metadata: Value,
}

async fn handle_websocket_text(
    state: Arc<GatewayState>,
    session_id: Uuid,
    text: String,
) -> anyhow::Result<()> {
    let parsed = match serde_json::from_str::<WebSocketInbound>(&text) {
        Ok(value) => value,
        Err(_) => WebSocketInbound {
            text,
            chat_id: None,
            user_id: None,
            thread_id: None,
            metadata: Value::Null,
        },
    };

    let mut incoming = IncomingMessage::new(
        Channel::Web,
        parsed
            .chat_id
            .unwrap_or_else(|| format!("web-{session_id}")),
        parsed
            .user_id
            .unwrap_or_else(|| format!("web-user-{session_id}")),
    );

    incoming.thread_id = parsed.thread_id;
    incoming.text = Some(parsed.text);
    if !parsed.metadata.is_null() {
        incoming
            .metadata
            .insert("ws_metadata".to_string(), parsed.metadata);
    }

    let accepted = process_incoming_message(state.clone(), incoming).await;
    state.broadcast_json(json!({
        "type": "ws_ack",
        "session_id": session_id,
        "message_id": accepted.id,
        "target": accepted.target,
        "reason": accepted.reason,
    }));

    Ok(())
}

async fn process_incoming_message(
    state: Arc<GatewayState>,
    incoming: IncomingMessage,
) -> InboundAcceptedResponse {
    let route = state.router.route(&incoming);
    state.prune_messages(10_000);
    state.inbound_messages.insert(incoming.id, incoming.clone());

    state.broadcast_json(json!({
        "type": "incoming_message",
        "id": incoming.id,
        "channel": incoming.channel,
        "chat_id": incoming.chat_id,
        "thread_id": incoming.thread_id,
        "user_id": incoming.from_user_id,
        "text": incoming.text,
        "route": {
            "target": route.target,
            "reason": route.reason,
            "mentions_bot": route.mentions_bot,
            "command": route.command,
            "tool": route.tool,
        },
        "received_at": incoming.received_at,
    }));

    InboundAcceptedResponse {
        accepted: true,
        id: incoming.id.to_string(),
        channel: incoming.channel,
        target: route.target,
        reason: route.reason,
    }
}

// ─── Agent Session Endpoints ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CreateAgentSessionRequest {
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    llm_base_url: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
}

#[derive(Debug, Serialize)]
struct CreateAgentSessionResponse {
    session_id: String,
    status: &'static str,
    model: String,
    llm_base_url: String,
}

async fn create_agent_session(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<CreateAgentSessionRequest>,
) -> Result<Json<CreateAgentSessionResponse>, (StatusCode, Json<Value>)> {
    let model = req
        .model
        .unwrap_or_else(|| "Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf".to_string());
    let llm_base_url = req
        .llm_base_url
        .unwrap_or_else(|| "http://192.168.5.102:51000".to_string());
    let working_dir = req
        .working_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        });

    // Security: validate working_dir is not trying to escape to system paths.
    // Rejects both raw path traversal and canonicalized system directories.
    let forbidden = [
        "/etc",
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/sys",
        "/dev",
        "/proc",
        "/var/log",
        "/var/spool",
        "/boot",
    ];
    let raw = working_dir.to_string_lossy();
    for prefix in &forbidden {
        if raw.starts_with(prefix) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": format!("working_dir cannot be under {}", prefix)})),
            ));
        }
    }
    // Reject obvious path-traversal sequences in the raw path.
    if raw.split('/').any(|s| s == "..") {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "working_dir contains path traversal ('..')"})),
        ));
    }
    if let Ok(canonical) = working_dir.canonicalize() {
        for prefix in &forbidden {
            if canonical.starts_with(prefix) {
                return Err((
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("working_dir cannot be under {}", prefix)})),
                ));
            }
        }
    }

    let config = AgentSessionConfig {
        model: model.clone(),
        llm_base_url: llm_base_url.clone(),
        working_dir,
        system_prompt: req.system_prompt,
        max_turns: req.max_turns.unwrap_or(30),
        pg_pool: state
            .operational_store
            .as_ref()
            .and_then(|s| s.pg_pool().cloned()),
        ..Default::default()
    };

    let mut session = AgentSession::new(config);
    let session_id = session.id;
    let cancel_token = session.cancel_token.clone();

    let handle = AgentSessionHandle {
        cancel_token: cancel_token.clone(),
        created_at: Utc::now(),
        model: model.clone(),
        llm_base_url: llm_base_url.clone(),
        status: Arc::new(std::sync::atomic::AtomicU8::new(0)),
    };

    state.prune_agent_sessions(100);
    state.agent_sessions.insert(session_id, handle.clone());

    // Spawn the agent loop in a background task
    let ws_hub = state.ws_hub.clone();
    let _sessions = state.agent_sessions.clone();
    let status = handle.status.clone();
    let prompt = req.prompt;

    tokio::spawn(async move {
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);

        // Forward agent events to WebSocket hub in real-time
        let ws_hub_fwd = ws_hub.clone();
        let fwd_handle = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let Ok(payload) = serde_json::to_value(&event) {
                    ws_hub_fwd.broadcast_event(crate::websocket::EventType::AgentEvent, payload);
                }
            }
        });

        let outcome = session.run(&prompt, Some(event_tx)).await;
        let _ = fwd_handle.await;

        match &outcome {
            AgentOutcome::EndTurn { .. } => {
                status.store(1, std::sync::atomic::Ordering::Relaxed);
            }
            AgentOutcome::MaxTurns { .. } => {
                status.store(1, std::sync::atomic::Ordering::Relaxed);
            }
            AgentOutcome::Cancelled => {
                status.store(3, std::sync::atomic::Ordering::Relaxed);
            }
            AgentOutcome::Error(_) => {
                status.store(2, std::sync::atomic::Ordering::Relaxed);
            }
        }

        info!(session = %session_id, "agent session completed");
    });

    Ok(Json(CreateAgentSessionResponse {
        session_id: session_id.to_string(),
        status: "running",
        model,
        llm_base_url,
    }))
}

async fn agent_session_message(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let session_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid session ID"})),
            );
        }
    };

    if !state.agent_sessions.contains_key(&session_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        );
    }

    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // For now, we broadcast a user message event. Full multi-turn injection
    // requires a message channel to the running session (Phase 2).
    state.ws_hub.broadcast_event(
        crate::websocket::EventType::AgentEvent,
        json!({
            "event": "user_message",
            "session_id": session_id.to_string(),
            "message": message,
        }),
    );

    (
        StatusCode::OK,
        Json(json!({"status": "queued", "session_id": session_id.to_string()})),
    )
}

async fn cancel_agent_session(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid session ID"})),
            );
        }
    };

    if let Some(handle) = state.agent_sessions.get(&session_id) {
        handle.cancel_token.cancel();
        (
            StatusCode::OK,
            Json(json!({"status": "cancelled", "session_id": session_id.to_string()})),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        )
    }
}

async fn agent_session_status(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid session ID"})),
            );
        }
    };

    if let Some(handle) = state.agent_sessions.get(&session_id) {
        (
            StatusCode::OK,
            Json(json!({
                "session_id": session_id.to_string(),
                "status": handle.status_str(),
                "model": handle.model,
                "llm_base_url": handle.llm_base_url,
                "created_at": handle.created_at.to_rfc3339(),
            })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        )
    }
}

async fn list_agent_sessions(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    let sessions: Vec<Value> = state
        .agent_sessions
        .iter()
        .map(|entry| {
            json!({
                "session_id": entry.key().to_string(),
                "status": entry.value().status_str(),
                "model": entry.value().model,
                "llm_base_url": entry.value().llm_base_url,
                "created_at": entry.value().created_at.to_rfc3339(),
            })
        })
        .collect();

    // Also return V54 outcome-driven sessions from the agent_sessions
    // table. Surfacing both lets the dashboard show ad-hoc agent
    // sessions AND multi-LLM-team sessions in one place per the
    // multi-LLM CLI integration plan ("dashboard view becomes the
    // multi-LLM observability surface — no new namespace").
    let v54_sessions: Vec<Value> =
        match state.operational_store.as_ref().and_then(|os| os.pg_pool()) {
            Some(pool) => ff_agent::session_runner::list_sessions(pool, 50)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };

    Json(json!({
        "sessions": sessions,
        "v54_sessions": v54_sessions,
    }))
}

/// `GET /api/agent/v54/session/{id}` — full V54 session detail with
/// step DAG, per-step results, and brain entries. Powers the
/// dashboard's per-session drill-down.
async fn get_v54_session(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"postgres not configured"})),
        ));
    };
    let sid = uuid::Uuid::parse_str(&id).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid uuid: {e}")})),
        )
    })?;
    let mut session = ff_agent::session_runner::get_session(pool, sid)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("{e}")})),
            )
        })?;
    // Append session_brain entries — observability surface.
    let brain = ff_agent::session_runner::brain_list(pool, sid)
        .await
        .unwrap_or_default();
    if let Some(obj) = session.as_object_mut() {
        obj.insert("brain".to_string(), Value::Array(brain));
    }
    Ok(Json(session))
}

// ─── Chat Management Endpoints ──────────────────────────────────────────────

async fn list_chats() -> impl IntoResponse {
    let manager = ff_agent::chat_manager::ChatManager::load().await;
    let chats = manager.list_all();
    Json(json!({ "chats": chats }))
}

async fn create_chat(Json(body): Json<Value>) -> impl IntoResponse {
    let mut manager = ff_agent::chat_manager::ChatManager::load().await;
    let scope_str = body
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("global");
    let scope = match scope_str {
        "global" => ff_agent::scoped_memory::MemoryScope::Global,
        _ => ff_agent::scoped_memory::MemoryScope::Global,
    };

    let chat = manager
        .create(
            body.get("name").and_then(Value::as_str).map(String::from),
            scope,
            "http://192.168.5.102:51000".into(),
            "auto".into(),
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
            ff_agent::chat_manager::ModelSelection::Auto,
        )
        .await;

    Json(json!({ "chat": {
        "id": chat.id,
        "name": chat.name,
        "scope_display": chat.scope.display_name(),
        "status": "active",
        "last_active_at": chat.last_active_at.to_rfc3339(),
        "message_count": 0,
        "preview": "",
        "stack_depth": 0,
        "backlog_count": 0,
    }}))
}

async fn list_chat_folders() -> impl IntoResponse {
    let manager = ff_agent::chat_manager::ChatManager::load().await;
    let folders = manager.list_folders();
    Json(json!({ "folders": folders }))
}

async fn get_chat(Path(id): Path<String>) -> impl IntoResponse {
    let manager = ff_agent::chat_manager::ChatManager::load().await;
    match manager.get(&id) {
        Some(chat) => (StatusCode::OK, Json(json!({ "chat": chat }))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "chat not found" })),
        ),
    }
}

async fn delete_chat(Path(id): Path<String>) -> impl IntoResponse {
    let mut manager = ff_agent::chat_manager::ChatManager::load().await;
    manager.delete(&id).await;
    Json(json!({ "deleted": true }))
}

/// GET /api/brain/status — three-brain memory status.
async fn brain_status() -> impl IntoResponse {
    let cwd = std::env::current_dir().unwrap_or_default();
    let ctx = ff_agent::brain::BrainLoader::load_for_dir(&cwd).await;
    let status = ff_agent::brain::BrainLoadedStatus::from(&ctx);

    Json(json!({
        "project": {
            "name": ctx.project_name,
            "root": ctx.project_root.map(|p| p.to_string_lossy().to_string()),
            "entries": status.project_entries,
            "has_forgefleet_md": ctx.project_forgefleet_md.is_some(),
            "has_context_md": ctx.project_context_md.is_some(),
        },
        "brain": {
            "entries": status.brain_entries,
            "has_brain_md": ctx.brain_md.is_some(),
        },
        "hive": {
            "entries": status.hive_entries,
            "has_hive_md": ctx.hive_md.is_some(),
        },
    }))
}

/// Parse a `http://host:port` URL returned by `ff_api::autoload::ensure_deployed`
/// into `(host, port)`. Returns `None` for malformed URLs.
fn parse_autoload_url(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let rest = rest.split('/').next().unwrap_or(rest);
    let (host, port_str) = rest.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    Some((host.to_string(), port))
}

#[cfg(test)]
mod build_sha_tests {
    /// The /health build_sha is baked by ff-gateway/build.rs. `env!` already
    /// fails the build if the var is missing; this pins that it is non-empty
    /// and, when git was available at build time, a 10-char short SHA (matching
    /// forgefleetd's `(pushed <sha>)`) rather than the "unknown" fallback.
    #[test]
    fn build_sha_is_baked_and_well_formed() {
        let sha = env!("FF_GATEWAY_GIT_SHA");
        assert!(!sha.is_empty(), "FF_GATEWAY_GIT_SHA must be baked");
        if sha != "unknown" {
            assert_eq!(sha.len(), 10, "short SHA should be 10 chars, got {sha:?}");
            assert!(
                sha.chars().all(|c| c.is_ascii_hexdigit()),
                "SHA must be hex, got {sha:?}"
            );
        }
    }

    #[test]
    fn current_build_sha_falls_back_to_compile_time_bake() {
        // Without a runtime injection, /health reports the compile-time bake —
        // never empty (the binary always has SOME identity to report).
        assert!(!super::current_build_sha().is_empty());
    }
}

#[cfg(test)]
mod ci_status_tests {
    use super::{is_terminal_ci_status, map_workflow_run_status};

    #[test]
    fn maps_github_status_conclusion_pairs() {
        assert_eq!(map_workflow_run_status("queued", ""), "queued");
        assert_eq!(map_workflow_run_status("in_progress", ""), "in_progress");
        assert_eq!(map_workflow_run_status("completed", "success"), "success");
        assert_eq!(map_workflow_run_status("completed", "failure"), "failure");
        assert_eq!(
            map_workflow_run_status("completed", "cancelled"),
            "cancelled"
        );
        // completed with an unusual conclusion (timed_out, neutral, …) → completed.
        assert_eq!(
            map_workflow_run_status("completed", "timed_out"),
            "completed"
        );
        // Anything unexpected → unknown.
        assert_eq!(map_workflow_run_status("waiting", ""), "unknown");
    }

    #[test]
    fn terminal_status_classification() {
        for s in ["success", "failure", "cancelled", "completed"] {
            assert!(is_terminal_ci_status(s), "{s} should be terminal");
        }
        for s in ["queued", "in_progress", "unknown", ""] {
            assert!(!is_terminal_ci_status(s), "{s} should be non-terminal");
        }
    }

    /// Models the UPDATE guard `($5 OR status NOT IN (terminal))`: an update is
    /// applied iff the new status is terminal OR the stored one is non-terminal.
    fn update_applies(stored: &str, new: &str) -> bool {
        is_terminal_ci_status(new) || !is_terminal_ci_status(stored)
    }

    #[test]
    fn out_of_order_delivery_never_regresses_a_terminal_run() {
        // Forward progress always allowed.
        assert!(update_applies("queued", "in_progress"));
        assert!(update_applies("in_progress", "success"));
        assert!(update_applies("queued", "queued"));
        // Terminal refinements (both terminal) allowed.
        assert!(update_applies("completed", "success"));
        assert!(update_applies("success", "failure"));
        // THE FIX: a late/retried non-terminal delivery must NOT clobber a
        // finished run.
        assert!(!update_applies("success", "in_progress"));
        assert!(!update_applies("failure", "queued"));
        assert!(!update_applies("cancelled", "in_progress"));
        assert!(!update_applies("completed", "unknown"));
    }
}

#[cfg(test)]
mod node_status_tests {
    use super::{
        HealthStatus, classify_heartbeat_freshness, derive_computer_reachable,
        derive_daemon_joined, derive_node_status, normalize_status_for_runtime,
        parse_heartbeat_timestamp,
    };
    use chrono::{Duration, Utc};

    #[test]
    fn derive_node_status_prefers_live_health_over_db() {
        // Live health wins regardless of db status.
        assert_eq!(
            derive_node_status(Some(&HealthStatus::Healthy), Some("offline")),
            "online"
        );
        assert_eq!(
            derive_node_status(Some(&HealthStatus::Degraded), None),
            "degraded"
        );
        assert_eq!(
            derive_node_status(Some(&HealthStatus::Unreachable), Some("online")),
            "offline"
        );
    }

    #[test]
    fn derive_node_status_db_fallback_and_unknown() {
        assert_eq!(derive_node_status(None, Some("online")), "online");
        // starting/maintenance collapse to degraded.
        assert_eq!(derive_node_status(None, Some("starting")), "degraded");
        assert_eq!(derive_node_status(None, Some("maintenance")), "degraded");
        assert_eq!(derive_node_status(None, Some("offline")), "offline");
        // Case/space-insensitive.
        assert_eq!(derive_node_status(None, Some("  ONLINE ")), "online");
        // No signal / unrecognized → unknown.
        assert_eq!(derive_node_status(None, None), "unknown");
        assert_eq!(derive_node_status(None, Some("weird")), "unknown");
    }

    #[test]
    fn computer_reachability_uses_only_discovery_health() {
        assert_eq!(
            derive_computer_reachable(Some(&HealthStatus::Healthy)),
            Some(true)
        );
        assert_eq!(
            derive_computer_reachable(Some(&HealthStatus::Degraded)),
            Some(true)
        );
        assert_eq!(
            derive_computer_reachable(Some(&HealthStatus::Unreachable)),
            Some(false)
        );
        assert_eq!(derive_computer_reachable(None), None);
    }

    #[test]
    fn daemon_join_requires_an_available_db_snapshot() {
        assert_eq!(derive_daemon_joined(true, true), Some(true));
        assert_eq!(derive_daemon_joined(true, false), Some(false));
        assert_eq!(derive_daemon_joined(false, true), None);
        assert_eq!(derive_daemon_joined(false, false), None);
    }

    #[test]
    fn normalize_status_aliases() {
        for s in ["online", "healthy", "ok", "  OK "] {
            assert_eq!(normalize_status_for_runtime(Some(s.to_string())), "online");
        }
        for s in ["degraded", "starting", "maintenance", "busy"] {
            assert_eq!(
                normalize_status_for_runtime(Some(s.to_string())),
                "degraded"
            );
        }
        for s in ["offline", "unreachable", "down"] {
            assert_eq!(normalize_status_for_runtime(Some(s.to_string())), "offline");
        }
        // None defaults to online; unrecognized → unknown.
        assert_eq!(normalize_status_for_runtime(None), "online");
        assert_eq!(
            normalize_status_for_runtime(Some("garbage".to_string())),
            "unknown"
        );
    }

    fn freshness_at(age_secs: i64) -> String {
        let ts = (Utc::now() - Duration::seconds(age_secs)).to_rfc3339();
        classify_heartbeat_freshness(&ts).0
    }

    #[test]
    fn heartbeat_freshness_thresholds() {
        // <=90s fresh, <=300s stale, else expired (use values away from the
        // exact boundaries to avoid sub-second races).
        assert_eq!(freshness_at(10), "fresh");
        assert_eq!(freshness_at(80), "fresh");
        assert_eq!(freshness_at(120), "stale");
        assert_eq!(freshness_at(280), "stale");
        assert_eq!(freshness_at(400), "expired");

        // A future timestamp clamps age to 0 → fresh, never negative.
        let future = (Utc::now() + Duration::seconds(120)).to_rfc3339();
        let (fresh, age) = classify_heartbeat_freshness(&future);
        assert_eq!(fresh, "fresh");
        assert_eq!(age, Some(0));

        // Unparseable / unknown → ("unknown", None).
        assert_eq!(
            classify_heartbeat_freshness("not-a-timestamp"),
            ("unknown".to_string(), None)
        );
        assert_eq!(
            classify_heartbeat_freshness("unknown"),
            ("unknown".to_string(), None)
        );
    }

    #[test]
    fn parse_heartbeat_timestamp_formats() {
        assert!(parse_heartbeat_timestamp("2026-06-27T04:05:06Z").is_some());
        // Space-separated "YYYY-MM-DD HH:MM:SS[.f]" (Postgres timestamp text).
        assert!(parse_heartbeat_timestamp("2026-06-27 04:05:06").is_some());
        assert!(parse_heartbeat_timestamp("2026-06-27 04:05:06.123").is_some());
        // Empty / unknown / junk → None.
        assert!(parse_heartbeat_timestamp("").is_none());
        assert!(parse_heartbeat_timestamp("   ").is_none());
        assert!(parse_heartbeat_timestamp("UNKNOWN").is_none());
        assert!(parse_heartbeat_timestamp("nonsense").is_none());
    }
}

#[cfg(test)]
mod router_interaction_capture_tests {
    use super::{build_router_interaction, last_user_message_text};
    use serde_json::json;

    #[test]
    fn last_user_message_prefers_final_user_turn() {
        let body = json!({
            "model": "qwen3-30b",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": "first answer"},
                {"role": "user", "content": "second question"}
            ]
        });
        assert_eq!(last_user_message_text(&body), "second question");
        // No messages at all → empty, never panics.
        assert_eq!(last_user_message_text(&json!({"model": "x"})), "");
    }

    #[test]
    fn build_router_interaction_maps_pulse_response() {
        let response = json!({
            "model": "qwen3-30b-a3b",
            "choices": [{"message": {"role": "assistant", "content": "42"}}],
            "usage": {"prompt_tokens": 17, "completion_tokens": 5},
            "_forgefleet_route": {
                "computer": "forge-01",
                "endpoint": "http://10.0.0.5:51001",
                "runtime": "llama-server"
            }
        });
        let rec = build_router_interaction("meaning of life?".to_string(), &response, 321);
        assert_eq!(rec.channel, "gateway-router");
        assert_eq!(rec.request_text, "meaning of life?");
        assert_eq!(rec.response_text, "42");
        assert_eq!(rec.engine.as_deref(), Some("qwen3-30b-a3b"));
        assert_eq!(rec.tokens_in, 17);
        assert_eq!(rec.tokens_out, 5);
        assert_eq!(rec.latency_ms, Some(321));
        assert_eq!(rec.outcome, "ok");
        assert_eq!(rec.worker_name.as_deref(), Some("forge-01"));
        assert_eq!(rec.endpoint.as_deref(), Some("http://10.0.0.5:51001"));
        assert_eq!(rec.route_decision["computer"], "forge-01");
    }

    #[test]
    fn build_router_interaction_defaults_without_route_metadata() {
        // Legacy tier-router responses have no `_forgefleet_route`; the
        // caller backfills worker/endpoint from the backend registry entry.
        let response = json!({
            "choices": [{"message": {"content": "ok"}}],
        });
        let rec = build_router_interaction("hi".to_string(), &response, 5);
        assert_eq!(rec.engine, None);
        assert_eq!(rec.worker_name, None);
        assert_eq!(rec.endpoint, None);
        assert_eq!(rec.tokens_in, 0);
        assert_eq!(rec.tokens_out, 0);
        assert_eq!(rec.route_decision, json!({}));
    }

    /// DB round-trip: insert one captured row through the same
    /// `pg_record_interaction` call the router uses, proving the INSERT
    /// column list matches the LIVE `ff_interactions` schema, then delete it.
    /// Early-returns when no Postgres is configured (`cargo test --lib` in CI
    /// has no database and must never panic).
    #[tokio::test]
    async fn router_interaction_round_trips_against_live_schema() {
        let url = match std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        {
            Ok(u) => u,
            Err(_) => {
                eprintln!(
                    "skipping router interaction DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
                );
                return;
            }
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to Postgres");

        let response = json!({
            "model": "test-model",
            "choices": [{"message": {"content": "router capture round-trip"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4},
            "_forgefleet_route": {"computer": "test-node", "endpoint": "http://test:1"}
        });
        let rec = build_router_interaction("router capture test".to_string(), &response, 42);
        let id = ff_db::pg_record_interaction(&pool, &rec)
            .await
            .expect("insert into live ff_interactions");

        let (channel, worker): (String, Option<String>) =
            sqlx::query_as("SELECT channel, worker_name FROM ff_interactions WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("read back inserted row");
        assert_eq!(channel, "gateway-router");
        assert_eq!(worker.as_deref(), Some("test-node"));

        sqlx::query("DELETE FROM ff_interactions WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("clean up test row");
    }
}

#[cfg(test)]
mod merge_train_webhook_tests {
    use super::*;

    #[test]
    fn parse_merge_group_pr_number_handles_common_formats() {
        assert_eq!(
            parse_merge_group_pr_number("refs/heads/gh-readonly-queue/main/pr-123-deadbeef"),
            Some(123)
        );
        assert_eq!(
            parse_merge_group_pr_number("refs/heads/gh-readonly-queue/feature/x/pr-42"),
            Some(42)
        );
        assert_eq!(
            parse_merge_group_pr_number("refs/heads/gh-readonly-queue/main/pr-0-abc"),
            Some(0)
        );
        assert_eq!(parse_merge_group_pr_number("refs/heads/main"), None);
        assert_eq!(parse_merge_group_pr_number(""), None);
    }

    #[test]
    fn map_merge_train_ci_status_values() {
        assert_eq!(
            map_merge_train_ci_status("completed", "success"),
            "mergeable"
        );
        assert_eq!(
            map_merge_train_ci_status("completed", "skipped"),
            "mergeable"
        );
        assert_eq!(map_merge_train_ci_status("completed", "failure"), "failed");
        assert_eq!(
            map_merge_train_ci_status("completed", "cancelled"),
            "failed"
        );
        assert_eq!(map_merge_train_ci_status("in_progress", ""), "ci_running");
        assert_eq!(map_merge_train_ci_status("queued", ""), "queued");
        assert_eq!(map_merge_train_ci_status("pending", ""), "queued");
        assert_eq!(map_merge_train_ci_status("unknown", ""), "unknown");
    }
}
