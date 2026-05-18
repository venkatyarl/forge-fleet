//! HTTP API endpoints for the Virtual Brain.
//!
//! Mounted in server.rs under /api/brain/*.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::server::GatewayState;

fn pool_from_state(state: &GatewayState) -> Result<&ff_db::PgPool, (StatusCode, Json<Value>)> {
    state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"postgres pool not available"})),
            )
        })
}

fn db_err(op: &str, e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!("brain api error ({op}): {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("{op}: {e}")})),
    )
}

// ─── Threads ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListThreadsQuery {
    pub user: Option<String>,
}

pub async fn list_threads(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let user_name = q.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("pg_get_brain_user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("user '{user_name}' not found")})),
            )
        })?;
    let threads = ff_db::pg_list_brain_threads(pool, user.id)
        .await
        .map_err(|e| db_err("pg_list_brain_threads", e))?;
    Ok(Json(json!({ "threads": threads })))
}

#[derive(Debug, Deserialize)]
pub struct CreateThreadBody {
    pub slug: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub user: Option<String>,
}

pub async fn create_thread(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CreateThreadBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let user_name = body.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("pg_get_brain_user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let id = ff_db::pg_create_brain_thread(
        pool,
        user.id,
        &body.slug,
        body.title.as_deref(),
        body.project.as_deref(),
    )
    .await
    .map_err(|e| db_err("pg_create_brain_thread", e))?;
    Ok(Json(json!({ "id": id, "slug": body.slug })))
}

// ─── Messages ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    pub limit: Option<i64>,
}

pub async fn thread_messages(
    State(state): State<Arc<GatewayState>>,
    Path(slug): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    // Find thread by slug. Use the default user in single-user mode.
    let user = ff_db::pg_get_brain_user(pool, "venkat")
        .await
        .map_err(|e| db_err("pg_get_brain_user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"default user not found; run brain seed first"})),
            )
        })?;
    let thread_row = ff_db::pg_get_brain_thread(pool, user.id, &slug)
        .await
        .map_err(|e| db_err("pg_get_brain_thread", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("thread '{slug}' not found")})),
            )
        })?;

    let mut msgs = ff_db::pg_list_brain_messages(pool, thread_row.id, q.limit.unwrap_or(50))
        .await
        .map_err(|e| db_err("pg_list_brain_messages", e))?;
    msgs.reverse(); // oldest first for chat display
    Ok(Json(json!({ "messages": msgs })))
}

#[derive(Debug, Deserialize)]
pub struct SendMessageBody {
    pub content: String,
    pub channel: Option<String>,
    pub user: Option<String>,
    /// Target model for the LLM reply. If omitted, the system tries to pick
    /// a sensible default from available fleet models.
    pub model: Option<String>,
}

pub async fn send_thread_message(
    State(state): State<Arc<GatewayState>>,
    Path(slug): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let user_name = body.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("pg_get_brain_user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let thread = ff_db::pg_get_brain_thread(pool, user.id, &slug)
        .await
        .map_err(|e| db_err("pg_get_brain_thread", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("thread '{slug}' not found")})),
            )
        })?;

    let channel = body.channel.as_deref().unwrap_or("web");
    let msg_id = ff_db::pg_insert_brain_message(
        pool,
        thread.id,
        user.id,
        channel,
        "web-dashboard",
        "user",
        &body.content,
        None,
    )
    .await
    .map_err(|e| db_err("pg_insert_brain_message", e))?;

    let _ = ff_db::pg_touch_brain_thread(pool, thread.id).await;

    // Generate LLM reply via fleet routing (cloud → pulse → tier → model).
    // Broadcast user message to WebSocket subscribers.
    state.ws_hub.broadcast_event(
        crate::websocket::EventType::Message,
        json!({
            "thread_id": thread.id,
            "thread_slug": slug,
            "message_id": msg_id,
            "role": "user",
            "content": body.content,
            "channel": channel,
        }),
    );

    let assistant_reply = match generate_brain_reply(&state, pool, &thread, &user, &body).await {
        Ok(reply) => reply,
        Err(e) => {
            warn!(error = %e, thread = %slug, "brain reply generation failed; storing error as assistant message");
            format!("(Unable to generate reply: {e})")
        }
    };

    let assistant_msg_id = ff_db::pg_insert_brain_message(
        pool,
        thread.id,
        user.id,
        "system",
        "brain",
        "assistant",
        &assistant_reply,
        None,
    )
    .await
    .map_err(|e| db_err("pg_insert_brain_message (assistant)", e))?;

    let _ = ff_db::pg_touch_brain_thread(pool, thread.id).await;

    // Broadcast assistant message to WebSocket subscribers.
    state.ws_hub.broadcast_event(
        crate::websocket::EventType::Message,
        json!({
            "thread_id": thread.id,
            "thread_slug": slug,
            "message_id": assistant_msg_id,
            "role": "assistant",
            "content": assistant_reply,
            "channel": "brain",
        }),
    );

    Ok(Json(json!({
        "message_id": msg_id,
        "assistant_message_id": assistant_msg_id,
        "thread_slug": slug,
        "reply_preview": assistant_reply.chars().take(120).collect::<String>(),
    })))
}

// ─── Brain reply generation ──────────────────────────────────────────────

/// Build an OpenAI-compatible messages array from the thread history.
async fn build_chat_messages(
    pool: &ff_db::PgPool,
    thread_id: uuid::Uuid,
    new_user_content: &str,
) -> Result<Vec<Value>, anyhow::Error> {
    let history = ff_db::pg_list_brain_messages(pool, thread_id, 20)
        .await
        .map_err(|e| anyhow::anyhow!("list messages: {e}"))?;

    let mut messages: Vec<Value> = Vec::with_capacity(history.len() + 2);

    // System prompt so the model knows what ForgeFleet actually is.
    messages.push(json!({
        "role": "system",
        "content": "You are the ForgeFleet AI assistant. ForgeFleet is an open-source platform for orchestrating fleets of AI models across multiple computers (nodes). You help users manage their AI fleet: deploy models, route inference requests, monitor node health, manage costs, schedule tasks, and coordinate multi-agent workflows. You are NOT a logistics or trucking fleet management system."
    }));

    // History is newest-first; reverse to oldest-first for the LLM.
    for msg in history.iter().rev() {
        let role = match msg.role.as_str() {
            "user" | "assistant" | "system" => msg.role.clone(),
            _ => "user".to_string(),
        };
        messages.push(json!({
            "role": role,
            "content": msg.content,
        }));
    }
    messages.push(json!({
        "role": "user",
        "content": new_user_content,
    }));
    Ok(messages)
}

/// Extract assistant text from an OpenAI-shaped chat completion JSON value.
fn extract_assistant_content(v: &Value) -> Option<String> {
    v.get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()
        .map(|s| s.to_string())
}

/// Pick a default model when the caller does not specify one.
/// Priority: first available from Pulse router live scan.
async fn pick_default_model(state: &GatewayState) -> Option<String> {
    if let Some(pulse) = state.pulse_router.as_ref()
        && let Ok(servers) = pulse.list_servers().await
    {
        for s in servers {
            if let Some(model) = s.get("model").and_then(|v| v.as_str()) {
                return Some(model.to_string());
            }
        }
    }
    None
}

/// Generate an assistant reply for a brain thread by routing through the
/// fleet's LLM infrastructure: cloud providers → Pulse local fleet → tier
/// escalation → legacy model router.
async fn generate_brain_reply(
    state: &GatewayState,
    pool: &ff_db::PgPool,
    thread: &ff_db::BrainThreadRow,
    _user: &ff_db::BrainUserRow,
    body: &SendMessageBody,
) -> Result<String, anyhow::Error> {
    let model = match body.model.as_deref() {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => pick_default_model(state).await.ok_or_else(|| {
            anyhow::anyhow!(
                "no models available in fleet; start an LLM server or configure a cloud provider"
            )
        })?,
    };

    let messages = build_chat_messages(pool, thread.id, &body.content).await?;

    let payload = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "max_tokens": 2048,
    });

    // ── 1) Cloud-LLM routing (first pass) ───────────────────────────────
    if let Some(result) =
        crate::cloud_llm::try_route_to_cloud(pool, &model, &payload, None, &state.http_client).await
    {
        match result {
            Ok(resp) => {
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .map_err(|e| anyhow::anyhow!("read cloud response body: {e}"))?;
                let v: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| anyhow::anyhow!("parse cloud response: {e}"))?;
                if let Some(content) = extract_assistant_content(&v) {
                    info!(model = %model, provider = "cloud", thread = %thread.slug, "brain reply from cloud");
                    return Ok(content);
                }
                return Err(anyhow::anyhow!("cloud response missing content"));
            }
            Err(resp) => {
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .map_err(|e| anyhow::anyhow!("read cloud error body: {e}"))?;
                let txt = String::from_utf8_lossy(&bytes);
                return Err(anyhow::anyhow!("cloud routing error: {txt}"));
            }
        }
    }

    // ── 2) Pulse-backed local fleet routing ──────────────────────────────
    if let Some(pulse) = state.pulse_router.as_ref() {
        let cache_ref = state.pulse_cache.as_deref();
        let pg_ref = Some(pool);
        match pulse
            .route_completion_cached(&payload, cache_ref, pg_ref)
            .await
        {
            Ok(v) => {
                if let Some(content) = extract_assistant_content(&v) {
                    info!(model = %model, provider = "pulse", thread = %thread.slug, "brain reply from pulse");
                    // Record usage if the response has usage info.
                    if let Some(usage) = v.get("usage")
                        && let (Some(prompt), Some(comp)) = (
                            usage.get("prompt_tokens").and_then(|x| x.as_u64()),
                            usage.get("completion_tokens").and_then(|x| x.as_u64()),
                        )
                    {
                        let record = ff_api::token_ledger::TokenUsageRecord::new(
                            uuid::Uuid::new_v4().to_string(),
                            &model,
                            "pulse",
                        )
                        .with_tokens(prompt as u32, comp as u32);
                        state.cost_tracker.record_usage(record).await;
                    }
                    return Ok(content);
                }
                return Err(anyhow::anyhow!("pulse response missing content"));
            }
            Err(crate::llm_routing::LlmRoutingError::NoMatch { .. }) => {
                debug!(model = %model, "pulse found no matching server");
            }
            Err(crate::llm_routing::LlmRoutingError::MissingModel) => {
                return Err(anyhow::anyhow!(
                    "model '{model}' not recognized by pulse router"
                ));
            }
            Err(e) => {
                warn!(error = %e, "pulse routing failed; trying tier fallback");
            }
        }
    }

    // ── 3) Tier-router fallback ─────────────────────────────────────────
    if let Some(tier_router) = state.tier_router.as_ref() {
        let payload_typed: ff_api::types::ChatCompletionRequest =
            serde_json::from_value(payload.clone())
                .map_err(|e| anyhow::anyhow!("invalid payload for tier router: {e}"))?;

        let chain = tier_router
            .route_with_escalation(&payload_typed.model, None, None)
            .await;

        if !chain.is_empty() {
            let mut last_error = None::<String>;
            for (_tier, backends) in &chain {
                for backend in backends {
                    let url = format!("{}/v1/chat/completions", backend.base_url());
                    let start = std::time::Instant::now();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        state.http_client.post(&url).json(&payload).send(),
                    )
                    .await
                    {
                        Ok(Ok(upstream)) if upstream.status().is_success() => {
                            let latency = start.elapsed();
                            tier_router.record_success(&backend.id, latency);
                            let bytes = upstream.bytes().await.unwrap_or_default();
                            let v: Value = serde_json::from_slice(&bytes)
                                .map_err(|e| anyhow::anyhow!("parse tier response: {e}"))?;
                            if let Some(content) = extract_assistant_content(&v) {
                                info!(model = %model, backend = %backend.id, "brain reply from tier router");
                                let prompt =
                                    v.get("usage")
                                        .and_then(|u| u.get("prompt_tokens"))
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0) as u32;
                                let comp = v
                                    .get("usage")
                                    .and_then(|u| u.get("completion_tokens"))
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0) as u32;
                                let record = ff_api::token_ledger::TokenUsageRecord::new(
                                    uuid::Uuid::new_v4().to_string(),
                                    &model,
                                    &backend.id,
                                )
                                .with_tokens(prompt, comp);
                                state.cost_tracker.record_usage(record).await;
                                return Ok(content);
                            }
                        }
                        Ok(Ok(upstream)) => {
                            let status = upstream.status();
                            let latency = start.elapsed();
                            tier_router.record_failure(&backend.id, latency);
                            last_error = Some(format!("{} returned {}", backend.id, status));
                        }
                        Ok(Err(e)) => {
                            let latency = start.elapsed();
                            tier_router.record_failure(&backend.id, latency);
                            last_error = Some(format!("{} request failed: {e}", backend.id));
                        }
                        Err(_) => {
                            let latency = start.elapsed();
                            tier_router.record_failure(&backend.id, latency);
                            last_error = Some(format!("{} timed out", backend.id));
                        }
                    }
                }
            }
            if let Some(e) = last_error {
                return Err(anyhow::anyhow!("tier router failed: {e}"));
            }
        }
    }

    // ── 4) Legacy model-router fallback ─────────────────────────────────
    if let Some(model_router) = state.model_router.as_ref() {
        let target_model = payload["model"].as_str().unwrap_or(&model);
        if let Some(backend) = model_router.route(target_model).await {
            let url = format!("{}/v1/chat/completions", backend.base_url());
            let resp = state
                .http_client
                .post(&url)
                .json(&payload)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("legacy router request failed: {e}"))?;
            let bytes = resp.bytes().await.unwrap_or_default();
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("parse legacy response: {e}"))?;
            if let Some(content) = extract_assistant_content(&v) {
                info!(model = %model, backend = %backend.id, "brain reply from legacy model router");
                return Ok(content);
            }
        }
    }

    Err(anyhow::anyhow!(
        "no LLM backend available for model '{}'. Start a local model or configure a cloud provider.",
        model
    ))
}

// ─── Thread attachment ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AttachBody {
    pub channel: String,
    pub external_id: String,
    pub thread_slug: String,
    pub user: Option<String>,
}

pub async fn attach_to_thread(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<AttachBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let user_name = body.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("pg_get_brain_user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let thread = ff_db::pg_get_brain_thread(pool, user.id, &body.thread_slug)
        .await
        .map_err(|e| db_err("pg_get_brain_thread", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"thread not found"})),
            )
        })?;

    // Ensure channel identity exists
    let _ =
        ff_db::pg_upsert_channel_identity(pool, &body.channel, &body.external_id, user.id).await;

    ff_db::pg_attach_thread(pool, &body.channel, &body.external_id, user.id, thread.id)
        .await
        .map_err(|e| db_err("pg_attach_thread", e))?;

    Ok(Json(
        json!({ "attached": true, "thread_slug": body.thread_slug }),
    ))
}

// ─── Knowledge candidates ────────────────────────────────────────────────

pub async fn list_candidates(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let user_name = q.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("pg_get_brain_user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let candidates = ff_db::pg_list_brain_candidates_pending(pool, user.id)
        .await
        .map_err(|e| db_err("pg_list_brain_candidates_pending", e))?;
    Ok(Json(json!({ "candidates": candidates })))
}

#[derive(Debug, Deserialize)]
pub struct CandidateActionBody {
    pub status: String, // 'approved' | 'rejected'
}

pub async fn update_candidate(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Json(body): Json<CandidateActionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("bad uuid: {e}")})),
        )
    })?;
    ff_db::pg_update_brain_candidate_status(pool, uuid, &body.status)
        .await
        .map_err(|e| db_err("pg_update_brain_candidate_status", e))?;
    Ok(Json(json!({ "id": id, "status": body.status })))
}

// ─── Vault graph (for BrainGraph.tsx) ────────────────────────────────────

pub async fn vault_graph(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let nodes = ff_db::pg_list_brain_vault_nodes_current(pool, None)
        .await
        .map_err(|e| db_err("pg_list_brain_vault_nodes_current", e))?;
    let node_ids: Vec<uuid::Uuid> = nodes.iter().map(|n| n.id).collect();

    // Collect all edges for current nodes
    let mut all_edges = Vec::new();
    for nid in &node_ids {
        let edges = ff_db::pg_list_brain_vault_edges_for_node(pool, *nid)
            .await
            .unwrap_or_default();
        for e in edges {
            if !all_edges.iter().any(|x: &ff_db::BrainVaultEdgeRow| {
                x.src_id == e.src_id && x.dst_id == e.dst_id && x.edge_type == e.edge_type
            }) {
                all_edges.push(e);
            }
        }
    }

    let communities = ff_db::pg_list_brain_communities(pool)
        .await
        .unwrap_or_default();

    Ok(Json(json!({
        "nodes": nodes,
        "edges": all_edges,
        "communities": communities,
    })))
}

// ─── Reminders ───────────────────────────────────────────────────────────

pub async fn list_reminders(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let reminders = ff_db::pg_list_due_reminders(pool)
        .await
        .map_err(|e| db_err("pg_list_due_reminders", e))?;
    Ok(Json(json!({ "reminders": reminders })))
}

#[derive(Debug, Deserialize)]
pub struct CreateReminderBody {
    pub content: String,
    pub remind_at: String, // ISO 8601
    pub channel_pref: Option<String>,
    pub thread_slug: Option<String>,
    pub user: Option<String>,
}

pub async fn create_reminder(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CreateReminderBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let user_name = body.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("pg_get_brain_user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let remind_at: chrono::DateTime<chrono::Utc> = body.remind_at.parse().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("bad remind_at: {e}")})),
        )
    })?;
    let id = ff_db::pg_insert_brain_reminder(
        pool,
        user.id,
        None,
        &body.content,
        remind_at,
        body.channel_pref.as_deref(),
    )
    .await
    .map_err(|e| db_err("pg_insert_brain_reminder", e))?;
    Ok(Json(json!({ "id": id, "remind_at": body.remind_at })))
}

// ─── Vault search ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct VaultSearchQuery {
    pub q: String,
    pub limit: Option<i64>,
}

pub async fn vault_search(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<VaultSearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let nodes = ff_db::pg_search_brain_vault_nodes(pool, &query.q, query.limit.unwrap_or(20))
        .await
        .map_err(|e| db_err("pg_search_brain_vault_nodes", e))?;
    Ok(Json(json!({ "results": nodes })))
}

// ─── Hybrid search (vector + keyword) ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct HybridSearchQuery {
    pub q: String,
    pub limit: Option<i64>,
}

pub async fn hybrid_search_handler(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<HybridSearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let limit = query.limit.unwrap_or(10);
    let results = ff_brain::hybrid_search(&query.q, limit, pool)
        .await
        .map_err(|e| db_err("hybrid_search", e))?;
    Ok(Json(json!({ "results": results, "query": query.q })))
}

// ─── User identity (whoami) ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WhoamiQuery {
    pub channel: String,
    pub external_id: String,
}

pub async fn whoami(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<WhoamiQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let user_id = ff_db::pg_resolve_channel_user(pool, &q.channel, &q.external_id)
        .await
        .map_err(|e| db_err("pg_resolve_channel_user", e))?;
    match user_id {
        Some(id) => {
            let user = ff_db::pg_get_brain_user_by_id(pool, id)
                .await
                .map_err(|e| db_err("pg_get_brain_user_by_id", e))?;
            Ok(Json(json!({
                "user_id": id,
                "user_name": user.map(|u| u.name),
                "channel": q.channel,
                "external_id": q.external_id,
            })))
        }
        None => Ok(Json(json!({
            "user_id": null,
            "user_name": null,
            "channel": q.channel,
            "external_id": q.external_id,
        }))),
    }
}

// ─── Stack + Backlog (Redis-backed) ──────────────────────────────────────

async fn get_brain_state_client(
    state: &GatewayState,
) -> Result<ff_brain::BrainStateClient, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(state)?;
    let redis_url = std::env::var("FORGEFLEET_REDIS_URL")
        .unwrap_or_else(|_| "redis://192.168.5.100:56379".to_string());
    ff_brain::BrainStateClient::new(&redis_url, pool.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": format!("redis: {e}")})),
            )
        })
}

#[derive(Debug, Deserialize)]
pub struct StackQuery {
    pub user: Option<String>,
}

pub async fn stack_list(
    State(state): State<Arc<GatewayState>>,
    Path(thread_slug): Path<String>,
    Query(q): Query<StackQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let mut client = get_brain_state_client(&state).await?;
    let user_name = q.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let thread = ff_db::pg_get_brain_thread(pool, user.id, &thread_slug)
        .await
        .map_err(|e| db_err("thread", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"thread not found"})),
            )
        })?;
    let items = client
        .stack_list(&user.id, &thread.id, 20)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(json!({ "items": items, "thread_slug": thread_slug })))
}

#[derive(Debug, Deserialize)]
pub struct StackPushBody {
    pub title: String,
    pub context: Option<String>,
    pub push_reason: Option<String>,
    pub user: Option<String>,
}

pub async fn stack_push(
    State(state): State<Arc<GatewayState>>,
    Path(thread_slug): Path<String>,
    Json(body): Json<StackPushBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let mut client = get_brain_state_client(&state).await?;
    let user_name = body.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let thread = ff_db::pg_get_brain_thread(pool, user.id, &thread_slug)
        .await
        .map_err(|e| db_err("thread", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"thread not found"})),
            )
        })?;
    let item = ff_brain::StackItem {
        id: uuid::Uuid::new_v4().to_string(),
        title: body.title,
        context: body.context,
        push_reason: body.push_reason,
        progress: 0.0,
        pushed_at: chrono::Utc::now().to_rfc3339(),
    };
    let depth = client
        .stack_push(&user.id, &thread.id, &item)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(
        json!({ "pushed": true, "depth": depth, "item": item }),
    ))
}

pub async fn stack_pop(
    State(state): State<Arc<GatewayState>>,
    Path(thread_slug): Path<String>,
    Query(q): Query<StackQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let mut client = get_brain_state_client(&state).await?;
    let user_name = q.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let thread = ff_db::pg_get_brain_thread(pool, user.id, &thread_slug)
        .await
        .map_err(|e| db_err("thread", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"thread not found"})),
            )
        })?;
    let popped = client
        .stack_pop(&user.id, &thread.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(json!({ "popped": popped })))
}

#[derive(Debug, Deserialize)]
pub struct BacklogQuery {
    pub user: Option<String>,
    pub limit: Option<usize>,
}

pub async fn backlog_list(
    State(state): State<Arc<GatewayState>>,
    Path(project): Path<String>,
    Query(q): Query<BacklogQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let mut client = get_brain_state_client(&state).await?;
    let user_name = q.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let items = client
        .backlog_list(&user.id, &project, q.limit.unwrap_or(50))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(json!({ "items": items, "project": project })))
}

#[derive(Debug, Deserialize)]
pub struct BacklogAddBody {
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub tags: Option<Vec<String>>,
    pub project: Option<String>,
    pub user: Option<String>,
}

pub async fn backlog_add(
    State(state): State<Arc<GatewayState>>,
    Path(project): Path<String>,
    Json(body): Json<BacklogAddBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let mut client = get_brain_state_client(&state).await?;
    let user_name = body.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let item = ff_brain::BacklogItem {
        id: uuid::Uuid::new_v4().to_string(),
        title: body.title,
        description: body.description,
        priority: body.priority.unwrap_or_else(|| "medium".to_string()),
        tags: body.tags.unwrap_or_default(),
        from_thread_id: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let count = client
        .backlog_add(&user.id, &project, &item)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(json!({ "added": true, "count": count, "item": item })))
}

#[derive(Debug, Deserialize)]
pub struct BacklogCompleteBody {
    pub item_id: String,
    pub user: Option<String>,
}

pub async fn backlog_complete(
    State(state): State<Arc<GatewayState>>,
    Path(project): Path<String>,
    Json(body): Json<BacklogCompleteBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = pool_from_state(&state)?;
    let mut client = get_brain_state_client(&state).await?;
    let user_name = body.user.as_deref().unwrap_or("venkat");
    let user = ff_db::pg_get_brain_user(pool, user_name)
        .await
        .map_err(|e| db_err("user", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"user not found"})),
            )
        })?;
    let done = client
        .backlog_complete(&user.id, &project, &body.item_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(json!({ "completed": done, "item_id": body.item_id })))
}
