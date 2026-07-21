//! Local SLM status route.

use std::{path::Path, sync::Arc, time::Duration};

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::server::AppState;

const DEFAULT_SLM_URL: &str = "http://127.0.0.1:55000";

#[derive(Debug, Serialize)]
pub struct SlmStatusResponse {
    pub model: String,
    pub memory_usage_mb: Option<u64>,
    pub thread_count: Option<usize>,
    pub last_ping: DateTime<Utc>,
    pub online: bool,
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<SlmStatusResponse> {
    let endpoint = std::env::var("FF_AGENT_SLM_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_SLM_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let model = configured_model(&state).await;
    let online = state
        .http_client
        .get(format!("{endpoint}/ping"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success());

    Json(SlmStatusResponse {
        model,
        memory_usage_mb: parse_env("FORGEFLEET_SLM_MEM_BUDGET_MB"),
        thread_count: parse_env("FORGEFLEET_SLM_THREADS"),
        last_ping: Utc::now(),
        online,
    })
}

async fn configured_model(state: &AppState) -> String {
    if let Some(model) = std::env::var_os("FORGEFLEET_SLM_MODEL") {
        return Path::new(&model)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
    }

    state
        .registry
        .all_endpoints()
        .await
        .into_iter()
        .find(|endpoint| endpoint.is_local)
        .map(|endpoint| endpoint.model)
        .unwrap_or_else(|| "Not configured".to_string())
}

fn parse_env<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}
