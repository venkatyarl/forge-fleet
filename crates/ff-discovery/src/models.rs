use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCard {
    pub id: String,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub created: Option<u64>,
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelListResponse {
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub data: Vec<ModelCard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointModelInfo {
    pub endpoint: String,
    pub queried_at: DateTime<Utc>,
    pub latency_ms: u128,
    pub models: Vec<ModelCard>,
    pub error: Option<String>,
}

/// Query a single OpenAI-compatible endpoint for available models via GET /v1/models.
pub async fn query_models_endpoint(endpoint: &str, timeout: Duration) -> EndpointModelInfo {
    let endpoint = endpoint.trim_end_matches('/').to_string();
    let url = format!("{endpoint}/v1/models");
    let started = Instant::now();
    let queried_at = Utc::now();

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    match client.get(&url).send().await {
        Ok(response) => {
            let status = response.status();
            if !status.is_success() {
                return EndpointModelInfo {
                    endpoint,
                    queried_at,
                    latency_ms: started.elapsed().as_millis(),
                    models: vec![],
                    error: Some(format!("HTTP {} from {}", status.as_u16(), url)),
                };
            }

            match response.json::<ModelListResponse>().await {
                Ok(payload) => EndpointModelInfo {
                    endpoint,
                    queried_at,
                    latency_ms: started.elapsed().as_millis(),
                    models: payload.data,
                    error: None,
                },
                Err(err) => EndpointModelInfo {
                    endpoint,
                    queried_at,
                    latency_ms: started.elapsed().as_millis(),
                    models: vec![],
                    error: Some(format!("invalid JSON payload from {}: {}", url, err)),
                },
            }
        }
        Err(err) => EndpointModelInfo {
            endpoint,
            queried_at,
            latency_ms: started.elapsed().as_millis(),
            models: vec![],
            error: Some(err.to_string()),
        },
    }
}

/// Query multiple endpoints in parallel.
pub async fn query_models_endpoints(
    endpoints: &[String],
    timeout: Duration,
) -> Vec<EndpointModelInfo> {
    let mut tasks = JoinSet::new();

    for endpoint in endpoints {
        let endpoint = endpoint.clone();
        tasks.spawn(async move { query_models_endpoint(&endpoint, timeout).await });
    }

    let mut results = Vec::with_capacity(endpoints.len());
    while let Some(result) = tasks.join_next().await {
        if let Ok(info) = result {
            results.push(info);
        }
    }

    results.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    results
}
