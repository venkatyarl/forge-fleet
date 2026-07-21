//! Metrics scraper — per-node poll of local inference servers' `/metrics`.
//!
//! Every 30s ([`DEFAULT_INTERVAL`]) the scraper looks up this node's
//! non-stopped rows in `fleet_model_deployments`, fetches each server's
//! Prometheus `/metrics` endpoint on `127.0.0.1:<port>` with a 2s timeout
//! ([`SCRAPE_TIMEOUT`]), and appends one row per reachable deployment to
//! `deployment_metrics_scrapes` (Schema V175).
//!
//! Stale records are handled in two ways:
//!   - samples reference `fleet_model_deployments(id)` with `ON DELETE
//!     CASCADE`, so history disappears with its deployment row;
//!   - each pass prunes samples older than the retention window
//!     (`FF_METRICS_SCRAPER_RETENTION_HOURS`, default 24).
//!
//! The fleet Postgres is fronted by pgcat in transaction pooling mode: a
//! server connection is pinned only for the duration of one transaction, and
//! prepared statements cached on one server connection are not visible on the
//! next. Writes therefore run inside a single explicit transaction per pass,
//! and every statement is marked non-persistent so sqlx never relies on a
//! cross-transaction statement cache.

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::PathBuf;
use std::time::Duration;

use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default interval between scrape passes.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// Per-endpoint HTTP timeout for one `/metrics` fetch.
pub const SCRAPE_TIMEOUT: Duration = Duration::from_secs(2);

const DEFAULT_RETENTION_HOURS: u32 = 24;

/// Errors returned by the scraper.
#[derive(Debug, Error)]
pub enum ScrapeError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("http client: {0}")]
    Http(#[from] reqwest::Error),
}

/// Summary of one [`MetricsScraper::scrape_once`] pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrapeReport {
    /// Non-stopped deployments found for this node.
    pub targets: usize,
    /// Endpoints that answered within the timeout and produced a row.
    pub rows_written: usize,
    /// Endpoints that timed out or refused the connection (no row written).
    pub unreachable: usize,
    /// Old samples removed by the retention prune.
    pub rows_pruned: u64,
}

/// Key metrics parsed out of one Prometheus `/metrics` body.
///
/// Metric names mirror the llama.cpp / vllm names probed in
/// `ff_pulse::llm_probe` so both readers agree on semantics.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ParsedMetrics {
    pub tokens_per_sec: Option<f64>,
    pub queue_depth: Option<i32>,
    pub active_requests: Option<i32>,
    pub prompt_tokens_total: Option<f64>,
    pub predicted_tokens_total: Option<f64>,
    pub inference_seconds_total: Option<f64>,
    /// Non-comment sample lines seen — a liveness signal even when none of
    /// the known metric names matched.
    pub metric_count: i32,
}

/// One local deployment to scrape.
#[derive(Debug, sqlx::FromRow)]
struct ScrapeTarget {
    id: uuid::Uuid,
    port: i32,
    runtime: String,
    catalog_id: Option<String>,
    parallel_slots: Option<i32>,
    previous_prompt_tokens: Option<f64>,
    previous_predicted_tokens: Option<f64>,
    previous_inference_seconds: Option<f64>,
}

/// One sample ready to insert.
struct Sample {
    deployment_id: uuid::Uuid,
    port: i32,
    runtime: String,
    metrics: ParsedMetrics,
    endpoint: String,
    requests_per_sec: Option<f64>,
    batch_occupancy: Option<f64>,
    avg_latency_ms: Option<f64>,
}

/// Retention window for scrape samples, tunable per node without a restart.
fn retention_hours() -> u32 {
    std::env::var("FF_METRICS_SCRAPER_RETENTION_HOURS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|h| *h > 0)
        .unwrap_or(DEFAULT_RETENTION_HOURS)
}

/// Per-node scraper: polls local `/metrics` endpoints and appends samples
/// to `deployment_metrics_scrapes`.
pub struct MetricsScraper {
    pg: PgPool,
    my_name: String,
    http: reqwest::Client,
}

impl MetricsScraper {
    /// Build a new scraper for this node.
    pub fn new(pg: PgPool, my_name: String) -> Result<Self, ScrapeError> {
        let http = reqwest::Client::builder()
            .timeout(SCRAPE_TIMEOUT)
            .connect_timeout(SCRAPE_TIMEOUT)
            .build()?;
        Ok(Self { pg, my_name, http })
    }

    /// Run one scrape pass: fetch every local non-stopped deployment's
    /// `/metrics`, then insert the samples and prune stale rows in a single
    /// pgcat-transaction-mode-safe transaction.
    pub async fn scrape_once(&self) -> Result<ScrapeReport, ScrapeError> {
        let targets: Vec<ScrapeTarget> = sqlx::query_as(
            "SELECT d.id, d.port, d.runtime, d.catalog_id, d.parallel_slots, \
                    prev.prompt_tokens_total AS previous_prompt_tokens, \
                    prev.predicted_tokens_total AS previous_predicted_tokens, \
                    prev.inference_seconds_total AS previous_inference_seconds \
             FROM fleet_model_deployments d \
             LEFT JOIN LATERAL ( \
                 SELECT prompt_tokens_total, predicted_tokens_total, inference_seconds_total \
                 FROM deployment_metrics_scrapes \
                 WHERE deployment_id = d.id AND endpoint = 'all' \
                 ORDER BY scraped_at DESC LIMIT 1 \
             ) prev ON TRUE \
             WHERE d.worker_name = $1 AND d.health_status <> 'stopped' \
             ORDER BY d.port",
        )
        .persistent(false)
        .bind(&self.my_name)
        .fetch_all(&self.pg)
        .await?;

        let mut report = ScrapeReport {
            targets: targets.len(),
            ..ScrapeReport::default()
        };

        let mut samples = Vec::with_capacity(targets.len());
        for target in targets {
            let requests = if target.runtime == "llama.cpp" {
                let (requests, error_lines) = consume_llama_log(target.port);
                for line in error_lines {
                    if let Err(error) = ff_observability::classify_and_write(
                        &self.pg,
                        &self.my_name,
                        Some(target.port),
                        target.catalog_id.as_deref(),
                        &line,
                    )
                    .await
                    {
                        warn!(port = target.port, %error, "failed to persist model log error");
                    }
                }
                requests
            } else {
                HashMap::new()
            };
            let url = format!("http://127.0.0.1:{}/metrics", target.port);
            let body = match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => resp.text().await.unwrap_or_default(),
                Ok(resp) => {
                    debug!(port = target.port, status = %resp.status(), "metrics scrape: non-success status");
                    report.unreachable += 1;
                    continue;
                }
                Err(err) => {
                    debug!(port = target.port, error = %err, "metrics scrape: endpoint unreachable");
                    report.unreachable += 1;
                    continue;
                }
            };
            let metrics = parse_prometheus(&body);
            let request_count: u64 = requests.values().sum();
            let inference_delta = counter_delta(
                metrics.inference_seconds_total,
                target.previous_inference_seconds,
            );
            let token_delta =
                counter_delta(metrics.prompt_tokens_total, target.previous_prompt_tokens)
                    .unwrap_or(0.0)
                    + counter_delta(
                        metrics.predicted_tokens_total,
                        target.previous_predicted_tokens,
                    )
                    .unwrap_or(0.0);
            let interval_secs = DEFAULT_INTERVAL.as_secs_f64();
            let avg_latency_ms = inference_delta
                .filter(|_| request_count > 0)
                .map(|seconds| seconds * 1000.0 / request_count as f64);
            let batch_occupancy = metrics.active_requests.and_then(|active| {
                target
                    .parallel_slots
                    .filter(|slots| *slots > 0)
                    .map(|slots| (active.max(0) as f64 / slots as f64).clamp(0.0, 1.0))
            });
            let mut all_metrics = metrics;
            if token_delta > 0.0 {
                all_metrics.tokens_per_sec = Some(token_delta / interval_secs);
            }
            let all_sample = Sample {
                deployment_id: target.id,
                port: target.port,
                runtime: target.runtime,
                metrics: all_metrics,
                endpoint: "all".into(),
                requests_per_sec: Some(request_count as f64 / interval_secs),
                batch_occupancy,
                avg_latency_ms,
            };
            for (endpoint, count) in requests {
                samples.push(Sample {
                    deployment_id: target.id,
                    port: target.port,
                    runtime: "llama.cpp".into(),
                    metrics: ParsedMetrics::default(),
                    endpoint,
                    requests_per_sec: Some(count as f64 / interval_secs),
                    batch_occupancy,
                    avg_latency_ms,
                });
            }
            // Keep the aggregate row newest: the existing model_capacity view
            // selects the latest deployment sample and must not land on a
            // path-specific row whose process counters are intentionally NULL.
            samples.push(all_sample);
        }

        let (written, pruned) = self.write_pass(&samples).await?;
        report.rows_written = written;
        report.rows_pruned = pruned;

        Ok(report)
    }

    /// Insert this pass's samples and prune expired rows in one transaction.
    async fn write_pass(&self, samples: &[Sample]) -> Result<(usize, u64), sqlx::Error> {
        let mut tx = self.pg.begin().await?;
        let mut written = 0usize;

        for sample in samples {
            // Skip-if-gone rather than plain INSERT: a deployment row deleted
            // between the target query and here would otherwise abort the
            // whole pass on an FK violation.
            let result = sqlx::query(
                "INSERT INTO deployment_metrics_scrapes \
                    (deployment_id, worker_name, port, runtime, \
                    tokens_per_sec, queue_depth, active_requests, metric_count, endpoint, \
                    requests_per_sec, batch_occupancy, avg_latency_ms, prompt_tokens_total, \
                    predicted_tokens_total, inference_seconds_total) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15 \
                 WHERE EXISTS (SELECT 1 FROM fleet_model_deployments WHERE id = $1)",
            )
            .persistent(false)
            .bind(sample.deployment_id)
            .bind(&self.my_name)
            .bind(sample.port)
            .bind(&sample.runtime)
            .bind(sample.metrics.tokens_per_sec)
            .bind(sample.metrics.queue_depth)
            .bind(sample.metrics.active_requests)
            .bind(sample.metrics.metric_count)
            .bind(&sample.endpoint)
            .bind(sample.requests_per_sec)
            .bind(sample.batch_occupancy)
            .bind(sample.avg_latency_ms)
            .bind(sample.metrics.prompt_tokens_total)
            .bind(sample.metrics.predicted_tokens_total)
            .bind(sample.metrics.inference_seconds_total)
            .execute(&mut *tx)
            .await?;
            written += result.rows_affected() as usize;
        }

        let pruned = sqlx::query(
            "DELETE FROM deployment_metrics_scrapes \
             WHERE scraped_at < NOW() - ($1 || ' hours')::interval",
        )
        .persistent(false)
        .bind(retention_hours().to_string())
        .execute(&mut *tx)
        .await?
        .rows_affected();

        tx.commit().await?;
        Ok((written, pruned))
    }

    /// Spawn a background task that scrapes every 30 seconds. Runs on every
    /// node (no leader gate) — each scraper only touches its own local ports.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(DEFAULT_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match self.scrape_once().await {
                            Ok(report) => {
                                debug!(
                                    targets = report.targets,
                                    rows = report.rows_written,
                                    unreachable = report.unreachable,
                                    pruned = report.rows_pruned,
                                    "metrics scrape tick"
                                );
                            }
                            Err(err) => {
                                warn!(error = %err, "metrics scrape failed");
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!("metrics scraper shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}

/// Standalone tick entry point used by the daemon tick registry.
pub async fn run_metrics_scraper_tick(
    pg: &PgPool,
    worker_name: &str,
) -> Result<ScrapeReport, ScrapeError> {
    let scraper = MetricsScraper::new(pg.clone(), worker_name.to_string())?;
    scraper.scrape_once().await
}

fn counter_delta(current: Option<f64>, previous: Option<f64>) -> Option<f64> {
    match (current, previous) {
        (Some(current), Some(previous)) if current >= previous => Some(current - previous),
        _ => None,
    }
}

/// Consume llama-server's text stream after `/metrics` has captured its useful
/// counters. Completion lines become bounded endpoint counts; startup/errors
/// are retained in a small events log; verbose slot dumps are discarded.
fn consume_llama_log(port: i32) -> (HashMap<String, u64>, Vec<String>) {
    const READ_MAX: u64 = 16 * 1024 * 1024;
    const EVENTS_MAX: u64 = 10 * 1024 * 1024;

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let log_dir = PathBuf::from(home).join(".forgefleet/logs");
    let raw_path = log_dir.join(format!("model-{port}.log"));
    let events_path = log_dir.join(format!("model-{port}.events.log"));
    let counts = HashMap::new();

    if crate::model_runtime::cap_model_log(&raw_path, crate::model_runtime::MODEL_LOG_MAX_BYTES)
        .is_err()
    {
        return (counts, Vec::new());
    }
    let Ok(mut raw) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&raw_path)
    else {
        return (counts, Vec::new());
    };
    let len = raw.metadata().map(|m| m.len()).unwrap_or(0);
    if len > READ_MAX && raw.seek(std::io::SeekFrom::Start(len - READ_MAX)).is_err() {
        return (counts, Vec::new());
    }
    let mut text = String::new();
    if raw.read_to_string(&mut text).is_err() {
        return (counts, Vec::new());
    }

    let (counts, retained) = parse_llama_log(&text);
    if !retained.is_empty() {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, retained.as_bytes()));
        let _ = crate::model_runtime::cap_model_log(&events_path, EVENTS_MAX);
    }
    let _ = raw.set_len(0);
    let error_lines = retained.lines().map(str::to_owned).collect();
    (counts, error_lines)
}

fn parse_llama_log(text: &str) -> (HashMap<String, u64>, String) {
    let mut counts = HashMap::new();
    let mut retained = String::new();
    for line in text.lines() {
        if ff_observability::is_slot_dump(line) {
            continue;
        }
        if let Some(rest) = line.split("done request:").nth(1) {
            let mut fields = rest.split_whitespace();
            let _method = fields.next();
            if let Some(path) = fields.next() {
                let endpoint = path.split('?').next().unwrap_or(path).to_string();
                *counts.entry(endpoint).or_insert(0) += 1;
            }
        } else {
            let lower = line.to_ascii_lowercase();
            if ff_observability::classify(line).is_some()
                || lower.contains("error")
                || lower.contains("warn")
                || lower.contains("failed")
                || lower.contains("listening")
                || lower.contains("loading model")
            {
                retained.push_str(line);
                retained.push('\n');
            }
        }
    }
    (counts, retained)
}

/// Parse a Prometheus text-format body into the key metrics we track.
///
/// Labels are ignored; for a metric that appears multiple times (one per
/// label set) the last sample wins, matching `llm_probe`'s reading.
fn parse_prometheus(body: &str) -> ParsedMetrics {
    let mut parsed = ParsedMetrics::default();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Split "metric_name{labels} value [timestamp]" on the value column.
        let (name_part, value_part) = match line.rsplit_once(' ') {
            Some(p) => p,
            None => continue,
        };
        let name = name_part.split('{').next().unwrap_or(name_part).trim();
        let value: f64 = match value_part.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        parsed.metric_count += 1;

        match name {
            "llamacpp:prompt_tokens_per_second"
            | "llamacpp:prompt_tokens_seconds"
            | "llamacpp:predicted_tokens_seconds"
            | "vllm:avg_generation_throughput_tokens_per_s"
            | "tokens_per_second" => {
                parsed.tokens_per_sec = Some(value);
            }
            "llamacpp:requests_deferred" | "vllm:num_requests_waiting" | "queue_depth" => {
                parsed.queue_depth = Some(value as i32);
            }
            "llamacpp:requests_processing" | "vllm:num_requests_running" | "active_requests" => {
                parsed.active_requests = Some(value as i32);
            }
            "llamacpp:prompt_tokens_total" | "vllm:prompt_tokens_total" => {
                parsed.prompt_tokens_total = Some(value);
            }
            "llamacpp:tokens_predicted_total" | "vllm:generation_tokens_total" => {
                parsed.predicted_tokens_total = Some(value);
            }
            "llamacpp:prompt_seconds_total" => {
                parsed.inference_seconds_total =
                    Some(parsed.inference_seconds_total.unwrap_or(0.0) + value);
            }
            "llamacpp:tokens_predicted_seconds_total" => {
                parsed.inference_seconds_total =
                    Some(parsed.inference_seconds_total.unwrap_or(0.0) + value);
            }
            _ => {}
        }
    }

    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_llamacpp_metrics() {
        let body = "\
# HELP llamacpp:prompt_tokens_per_second Average prompt throughput.
# TYPE llamacpp:prompt_tokens_per_second gauge
llamacpp:prompt_tokens_per_second 42.5
llamacpp:requests_deferred 3
llamacpp:requests_processing 2
llamacpp:kv_cache_tokens 1024
llamacpp:prompt_tokens_total 100
llamacpp:tokens_predicted_total 50
llamacpp:prompt_seconds_total 2.5
llamacpp:tokens_predicted_seconds_total 5.0
";
        let parsed = parse_prometheus(body);
        assert_eq!(parsed.tokens_per_sec, Some(42.5));
        assert_eq!(parsed.queue_depth, Some(3));
        assert_eq!(parsed.active_requests, Some(2));
        assert_eq!(parsed.prompt_tokens_total, Some(100.0));
        assert_eq!(parsed.predicted_tokens_total, Some(50.0));
        assert_eq!(parsed.inference_seconds_total, Some(7.5));
        assert_eq!(parsed.metric_count, 8);
    }

    #[test]
    fn llama_log_converts_requests_and_drops_slot_dumps() {
        let log = "slot update_slots: id 2 | noisy dump\n\
srv log_server_r: done request: POST /v1/embeddings 127.0.0.1 200\n\
srv log_server_r: done request: POST /v1/chat/completions?x=1 127.0.0.1 200\n\
server is listening on 0.0.0.0:55001\n\
ERROR failed to decode request\n";
        let (counts, retained) = parse_llama_log(log);
        assert_eq!(counts.get("/v1/embeddings"), Some(&1));
        assert_eq!(counts.get("/v1/chat/completions"), Some(&1));
        assert!(!retained.contains("slot update_slots"));
        assert!(retained.contains("server is listening"));
        assert!(retained.contains("ERROR failed"));
    }

    #[test]
    fn test_parse_vllm_metrics_with_labels() {
        let body = "\
vllm:avg_generation_throughput_tokens_per_s{model_name=\"m\"} 17.25
vllm:num_requests_waiting{model_name=\"m\"} 5
vllm:num_requests_running{model_name=\"m\"} 1
";
        let parsed = parse_prometheus(body);
        assert_eq!(parsed.tokens_per_sec, Some(17.25));
        assert_eq!(parsed.queue_depth, Some(5));
        assert_eq!(parsed.active_requests, Some(1));
        assert_eq!(parsed.metric_count, 3);
    }

    #[test]
    fn test_parse_skips_comments_and_garbage() {
        let body = "\
# just a comment
not-a-metric-line
some_metric not_a_number
other_metric 1.0
";
        let parsed = parse_prometheus(body);
        assert_eq!(parsed.metric_count, 1);
        assert_eq!(parsed.tokens_per_sec, None);
        assert_eq!(parsed.queue_depth, None);
        assert_eq!(parsed.active_requests, None);
    }

    #[test]
    fn test_parse_empty_body() {
        assert_eq!(parse_prometheus(""), ParsedMetrics::default());
    }

    #[test]
    fn test_retention_hours_default_and_override() {
        // Unset → default. (Distinct env name is NOT used here because the
        // function reads a fixed variable; serialize by testing both cases
        // in one test to avoid a parallel-test race on the env var.)
        unsafe {
            std::env::remove_var("FF_METRICS_SCRAPER_RETENTION_HOURS");
        }
        assert_eq!(retention_hours(), DEFAULT_RETENTION_HOURS);

        unsafe {
            std::env::set_var("FF_METRICS_SCRAPER_RETENTION_HOURS", "72");
        }
        assert_eq!(retention_hours(), 72);

        // Zero and garbage fall back to the default.
        unsafe {
            std::env::set_var("FF_METRICS_SCRAPER_RETENTION_HOURS", "0");
        }
        assert_eq!(retention_hours(), DEFAULT_RETENTION_HOURS);
        unsafe {
            std::env::set_var("FF_METRICS_SCRAPER_RETENTION_HOURS", "nope");
        }
        assert_eq!(retention_hours(), DEFAULT_RETENTION_HOURS);

        unsafe {
            std::env::remove_var("FF_METRICS_SCRAPER_RETENTION_HOURS");
        }
    }
}
