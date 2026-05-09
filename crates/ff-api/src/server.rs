use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode, header},
    routing::{get, post},
};
use chrono::Utc;
use dashmap::DashMap;
use serde::Serialize;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::warn;

use crate::{
    adaptive_router::AdaptiveRouter,
    circuit_breaker::CircuitState,
    error::ApiError,
    quality_tracker::QualityTracker,
    registry::BackendRegistry,
    router::{ModelRouter, TierRouter},
    types::{
        ChatCompletionRequest, ChatMessage, CompletionRequest, HealthResponse, ModelInfo,
        ModelListResponse,
    },
};

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<BackendRegistry>,
    pub model_router: Arc<ModelRouter>,
    pub adaptive_router: Arc<AdaptiveRouter>,
    pub http_client: reqwest::Client,
    pub request_metrics: Arc<RequestMetrics>,
}

impl AppState {
    pub fn new(registry: Arc<BackendRegistry>) -> anyhow::Result<Self> {
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to create reqwest client")?;

        let model_router = Arc::new(ModelRouter::new(registry.clone()));
        let tier_router = Arc::new(TierRouter::with_defaults(registry.clone()));
        let quality_tracker = Arc::new(QualityTracker::with_defaults());
        let adaptive_router = Arc::new(AdaptiveRouter::with_defaults(
            registry.clone(),
            tier_router,
            quality_tracker,
        ));

        Ok(Self {
            registry,
            model_router,
            adaptive_router,
            http_client,
            request_metrics: Arc::new(RequestMetrics::default()),
        })
    }
}

/// Simple Prometheus-style metrics collector.
#[derive(Default)]
pub struct RequestMetrics {
    requests_total: DashMap<(String, String), u64>,
    duration_ms_total: DashMap<(String, String), u64>,
}

impl RequestMetrics {
    pub fn record(&self, node: &str, model: &str, duration_ms: u64) {
        *self
            .requests_total
            .entry((node.to_string(), model.to_string()))
            .or_insert(0) += 1;
        *self
            .duration_ms_total
            .entry((node.to_string(), model.to_string()))
            .or_insert(0) += duration_ms;
    }
}

pub fn build_http_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
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

pub async fn metrics(State(state): State<Arc<AppState>>) -> Response<Body> {
    let mut lines = Vec::new();

    // forgefleet_requests_total{node, model}
    for entry in state.request_metrics.requests_total.iter() {
        let (node, model) = entry.key();
        lines.push(format!(
            "forgefleet_requests_total{{node=\"{}\",model=\"{}\"}} {}",
            node,
            model,
            entry.value()
        ));
    }

    // forgefleet_request_duration_ms{node, model}
    for entry in state.request_metrics.duration_ms_total.iter() {
        let (node, model) = entry.key();
        lines.push(format!(
            "forgefleet_request_duration_ms{{node=\"{}\",model=\"{}\"}} {}",
            node,
            model,
            entry.value()
        ));
    }

    // forgefleet_circuit_breaker_state{node} (0=closed, 1=open, 2=halfopen)
    for entry in state.adaptive_router.circuit_breakers().iter() {
        let node = entry.key();
        let state_num = match entry.value().state() {
            CircuitState::Closed => 0,
            CircuitState::Open => 1,
            CircuitState::HalfOpen => 2,
        };
        lines.push(format!(
            "forgefleet_circuit_breaker_state{{node=\"{}\"}} {}",
            node, state_num
        ));
    }

    let body = lines.join("\n");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
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
    forward_chat_adaptive(
        state,
        "/v1/chat/completions",
        &payload.model,
        &payload.messages,
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

async fn forward_chat_adaptive<T: Serialize>(
    state: Arc<AppState>,
    path: &str,
    model: &str,
    messages: &[ChatMessage],
    stream: bool,
    payload: &T,
) -> Result<Response<Body>, ApiError> {
    let start = Instant::now();
    let route_chain = state
        .adaptive_router
        .route_chain_with_fallback(model, messages)
        .await;

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
                let latency = start.elapsed().as_millis() as u64;
                if is_busy_status(upstream.status()) {
                    state
                        .adaptive_router
                        .circuit_breakers()
                        .entry(backend.node.clone())
                        .or_default()
                        .record_failure();
                    last_error = Some(format!(
                        "{} responded {} (busy)",
                        backend.id,
                        upstream.status()
                    ));
                    continue;
                }

                state
                    .request_metrics
                    .record(&backend.node, &backend.model, latency);
                state
                    .adaptive_router
                    .circuit_breakers()
                    .entry(backend.node.clone())
                    .or_default()
                    .record_success();

                if stream {
                    return passthrough_streaming_response(upstream).await;
                }
                return passthrough_response(upstream).await;
            }
            Err(error) => {
                warn!(backend = %backend.id, %error, "upstream request failed; trying fallback");
                state
                    .adaptive_router
                    .circuit_breakers()
                    .entry(backend.node.clone())
                    .or_default()
                    .record_failure();
                last_error = Some(format!("{} request failed: {}", backend.id, error));
            }
        }
    }

    Err(ApiError::BackendUnavailable(last_error.unwrap_or_else(
        || "all fallback backends failed".to_string(),
    )))
}

async fn forward_with_fallback<T: Serialize>(
    state: Arc<AppState>,
    path: &str,
    model: &str,
    stream: bool,
    payload: &T,
) -> Result<Response<Body>, ApiError> {
    let start = Instant::now();
    let route_chain = state.model_router.route_chain(model).await;

    if route_chain.is_empty() {
        return Err(ApiError::BackendUnavailable(format!(
            "no healthy backend for model selector '{model}'"
        )));
    }

    let mut last_error = None::<String>;

    for backend in route_chain {
        // Check circuit breaker
        let allowed = state
            .adaptive_router
            .circuit_breakers()
            .get(&backend.node)
            .map(|cb| cb.allow_request())
            .unwrap_or(true);
        if !allowed {
            last_error = Some(format!("{} circuit breaker open", backend.id));
            continue;
        }

        let url = format!("{}{}", backend.base_url(), path);

        match state.http_client.post(&url).json(payload).send().await {
            Ok(upstream) => {
                let latency = start.elapsed().as_millis() as u64;
                if is_busy_status(upstream.status()) {
                    state
                        .adaptive_router
                        .circuit_breakers()
                        .entry(backend.node.clone())
                        .or_default()
                        .record_failure();
                    last_error = Some(format!(
                        "{} responded {} (busy)",
                        backend.id,
                        upstream.status()
                    ));
                    continue;
                }

                state
                    .request_metrics
                    .record(&backend.node, &backend.model, latency);
                state
                    .adaptive_router
                    .circuit_breakers()
                    .entry(backend.node.clone())
                    .or_default()
                    .record_success();

                if stream {
                    return passthrough_streaming_response(upstream).await;
                }

                return passthrough_response(upstream).await;
            }
            Err(error) => {
                warn!(backend = %backend.id, %error, "upstream request failed; trying fallback");
                state
                    .adaptive_router
                    .circuit_breakers()
                    .entry(backend.node.clone())
                    .or_default()
                    .record_failure();
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
