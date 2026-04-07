use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::collector::{MetricSummary, RequestSample, summarize};
use crate::scenarios::BenchmarkScenario;

#[derive(Debug, Error)]
pub enum BenchmarkRunnerError {
    #[error("request failed after retries: {0}")]
    RequestFailed(String),
    #[error("failed to acquire concurrency permit")]
    Permit,
    #[error("task join error: {0}")]
    Join(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

pub type RunnerResult<T> = std::result::Result<T, BenchmarkRunnerError>;

/// Node/model endpoint used for benchmark execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkEndpoint {
    pub node_name: String,
    pub base_url: String,
    pub default_model: Option<String>,
    pub api_key: Option<String>,
}

impl BenchmarkEndpoint {
    pub fn id(&self) -> String {
        format!("{} ({})", self.node_name, self.base_url)
    }
}

/// Runtime behavior for the benchmark runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: 120,
            max_retries: 1,
            retry_backoff_ms: 250,
        }
    }
}

/// Result for one endpoint within a scenario run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointRunResult {
    pub endpoint: BenchmarkEndpoint,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub metrics: MetricSummary,
    pub samples: Vec<RequestSample>,
}

/// Result of executing one scenario across multiple endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioRun {
    pub run_id: Uuid,
    pub scenario: BenchmarkScenario,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub endpoint_results: Vec<EndpointRunResult>,
}

/// Executes benchmark scenarios against model-serving endpoints.
#[derive(Debug, Clone)]
pub struct BenchmarkRunner {
    client: Client,
    config: RunnerConfig,
}

impl BenchmarkRunner {
    pub fn new(config: RunnerConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .context("failed to build reqwest client for benchmark runner")?;

        Ok(Self { client, config })
    }

    pub async fn run_suite(
        &self,
        scenarios: &[BenchmarkScenario],
        endpoints: &[BenchmarkEndpoint],
    ) -> anyhow::Result<Vec<ScenarioRun>> {
        let mut runs = Vec::with_capacity(scenarios.len());

        for scenario in scenarios {
            runs.push(self.run_scenario(scenario, endpoints).await?);
        }

        Ok(runs)
    }

    pub async fn run_scenario(
        &self,
        scenario: &BenchmarkScenario,
        endpoints: &[BenchmarkEndpoint],
    ) -> anyhow::Result<ScenarioRun> {
        let started_at = Utc::now();
        let mut endpoint_results = Vec::with_capacity(endpoints.len());

        for endpoint in endpoints {
            endpoint_results.push(
                self.run_on_endpoint(scenario, endpoint)
                    .await
                    .with_context(|| {
                        format!(
                            "scenario '{}' failed on endpoint '{}'",
                            scenario.name,
                            endpoint.id()
                        )
                    })?,
            );
        }

        Ok(ScenarioRun {
            run_id: Uuid::new_v4(),
            scenario: scenario.clone(),
            started_at,
            finished_at: Utc::now(),
            endpoint_results,
        })
    }

    async fn run_on_endpoint(
        &self,
        scenario: &BenchmarkScenario,
        endpoint: &BenchmarkEndpoint,
    ) -> RunnerResult<EndpointRunResult> {
        let started_at = Utc::now();
        let total_requests = scenario.iterations + scenario.warmup_requests;
        let semaphore = Arc::new(Semaphore::new(scenario.concurrency.max(1) as usize));
        let mut join_set = JoinSet::new();

        for iteration in 0..total_requests {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| BenchmarkRunnerError::Permit)?;

            let endpoint = endpoint.clone();
            let scenario = scenario.clone();
            let client = self.client.clone();
            let config = self.config.clone();

            join_set.spawn(async move {
                let _permit = permit;
                let sample =
                    execute_request(&client, &config, &scenario, &endpoint, iteration).await;
                (iteration, sample)
            });
        }

        let mut samples = Vec::with_capacity(scenario.iterations as usize);

        while let Some(joined) = join_set.join_next().await {
            let (iteration, sample) =
                joined.map_err(|err| BenchmarkRunnerError::Join(err.to_string()))?;

            // warmup requests are intentionally discarded from statistical output.
            if iteration < scenario.warmup_requests {
                debug!(
                    scenario = %scenario.name,
                    endpoint = %endpoint.id(),
                    "discarded warmup sample"
                );
                continue;
            }

            samples.push(sample);
        }

        let metrics = summarize(&samples);
        Ok(EndpointRunResult {
            endpoint: endpoint.clone(),
            started_at,
            finished_at: Utc::now(),
            metrics,
            samples,
        })
    }
}

async fn execute_request(
    client: &Client,
    config: &RunnerConfig,
    scenario: &BenchmarkScenario,
    endpoint: &BenchmarkEndpoint,
    iteration: u32,
) -> RequestSample {
    let model = scenario
        .resolve_model_for_iteration(iteration)
        .or_else(|| endpoint.default_model.clone())
        .unwrap_or_else(|| "unknown-model".to_string());

    let url = format!(
        "{}/v1/chat/completions",
        endpoint.base_url.trim_end_matches('/')
    );
    let payload = json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": scenario.request.prompt,
            }
        ],
        "max_tokens": scenario.request.max_tokens,
        "temperature": scenario.request.temperature,
        "stream": false,
    });

    let mut last_error = String::new();

    for attempt in 0..=config.max_retries {
        let started = Instant::now();

        let mut request = client.post(&url).json(&payload);
        if let Some(api_key) = &endpoint.api_key {
            request = request.bearer_auth(api_key);
        }

        match request.send().await {
            Ok(response) => {
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                let status = response.status();
                let headers = response.headers().clone();
                let body_text = response.text().await.unwrap_or_default();

                if status.is_success() {
                    let parsed = serde_json::from_str::<Value>(&body_text).ok();
                    let (prompt_tokens, completion_tokens, total_tokens) =
                        parsed.as_ref().map(parse_usage).unwrap_or((0, 0, 0));

                    let queue_time_ms = parse_queue_time_ms(&headers, parsed.as_ref());

                    return RequestSample {
                        timestamp: Utc::now(),
                        endpoint: endpoint.id(),
                        scenario: scenario.name.clone(),
                        model: model.clone(),
                        success: true,
                        latency_ms: elapsed_ms,
                        queue_time_ms,
                        prompt_tokens,
                        completion_tokens,
                        total_tokens,
                        error: None,
                    };
                }

                last_error = format_status_error(status, &body_text);
                warn!(
                    scenario = %scenario.name,
                    endpoint = %endpoint.id(),
                    status = %status,
                    attempt,
                    "benchmark request failed"
                );
            }
            Err(err) => {
                last_error = err.to_string();
                warn!(
                    scenario = %scenario.name,
                    endpoint = %endpoint.id(),
                    attempt,
                    error = %last_error,
                    "benchmark request error"
                );
            }
        }

        if attempt < config.max_retries {
            let wait = config.retry_backoff_ms * (attempt as u64 + 1);
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }

    RequestSample {
        timestamp: Utc::now(),
        endpoint: endpoint.id(),
        scenario: scenario.name.clone(),
        model,
        success: false,
        latency_ms: 0.0,
        queue_time_ms: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        error: Some(BenchmarkRunnerError::RequestFailed(last_error).to_string()),
    }
}

fn parse_usage(json: &Value) -> (u32, u32, u32) {
    let usage = json.get("usage");

    let prompt = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let completion = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let total = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(prompt as u64 + completion as u64) as u32;

    (prompt, completion, total)
}

fn parse_queue_time_ms(headers: &reqwest::header::HeaderMap, body: Option<&Value>) -> Option<f64> {
    for key in ["x-queue-time-ms", "x-queue-time"] {
        if let Some(value) = headers.get(key)
            && let Ok(s) = value.to_str()
            && let Ok(v) = s.parse::<f64>()
        {
            return Some(v);
        }
    }

    body.and_then(|json| json.get("queue_time_ms").and_then(|v| v.as_f64()))
}

fn format_status_error(status: StatusCode, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return format!("http status {}", status);
    }

    let max = 240;
    if body.len() <= max {
        format!("http status {}: {}", status, body)
    } else {
        format!("http status {}: {}…", status, &body[..max])
    }
}
