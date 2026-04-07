use std::sync::Arc;

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode, header},
    routing::{get, post},
};
use chrono::Utc;
use serde::Serialize;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::warn;

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

impl AppState {
    pub fn new(registry: Arc<BackendRegistry>) -> anyhow::Result<Self> {
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to create reqwest client")?;

        let model_router = Arc::new(ModelRouter::new(registry.clone()));

        Ok(Self {
            registry,
            model_router,
            http_client,
        })
    }
}

pub fn build_http_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .fallback(not_found)
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
    let streaming = payload.stream.unwrap_or(false);
    forward_with_fallback(
        state,
        "/v1/chat/completions",
        &payload.model,
        streaming,
        &payload,
    )
    .await
}

pub async fn completions(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CompletionRequest>,
) -> Result<Response<Body>, ApiError> {
    let streaming = payload.stream.unwrap_or(false);
    forward_with_fallback(
        state,
        "/v1/completions",
        &payload.model,
        streaming,
        &payload,
    )
    .await
}

async fn forward_with_fallback<T: Serialize>(
    state: Arc<AppState>,
    path: &str,
    model: &str,
    stream: bool,
    payload: &T,
) -> Result<Response<Body>, ApiError> {
    let route_chain = state.model_router.route_chain(model).await;

    if route_chain.is_empty() {
        return Err(ApiError::BackendUnavailable(format!(
            "no healthy backend for model selector '{model}'"
        )));
    }

    let mut last_error = None::<String>;

    for backend in route_chain {
        let url = format!("{}{}", backend.base_url(), path);

        match state.http_client.post(&url).json(payload).send().await {
            Ok(upstream) => {
                if is_busy_status(upstream.status()) {
                    last_error = Some(format!(
                        "{} responded {} (busy)",
                        backend.id,
                        upstream.status()
                    ));
                    continue;
                }

                if stream {
                    return passthrough_streaming_response(upstream).await;
                }

                return passthrough_response(upstream).await;
            }
            Err(error) => {
                warn!(backend = %backend.id, %error, "upstream request failed; trying fallback");
                last_error = Some(format!("{} request failed: {}", backend.id, error));
            }
        }
    }

    Err(ApiError::BackendUnavailable(last_error.unwrap_or_else(
        || "all fallback backends failed".to_string(),
    )))
}

fn is_busy_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
    )
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

async fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": {
                "message": "route not found",
                "type": "not_found"
            }
        })),
    )
}
