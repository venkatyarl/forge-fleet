//! Periodic health monitoring for a local small-language-model endpoint.

use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tracing::warn;
use uuid::Uuid;

use crate::nats;

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ERROR_LEN: usize = 1_024;

/// Configuration for one local SLM health monitor.
#[derive(Clone, Debug)]
pub struct SlmMonitorConfig {
    pub computer_id: Uuid,
    pub endpoint: String,
    pub model: String,
    pub interval: Duration,
    pub timeout: Duration,
}

impl SlmMonitorConfig {
    pub fn new(computer_id: Uuid, endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            computer_id,
            endpoint: endpoint.into(),
            model: model.into(),
            interval: DEFAULT_INTERVAL,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Debug)]
struct ProbeResult {
    healthy: bool,
    latency_ms: i64,
    error: Option<String>,
}

/// Spawn a monitor that checks immediately and then at the configured interval.
pub fn spawn_slm_monitor(pool: PgPool, config: SlmMonitorConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(config.timeout).build() {
            Ok(client) => client,
            Err(error) => {
                warn!(%error, "slm monitor could not build HTTP client");
                return;
            }
        };
        let mut ticker = tokio::time::interval(config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            check_once(&pool, &client, &config).await;
        }
    })
}

async fn check_once(pool: &PgPool, client: &reqwest::Client, config: &SlmMonitorConfig) {
    let result = probe(client, config).await;
    let previous = sqlx::query_scalar::<_, bool>(
        "SELECT healthy FROM slm_health_status WHERE computer_id = $1 AND endpoint = $2",
    )
    .bind(config.computer_id)
    .bind(normalize_endpoint(&config.endpoint))
    .fetch_optional(pool)
    .await;

    if let Err(error) = sqlx::query(
        "INSERT INTO slm_health_status \
         (computer_id, endpoint, healthy, checked_at, latency_ms, error) \
         VALUES ($1, $2, $3, NOW(), $4, $5) \
         ON CONFLICT (computer_id, endpoint) DO UPDATE SET \
         healthy = EXCLUDED.healthy, checked_at = EXCLUDED.checked_at, \
         latency_ms = EXCLUDED.latency_ms, error = EXCLUDED.error",
    )
    .bind(config.computer_id)
    .bind(normalize_endpoint(&config.endpoint))
    .bind(result.healthy)
    .bind(result.latency_ms)
    .bind(result.error.as_deref())
    .execute(pool)
    .await
    {
        warn!(%error, "slm monitor failed to record health status");
    }

    if !result.healthy && !matches!(previous, Ok(Some(false))) {
        let error = result.error.as_deref().unwrap_or("SLM health probe failed");
        nats::publish_slm_unhealthy(config.computer_id, &config.endpoint, error).await;
    }
}

async fn probe(client: &reqwest::Client, config: &SlmMonitorConfig) -> ProbeResult {
    let started = Instant::now();
    let base = normalize_endpoint(&config.endpoint);
    let ping_error = match client.get(format!("{base}/ping")).send().await {
        Ok(response) if response.status().is_success() => return successful_probe(started),
        Ok(response) => format!("/ping returned {}", response.status()),
        Err(error) => format!("/ping failed: {error}"),
    };

    let inference = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": config.model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
            "stream": false
        }))
        .send()
        .await;
    match inference {
        Ok(response) if response.status().is_success() => successful_probe(started),
        Ok(response) => failed_probe(
            started,
            format!(
                "{ping_error}; test inference returned {}",
                response.status()
            ),
        ),
        Err(error) => failed_probe(
            started,
            format!("{ping_error}; test inference failed: {error}"),
        ),
    }
}

fn successful_probe(started: Instant) -> ProbeResult {
    ProbeResult {
        healthy: true,
        latency_ms: elapsed_ms(started),
        error: None,
    }
}

fn failed_probe(started: Instant, error: String) -> ProbeResult {
    let mut error = error;
    error.truncate(MAX_ERROR_LEN);
    ProbeResult {
        healthy: false,
        latency_ms: elapsed_ms(started),
        error: Some(error),
    }
}

fn elapsed_ms(started: Instant) -> i64 {
    started.elapsed().as_millis().min(i64::MAX as u128) as i64
}

fn normalize_endpoint(endpoint: &str) -> &str {
    endpoint.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_thirty_seconds() {
        let config = SlmMonitorConfig::new(Uuid::nil(), "http://127.0.0.1:51001", "test");
        assert_eq!(config.interval, Duration::from_secs(30));
        assert_eq!(config.timeout, Duration::from_secs(5));
    }

    #[test]
    fn endpoint_is_normalized_for_probe_and_storage() {
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:51001///"),
            "http://127.0.0.1:51001"
        );
    }

    #[test]
    fn errors_are_bounded_before_persistence_and_events() {
        let result = failed_probe(Instant::now(), "x".repeat(MAX_ERROR_LEN + 10));
        assert_eq!(result.error.unwrap().len(), MAX_ERROR_LEN);
    }
}
