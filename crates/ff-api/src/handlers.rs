use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Response, header},
    routing::{get, post},
};
use chrono::Utc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{
    error::ApiError,
    registry::BackendRegistry,
    router::ModelRouter,
    types::{
        ChatCompletionRequest, CompletionRequest, HealthResponse, ModelInfo, ModelListResponse,
    },
};

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<BackendRegistry>,
    pub model_router: Arc<ModelRouter>,
    pub http_client: reqwest::Client,
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let stats = state.registry.stats().await;

    Json(HealthResponse {
        status: "ok".to_string(),
        total_backends: stats.total,
        healthy_backends: stats.healthy,
        busy_backends: stats.busy,
    })
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ModelListResponse>, ApiError> {
    let now = Utc::now().timestamp();
    let data = state
        .registry
        .available_models()
        .await
        .into_iter()
        .map(|(model, tier)| ModelInfo {
            id: model,
            object: "model".to_string(),
            created: now,
            owned_by: "forgefleet".to_string(),
            tier,
        })
        .collect();

    Ok(Json(ModelListResponse {
        object: "list".to_string(),
        data,
    }))
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ChatCompletionRequest>,
) -> Result<Response<Body>, ApiError> {
    let stream = payload.stream.unwrap_or(false);
    let backend = state
        .model_router
        .route(&payload.model)
        .await
        .ok_or_else(|| {
            ApiError::BackendUnavailable(format!("no healthy backend for '{}'", payload.model))
        })?;

    let upstream = state
        .http_client
        .post(format!("{}/v1/chat/completions", backend.base_url()))
        .json(&payload)
        .send()
        .await?;

    if stream {
        return passthrough_streaming_response(upstream).await;
    }

    passthrough_response(upstream).await
}

pub async fn completions(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CompletionRequest>,
) -> Result<Response<Body>, ApiError> {
    let backend = state
        .model_router
        .route(&payload.model)
        .await
        .ok_or_else(|| {
            ApiError::BackendUnavailable(format!("no healthy backend for '{}'", payload.model))
        })?;

    let upstream = state
        .http_client
        .post(format!("{}/v1/completions", backend.base_url()))
        .json(&payload)
        .send()
        .await?;

    passthrough_response(upstream).await
}

async fn passthrough_response(upstream: reqwest::Response) -> Result<Response<Body>, ApiError> {
    let status = upstream.status();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = upstream.bytes().await?;

    let mut response = Response::builder().status(status);
    if let Some(content_type) = content_type {
        response = response.header(header::CONTENT_TYPE, content_type);
    }

    response
        .body(Body::from(bytes))
        .map_err(|error| ApiError::internal(error.to_string()))
}

async fn passthrough_streaming_response(
    upstream: reqwest::Response,
) -> Result<Response<Body>, ApiError> {
    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| header::HeaderValue::from_static("text/event-stream; charset=utf-8"));

    let stream = upstream.bytes_stream();

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .map_err(|error| ApiError::internal(error.to_string()))
}
