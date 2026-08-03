//! Workstreams API — read-only session-of-record view for the web console.
//!
//! Mirrors `ff workstream status`/`list` over HTTP so web-forge-fleet can show
//! which CLI sessions (claude/codex/kimi) are attached to each project
//! workstream without shelling out to the CLI.
//!
//! Endpoints:
//!   - `GET /api/workstreams` — all active workstreams with attached clients

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use sqlx::Row;

use crate::server::GatewayState;

/// `GET /api/workstreams` — every active workstream, its summary/focus, the
/// recent activity log (`open_threads`), and its currently attached clients.
pub async fn list_workstreams(State(state): State<Arc<GatewayState>>) -> Response {
    let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "operational store unavailable" })),
        )
            .into_response();
    };

    let rows = match sqlx::query(
        "SELECT id, project_key, basename, git_remote, goal, working_summary, focus, \
                open_threads, status \
           FROM ff_workstreams WHERE status = 'active' ORDER BY project_key",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("workstream query failed: {err}") })),
            )
                .into_response();
        }
    };

    let mut workstreams_out: Vec<Value> = Vec::with_capacity(rows.len());
    for row in rows {
        let id: uuid::Uuid = row.get("id");
        let clients = match ff_agent::workstreams::attached_clients(pool, id).await {
            Ok(clients) => clients
                .into_iter()
                .map(|c| {
                    json!({
                        "session_id": c.session_id,
                        "node": c.worker_name,
                        "tool": c.tool,
                        "goal": c.goal,
                        "status": c.status,
                        "last_report_at": c.last_report_at,
                    })
                })
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };

        workstreams_out.push(json!({
            "project_key": row.get::<String, _>("project_key"),
            "name": row.get::<Option<String>, _>("basename"),
            "remote": row.get::<Option<String>, _>("git_remote"),
            "goal": row.get::<Option<String>, _>("goal"),
            "working_summary": row.get::<Option<String>, _>("working_summary"),
            "focus": row.get::<Option<String>, _>("focus"),
            "open_threads": row.get::<Value, _>("open_threads"),
            "status": row.get::<String, _>("status"),
            "clients": clients,
        }));
    }

    Json(json!({ "workstreams": workstreams_out })).into_response()
}
