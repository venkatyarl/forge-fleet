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

    // TODO: Phase 2 — call brain_chat::stream_assistant_response here
    // to generate an LLM reply. For now, echo-acknowledge.
    let assistant_reply = format!(
        "Received on thread '{}': {}",
        slug,
        body.content.chars().take(100).collect::<String>()
    );
    let _ = ff_db::pg_insert_brain_message(
        pool,
        thread.id,
        user.id,
        "system",
        "brain",
        "assistant",
        &assistant_reply,
        None,
    )
    .await;

    Ok(Json(json!({ "message_id": msg_id, "thread_slug": slug })))
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
        .unwrap_or_else(|_| "redis://192.168.5.100:6380".to_string());
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
