use crate::state::{AgentStatus, SharedState};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chrono::Utc;
use ff_core::AgentTask;
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::info;

#[derive(Clone)]
pub struct AppContext {
    pub state: SharedState,
    pub task_tx: mpsc::Sender<AgentTask>,
}

pub fn build_router(ctx: AppContext) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/assign", post(assign_task))
        .route("/agent/message", post(receive_message))
        .with_state(ctx)
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    node_id: String,
    role: String,
    activity_level: String,
    timestamp: chrono::DateTime<Utc>,
}

async fn health(State(ctx): State<AppContext>) -> Json<HealthResponse> {
    let locked = ctx.state.read().await;

    Json(HealthResponse {
        ok: true,
        node_id: locked.node_id.clone(),
        role: format!("{:?}", locked.role),
        activity_level: format!("{:?}", locked.activity_level),
        timestamp: Utc::now(),
    })
}

async fn status(State(ctx): State<AppContext>) -> Json<AgentStatus> {
    let locked: tokio::sync::RwLockReadGuard<'_, crate::state::AgentState> = ctx.state.read().await;
    Json(locked.to_status())
}

#[derive(Debug, Serialize)]
struct AssignmentAccepted {
    accepted: bool,
    message: String,
}

async fn assign_task(
    State(ctx): State<AppContext>,
    Json(task): Json<AgentTask>,
) -> Result<Json<AssignmentAccepted>, (StatusCode, String)> {
    ctx.task_tx.send(task).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("executor not available: {e}"),
        )
    })?;

    Ok(Json(AssignmentAccepted {
        accepted: true,
        message: "task queued".to_string(),
    }))
}

async fn receive_message(
    State(_ctx): State<AppContext>,
    Json(payload): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let from = payload.get("from").and_then(|v| v.as_str()).unwrap_or("unknown");
    let to = payload.get("to").and_then(|v| v.as_str()).unwrap_or("unknown");
    let message = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let task_id = payload.get("task_id").and_then(|v| v.as_str());
    let status = payload.get("status").and_then(|v| v.as_str());
    let output = payload.get("output").and_then(|v| v.as_str());

    info!(
        from = %from,
        to = %to,
        task_id = ?task_id,
        status = ?status,
        message = %message,
        "received inter-agent message"
    );

    // If a task completion callback: update in-memory task store.
    if let (Some(tid), Some("completed"), Some(out)) = (task_id, status, output) {
        use ff_agent::tools::task_tools::TASK_STORE_PUB;
        if let Some(mut task) = TASK_STORE_PUB.get_mut(tid) {
            task.status = "completed".to_string();
            task.output = Some(out.to_string());
            task.updated_at = Utc::now().to_rfc3339();
            info!(task_id = %tid, "task marked completed via inter-agent message");
        }
    }

    Json(serde_json::json!({ "ok": true, "received": true }))
}
