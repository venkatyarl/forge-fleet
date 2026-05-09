//! A2A Axum server handlers.

use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use tracing::info;
use uuid::Uuid;

use crate::card::AgentCard;
use crate::task::{Task, TaskMessage, TaskStatus, TaskUpdate};

/// Mount A2A routes onto the given router.
pub fn routes(card: AgentCard) -> Router {
    Router::new()
        .route("/.well-known/agent.json", get(move || async move { Json(card.clone()) }))
        .route("/tasks/send", post(handle_send_task))
        .route("/tasks/{task_id}/updates", get(handle_task_updates))
}

async fn handle_send_task(Json(body): Json<Value>) -> Result<Json<Task>, StatusCode> {
    let messages: Vec<TaskMessage> = serde_json::from_value(
        body.get("messages").cloned().unwrap_or(json!([])),
    )
    .map_err(|_| StatusCode::BAD_REQUEST)?;

    let task = Task {
        id: Uuid::new_v4(),
        session_id: None,
        status: TaskStatus::Submitted,
        messages,
        artifacts: vec![],
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    info!(task_id = %task.id, "a2a task received");
    Ok(Json(task))
}

async fn handle_task_updates(Path(task_id): Path<Uuid>) -> Result<String, StatusCode> {
    // In a real implementation this would stream SSE updates.
    // For now return a static SSE heartbeat.
    let update = TaskUpdate {
        task_id,
        status: TaskStatus::Working,
        message: None,
        artifact: None,
        timestamp: chrono::Utc::now(),
    };
    let json = serde_json::to_string(&update).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(format!("data: {}\n\n", json))
}
