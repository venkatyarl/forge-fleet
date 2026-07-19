//! MLX degraded adapter — health-ping + derived throughput metrics for `mlx_lm.server`.
//!
//! Used by the Pulse LLM metrics scraper (`llm_probe`) and exposed as an
//! agent tool via `ff-agent::tools::mlx_degraded`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::debug;

/// Snapshot returned by [`MlxDegradedAdapter::check_endpoint`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlxAdapterState {
    pub endpoint: String,
    pub health_status: MlxHealthStatus,
    pub tokens_per_sec: f64,
    pub queue_depth: i32,
    pub response_time_ms: u64,
    pub degraded: bool,
}

/// Tri-state health for an MLX endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MlxHealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

impl MlxHealthStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

impl std::fmt::Display for MlxHealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors that can occur while probing an MLX endpoint.
#[derive(thiserror::Error, Debug)]
pub enum MlxAdapterError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid metric value '{value}' for {name}")]
    InvalidMetric { name: String, value: String },
}

/// Adapter that health-pings an MLX server and derives throughput metrics.
///
/// # Degraded logic
///
/// An endpoint is reported as [`MlxHealthStatus::Degraded`] when:
/// - `/v1/models` responds OK but advertises no models, **or**
/// - measured throughput is positive but below [`Self::throughput_threshold`], **or**
/// - reported queue depth exceeds [`Self::queue_depth_threshold`], **or**
/// - the `/v1/models` round-trip exceeds [`Self::response_time_ms_threshold`].
#[derive(Debug, Clone)]
pub struct MlxDegradedAdapter {
    client: reqwest::Client,
    health_timeout: Duration,
    metric_timeout: Duration,
    throughput_threshold: f64,
    queue_depth_threshold: i32,
    response_time_ms_threshold: u64,
}

impl Default for MlxDegradedAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl MlxDegradedAdapter {
    /// Build an adapter with sensible defaults.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("build mlx adapter reqwest client"),
            health_timeout: Duration::from_secs(5),
            metric_timeout: Duration::from_secs(5),
            throughput_threshold: 5.0,
            queue_depth_threshold: 8,
            response_time_ms_threshold: 5_000,
        }
    }

    /// Build an adapter from an existing HTTP client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            health_timeout: Duration::from_secs(5),
            metric_timeout: Duration::from_secs(5),
            throughput_threshold: 5.0,
            queue_depth_threshold: 8,
            response_time_ms_threshold: 5_000,
        }
    }

    /// Override the thresholds used to classify an endpoint as degraded.
    pub fn with_thresholds(
        mut self,
        throughput_threshold: f64,
        queue_depth_threshold: i32,
        response_time_ms_threshold: u64,
    ) -> Self {
        self.throughput_threshold = throughput_threshold;
        self.queue_depth_threshold = queue_depth_threshold;
        self.response_time_ms_threshold = response_time_ms_threshold;
        self
    }

    /// Health-ping `base_url/v1/models` and return the raw health status.
    pub async fn health_ping(&self, base_url: &str) -> Result<MlxHealthStatus, MlxAdapterError> {
        let url = format!("{base_url}/v1/models");
        debug!(url, "pinging MLX health endpoint");
        match self
            .client
            .get(&url)
            .timeout(self.health_timeout)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                let has_data = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("data").cloned())
                    .and_then(|d| d.as_array().map(|a| !a.is_empty()))
                    .unwrap_or(false);
                if has_data {
                    Ok(MlxHealthStatus::Healthy)
                } else {
                    Ok(MlxHealthStatus::Degraded)
                }
            }
            Ok(resp) => {
                debug!(status = %resp.status(), "MLX health endpoint non-success");
                Ok(MlxHealthStatus::Unhealthy)
            }
            Err(e) => {
                debug!(err = %e, "MLX health endpoint unreachable");
                Ok(MlxHealthStatus::Unhealthy)
            }
        }
    }

    /// Scrape `base_url/metrics` and derive throughput + queue depth.
    ///
    /// Returns `(tokens_per_second, queue_depth)`. Missing metrics default to `0.0` / `0`.
    pub async fn derive_throughput(&self, base_url: &str) -> Result<(f64, i32), MlxAdapterError> {
        let url = format!("{base_url}/metrics");
        debug!(url, "scraping MLX metrics endpoint");
        let body = match self
            .client
            .get(&url)
            .timeout(self.metric_timeout)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
            Ok(r) => {
                debug!(status = %r.status(), "MLX metrics endpoint non-success");
                return Ok((0.0, 0));
            }
            Err(e) => {
                debug!(err = %e, "MLX metrics endpoint unreachable");
                return Ok((0.0, 0));
            }
        };

        Ok(parse_mlx_metrics_body(&body))
    }

    /// Apply the degraded thresholds to a raw health snapshot.
    pub fn classify(
        &self,
        health: MlxHealthStatus,
        tokens_per_sec: f64,
        queue_depth: i32,
        response_time_ms: u64,
    ) -> MlxHealthStatus {
        if health == MlxHealthStatus::Unhealthy {
            return health;
        }
        if health == MlxHealthStatus::Degraded {
            return MlxHealthStatus::Degraded;
        }
        let low_throughput = tokens_per_sec > 0.0 && tokens_per_sec < self.throughput_threshold;
        let deep_queue = queue_depth > self.queue_depth_threshold;
        let slow = response_time_ms > self.response_time_ms_threshold;
        if low_throughput || deep_queue || slow {
            MlxHealthStatus::Degraded
        } else {
            MlxHealthStatus::Healthy
        }
    }

    /// Combined check: ping health, scrape metrics, and classify degradation.
    pub async fn check_endpoint(&self, base_url: &str) -> Result<MlxAdapterState, MlxAdapterError> {
        let start = std::time::Instant::now();
        let raw_health = self.health_ping(base_url).await?;
        let response_time_ms = start.elapsed().as_millis() as u64;

        let (tokens_per_sec, queue_depth) = if raw_health != MlxHealthStatus::Unhealthy {
            self.derive_throughput(base_url).await.unwrap_or((0.0, 0))
        } else {
            (0.0, 0)
        };

        let health_status =
            self.classify(raw_health, tokens_per_sec, queue_depth, response_time_ms);
        let degraded = health_status == MlxHealthStatus::Degraded;

        Ok(MlxAdapterState {
            endpoint: base_url.to_string(),
            health_status,
            tokens_per_sec,
            queue_depth,
            response_time_ms,
            degraded,
        })
    }
}

/// Parse a Prometheus-style `/metrics` body for MLX-style counters.
///
/// Recognises both the generic names (`tokens_per_second`, `queue_depth`) and
/// a future `mlx:` prefix (`mlx:tokens_per_second`, `mlx:queue_depth`).
fn parse_mlx_metrics_body(body: &str) -> (f64, i32) {
    let mut tokens_per_sec = 0.0f64;
    let mut queue_depth = 0i32;

    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        let (name_part, value_part) = match line.rsplit_once(' ') {
            Some(p) => p,
            None => continue,
        };
        let name = name_part.split('{').next().unwrap_or(name_part);
        let value: f64 = match value_part.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        match name {
            "mlx:tokens_per_second" | "tokens_per_second" => tokens_per_sec = value,
            "mlx:queue_depth" | "queue_depth" => queue_depth = value as i32,
            _ => {}
        }
    }

    (tokens_per_sec, queue_depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_status_strings() {
        assert_eq!(MlxHealthStatus::Healthy.as_str(), "healthy");
        assert_eq!(MlxHealthStatus::Degraded.as_str(), "degraded");
        assert_eq!(MlxHealthStatus::Unhealthy.as_str(), "unhealthy");
    }

    #[test]
    fn parse_generic_metrics() {
        let body = "# comment\ntokens_per_second 12.5\nqueue_depth 3\n";
        assert_eq!(parse_mlx_metrics_body(body), (12.5, 3));
    }

    #[test]
    fn parse_mlx_prefixed_metrics() {
        let body = "mlx:tokens_per_second 8.0\nmlx:queue_depth 10\n";
        assert_eq!(parse_mlx_metrics_body(body), (8.0, 10));
    }

    #[test]
    fn parse_prefixed_wins_over_generic() {
        // Last matching name wins; mlx: prefix appears after generic.
        let body = "tokens_per_second 2.0\nmlx:tokens_per_second 7.0\n";
        assert_eq!(parse_mlx_metrics_body(body).0, 7.0);
    }

    #[test]
    fn classify_degrades_on_low_throughput() {
        let adapter = MlxDegradedAdapter::new();
        assert_eq!(
            adapter.classify(MlxHealthStatus::Healthy, 1.0, 0, 10),
            MlxHealthStatus::Degraded
        );
    }

    #[test]
    fn classify_degrades_on_deep_queue() {
        let adapter = MlxDegradedAdapter::new();
        assert_eq!(
            adapter.classify(MlxHealthStatus::Healthy, 20.0, 20, 10),
            MlxHealthStatus::Degraded
        );
    }

    #[test]
    fn classify_degrades_on_slow_ping() {
        let adapter = MlxDegradedAdapter::new();
        assert_eq!(
            adapter.classify(MlxHealthStatus::Healthy, 20.0, 0, 10_000),
            MlxHealthStatus::Degraded
        );
    }

    #[test]
    fn classify_healthy_when_zero_throughput_unknown() {
        let adapter = MlxDegradedAdapter::new();
        assert_eq!(
            adapter.classify(MlxHealthStatus::Healthy, 0.0, 0, 10),
            MlxHealthStatus::Healthy
        );
    }

    #[test]
    fn unhealthy_stays_unhealthy() {
        let adapter = MlxDegradedAdapter::new();
        assert_eq!(
            adapter.classify(MlxHealthStatus::Unhealthy, 100.0, 0, 10),
            MlxHealthStatus::Unhealthy
        );
    }
}
