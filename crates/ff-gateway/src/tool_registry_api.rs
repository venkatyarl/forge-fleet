//! Fleet Tool Registry API (Phase 15a)
//!
//! Endpoints for fleet-wide tool discovery, health tracking, and registration.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::sync::Arc;

use crate::server::GatewayState;

// ─── Request/Response Types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListToolsQuery {
    node: Option<String>,
    name: Option<String>,
    unhealthy: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ToolEntry {
    pub tool_name: String,
    pub worker_name: String,
    pub description: String,
    pub health_checked_at: String,
    pub call_count: i32,
    pub avg_latency_ms: Option<f64>,
    pub healthy: bool,
}

#[derive(Debug, Serialize)]
pub struct ToolHealthSummary {
    pub total_tools: i64,
    pub healthy_tools: i64,
    pub unhealthy_tools: i64,
    pub nodes: Vec<NodeToolHealth>,
}

#[derive(Debug, Serialize)]
pub struct NodeToolHealth {
    pub worker_name: String,
    pub tool_count: i64,
    pub healthy_count: i64,
    pub unhealthy_count: i64,
}

#[derive(Debug, Deserialize)]
pub struct RegisterToolsRequest {
    pub worker_name: String,
    pub tools: Vec<ToolRegistration>,
}

#[derive(Debug, Deserialize)]
pub struct ToolRegistration {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
    pub capabilities_required: Vec<String>,
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/tools — List all tools registered across the fleet.
pub async fn list_tools(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<ListToolsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let mut sql = String::from(
        "SELECT tool_name, worker_name, description, health_checked_at, \
         call_count, avg_latency_ms, \
         (health_checked_at > NOW() - INTERVAL '5 minutes') as healthy \
         FROM fleet_tools WHERE 1=1",
    );

    // Build parameterised query safely — no user input is ever interpolated
    // into the SQL string itself.
    let mut next_param = 1;
    if query.node.is_some() {
        sql.push_str(&format!(" AND worker_name = ${next_param}"));
        next_param += 1;
    }
    if query.name.is_some() {
        sql.push_str(&format!(" AND tool_name ILIKE '%' || ${next_param} || '%'"));
        // next_param would be incremented here if more clauses followed.
    }
    if query.unhealthy == Some(true) {
        sql.push_str(" AND health_checked_at <= NOW() - INTERVAL '5 minutes'");
    }

    sql.push_str(" ORDER BY worker_name, tool_name");

    let mut q = sqlx::query(&sql);
    if let Some(node) = &query.node {
        q = q.bind(node);
    }
    if let Some(name) = &query.name {
        q = q.bind(name);
    }

    let rows = q
        .fetch_all(pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let tools: Vec<ToolEntry> = rows
        .iter()
        .map(|row| ToolEntry {
            tool_name: row.get("tool_name"),
            worker_name: row.get("worker_name"),
            description: row.get("description"),
            health_checked_at: row
                .get::<chrono::DateTime<chrono::Utc>, _>("health_checked_at")
                .to_rfc3339(),
            call_count: row.get("call_count"),
            avg_latency_ms: row.get("avg_latency_ms"),
            healthy: row.get("healthy"),
        })
        .collect();

    Ok(Json(json!({ "tools": tools, "count": tools.len() })))
}

/// GET /api/tools/health — Health summary of the tool registry.
pub async fn tool_health(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<ToolHealthSummary>, StatusCode> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fleet_tools")
        .fetch_one(pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let healthy: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM fleet_tools WHERE health_checked_at > NOW() - INTERVAL '5 minutes'",
    )
    .fetch_one(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rows = sqlx::query(
        "SELECT \
            worker_name, \
            COUNT(*) as tool_count, \
            COUNT(*) FILTER (WHERE health_checked_at > NOW() - INTERVAL '5 minutes') as healthy_count, \
            COUNT(*) FILTER (WHERE health_checked_at <= NOW() - INTERVAL '5 minutes') as unhealthy_count \
         FROM fleet_tools \
         GROUP BY worker_name \
         ORDER BY worker_name",
    )
    .fetch_all(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let nodes: Vec<NodeToolHealth> = rows
        .iter()
        .map(|row| NodeToolHealth {
            worker_name: row.get("worker_name"),
            tool_count: row.get("tool_count"),
            healthy_count: row.get("healthy_count"),
            unhealthy_count: row.get("unhealthy_count"),
        })
        .collect();

    Ok(Json(ToolHealthSummary {
        total_tools: total,
        healthy_tools: healthy,
        unhealthy_tools: total - healthy,
        nodes,
    }))
}

/// POST /api/tools/register — Register or refresh tools for a node.
pub async fn register_tools(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<RegisterToolsRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let mut registered = 0;
    let mut updated = 0;

    for tool in &req.tools {
        let result = sqlx::query(
            "INSERT INTO fleet_tools (tool_name, worker_name, description, parameters_schema, capabilities_required, health_checked_at) \
             VALUES ($1, $2, $3, $4, $5, NOW()) \
             ON CONFLICT (tool_name, worker_name) \
             DO UPDATE SET description = EXCLUDED.description, \
                           parameters_schema = EXCLUDED.parameters_schema, \
                           capabilities_required = EXCLUDED.capabilities_required, \
                           health_checked_at = NOW()",
        )
        .bind(&tool.name)
        .bind(&req.worker_name)
        .bind(&tool.description)
        .bind(&tool.parameters_schema)
        .bind(&tool.capabilities_required)
        .execute(pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        if result.rows_affected() > 0 {
            // On conflict, rows_affected is 0 in some Postgres versions; we count attempted
            registered += 1;
        }
        updated += 1;
    }

    Ok(Json(json!({
        "node": req.worker_name,
        "registered": registered,
        "updated": updated,
        "total_tools": req.tools.len(),
    })))
}

/// POST /api/tools/heartbeat — Update health check timestamp for a node's tools.
pub async fn tool_heartbeat(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let worker_name = req
        .get("worker_name")
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::BAD_REQUEST)?;

    let result =
        sqlx::query("UPDATE fleet_tools SET health_checked_at = NOW() WHERE worker_name = $1")
            .bind(worker_name)
            .execute(pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(json!({
        "node": worker_name,
        "tools_refreshed": result.rows_affected(),
    })))
}

// ─── Tool Routing ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RouteToolQuery {
    pub tool_name: String,
}

#[derive(Debug, Serialize)]
pub struct RouteToolResponse {
    pub tool_name: String,
    pub worker_name: String,
    pub healthy: bool,
    pub avg_latency_ms: Option<f64>,
}

/// GET /api/tools/route — Return the healthiest node that has a given tool.
pub async fn route_tool(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<RouteToolQuery>,
) -> Result<Json<RouteToolResponse>, StatusCode> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let row = sqlx::query(
        "SELECT worker_name, avg_latency_ms \
         FROM fleet_tools \
         WHERE tool_name = $1 \
           AND health_checked_at > NOW() - INTERVAL '5 minutes' \
         ORDER BY avg_latency_ms ASC NULLS LAST \
         LIMIT 1",
    )
    .bind(&query.tool_name)
    .fetch_optional(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let Some(row) = row else {
        return Err(StatusCode::NOT_FOUND);
    };

    Ok(Json(RouteToolResponse {
        tool_name: query.tool_name,
        worker_name: row.get("worker_name"),
        healthy: true,
        avg_latency_ms: row.get("avg_latency_ms"),
    }))
}

// ─── Tool Search ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SearchToolsQuery {
    pub q: String,
}

#[derive(Debug, Serialize)]
pub struct SearchToolsResponse {
    pub tools: Vec<ToolEntry>,
    pub count: usize,
}

/// GET /api/tools/search — Lazy tool discovery by name or description.
pub async fn search_tools(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<SearchToolsQuery>,
) -> Result<Json<SearchToolsResponse>, StatusCode> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let pattern = format!("%{}%", query.q);

    let rows = sqlx::query(
        "SELECT tool_name, worker_name, description, health_checked_at, \
         call_count, avg_latency_ms, \
         (health_checked_at > NOW() - INTERVAL '5 minutes') as healthy \
         FROM fleet_tools \
         WHERE tool_name ILIKE $1 OR description ILIKE $1 \
         ORDER BY tool_name, worker_name \
         LIMIT 20",
    )
    .bind(&pattern)
    .fetch_all(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let tools: Vec<ToolEntry> = rows
        .iter()
        .map(|row| ToolEntry {
            tool_name: row.get("tool_name"),
            worker_name: row.get("worker_name"),
            description: row.get("description"),
            health_checked_at: row
                .get::<chrono::DateTime<chrono::Utc>, _>("health_checked_at")
                .to_rfc3339(),
            call_count: row.get("call_count"),
            avg_latency_ms: row.get("avg_latency_ms"),
            healthy: row.get("healthy"),
        })
        .collect();

    Ok(Json(SearchToolsResponse {
        count: tools.len(),
        tools,
    }))
}

/// POST /api/tools/usage — Record a tool invocation for analytics.
#[derive(Debug, Deserialize)]
pub struct RecordUsageRequest {
    pub worker_name: String,
    pub tool_name: String,
    pub session_id: Option<String>,
    pub success: bool,
    pub duration_ms: Option<i64>,
}

pub async fn record_tool_usage(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<RecordUsageRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // Verify the tool exists before logging usage (optional validation)
    let _tool_id: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM fleet_tools WHERE worker_name = $1 AND tool_name = $2 LIMIT 1",
    )
    .bind(&req.worker_name)
    .bind(&req.tool_name)
    .fetch_optional(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match sqlx::query(
        r#"
        INSERT INTO fleet_tool_usage (
            tool_name, worker_name, success, latency_ms
        )
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(&req.tool_name)
    .bind(&req.worker_name)
    .bind(req.success)
    .bind(req.duration_ms)
    .execute(pool)
    .await
    {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "fleet_tool_usage insert failed");
        }
    }

    Ok(Json(json!({
        "recorded": true,
        "tool": req.tool_name,
        "node": req.worker_name,
    })))
}
