//! Virtual Brain MCP tools — read/write the shared knowledge graph.
//!
//! Read tools are safe (no approval needed). Write tools stage proposals to the
//! Inbox as candidates for human review. Thread/stack/backlog tools are
//! immediate (no approval).

use chrono::Utc;
use ff_brain::BrainStateClient;
use ff_core::config;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tracing::info;
use uuid::Uuid;

use crate::handlers::HandlerResult;

const DEFAULT_USER: &str = "venkat";
const REDIS_URL: &str = "redis://192.168.5.100:6380";

/// Get a Postgres pool using the fleet config (same pattern as other handlers).
async fn get_pool() -> Result<sqlx::PgPool, String> {
    let (cfg, _) =
        config::load_config_auto().map_err(|e| format!("failed to load fleet config: {e}"))?;
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&cfg.database.url)
        .await
        .map_err(|e| format!("Postgres connection failed: {e}"))
}

/// Resolve the default user, creating if needed.
async fn resolve_default_user(pool: &sqlx::PgPool) -> Result<uuid::Uuid, String> {
    match ff_db::pg_get_brain_user(pool, DEFAULT_USER).await {
        Ok(Some(u)) => Ok(u.id),
        Ok(None) => ff_db::pg_create_brain_user(pool, DEFAULT_USER, Some("Venkat"))
            .await
            .map_err(|e| format!("create user: {e}")),
        Err(e) => Err(format!("get user: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Read tools (safe, no approval needed)
// ═══════════════════════════════════════════════════════════════════════════

/// Search the vault knowledge graph by text query.
pub async fn brain_search(params: Option<Value>) -> HandlerResult {
    let query = params
        .as_ref()
        .and_then(|p| p.get("query"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let node_type = params
        .as_ref()
        .and_then(|p| p.get("node_type"))
        .and_then(|v| v.as_str());
    let tags: Option<Vec<String>> = params
        .as_ref()
        .and_then(|p| p.get("tags"))
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let limit = params
        .as_ref()
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_i64())
        .unwrap_or(20);

    let pool = get_pool().await?;
    let mut nodes = ff_db::pg_search_brain_vault_nodes(&pool, query, limit)
        .await
        .map_err(|e| format!("search: {e}"))?;

    // Post-filter by node_type if specified.
    if let Some(nt) = node_type {
        nodes.retain(|n| n.node_type.as_deref() == Some(nt));
    }
    // Post-filter by tags if specified.
    if let Some(ref tag_list) = tags {
        nodes.retain(|n| tag_list.iter().any(|t| n.tags.contains(t)));
    }

    Ok(json!({
        "count": nodes.len(),
        "nodes": nodes.iter().map(|n| json!({
            "id": n.id.to_string(),
            "path": n.path,
            "title": n.title,
            "node_type": n.node_type,
            "project": n.project,
            "tags": n.tags,
            "confidence": n.confidence,
            "hits": n.hits,
            "updated_at": n.updated_at.to_rfc3339(),
        })).collect::<Vec<_>>()
    }))
}

/// Read a specific vault node by path.
pub async fn brain_vault_read(params: Option<Value>) -> HandlerResult {
    let path = params
        .as_ref()
        .and_then(|p| p.get("path"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: path".to_string())?;

    let pool = get_pool().await?;
    let node = ff_db::pg_get_brain_vault_node(&pool, path)
        .await
        .map_err(|e| format!("vault read: {e}"))?;

    match node {
        Some(n) => {
            // Bump hit counter.
            let _ = ff_db::pg_bump_vault_node_hits(&pool, n.id).await;
            Ok(json!({
                "id": n.id.to_string(),
                "path": n.path,
                "title": n.title,
                "node_type": n.node_type,
                "project": n.project,
                "tags": n.tags,
                "extends_path": n.extends_path,
                "applies_to": n.applies_to,
                "from_thread": n.from_thread,
                "confidence": n.confidence,
                "hits": n.hits + 1,
                "community_id": n.community_id,
                "updated_at": n.updated_at.to_rfc3339(),
            }))
        }
        None => Ok(json!({ "error": "not found", "path": path })),
    }
}

/// Get the graph neighbors (edges) of a node.
pub async fn brain_graph_neighbors(params: Option<Value>) -> HandlerResult {
    let node_path = params
        .as_ref()
        .and_then(|p| p.get("node_path"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: node_path".to_string())?;

    let pool = get_pool().await?;
    let node = ff_db::pg_get_brain_vault_node(&pool, node_path)
        .await
        .map_err(|e| format!("vault read: {e}"))?
        .ok_or_else(|| format!("node not found: {node_path}"))?;

    let edges = ff_db::pg_list_brain_vault_edges_for_node(&pool, node.id)
        .await
        .map_err(|e| format!("edges: {e}"))?;

    Ok(json!({
        "node_id": node.id.to_string(),
        "node_path": node.path,
        "edge_count": edges.len(),
        "edges": edges.iter().map(|e| json!({
            "src_id": e.src_id.to_string(),
            "dst_id": e.dst_id.to_string(),
            "edge_type": e.edge_type,
            "confidence": e.confidence,
            "provenance": e.provenance,
        })).collect::<Vec<_>>()
    }))
}

/// List the user's threads.
pub async fn brain_list_threads(params: Option<Value>) -> HandlerResult {
    let user_name = params
        .as_ref()
        .and_then(|p| p.get("user"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_USER);

    let pool = get_pool().await?;
    let user = ff_db::pg_get_brain_user(&pool, user_name)
        .await
        .map_err(|e| format!("get user: {e}"))?
        .ok_or_else(|| format!("user not found: {user_name}"))?;

    let threads = ff_db::pg_list_brain_threads(&pool, user.id)
        .await
        .map_err(|e| format!("list threads: {e}"))?;

    Ok(json!({
        "user": user_name,
        "count": threads.len(),
        "threads": threads.iter().map(|t| json!({
            "id": t.id.to_string(),
            "slug": t.slug,
            "title": t.title,
            "project": t.project,
            "status": t.status,
            "last_message_at": t.last_message_at.map(|d| d.to_rfc3339()),
            "created_at": t.created_at.to_rfc3339(),
        })).collect::<Vec<_>>()
    }))
}

/// Get vault graph stats (node count, edge count, community count).
pub async fn brain_stats(_params: Option<Value>) -> HandlerResult {
    let pool = get_pool().await?;

    let node_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM brain_vault_nodes WHERE valid_until IS NULL")
            .fetch_one(&pool)
            .await
            .map_err(|e| format!("count nodes: {e}"))?;

    let edge_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM brain_vault_edges")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("count edges: {e}"))?;

    let community_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM brain_communities")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("count communities: {e}"))?;

    let thread_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM brain_threads WHERE status = 'active'")
            .fetch_one(&pool)
            .await
            .map_err(|e| format!("count threads: {e}"))?;

    let pending_candidates: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM brain_knowledge_candidates WHERE status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| format!("count candidates: {e}"))?;

    Ok(json!({
        "vault_nodes": node_count,
        "vault_edges": edge_count,
        "communities": community_count,
        "active_threads": thread_count,
        "pending_candidates": pending_candidates,
    }))
}

// ═══════════════════════════════════════════════════════════════════════════
// Write tools (staged to Inbox, needs operator approval)
// ═══════════════════════════════════════════════════════════════════════════

/// Propose a new knowledge node. Stages as a candidate for human review.
pub async fn brain_propose_node(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let kind = p
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: kind".to_string())?;
    let title = p
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: title".to_string())?;
    let body = p
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: body".to_string())?;
    let tags: Vec<String> = p
        .get("tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let project = p.get("project").and_then(|v| v.as_str());

    let pool = get_pool().await?;
    let user_id = resolve_default_user(&pool).await?;

    let candidate_id = ff_db::pg_insert_brain_candidate(
        &pool,
        user_id,
        None,     // thread_id
        "create", // action
        Some(kind),
        Some(title),
        Some(body),
        &tags,
        project,
        None,      // target_path
        None,      // from_thread
        Some(0.8), // default confidence for MCP proposals
    )
    .await
    .map_err(|e| format!("insert candidate: {e}"))?;

    info!(id = %candidate_id, title, "brain: proposed new knowledge node");

    Ok(json!({
        "status": "staged",
        "candidate_id": candidate_id.to_string(),
        "message": "Proposal staged for human review. Use 'ff brain inbox' to approve/reject.",
    }))
}

/// Propose a link between two existing nodes.
pub async fn brain_propose_link(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let src_path = p
        .get("src_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: src_path".to_string())?;
    let dst_path = p
        .get("dst_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: dst_path".to_string())?;
    let edge_type = p
        .get("edge_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: edge_type".to_string())?;

    let pool = get_pool().await?;
    let user_id = resolve_default_user(&pool).await?;

    // Validate both nodes exist.
    let _src = ff_db::pg_get_brain_vault_node(&pool, src_path)
        .await
        .map_err(|e| format!("lookup src: {e}"))?
        .ok_or_else(|| format!("source node not found: {src_path}"))?;
    let _dst = ff_db::pg_get_brain_vault_node(&pool, dst_path)
        .await
        .map_err(|e| format!("lookup dst: {e}"))?
        .ok_or_else(|| format!("destination node not found: {dst_path}"))?;

    let body = format!("{src_path} --[{edge_type}]--> {dst_path}");
    let candidate_id = ff_db::pg_insert_brain_candidate(
        &pool,
        user_id,
        None,
        "link",
        Some(edge_type),
        Some(&format!("Link: {src_path} -> {dst_path}")),
        Some(&body),
        &[],
        None,
        Some(src_path), // target_path = source node
        None,
        Some(0.8),
    )
    .await
    .map_err(|e| format!("insert candidate: {e}"))?;

    info!(id = %candidate_id, src_path, dst_path, edge_type, "brain: proposed new link");

    Ok(json!({
        "status": "staged",
        "candidate_id": candidate_id.to_string(),
        "message": "Link proposal staged for human review.",
    }))
}

// ═══════════════════════════════════════════════════════════════════════════
// Thread / Stack / Backlog tools (immediate, no approval)
// ═══════════════════════════════════════════════════════════════════════════

/// Add a message to a thread.
pub async fn brain_thread_append(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let thread_slug = p
        .get("thread_slug")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: thread_slug".to_string())?;
    let content = p
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: content".to_string())?;

    let pool = get_pool().await?;
    let user_id = resolve_default_user(&pool).await?;

    // Get or create the thread.
    let thread = match ff_db::pg_get_brain_thread(&pool, user_id, thread_slug).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            let tid = ff_db::pg_create_brain_thread(&pool, user_id, thread_slug, None, None)
                .await
                .map_err(|e| format!("create thread: {e}"))?;
            ff_db::pg_get_brain_thread_by_id(&pool, tid)
                .await
                .map_err(|e| format!("get thread: {e}"))?
                .ok_or_else(|| "thread not found after creation".to_string())?
        }
        Err(e) => return Err(format!("get thread: {e}")),
    };

    let now = Utc::now();

    let msg_id = ff_db::pg_insert_brain_message(
        &pool,
        thread.id,
        user_id,
        "mcp",      // channel
        "mcp-tool", // external_id
        "assistant",
        content,
        None, // metadata
    )
    .await
    .map_err(|e| format!("insert message: {e}"))?;

    ff_db::pg_touch_brain_thread(&pool, thread.id)
        .await
        .map_err(|e| format!("touch thread: {e}"))?;

    Ok(json!({
        "thread_slug": thread_slug,
        "thread_id": thread.id.to_string(),
        "message_id": msg_id.to_string(),
        "created_at": now.to_rfc3339(),
    }))
}

/// Push an item onto the current thread's stack.
pub async fn brain_stack_push(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let thread_slug = p
        .get("thread_slug")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: thread_slug".to_string())?;
    let title = p
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: title".to_string())?;

    let pool = get_pool().await?;
    let user_id = resolve_default_user(&pool).await?;

    let thread = ff_db::pg_get_brain_thread(&pool, user_id, thread_slug)
        .await
        .map_err(|e| format!("get thread: {e}"))?
        .ok_or_else(|| format!("thread not found: {thread_slug}"))?;

    let mut client = BrainStateClient::new(REDIS_URL, pool)
        .await
        .map_err(|e| format!("redis: {e}"))?;

    let item = ff_brain::StackItem {
        id: Uuid::new_v4().to_string(),
        title: title.to_string(),
        context: None,
        push_reason: Some("via MCP tool".to_string()),
        progress: 0.0,
        pushed_at: Utc::now().to_rfc3339(),
    };

    let depth = client
        .stack_push(&user_id, &thread.id, &item)
        .await
        .map_err(|e| format!("stack push: {e}"))?;

    Ok(json!({
        "item_id": item.id,
        "thread_slug": thread_slug,
        "stack_depth": depth,
    }))
}

/// Add an item to a project's backlog.
pub async fn brain_backlog_add(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let title = p
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: title".to_string())?;
    let priority = p
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("medium");
    let project = p
        .get("project")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: project".to_string())?;

    let pool = get_pool().await?;
    let user_id = resolve_default_user(&pool).await?;

    let mut client = BrainStateClient::new(REDIS_URL, pool)
        .await
        .map_err(|e| format!("redis: {e}"))?;

    let item = ff_brain::BacklogItem {
        id: Uuid::new_v4().to_string(),
        title: title.to_string(),
        description: None,
        priority: priority.to_string(),
        tags: vec![],
        from_thread_id: None,
        created_at: Utc::now().to_rfc3339(),
    };

    let count = client
        .backlog_add(&user_id, project, &item)
        .await
        .map_err(|e| format!("backlog add: {e}"))?;

    Ok(json!({
        "item_id": item.id,
        "project": project,
        "priority": priority,
        "backlog_size": count,
    }))
}
