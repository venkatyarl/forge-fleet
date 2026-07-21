//! Degraded metrics adapter for MLX model servers.
//!
//! MLX servers do not consistently expose the vLLM metric set. This adapter
//! first checks the OpenAI-compatible models endpoint, then normalizes whatever
//! is available from `/metrics` to the same row consumed by `write_metrics`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::Utc;
use serde::Serialize;

use crate::NormalizedMetricRow;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Identity attached to a normalized MLX metrics sample.
#[derive(Debug, Clone)]
pub struct MlxTarget {
    pub base_url: String,
    pub node: String,
    pub port: i32,
    pub model: String,
    pub boot_id: Option<String>,
}

/// A health and metrics snapshot returned to the main scraper.
#[derive(Debug, Clone, Serialize)]
pub struct MlxScrape {
    pub available: bool,
    pub degraded: bool,
    pub tokens_per_sec: Option<f64>,
    pub metric: NormalizedMetricRow,
}

#[derive(Debug, Clone, Copy)]
struct CounterSnapshot {
    at: Instant,
    tokens: f64,
}

/// Stateful MLX scraper. State is only used to derive throughput from token
/// counters between successive scrapes.
#[derive(Debug, Clone)]
pub struct MlxAdapter {
    client: reqwest::Client,
    previous: Arc<Mutex<HashMap<String, CounterSnapshot>>>,
}

impl Default for MlxAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl MlxAdapter {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .connect_timeout(DEFAULT_TIMEOUT)
            .build()
            .expect("build MLX observability client");
        Self::with_client(client)
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            previous: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Lightweight availability check using MLX's OpenAI-compatible endpoint.
    pub async fn health_ping(&self, base_url: &str) -> bool {
        self.client
            .get(endpoint(base_url, "/v1/models"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    /// Ping and scrape one MLX server. A missing or non-standard metrics
    /// endpoint is degraded, not unavailable: the returned row remains safe to
    /// pass directly to the standard metrics writer.
    pub async fn scrape(&self, target: &MlxTarget) -> MlxScrape {
        let mut metric = empty_row(target);
        if !self.health_ping(&target.base_url).await {
            return MlxScrape {
                available: false,
                degraded: true,
                tokens_per_sec: None,
                metric,
            };
        }

        let body = match self
            .client
            .get(endpoint(&target.base_url, "/metrics"))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response.text().await.ok(),
            _ => None,
        };
        let Some(body) = body else {
            return MlxScrape {
                available: true,
                degraded: true,
                tokens_per_sec: None,
                metric,
            };
        };

        let parsed = parse_metrics(&body);
        metric.batch_occupancy = parsed.batch_occupancy();
        metric.kv_cache_util = parsed.kv_cache_util();
        metric.queue_depth = parsed.queue_depth.map(nonnegative_i64);
        metric.prompt_tokens_total = parsed.prompt_tokens.map(nonnegative_i64);
        metric.output_tokens_total = parsed.output_tokens.map(nonnegative_i64);

        let total_tokens =
            parsed.prompt_tokens.unwrap_or(0.0) + parsed.output_tokens.unwrap_or(0.0);
        let tokens_per_sec = parsed.tokens_per_sec.or_else(|| {
            (total_tokens > 0.0)
                .then(|| self.derive_rate(&target.base_url, total_tokens))
                .flatten()
        });

        MlxScrape {
            available: true,
            degraded: parsed.seen == 0,
            tokens_per_sec,
            metric,
        }
    }

    fn derive_rate(&self, endpoint: &str, tokens: f64) -> Option<f64> {
        let now = Instant::now();
        let mut previous = self.previous.lock().unwrap_or_else(|e| e.into_inner());
        let old = previous.insert(endpoint.to_owned(), CounterSnapshot { at: now, tokens })?;
        let elapsed = now.duration_since(old.at).as_secs_f64();
        (elapsed > 0.0 && tokens >= old.tokens).then_some((tokens - old.tokens) / elapsed)
    }
}

fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}{path}", base_url.trim_end_matches('/'))
}

fn empty_row(target: &MlxTarget) -> NormalizedMetricRow {
    NormalizedMetricRow {
        recorded_at: Utc::now(),
        node: target.node.clone(),
        port: target.port,
        model: target.model.clone(),
        boot_id: target.boot_id.clone(),
        batch_occupancy: None,
        kv_cache_util: None,
        queue_depth: None,
        prompt_tokens_total: None,
        output_tokens_total: None,
    }
}

#[derive(Default)]
struct ParsedMetrics {
    seen: usize,
    active: Option<f64>,
    max_batch: Option<f64>,
    batch_occupancy: Option<f64>,
    kv_used: Option<f64>,
    kv_capacity: Option<f64>,
    kv_cache_util: Option<f64>,
    queue_depth: Option<f64>,
    prompt_tokens: Option<f64>,
    output_tokens: Option<f64>,
    tokens_per_sec: Option<f64>,
}

impl ParsedMetrics {
    fn batch_occupancy(&self) -> Option<f64> {
        self.batch_occupancy
            .map(normalize_ratio)
            .or_else(|| ratio(self.active, self.max_batch))
    }

    fn kv_cache_util(&self) -> Option<f64> {
        self.kv_cache_util
            .map(normalize_ratio)
            .or_else(|| ratio(self.kv_used, self.kv_capacity))
    }
}

fn ratio(value: Option<f64>, capacity: Option<f64>) -> Option<f64> {
    let (value, capacity) = (value?, capacity?);
    (capacity > 0.0).then_some((value / capacity).clamp(0.0, 1.0))
}

fn normalize_ratio(value: f64) -> f64 {
    if value > 1.0 { value / 100.0 } else { value }.clamp(0.0, 1.0)
}

fn nonnegative_i64(value: f64) -> i64 {
    value.max(0.0).min(i64::MAX as f64) as i64
}

fn parse_metrics(body: &str) -> ParsedMetrics {
    let mut parsed = ParsedMetrics::default();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, raw_value)) = line.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let name = name.split('{').next().unwrap_or(name).trim();
        let Ok(value) = raw_value.trim().parse::<f64>() else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        let destination = match name {
            "mlx:batch_occupancy" | "mlx_batch_occupancy" | "batch_occupancy" => {
                &mut parsed.batch_occupancy
            }
            "mlx:active_requests" | "mlx_active_requests" | "active_requests" => &mut parsed.active,
            "mlx:max_batch_size" | "mlx_max_batch_size" | "max_batch_size" => &mut parsed.max_batch,
            "mlx:kv_cache_used" | "mlx_kv_cache_used" | "kv_cache_used" => &mut parsed.kv_used,
            "mlx:kv_cache_capacity" | "mlx_kv_cache_capacity" | "kv_cache_capacity" => {
                &mut parsed.kv_capacity
            }
            "mlx:kv_cache_util" | "mlx_kv_cache_util" | "kv_cache_utilization" => {
                &mut parsed.kv_cache_util
            }
            "mlx:queue_depth" | "mlx_queue_depth" | "queue_depth" => &mut parsed.queue_depth,
            "mlx:prompt_tokens_total" | "mlx_prompt_tokens_total" | "prompt_tokens_total" => {
                &mut parsed.prompt_tokens
            }
            "mlx:output_tokens_total"
            | "mlx_output_tokens_total"
            | "output_tokens_total"
            | "generated_tokens_total" => &mut parsed.output_tokens,
            "mlx:tokens_per_second" | "mlx_tokens_per_second" | "tokens_per_second" => {
                &mut parsed.tokens_per_sec
            }
            _ => continue,
        };
        *destination = Some(value);
        parsed.seen += 1;
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_mlx_metrics_to_standard_schema() {
        let parsed = parse_metrics(
            "mlx_active_requests 3\nmlx_max_batch_size 4\nmlx_kv_cache_used 25\n\
             mlx_kv_cache_capacity 100\nmlx_queue_depth 2\nmlx_prompt_tokens_total 40\n\
             mlx_output_tokens_total 10\n",
        );
        assert_eq!(parsed.batch_occupancy(), Some(0.75));
        assert_eq!(parsed.kv_cache_util(), Some(0.25));
        assert_eq!(parsed.queue_depth, Some(2.0));
        assert_eq!(parsed.prompt_tokens, Some(40.0));
        assert_eq!(parsed.output_tokens, Some(10.0));
    }

    #[test]
    fn accepts_percentages_and_prometheus_labels() {
        let parsed = parse_metrics(
            "batch_occupancy{model=\"mlx\"} 80\nkv_cache_utilization 50\ntokens_per_second 12.5\n",
        );
        assert_eq!(parsed.batch_occupancy(), Some(0.8));
        assert_eq!(parsed.kv_cache_util(), Some(0.5));
        assert_eq!(parsed.tokens_per_sec, Some(12.5));
    }

    #[test]
    fn derives_throughput_from_counter_delta() {
        let adapter = MlxAdapter::new();
        assert_eq!(adapter.derive_rate("http://mlx", 10.0), None);
        let rate = adapter.derive_rate("http://mlx", 20.0).unwrap();
        assert!(rate.is_finite() && rate >= 0.0);
    }

    #[test]
    fn counter_reset_does_not_emit_negative_throughput() {
        let adapter = MlxAdapter::new();
        assert_eq!(adapter.derive_rate("http://mlx", 10.0), None);
        assert_eq!(adapter.derive_rate("http://mlx", 1.0), None);
    }
}
