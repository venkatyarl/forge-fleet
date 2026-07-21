use crate::state::{AgentStatus, SharedState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
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
    pub auth_secret: String,
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

async fn status(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
) -> Result<Json<AgentStatus>, (StatusCode, String)> {
    authorize(&ctx.auth_secret, "GET", "/status", &headers, "")?;
    let locked: tokio::sync::RwLockReadGuard<'_, crate::state::AgentState> = ctx.state.read().await;
    Ok(Json(locked.to_status()))
}

#[derive(Debug, Serialize)]
struct AssignmentAccepted {
    accepted: bool,
    message: String,
}

async fn assign_task(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<AssignmentAccepted>, (StatusCode, String)> {
    authorize(&ctx.auth_secret, "POST", "/assign", &headers, &body)?;
    let task = serde_json::from_str(&body)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("invalid JSON: {err}")))?;
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
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    authorize(&ctx.auth_secret, "POST", "/agent/message", &headers, &body)?;
    let payload: serde_json::Value = serde_json::from_str(&body)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("invalid JSON: {err}")))?;
    let from = payload
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let to = payload
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
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

    Ok(Json(serde_json::json!({ "ok": true, "received": true })))
}

fn authorize(
    secret: &str,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<(), (StatusCode, String)> {
    ff_agent::http_auth::authorize(secret, method, path, headers, body)
        .map_err(|message| (StatusCode::UNAUTHORIZED, message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    fn test_router() -> Router {
        let (task_tx, _task_rx) = mpsc::channel(1);
        build_router(AppContext {
            state: Arc::new(RwLock::new(crate::state::AgentState::new(
                "test-node".to_string(),
                ff_discovery::detect_hardware_profile(),
            ))),
            task_tx,
            auth_secret: "test-control-secret".to_string(),
        })
    }

    #[tokio::test]
    async fn assign_rejects_unsigned_and_accepts_authenticated_requests() {
        let app = test_router();
        let body = "{}";
        let unsigned = app
            .clone()
            .oneshot(
                Request::post("/assign")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);

        let timestamp = Utc::now().timestamp();
        let signature = ff_security::computer_auth::sign_request(
            "test-control-secret",
            "POST",
            "/assign",
            timestamp,
            body,
        );
        let authenticated = app
            .oneshot(
                Request::post("/assign")
                    .header("content-type", "application/json")
                    .header(ff_agent::http_auth::TIMESTAMP_HEADER, timestamp)
                    .header(ff_agent::http_auth::SIGNATURE_HEADER, signature)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::BAD_REQUEST);
    }
}
