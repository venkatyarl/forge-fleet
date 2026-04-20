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
    routing::{get, post},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{debug, info, warn};
use uuid::Uuid;

use ff_agent::agent_loop::{AgentEvent, AgentOutcome, AgentSession, AgentSessionConfig};
use ff_api::openai_compat::{self, validate_request};
use ff_api::registry::BackendRegistry;
use ff_api::router::{ModelRouter, TierRouter, TierRouterConfig, TierTimeouts};
use ff_api::types::ChatCompletionRequest;
use ff_db::sync::LeaderSync;
use ff_db::{OperationalStore, RuntimeRegistryStore, queries};
use ff_discovery::health::HealthStatus;
use ff_discovery::{FleetNode, NodeRegistry};
use ff_mcp::McpServer;
use ff_mcp::transport::HttpTransport;
use tokio_util::sync::CancellationToken;
use ff_observability::metrics::{
    init_prometheus_metrics, metrics_handler, prometheus_metrics_middleware,
};

use crate::{
    embed,
    llm_routing::{self, PulseLlmRouter},
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
    /// Optional legacy Mission Control SQLite database path.
    ///
    /// When absent (or when `operational_store` is Postgres-backed), gateway mounts
    /// the OperationalStore-backed Mission Control API routes instead.
    pub mc_db_path: Option<String>,
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
            mc_db_path: None,
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
    pub web_clients: Arc<DashMap<Uuid, mpsc::UnboundedSender<WsMessage>>>,
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
    /// HTTP client for upstream LLM requests.
    pub http_client: reqwest::Client,
    /// Discovery registry for fleet node status.
    pub discovery_registry: Option<Arc<NodeRegistry>>,
    /// Leader sync coordinator for replication endpoints.
    pub leader_sync: Option<Arc<LeaderSync>>,
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
            http_client: reqwest::Client::new(),
            discovery_registry: None,
            leader_sync: None,
            operational_store: None,
            runtime_registry: None,
            update_state: Arc::new(tokio::sync::RwLock::new(UpdateRolloutState::default())),
            ws_hub: crate::websocket::WsHub::new(),
            agent_sessions: Arc::new(DashMap::new()),
        }
    }

    pub fn broadcast_json(&self, payload: Value) {
        let text = payload.to_string();
        let mut disconnected = Vec::new();

        for entry in self.web_clients.iter() {
            if entry
                .value()
                .send(WsMessage::Text(text.clone().into()))
                .is_err()
            {
                disconnected.push(*entry.key());
            }
        }

        for key in disconnected {
            self.web_clients.remove(&key);
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
            .unwrap_or_else(|_| "redis://127.0.0.1:6380/".to_string());
        match PulseLlmRouter::new(&redis_url) {
            Ok(pr) => {
                state.pulse_router = Some(Arc::new(pr));
                info!(redis_url = %redis_url, "pulse-backed LLM router initialized");
            }
            Err(e) => {
                warn!(redis_url = %redis_url, error = %e, "failed to construct PulseLlmRouter; tier-router fallback only");
            }
        }

        // Build a proper HTTP client for upstream proxying
        state.http_client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let state = Arc::new(state);
        Ok(Self { config, state })
    }

    pub fn app(&self) -> Router {
        build_router(self.state.clone(), self.config.mc_db_path.as_deref())
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.config.bind_addr)
            .await
            .with_context(|| format!("failed to bind ff-gateway on {}", self.config.bind_addr))?;

        info!(address = %listener.local_addr()?, "ff-gateway listening");
        axum::serve(listener, self.app()).await?;
        Ok(())
    }

    pub fn shared_state(&self) -> Arc<GatewayState> {
        self.state.clone()
    }
}

pub async fn run(config: GatewayConfig) -> anyhow::Result<()> {
    GatewayServer::new(config)?.run().await
}

// ─── Router ──────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<GatewayState>, mc_db_path: Option<&str>) -> Router {
    let mut app = Router::new()
        // Core gateway routes
        .route("/health", get(health))
        .route("/ws", get(websocket_upgrade))
        .route("/api/messages", post(incoming_message_http))
        .route("/api/messages/raw", post(incoming_message_raw_http))
        .route("/api/send", post(outgoing_message_http))
        .route("/api/webhook", post(webhook::webhook_http_handler))
        .route("/embed/widget.js", get(embed::widget_js_handler))
        .route("/dashboard", get(dashboard))
        // ─── Fleet integration routes ────────────────────────────────
        .route("/api/fleet/status", get(fleet_status))
        .route("/api/status", get(fleet_status))
        .route("/api/fleet/enroll", post(fleet_enroll))
        .route("/api/fleet/heartbeat", post(fleet_heartbeat))
        // Onboarding (see crates/ff-gateway/src/onboard.rs + plan §§3–3h)
        .route("/onboard/bootstrap.sh", get(crate::onboard::bootstrap_script))
        .route("/onboard/bootstrap.ps1", get(crate::onboard::bootstrap_script_ps1))
        .route("/api/fleet/self-enroll", post(crate::onboard::self_enroll))
        .route("/api/fleet/enrollment-progress", post(crate::onboard::enrollment_progress))
        .route("/api/fleet/check-ip", get(crate::onboard::check_ip))
        .route("/api/fleet/check-tcp", get(crate::onboard::check_tcp))
        .route("/api/fleet/tooling", get(crate::onboard::get_fleet_tooling))
        // Virtual Brain API (see crates/ff-gateway/src/brain_api.rs)
        .route("/api/brain/threads", get(crate::brain_api::list_threads))
        .route("/api/brain/threads", post(crate::brain_api::create_thread))
        .route("/api/brain/threads/{slug}/messages", get(crate::brain_api::thread_messages))
        .route("/api/brain/threads/{slug}/message", post(crate::brain_api::send_thread_message))
        .route("/api/brain/attach", post(crate::brain_api::attach_to_thread))
        .route("/api/brain/candidates", get(crate::brain_api::list_candidates))
        .route("/api/brain/candidates/{id}", post(crate::brain_api::update_candidate))
        .route("/api/brain/graph", get(crate::brain_api::vault_graph))
        .route("/api/brain/vault/search", get(crate::brain_api::vault_search))
        .route("/api/brain/reminders", get(crate::brain_api::list_reminders))
        .route("/api/brain/reminders", post(crate::brain_api::create_reminder))
        .route("/api/brain/whoami", get(crate::brain_api::whoami))
        .route("/api/brain/stack/{thread_slug}", get(crate::brain_api::stack_list))
        .route("/api/brain/stack/{thread_slug}/push", post(crate::brain_api::stack_push))
        .route("/api/brain/stack/{thread_slug}/pop", post(crate::brain_api::stack_pop))
        .route("/api/brain/backlog/{project}", get(crate::brain_api::backlog_list))
        .route("/api/brain/backlog/{project}/add", post(crate::brain_api::backlog_add))
        .route("/api/brain/backlog/{project}/complete", post(crate::brain_api::backlog_complete))
        .route("/api/fleet/secret-peek", get(crate::onboard::secret_peek))
        .route("/api/fleet/mesh-check", get(crate::onboard::get_mesh_check))
        .route("/api/fleet/verify-node", post(crate::onboard::post_verify_node))
        .route("/api/fleet/deferred", get(crate::onboard::list_deferred))
        .route("/api/fleet/deferred/{id}/promote", post(crate::onboard::promote_deferred))
        .route(
            "/api/transports/telegram/status",
            get(telegram_transport_status),
        )
        .route("/api/fleet/nodes/{id}", get(fleet_node_detail))
        .route("/api/config", get(get_config).post(update_config))
        .route("/api/config/reload-status", get(config_reload_status))
        .route("/api/settings/runtime", get(settings_runtime))
        .route("/api/brain/status", get(brain_status))
        .route("/api/brain/search", get(brain_search))
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
        .route("/v1/models", get(list_models))
        // ─── Replication routes ──────────────────────────────────────
        .route("/api/fleet/replicate/snapshot", post(replicate_snapshot))
        .route("/api/fleet/replicate/sequence", get(replicate_sequence))
        .route("/api/fleet/replicate/pull", post(replicate_pull))
        // ─── Distributed tracing routes ──────────────────────────────
        .route("/api/traces/recent", get(traces_recent))
        // ─── Agent session routes ───────────────────────────────────
        .route("/api/agent/session", post(create_agent_session))
        .route("/api/agent/session/{id}/message", post(agent_session_message))
        .route("/api/agent/session/{id}/cancel", post(cancel_agent_session))
        .route("/api/agent/session/{id}/status", get(agent_session_status))
        .route("/api/agent/sessions", get(list_agent_sessions))
        // ─── Pulse v2 dashboard routes ──────────────────────────────
        .route("/api/fleet/computers", get(crate::pulse_api::list_computers))
        .route("/api/fleet/members", get(crate::pulse_api::list_members))
        .route("/api/fleet/leader", get(crate::pulse_api::get_leader))
        .route("/api/fleet/health", get(crate::pulse_api::fleet_health))
        .route("/api/llm/servers", get(crate::pulse_api::llm_servers))
        .route("/api/software/computers", get(crate::pulse_api::software_computers))
        .route("/api/software/drift", get(crate::pulse_api::software_drift))
        .route("/api/projects", get(crate::pulse_api::list_projects))
        .route("/api/projects/{id}/branches", get(crate::pulse_api::project_branches))
        .route("/api/pm/work-items", get(crate::pulse_api::list_work_items))
        .route("/api/alerts/policies", get(crate::pulse_api::alert_policies))
        .route("/api/alerts/events", get(crate::pulse_api::alert_events))
        .route("/api/metrics/{computer}/history", get(crate::pulse_api::metrics_history))
        .route("/api/ha/status", get(crate::pulse_api::ha_status))
        .route("/api/docker/projects", get(crate::pulse_api::docker_projects))
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
    // Priority:
    // 1) OperationalStore-backed router when a Postgres backend is active
    //    (or when no sqlite MC DB path was provided).
    // 2) Legacy sqlite mission-control router when an explicit mc_db_path exists.
    let mut mounted_mc_routes = false;

    if let Some(store) = state.operational_store.clone()
        && (matches!(store, OperationalStore::Postgres(_)) || mc_db_path.is_none())
    {
        let mc_routes = ff_mc::operational_api::mc_router_operational(store);
        app = app.merge(mc_routes.with_state(()));
        mounted_mc_routes = true;
        info!("mission control API mounted at /api/mc/* (operational store backend)");
    }

    if !mounted_mc_routes {
        if let Some(db_path) = mc_db_path {
            match ff_mc::McDb::open(db_path) {
                Ok(mc_db) => {
                    let mc_routes = ff_mc::api::mc_router(mc_db);
                    app = app.merge(mc_routes.with_state(()));
                    info!("mission control API mounted at /api/mc/* (sqlite backend)");
                }
                Err(err) => {
                    warn!(error = %err, "failed to open mission control DB; MC routes not mounted");
                }
            }
        } else {
            warn!("mission control API not mounted: no sqlite path or operational store available");
        }
    }

    // Initialize Prometheus metrics (idempotent).
    init_prometheus_metrics();

    app.route("/metrics", get(serve_prometheus_metrics))
        .fallback(crate::static_files::serve_dashboard)
        .layer(middleware::from_fn(crate::middleware::trace_id_middleware))
        .layer(middleware::from_fn(prometheus_metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ─── Health ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    uptime_epoch: i64,
    ws_clients: usize,
    inbound_buffered: usize,
    outbound_buffered: usize,
    telegram_transport: TelegramTransportStatus,
}

#[derive(Debug, Clone, Serialize)]
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

impl Default for TelegramTransportStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            running: false,
            allowed_chat_ids: Vec::new(),
            polling_interval_secs: None,
            polling_timeout_secs: None,
            media_download_dir: None,
            started_at: None,
            last_update_id: None,
            last_message_at: None,
            last_error: None,
        }
    }
}

async fn health(State(state): State<Arc<GatewayState>>) -> Json<HealthResponse> {
    let telegram_transport = telegram_transport_snapshot(state.as_ref()).await;

    Json(HealthResponse {
        status: "ok",
        service: "ff-gateway",
        version: ff_core::VERSION,
        uptime_epoch: Utc::now().timestamp(),
        ws_clients: state.web_clients.len(),
        inbound_buffered: state.inbound_messages.len(),
        outbound_buffered: state.outbound_messages.len(),
        telegram_transport,
    })
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
                "node": row.node_name,
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
struct RuntimeNodeSnapshot {
    node_id: String,
    hostname: String,
    ips: Vec<String>,
    role: String,
    status: String,
    last_heartbeat: String,
    heartbeat_age_secs: i64,
    resources: Value,
    services: Value,
    models: Vec<String>,
    capabilities: Value,
}

#[derive(Debug, Clone, Default)]
struct DbFleetSnapshot {
    nodes_by_name: HashMap<String, DbNodeSnapshot>,
    nodes_by_host: HashMap<String, DbNodeSnapshot>,
    runtime_nodes: Vec<RuntimeNodeSnapshot>,
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

/// GET /api/fleet/nodes/{id} — direct node detail endpoint used by dashboard.
async fn fleet_node_detail(
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

fn heartbeat_freshness_bucket(age_seconds: Option<i64>) -> String {
    match age_seconds {
        Some(age) if age < 60 => "fresh".to_string(),
        Some(age) if age < 180 => "warming".to_string(),
        Some(age) if age < 600 => "stale".to_string(),
        Some(_) => "expired".to_string(),
        None => "unknown".to_string(),
    }
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
    if matches!(&policy, ff_core::config::EnrollmentEnforcement::MisconfiguredRequired) {
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
                    addresses_json: serde_json::to_string(&ips).unwrap_or_else(|_| "[]".to_string()),
                    capabilities_json: serde_json::to_string(&capabilities)
                        .unwrap_or_else(|_| "{}".to_string()),
                    metadata_json: serde_json::to_string(&normalize_object(payload.metadata.clone()))
                        .unwrap_or_else(|_| "{}".to_string()),
                };
                let _ = runtime_registry.insert_enrollment_event(&event).await;

                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"message": "invalid enrollment token", "type": "unauthorized"}})),
                ));
            }
        }
        ff_core::config::EnrollmentEnforcement::MisconfiguredRequired => unreachable!("handled above"),
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

    let leader_sequence = state
        .leader_sync
        .as_ref()
        .map(|sync| sync.current_sequence());

    let fleet_config = if let Some(cfg_lock) = &state.fleet_config {
        Some(cfg_lock.read().await.clone())
    } else {
        None
    };

    let db_snapshot = load_db_fleet_snapshot(state).await;

    let leader_hint = state
        .discovery_registry
        .as_ref()
        .and_then(|registry| registry.current_leader())
        .or_else(|| {
            db_snapshot.as_ref().and_then(|snapshot| {
                snapshot
                    .runtime_nodes
                    .iter()
                    .find(|node| node.role.eq_ignore_ascii_case("leader"))
                    .map(|node| node.hostname.clone())
            })
        });

    Ok(assemble_fleet_status_payload(
        nodes,
        leader_hint,
        fleet_config.as_ref(),
        db_snapshot.as_ref(),
        leader_sequence,
    ))
}

fn assemble_fleet_status_payload(
    nodes: Vec<FleetNode>,
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

        let view = build_fleet_node_view(
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

    if let Some(snapshot) = db_snapshot {
        for runtime in &snapshot.runtime_nodes {
            let hostname_key = runtime.hostname.clone();
            let runtime_ip = runtime
                .ips
                .iter()
                .find(|ip| !ip.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());

            let already_present = seen_config_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&runtime.hostname))
                || seen_ips.contains(&runtime_ip);

            if already_present {
                continue;
            }

            let workload = snapshot
                .workloads
                .get(&runtime.hostname)
                .or_else(|| snapshot.workloads.get(&runtime_ip));

            node_views.push(build_runtime_node_view(
                runtime,
                leader_hint.as_deref(),
                workload,
                leader_sequence,
                snapshot.replication_sequence,
            ));

            seen_config_names.insert(hostname_key);
            if runtime_ip != "unknown" {
                seen_ips.insert(runtime_ip);
            }
        }
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

fn build_fleet_node_view(
    node: &FleetNode,
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

    let is_leader = if let Some(leader) = leader_hint {
        node.config_name.as_deref() == Some(leader)
            || node.hostname.as_deref() == Some(leader)
            || display_name == leader
    } else {
        role == "leader" || role == "gateway"
    };

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

fn build_runtime_node_view(
    runtime: &RuntimeNodeSnapshot,
    leader_hint: Option<&str>,
    workload: Option<&NodeWorkloadAggregate>,
    leader_sequence: Option<u64>,
    local_replication_sequence: Option<u64>,
) -> FleetNodeView {
    let first_ip = runtime
        .ips
        .iter()
        .find(|ip| !ip.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let role = role_to_string_runtime(&runtime.role);
    let is_leader = if let Some(leader) = leader_hint {
        runtime.hostname.eq_ignore_ascii_case(leader)
            || runtime.ips.iter().any(|ip| ip.eq_ignore_ascii_case(leader))
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

    let status = match runtime.status.trim().to_ascii_lowercase().as_str() {
        "online" | "degraded" | "offline" => runtime.status.trim().to_ascii_lowercase(),
        _ => "unknown".to_string(),
    };

    let health = match status.as_str() {
        "online" => "healthy".to_string(),
        "degraded" => "degraded".to_string(),
        "offline" => "offline".to_string(),
        _ => "unknown".to_string(),
    };

    let mut heartbeat = heartbeat_view(runtime.last_heartbeat.clone(), "db.runtime_heartbeat");
    if heartbeat.age_seconds.is_none() && runtime.heartbeat_age_secs >= 0 {
        heartbeat.age_seconds = Some(runtime.heartbeat_age_secs);
        heartbeat.freshness = heartbeat_freshness_bucket(heartbeat.age_seconds);
    }

    let models_loaded = runtime.models.clone();
    let models_loaded_state = if models_loaded.is_empty() {
        "unreported".to_string()
    } else {
        "reported".to_string()
    };

    let open_ports = runtime
        .resources
        .pointer("/open_ports")
        .and_then(Value::as_array)
        .map(|ports| {
            ports
                .iter()
                .filter_map(Value::as_u64)
                .filter(|port| *port <= u16::MAX as u64)
                .map(|port| port as u16)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let cpu = extract_json_string(
        &runtime.resources,
        &["/cpu", "/resources/cpu", "/hardware/cpu"],
    )
    .unwrap_or_else(|| "unknown".to_string());
    let ram = extract_json_string(
        &runtime.resources,
        &["/ram", "/memory", "/resources/ram", "/hardware/memory"],
    )
    .unwrap_or_else(|| "unknown".to_string());
    let gpu = extract_json_string(
        &runtime.resources,
        &["/gpu", "/resources/gpu", "/hardware/gpu"],
    )
    .unwrap_or_else(|| "unknown".to_string());

    let cpu_cores = runtime
        .resources
        .pointer("/cpu_cores")
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    let memory_gib = runtime
        .resources
        .pointer("/memory_gib")
        .and_then(Value::as_u64);

    let service_version = extract_json_string(
        &runtime.resources,
        &[
            "/service_version",
            "/serviceVersion",
            "/service/version",
            "/build/version",
            "/version",
        ],
    )
    .unwrap_or_else(|| "unreported".to_string());

    let replication_detail = extract_json_string(
        &runtime.capabilities,
        &[
            "/replication_state",
            "/replication/state",
            "/replication/health",
        ],
    )
    .unwrap_or_else(|| "runtime heartbeat telemetry".to_string());

    let replication_mode = if is_leader {
        "leader".to_string()
    } else if leader_hint.is_some() {
        "follower".to_string()
    } else {
        "unknown".to_string()
    };

    let replication_state = FleetReplicationView {
        mode: replication_mode,
        sequence: if is_leader {
            leader_sequence.or(local_replication_sequence)
        } else {
            local_replication_sequence
        },
        health: health.clone(),
        detail: replication_detail,
    };

    let current_workload = build_workload_view(workload, true);

    let model_rows = models_loaded
        .iter()
        .map(|id| FleetNodeModelView {
            id: id.clone(),
            name: id.clone(),
            node: Some(runtime.hostname.clone()),
            status: if status == "online" {
                "healthy".to_string()
            } else {
                status.clone()
            },
        })
        .collect::<Vec<_>>();

    let mut runtime_provenance = vec!["db.fleet_node_runtime".to_string()];
    if runtime
        .services
        .as_array()
        .is_some_and(|services| !services.is_empty())
    {
        runtime_provenance.push("heartbeat.services".to_string());
    }

    FleetNodeView {
        id: runtime.node_id.clone(),
        name: runtime.hostname.clone(),
        hostname: Some(runtime.hostname.clone()),
        ip: first_ip,
        open_ports: open_ports.clone(),
        status,
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
        source_kind: "runtime/db".to_string(),
        seeded_from_config: false,
        runtime_enrolled: true,
        runtime_provenance,
        last_heartbeat: heartbeat.value,
        heartbeat_source: heartbeat.source,
        heartbeat_freshness: heartbeat.freshness,
        heartbeat_age_seconds: heartbeat.age_seconds,
        service_version,
        replication_state,
        current_workload: current_workload.clone(),
        hardware: FleetNodeHardwareView {
            discovered_at: runtime.last_heartbeat.clone(),
            last_seen: runtime.last_heartbeat.clone(),
            open_ports,
            cpu,
            ram,
            gpu,
        },
        models: model_rows,
        metrics: FleetNodeMetricsView {
            latency_ms: None,
            tcp_ok: None,
            http_ok: None,
            checked_at: Some(runtime.last_heartbeat.clone()),
            active_tasks: current_workload.active_tasks,
        },
    }
}

fn role_to_string_runtime(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "leader" | "worker" | "gateway" | "builder" => raw.trim().to_ascii_lowercase(),
        _ => "worker".to_string(),
    }
}

fn is_runtime_enrolled(node: &FleetNode, db_node: Option<&DbNodeSnapshot>) -> bool {
    node.config_name.is_none()
        || node.health.is_some()
        || node.hardware.is_some()
        || !node.models.is_empty()
        || db_node.is_some()
}

fn derive_runtime_provenance(
    node: &FleetNode,
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

fn derive_node_resources(
    node: &FleetNode,
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
    node: &FleetNode,
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
    node: &FleetNode,
    db_node: Option<&DbNodeSnapshot>,
) -> (Vec<String>, String) {
    let mut ids = Vec::new();

    for model in &node.models {
        if !model.id.trim().is_empty() {
            ids.push(model.id.clone());
        }
    }

    if ids.is_empty() {
        if let Some(db_models) = db_node.map(|db| db.models.clone()) {
            ids = db_models;
        }
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
    node: &FleetNode,
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
            .map(|cores| format!("{} cores (configured)", cores));
        let ram = node_cfg
            .effective_ram_gb()
            .map(|gb| format!("{} GB (configured)", gb));

        let gpu = node_cfg
            .resources
            .as_ref()
            .and_then(|res| res.vram_gb)
            .map(|vram| format!("{} GB VRAM (configured)", vram))
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

fn parse_db_node_snapshot(row: &queries::NodeRow) -> DbNodeSnapshot {
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

    let runtime_result = if let Some(runtime_registry) = &state.runtime_registry {
        runtime_registry.list_runtime_nodes().await
    } else {
        Ok(Vec::new())
    };

    match runtime_result {
        Ok(runtime_rows) => {
            snapshot.runtime_nodes = runtime_rows
                .into_iter()
                .map(|row| RuntimeNodeSnapshot {
                    node_id: row.node_id,
                    hostname: row.hostname,
                    ips: serde_json::from_str::<Vec<String>>(&row.ips_json).unwrap_or_default(),
                    role: row.role,
                    status: row.derived_status,
                    last_heartbeat: row.last_heartbeat,
                    heartbeat_age_secs: row.heartbeat_age_secs,
                    resources: serde_json::from_str::<Value>(&row.resources_json)
                        .unwrap_or_else(|_| json!({})),
                    services: serde_json::from_str::<Value>(&row.services_json)
                        .unwrap_or_else(|_| json!([])),
                    models: serde_json::from_str::<Vec<String>>(&row.models_json)
                        .unwrap_or_default(),
                    capabilities: serde_json::from_str::<Value>(&row.capabilities_json)
                        .unwrap_or_else(|_| json!({})),
                })
                .collect();

            if !snapshot.runtime_nodes.is_empty() {
                has_any_data = true;
            }
        }
        Err(err) => {
            warn!(error = %err, "failed to load runtime registry snapshot");
        }
    }

    if has_any_data { Some(snapshot) } else { None }
}

#[cfg(test)]
mod fleet_visibility_tests {
    use super::*;
    use chrono::{Duration, Utc};
    use ff_core::Role;
    use ff_core::config::{FleetConfig, NodeConfig};
    use std::net::{IpAddr, Ipv4Addr};
    use uuid::Uuid;

    #[test]
    fn aggregate_workloads_counts_active_tasks_per_node() {
        let tasks = vec![
            (
                "running",
                vec![
                    queries::TaskRow {
                        id: "t1".to_string(),
                        kind: "shell".to_string(),
                        payload_json: "{}".to_string(),
                        status: "running".to_string(),
                        assigned_node: Some("alpha".to_string()),
                        priority: 0,
                        created_at: "now".to_string(),
                        started_at: None,
                        completed_at: None,
                    },
                    queries::TaskRow {
                        id: "t2".to_string(),
                        kind: "shell".to_string(),
                        payload_json: "{}".to_string(),
                        status: "running".to_string(),
                        assigned_node: Some("alpha".to_string()),
                        priority: 0,
                        created_at: "now".to_string(),
                        started_at: None,
                        completed_at: None,
                    },
                ],
            ),
            (
                "review",
                vec![queries::TaskRow {
                    id: "t3".to_string(),
                    kind: "shell".to_string(),
                    payload_json: "{}".to_string(),
                    status: "review".to_string(),
                    assigned_node: Some("beta".to_string()),
                    priority: 0,
                    created_at: "now".to_string(),
                    started_at: None,
                    completed_at: None,
                }],
            ),
        ];

        let workloads = aggregate_workloads(&tasks);
        assert_eq!(workloads.get("alpha").map(|w| w.active_tasks), Some(2));
        assert_eq!(workloads.get("beta").map(|w| w.active_tasks), Some(1));
        assert_eq!(
            workloads
                .get("alpha")
                .and_then(|w| w.status_breakdown.get("running"))
                .copied(),
            Some(2)
        );
    }

    #[test]
    fn node_view_marks_unreported_fields_when_data_missing() {
        let node = FleetNode {
            id: Uuid::new_v4(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 5, 42)),
            hostname: None,
            config_name: Some("alpha".to_string()),
            election_priority: Some(10),
            api_port: Some(51801),
            open_ports: vec![51801],
            discovered_at: Utc::now(),
            last_seen: Utc::now(),
            hardware: None,
            health: None,
            models: vec![],
        };

        let view =
            build_fleet_node_view(&node, Some("leader"), None, None, None, false, None, None);

        assert_eq!(view.status, "unknown");
        assert_eq!(view.models_loaded_state, "unreported");
        assert_eq!(view.service_version, "unreported");
        assert_eq!(view.current_workload.status, "unreported");
        assert_eq!(view.replication_state.mode, "follower");
    }

    #[test]
    fn fleet_payload_serializes_required_visibility_fields() {
        let node = FleetNode {
            id: Uuid::new_v4(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 5, 100)),
            hostname: Some("alpha.local".to_string()),
            config_name: Some("alpha".to_string()),
            election_priority: Some(1),
            api_port: Some(51800),
            open_ports: vec![51800],
            discovered_at: Utc::now(),
            last_seen: Utc::now(),
            hardware: None,
            health: None,
            models: vec![],
        };

        let payload =
            assemble_fleet_status_payload(vec![node], Some("alpha".to_string()), None, None, None);

        let json = serde_json::to_value(payload).expect("payload should serialize");
        let first_node = &json["nodes"][0];

        for key in [
            "name",
            "ip",
            "role",
            "status",
            "leader_state",
            "cpu",
            "ram",
            "cpu_cores",
            "memory_gib",
            "gpu",
            "models_loaded",
            "source_kind",
            "runtime_enrolled",
            "runtime_provenance",
            "last_heartbeat",
            "heartbeat_source",
            "heartbeat_freshness",
            "heartbeat_age_seconds",
            "service_version",
            "replication_state",
            "current_workload",
        ] {
            assert!(first_node.get(key).is_some(), "missing field: {key}");
        }
    }

    #[test]
    fn heartbeat_prefers_runtime_registry_over_db_snapshot() {
        let runtime_checked_at = Utc::now();
        let node = FleetNode {
            id: Uuid::new_v4(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 5, 55)),
            hostname: Some("alpha.local".to_string()),
            config_name: Some("alpha".to_string()),
            election_priority: Some(1),
            api_port: Some(51800),
            open_ports: vec![51800],
            discovered_at: Utc::now() - Duration::minutes(5),
            last_seen: Utc::now() - Duration::seconds(20),
            hardware: None,
            health: Some(ff_discovery::health::HealthCheckResult {
                name: "alpha".to_string(),
                host: "192.168.5.55".to_string(),
                port: 51800,
                checked_at: runtime_checked_at,
                latency_ms: 8,
                tcp_ok: true,
                http_ok: Some(true),
                http_status: Some(200),
                status: HealthStatus::Healthy,
                error: None,
            }),
            models: vec![],
        };

        let db_node = DbNodeSnapshot {
            role: Some("worker".to_string()),
            status: Some("online".to_string()),
            last_heartbeat: Some((Utc::now() - Duration::hours(2)).to_rfc3339()),
            models: vec![],
            service_version: None,
            replication_state: None,
        };

        let view = build_fleet_node_view(
            &node,
            Some("alpha"),
            None,
            Some(&db_node),
            None,
            true,
            None,
            None,
        );

        assert_eq!(view.last_heartbeat, runtime_checked_at.to_rfc3339());
        assert_eq!(view.heartbeat_source, "registry.health");
        assert_eq!(view.heartbeat_freshness, "fresh");
        assert!(view.runtime_enrolled);
        assert_eq!(view.source_kind, "enrolled/live");
    }

    #[test]
    fn runtime_metadata_overrides_static_role_hint() {
        let node = FleetNode {
            id: Uuid::new_v4(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 5, 56)),
            hostname: Some("alpha.local".to_string()),
            config_name: Some("alpha".to_string()),
            election_priority: Some(1),
            api_port: Some(51800),
            open_ports: vec![51800],
            discovered_at: Utc::now() - Duration::minutes(30),
            last_seen: Utc::now() - Duration::minutes(1),
            hardware: None,
            health: None,
            models: vec![],
        };

        let config_hint = ConfigNodeHints {
            role: Some("leader".to_string()),
            cpu: None,
            ram: None,
            cpu_cores: None,
            memory_gib: None,
            gpu: None,
        };

        let db_node = DbNodeSnapshot {
            role: Some("worker".to_string()),
            status: Some("online".to_string()),
            last_heartbeat: Some(Utc::now().to_rfc3339()),
            models: vec![],
            service_version: Some("1.2.3".to_string()),
            replication_state: None,
        };

        let view = build_fleet_node_view(
            &node,
            None,
            Some(&config_hint),
            Some(&db_node),
            None,
            true,
            None,
            None,
        );

        assert_eq!(view.role, "worker");
        assert!(!view.is_leader);
    }

    #[test]
    fn payload_keeps_seed_nodes_but_prefers_runtime_for_duplicates() {
        let mut config = FleetConfig::default();
        config.nodes.insert(
            "alpha".to_string(),
            NodeConfig {
                ip: "192.168.5.10".to_string(),
                role: Role::Gateway,
                port: Some(51800),
                ..Default::default()
            },
        );
        config.nodes.insert(
            "beta".to_string(),
            NodeConfig {
                ip: "192.168.5.11".to_string(),
                role: Role::Worker,
                port: Some(51801),
                ..Default::default()
            },
        );

        let runtime_alpha = FleetNode {
            id: Uuid::new_v4(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 5, 10)),
            hostname: Some("alpha.local".to_string()),
            config_name: Some("alpha".to_string()),
            election_priority: Some(1),
            api_port: Some(51800),
            open_ports: vec![51800],
            discovered_at: Utc::now() - Duration::minutes(2),
            last_seen: Utc::now() - Duration::seconds(10),
            hardware: None,
            health: Some(ff_discovery::health::HealthCheckResult {
                name: "alpha".to_string(),
                host: "192.168.5.10".to_string(),
                port: 51800,
                checked_at: Utc::now(),
                latency_ms: 12,
                tcp_ok: true,
                http_ok: Some(true),
                http_status: Some(200),
                status: HealthStatus::Healthy,
                error: None,
            }),
            models: vec![],
        };

        let payload = assemble_fleet_status_payload(
            vec![runtime_alpha],
            Some("alpha".to_string()),
            Some(&config),
            None,
            None,
        );

        assert_eq!(payload.nodes.len(), 2);
        assert_eq!(payload.summary.enrolled_nodes, 1);
        assert_eq!(payload.summary.seed_nodes, 1);

        let alpha_count = payload.nodes.iter().filter(|n| n.name == "alpha").count();
        assert_eq!(
            alpha_count, 1,
            "runtime node should replace duplicate static seed"
        );

        let alpha = payload
            .nodes
            .iter()
            .find(|node| node.name == "alpha")
            .expect("runtime alpha node present");
        assert_eq!(alpha.source_kind, "enrolled/live");
        assert!(alpha.runtime_enrolled);

        let beta = payload
            .nodes
            .iter()
            .find(|node| node.name == "beta")
            .expect("seed beta node present");
        assert_eq!(beta.source_kind, "seed/static");
        assert!(beta.seeded_from_config);
        assert!(!beta.runtime_enrolled);
        assert_eq!(beta.last_heartbeat, "unknown");
    }

    #[tokio::test]
    async fn enroll_endpoint_rejects_invalid_token() {
        use ff_db::{DbPool, DbPoolConfig, run_migrations};

        let pool = DbPool::open(DbPoolConfig::in_memory()).expect("in-memory pool");
        pool.with_conn(|conn| {
            run_migrations(conn)?;
            Ok(())
        })
        .await
        .expect("migrations should apply");

        let mut state = GatewayState::new(MessageRouter::default());
        state.operational_store = Some(OperationalStore::sqlite(pool.clone()));
        state.runtime_registry = Some(RuntimeRegistryStore::sqlite(pool.clone()));

        let mut cfg = FleetConfig::default();
        cfg.enrollment.shared_secret = Some("top-secret".to_string());
        state.fleet_config = Some(Arc::new(tokio::sync::RwLock::new(cfg)));

        let payload = FleetEnrollPayload {
            node_id: "enroll-node-1".to_string(),
            hostname: Some("enroll-1.local".to_string()),
            ip: Some("10.0.0.20".to_string()),
            ips: vec![],
            role: Some("worker".to_string()),
            token: Some("wrong-token".to_string()),
            heartbeat_at: None,
            resources: json!({}),
            services: json!([]),
            models: vec![],
            capabilities: json!({}),
            service_version: Some("0.1.0".to_string()),
            metadata: json!({}),
            stale: None,
        };

        let result = fleet_enroll(State(Arc::new(state)), HeaderMap::new(), Json(payload)).await;

        let Err((status, _)) = result else {
            panic!("expected enrollment rejection");
        };
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn enroll_endpoint_accepts_and_persists_runtime_row() {
        use ff_db::{DbPool, DbPoolConfig, run_migrations};

        let pool = DbPool::open(DbPoolConfig::in_memory()).expect("in-memory pool");
        pool.with_conn(|conn| {
            run_migrations(conn)?;
            Ok(())
        })
        .await
        .expect("migrations should apply");

        let mut state = GatewayState::new(MessageRouter::default());
        state.operational_store = Some(OperationalStore::sqlite(pool.clone()));
        state.runtime_registry = Some(RuntimeRegistryStore::sqlite(pool.clone()));

        let mut cfg = FleetConfig::default();
        cfg.enrollment.shared_secret = Some("top-secret".to_string());
        cfg.enrollment.default_role = Some("worker".to_string());
        state.fleet_config = Some(Arc::new(tokio::sync::RwLock::new(cfg)));

        let payload = FleetEnrollPayload {
            node_id: "enroll-node-2".to_string(),
            hostname: Some("enroll-2.local".to_string()),
            ip: Some("10.0.0.21".to_string()),
            ips: vec!["192.168.1.21".to_string()],
            role: Some("builder".to_string()),
            token: None,
            heartbeat_at: None,
            resources: json!({"cpu": "8 cores"}),
            services: json!(["gateway"]),
            models: vec!["qwen".to_string()],
            capabilities: json!({"shell": true}),
            service_version: Some("0.1.0".to_string()),
            metadata: json!({"source": "test"}),
            stale: Some(FleetHeartbeatStaleness {
                degraded_after_secs: Some(45),
                offline_after_secs: Some(120),
            }),
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-fleet-enrollment-token",
            header::HeaderValue::from_static("top-secret"),
        );

        let Json(response) = fleet_enroll(State(Arc::new(state)), headers, Json(payload))
            .await
            .expect("enrollment should succeed");

        assert_eq!(response["status"], "ok");
        assert_eq!(response["enrollment"]["accepted"], true);
        assert_eq!(response["enrollment"]["node_id"], "enroll-node-2");

        let runtime_rows = pool
            .with_conn(|conn| queries::list_fleet_node_runtime(conn))
            .await
            .expect("query runtime rows");
        assert_eq!(runtime_rows.len(), 1);
        assert_eq!(runtime_rows[0].node_id, "enroll-node-2");

        let events = pool
            .with_conn(|conn| queries::list_fleet_enrollment_events(conn, 10))
            .await
            .expect("query enrollment events");
        assert!(!events.is_empty());
        assert_eq!(events[0].outcome, "accepted");
    }

    #[tokio::test]
    async fn heartbeat_endpoint_persists_runtime_registry_row() {
        use ff_db::{DbPool, DbPoolConfig, run_migrations};

        let pool = DbPool::open(DbPoolConfig::in_memory()).expect("in-memory pool");
        pool.with_conn(|conn| {
            run_migrations(conn)?;
            Ok(())
        })
        .await
        .expect("migrations should apply");

        let mut state = GatewayState::new(MessageRouter::default());
        state.operational_store = Some(OperationalStore::sqlite(pool.clone()));
        state.runtime_registry = Some(RuntimeRegistryStore::sqlite(pool.clone()));
        let state = Arc::new(state);

        let payload = FleetHeartbeatPayload {
            node_id: "node-runtime-1".to_string(),
            hostname: Some("runtime-1.local".to_string()),
            ip: Some("10.0.0.11".to_string()),
            ips: vec!["192.168.1.11".to_string()],
            role: Some("worker".to_string()),
            status: Some("online".to_string()),
            heartbeat_at: None,
            resources: json!({"cpu": "8 cores", "ram": "32 GB", "gpu": "none"}),
            services: json!(["gateway", "runner"]),
            models: vec!["llama-3.1-8b".to_string()],
            capabilities: json!({"tool_exec": true, "voice": false}),
            stale: Some(FleetHeartbeatStaleness {
                degraded_after_secs: Some(30),
                offline_after_secs: Some(90),
            }),
        };

        let Json(response) = fleet_heartbeat(State(state.clone()), Json(payload))
            .await
            .expect("heartbeat endpoint succeeds");

        assert_eq!(response["status"], "ok");
        assert_eq!(response["hostname"], "runtime-1.local");
        assert_eq!(response["derived_status"], "online");

        let runtime_rows = pool
            .with_conn(|conn| queries::list_fleet_node_runtime(conn))
            .await
            .expect("query runtime rows");

        assert_eq!(runtime_rows.len(), 1);
        assert_eq!(runtime_rows[0].node_id, "node-runtime-1");
        assert_eq!(runtime_rows[0].hostname, "runtime-1.local");
        assert_eq!(runtime_rows[0].derived_status, "online");
        assert!(runtime_rows[0].services_json.contains("runner"));
    }

    #[tokio::test]
    async fn fleet_status_works_without_discovery_registry_using_runtime_db() {
        use ff_db::{DbPool, DbPoolConfig, run_migrations};

        let pool = DbPool::open(DbPoolConfig::in_memory()).expect("in-memory pool");
        pool.with_conn(|conn| {
            run_migrations(conn)?;
            Ok(())
        })
        .await
        .expect("migrations should apply");

        let old_heartbeat = (Utc::now() - Duration::seconds(240))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        pool.with_conn(move |conn| {
            queries::upsert_fleet_node_runtime(
                conn,
                &queries::FleetNodeRuntimeHeartbeatRow {
                    node_id: "node-runtime-stale".to_string(),
                    hostname: "runtime-stale.local".to_string(),
                    ips_json: r#"["10.0.0.44"]"#.to_string(),
                    role: "worker".to_string(),
                    reported_status: "online".to_string(),
                    last_heartbeat: old_heartbeat,
                    resources_json: "{}".to_string(),
                    services_json: "[]".to_string(),
                    models_json: r#"["qwen2.5"]"#.to_string(),
                    capabilities_json: "{}".to_string(),
                    stale_degraded_after_secs: 60,
                    stale_offline_after_secs: 180,
                },
            )?;
            Ok(())
        })
        .await
        .expect("seed runtime heartbeat row");

        let mut state = GatewayState::new(MessageRouter::default());
        state.operational_store = Some(OperationalStore::sqlite(pool.clone()));
        state.runtime_registry = Some(RuntimeRegistryStore::sqlite(pool.clone()));

        let payload = build_fleet_status_payload(&state)
            .await
            .expect("fleet status should build from runtime DB only");

        assert_eq!(payload.total_nodes, 1);
        assert_eq!(
            payload.nodes[0].hostname.as_deref(),
            Some("runtime-stale.local")
        );
        assert_eq!(payload.nodes[0].status, "offline");
        assert_eq!(payload.nodes[0].source_kind, "runtime/db");
        assert_eq!(payload.nodes[0].models_loaded, vec!["qwen2.5".to_string()]);
    }
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
async fn update_config(
    State(state): State<Arc<GatewayState>>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
                    "sqlite_path": cfg.database.sqlite_path.clone(),
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
                if let Some(pool) = store.sqlite_pool() {
                    let path = pool.path().to_string_lossy().to_string();
                    let exists = std::path::Path::new(&path).exists();
                    json!({
                        "active_mode": backend,
                        "status": if ping_ok { "ready" } else { "degraded" },
                        "sqlite": {
                            "path": path,
                            "file_exists": exists,
                            "wal_mode": pool.config().wal_mode,
                            "max_connections": pool.config().max_connections,
                            "config_kv_entries": kv_count,
                        }
                    })
                } else {
                    json!({
                        "active_mode": backend,
                        "status": if ping_ok { "ready" } else { "degraded" },
                        "postgres": {
                            "config_kv_entries": kv_count,
                        }
                    })
                }
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
    Json(raw_payload): Json<Value>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    // ── Pulse-first routing ──────────────────────────────────────────
    //
    // We try the Pulse router first. If it successfully picks a server
    // and the upstream call returns *something* (success or a 4xx from
    // the inference server itself), we return that. Only if Pulse cannot
    // find a matching server OR its upstream call fails outright do we
    // fall through to the legacy tier-router path.
    if let Some(pulse) = state.pulse_router.clone() {
        match pulse.route_completion(raw_payload.clone()).await {
            Ok(value) => {
                return Ok(Json(value).into_response());
            }
            Err(llm_routing::LlmRoutingError::NoMatch { .. }) => {
                // No Pulse-visible server loaded for this model — fall back
                // to the tier-router, which can auto-load on demand.
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
                // Pulse upstream failure — log and fall back.
                warn!(error = %e, "pulse routing failed; falling back to tier-router");
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
        if escalation_chain.is_empty() {
            if let (Some(store), Some(registry)) =
                (state.operational_store.as_ref(), state.api_registry.as_ref())
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
                let url = format!("{}/v1/chat/completions", backend.base_url());
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
                            return openai_compat::passthrough_response(upstream)
                                .await
                                .map_err(|e| (
                                    StatusCode::BAD_GATEWAY,
                                    Json(json!({"error": {"message": e, "type": "upstream_error"}})),
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
                        return openai_compat::passthrough_response(upstream)
                            .await
                            .map_err(|e| {
                                (
                                    StatusCode::BAD_GATEWAY,
                                    Json(
                                        json!({"error": {"message": e, "type": "upstream_error"}}),
                                    ),
                                )
                            });
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
        let url = format!("{}/v1/chat/completions", backend.base_url());
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
                return openai_compat::passthrough_response(upstream)
                    .await
                    .map_err(|e| {
                        (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({"error": {"message": e, "type": "upstream_error"}})),
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

/// GET /v1/models — list available models from the backend registry.
async fn list_models(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(registry) = &state.api_registry else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "backend registry not initialized", "type": "not_ready"}}),
            ),
        ));
    };

    let now = Utc::now().timestamp();
    let models = registry.available_models().await;
    let data: Vec<Value> = models
        .into_iter()
        .map(|(model, tier)| {
            json!({
                "id": model,
                "object": "model",
                "created": now,
                "owned_by": "forgefleet",
                "tier": tier,
            })
        })
        .collect();

    Ok(Json(json!({
        "object": "list",
        "data": data,
    })))
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

// ─── Replication ─────────────────────────────────────────────────────────────

/// POST /api/fleet/replicate/snapshot — leader creates and serves a full DB snapshot.
async fn replicate_snapshot(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(leader_sync) = &state.leader_sync else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "replication not configured (this node is not a leader)", "type": "not_leader"}}),
            ),
        ));
    };

    match leader_sync.create_fresh_snapshot().await {
        Ok(meta) => Ok(Json(json!({
            "status": "ok",
            "snapshot": meta,
        }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"error": {"message": format!("snapshot creation failed: {e}"), "type": "snapshot_error"}}),
            ),
        )),
    }
}

/// GET /api/fleet/replicate/sequence — returns current WAL sequence number.
async fn replicate_sequence(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(leader_sync) = &state.leader_sync else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "replication not configured (this node is not a leader)", "type": "not_leader"}}),
            ),
        ));
    };

    let sequence = leader_sync.current_sequence();
    Ok(Json(json!({
        "sequence": sequence,
    })))
}

/// POST /api/fleet/replicate/pull — follower requests changes since sequence N.
///
/// Request body: `{ "since_sequence": 5 }`
///
/// Response:
/// - If up to date: `{ "status": "up_to_date", "sequence": N }`
/// - If behind: binary snapshot with `X-Snapshot-Meta` header containing JSON metadata.
#[derive(Debug, Deserialize)]
struct PullRequest {
    since_sequence: u64,
}

async fn replicate_pull(
    State(state): State<Arc<GatewayState>>,
    Json(payload): Json<PullRequest>,
) -> Result<Response<Body>, (StatusCode, Json<Value>)> {
    let Some(leader_sync) = &state.leader_sync else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error": {"message": "replication not configured (this node is not a leader)", "type": "not_leader"}}),
            ),
        ));
    };

    let result = leader_sync.handle_pull(payload.since_sequence).await.map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": {"message": format!("pull failed: {e}"), "type": "pull_error"}})),
    ))?;

    match result {
        None => {
            // Follower is up to date.
            let body = serde_json::to_string(&json!({
                "status": "up_to_date",
                "sequence": leader_sync.current_sequence(),
            }))
            .unwrap();

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap())
        }
        Some((meta, path)) => {
            // Read the snapshot file and serve it as binary.
            let bytes = tokio::fs::read(&path).await.map_err(|e| (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("read snapshot file: {e}"), "type": "io_error"}})),
            ))?;

            let meta_json = serde_json::to_string(&meta).unwrap_or_default();

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header("X-Snapshot-Meta", meta_json)
                .body(Body::from(bytes))
                .unwrap())
        }
    }
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
    let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();

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
) -> impl IntoResponse {
    let model = req.model.unwrap_or_else(|| "Qwen2.5-Coder-32B-Instruct-Q4_K_M.gguf".to_string());
    let llm_base_url = req.llm_base_url.unwrap_or_else(|| "http://192.168.5.102:51000".to_string());
    let working_dir = req
        .working_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")));

    let config = AgentSessionConfig {
        model: model.clone(),
        llm_base_url: llm_base_url.clone(),
        working_dir,
        system_prompt: req.system_prompt,
        max_turns: req.max_turns.unwrap_or(30),
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

    state.agent_sessions.insert(session_id, handle.clone());

    // Spawn the agent loop in a background task
    let ws_hub = state.ws_hub.clone();
    let _sessions = state.agent_sessions.clone();
    let status = handle.status.clone();
    let prompt = req.prompt;

    tokio::spawn(async move {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();

        // Forward agent events to WebSocket hub
        let ws_hub_fwd = ws_hub.clone();
        let fwd_handle = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let Ok(payload) = serde_json::to_value(&event) {
                    ws_hub_fwd.broadcast_event(
                        crate::websocket::EventType::AgentEvent,
                        payload,
                    );
                }
            }
        });

        let outcome = session.run(&prompt, Some(event_tx)).await;

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

        // Clean up forwarding task
        fwd_handle.abort();

        info!(session = %session_id, "agent session completed");
    });

    Json(CreateAgentSessionResponse {
        session_id: session_id.to_string(),
        status: "running",
        model,
        llm_base_url,
    })
}

async fn agent_session_message(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let session_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid session ID"}))),
    };

    if !state.agent_sessions.contains_key(&session_id) {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "session not found"})));
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

    (StatusCode::OK, Json(json!({"status": "queued", "session_id": session_id.to_string()})))
}

async fn cancel_agent_session(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid session ID"}))),
    };

    if let Some(handle) = state.agent_sessions.get(&session_id) {
        handle.cancel_token.cancel();
        (StatusCode::OK, Json(json!({"status": "cancelled", "session_id": session_id.to_string()})))
    } else {
        (StatusCode::NOT_FOUND, Json(json!({"error": "session not found"})))
    }
}

async fn agent_session_status(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid session ID"}))),
    };

    if let Some(handle) = state.agent_sessions.get(&session_id) {
        (StatusCode::OK, Json(json!({
            "session_id": session_id.to_string(),
            "status": handle.status_str(),
            "model": handle.model,
            "llm_base_url": handle.llm_base_url,
            "created_at": handle.created_at.to_rfc3339(),
        })))
    } else {
        (StatusCode::NOT_FOUND, Json(json!({"error": "session not found"})))
    }
}

async fn list_agent_sessions(
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
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

    Json(json!({ "sessions": sessions }))
}

// ─── Chat Management Endpoints ──────────────────────────────────────────────

async fn list_chats() -> impl IntoResponse {
    let manager = ff_agent::chat_manager::ChatManager::load().await;
    let chats = manager.list_all();
    Json(json!({ "chats": chats }))
}

async fn create_chat(
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let mut manager = ff_agent::chat_manager::ChatManager::load().await;
    let scope_str = body.get("scope").and_then(Value::as_str).unwrap_or("global");
    let scope = match scope_str {
        "global" => ff_agent::scoped_memory::MemoryScope::Global,
        _ => ff_agent::scoped_memory::MemoryScope::Global,
    };

    let chat = manager.create(
        body.get("name").and_then(Value::as_str).map(String::from),
        scope,
        "http://192.168.5.102:51000".into(),
        "auto".into(),
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
        ff_agent::chat_manager::ModelSelection::Auto,
    ).await;

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
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "chat not found" }))),
    }
}

async fn delete_chat(Path(id): Path<String>) -> impl IntoResponse {
    let mut manager = ff_agent::chat_manager::ChatManager::load().await;
    manager.delete(&id).await;
    Json(json!({ "deleted": true }))
}

/// GET /api/brain/search?q=query — search across all three brains.
async fn brain_search(axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>) -> impl IntoResponse {
    let query = params.get("q").cloned().unwrap_or_default();
    if query.is_empty() {
        return Json(json!({ "results": [], "error": "missing ?q= parameter" }));
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let results = ff_agent::brain::search_all(&query, &cwd).await;
    Json(json!({ "results": results, "query": query }))
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
