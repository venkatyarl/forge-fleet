//! Explorer API — read-only REST surface for web-forge-fleet features that
//! have no dedicated HTTP route of their own.
//!
//! Endpoints:
//!   - `GET /api/cortex/{tool}`   — allowlisted read-only Cortex code-graph
//!     tools, forwarded to the in-process MCP server (`tools/call`). The
//!     anonymous trusted-LAN policy only permits GETs, so this exists as a
//!     GET proxy instead of letting the web call `POST /mcp` directly.
//!   - `GET /api/training/jobs`   — LoRA/finetune jobs, mirroring `ff train list`.
//!   - `GET /api/jira/status`     — Jira monitor configs, watch-state summary,
//!     and recent action-log entries.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use sqlx::Row;

use crate::server::GatewayState;

// ─── Cortex proxy ────────────────────────────────────────────────────────────

/// Read-only Cortex tools the web console may invoke. Anything not listed
/// here is rejected (no indexing/embedding/mutation over anonymous GET).
const CORTEX_TOOL_ALLOWLIST: &[&str] = &[
    "cortex_corpora",
    "cortex_find",
    "cortex_search",
    "cortex_cross_repo_find",
    "cortex_context",
    "cortex_show",
    "cortex_outline",
    "cortex_callers",
    "cortex_callees",
    "cortex_impact",
    "cortex_affected_flows",
    "cortex_path",
    "cortex_tests",
    "cortex_explain",
    "cortex_readers",
    "cortex_writers",
    "cortex_config_key",
    "cortex_deps",
];

/// Query params that must reach the tool as JSON numbers/bools rather than
/// strings (MCP tool schemas are typed).
const CORTEX_INT_PARAMS: &[&str] = &[
    "limit",
    "max_depth",
    "max_callers",
    "max_callees",
    "members",
    "context",
    "max_lines",
];
const CORTEX_BOOL_PARAMS: &[&str] = &["semantic", "all_corpora", "transitive", "include_snippet"];

fn mcp_server() -> &'static Arc<ff_mcp::McpServer> {
    static SERVER: std::sync::OnceLock<Arc<ff_mcp::McpServer>> = std::sync::OnceLock::new();
    SERVER.get_or_init(|| Arc::new(ff_mcp::McpServer::new()))
}

/// `GET /api/cortex/{tool}?arg=value…` — run a read-only Cortex tool and
/// return its result with the MCP envelope unwrapped.
pub async fn cortex_tool(
    Path(tool): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let tool_name = format!("cortex_{}", tool.strip_prefix("cortex_").unwrap_or(&tool));
    if !CORTEX_TOOL_ALLOWLIST.contains(&tool_name.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown or disallowed cortex tool: {tool_name}") })),
        )
            .into_response();
    }

    let mut args = serde_json::Map::new();
    for (key, value) in params {
        let coerced = if CORTEX_INT_PARAMS.contains(&key.as_str()) {
            value.parse::<i64>().map(Value::from).unwrap_or(Value::from(value))
        } else if CORTEX_BOOL_PARAMS.contains(&key.as_str()) {
            Value::from(matches!(value.as_str(), "true" | "1" | "yes"))
        } else {
            Value::from(value)
        };
        args.insert(key, coerced);
    }

    let request = ff_mcp::protocol::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: Some(json!({ "name": tool_name, "arguments": Value::Object(args) })),
        id: Some(json!(1)),
    };

    let Some(response) = mcp_server().handle_request(request).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "mcp server returned no response" })),
        )
            .into_response();
    };

    if let Some(error) = response.error {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": error.message, "detail": error.data })),
        )
            .into_response();
    }

    let result = response.result.unwrap_or(Value::Null);
    // MCP tool results are { content: [ { type: "text", text: "<json>" } ] } —
    // unwrap the text payload and re-parse it as JSON when possible so the
    // web console gets structured data instead of an escaped string.
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("tool error");
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": text }))).into_response();
    }
    if let Some(text) = result.pointer("/content/0/text").and_then(Value::as_str) {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            return Json(parsed).into_response();
        }
        return Json(json!({ "text": text })).into_response();
    }
    Json(result).into_response()
}

// ─── Training jobs ───────────────────────────────────────────────────────────

/// `GET /api/training/jobs` — all training jobs, newest first (`ff train list`).
pub async fn training_jobs(State(state): State<Arc<GatewayState>>) -> Response {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "operational store unavailable" })),
        )
            .into_response();
    };

    let rows = match sqlx::query(
        "SELECT id, name, base_model_id, training_data_path, adapter_output_path, \
                training_type, status, started_at, completed_at, loss_curve, params, \
                result_model_id, error_message, created_at, created_by \
           FROM training_jobs ORDER BY created_at DESC LIMIT 200",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("training jobs query failed: {err}") })),
            )
                .into_response();
        }
    };

    let jobs: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<uuid::Uuid, _>("id"),
                "name": row.get::<String, _>("name"),
                "base_model_id": row.get::<Option<String>, _>("base_model_id"),
                "training_data_path": row.get::<Option<String>, _>("training_data_path"),
                "adapter_output_path": row.get::<Option<String>, _>("adapter_output_path"),
                "training_type": row.get::<Option<String>, _>("training_type"),
                "status": row.get::<Option<String>, _>("status"),
                "started_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at"),
                "completed_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at"),
                "loss_curve": row.get::<Option<Value>, _>("loss_curve"),
                "params": row.get::<Option<Value>, _>("params"),
                "result_model_id": row.get::<Option<String>, _>("result_model_id"),
                "error_message": row.get::<Option<String>, _>("error_message"),
                "created_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("created_at"),
                "created_by": row.get::<Option<String>, _>("created_by"),
            })
        })
        .collect();

    Json(json!({ "jobs": jobs })).into_response()
}

// ─── Jira monitor status ─────────────────────────────────────────────────────

/// `GET /api/jira/status` — monitor configs, per-config watch-state summary,
/// and the most recent action-log entries.
pub async fn jira_status(State(state): State<Arc<GatewayState>>) -> Response {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "operational store unavailable" })),
        )
            .into_response();
    };

    let configs = sqlx::query(
        "SELECT name, project_key, poll_interval_s, retag_after_s, queue_jql, ruleset_id, version \
           FROM jira_configs ORDER BY name",
    )
    .fetch_all(pool)
    .await;

    let Ok(configs) = configs else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "jira_configs query failed" })),
        )
            .into_response();
    };

    let mut out = Vec::with_capacity(configs.len());
    for config in &configs {
        let name: String = config.get("name");
        let summary = sqlx::query(
            "SELECT count(*) AS watched, \
                    count(*) FILTER (WHERE awaiting_party IS NOT NULL) AS awaiting, \
                    max(next_action_at) AS next_action_at \
               FROM jira_watch_state WHERE config_id = $1",
        )
        .bind(&name)
        .fetch_one(pool)
        .await;

        let (watched, awaiting, next_action_at) = match summary {
            Ok(row) => (
                row.get::<i64, _>("watched"),
                row.get::<i64, _>("awaiting"),
                row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("next_action_at"),
            ),
            Err(_) => (0, 0, None),
        };

        out.push(json!({
            "name": name,
            "project_key": config.get::<Option<String>, _>("project_key"),
            "poll_interval_s": config.get::<Option<i32>, _>("poll_interval_s"),
            "retag_after_s": config.get::<Option<i32>, _>("retag_after_s"),
            "queue_jql": config.get::<Option<String>, _>("queue_jql"),
            "ruleset_id": config.get::<Option<String>, _>("ruleset_id"),
            "version": config.get::<Option<i32>, _>("version"),
            "watched_issues": watched,
            "awaiting_issues": awaiting,
            "next_action_at": next_action_at,
        }));
    }

    let recent = sqlx::query(
        "SELECT event_key, config_id, issue_id, kind, created_at \
           FROM jira_action_log ORDER BY created_at DESC LIMIT 50",
    )
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.iter()
            .map(|row| {
                json!({
                    "event_key": row.get::<Option<String>, _>("event_key"),
                    "config_id": row.get::<Option<String>, _>("config_id"),
                    "issue_id": row.get::<Option<String>, _>("issue_id"),
                    "kind": row.get::<Option<String>, _>("kind"),
                    "created_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("created_at"),
                })
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    Json(json!({ "configs": out, "recent_events": recent })).into_response()
}
