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
    let locked = ctx.state.read().await;
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
