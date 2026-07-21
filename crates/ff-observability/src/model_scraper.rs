//! Supply-side metrics scraper for llama.cpp and vLLM servers.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Utc;
use serde::Serialize;

use crate::NormalizedMetricRow;

/// Cron invokes the scraper on this cadence; the timeout is deliberately much
/// shorter so a dead model server cannot hold up the collection pass.
pub const SCRAPE_INTERVAL: Duration = Duration::from_secs(30);
pub const SCRAPE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct ModelServerTarget {
    pub base_url: String,
    pub node: String,
    pub port: i32,
    pub model: String,
}

/// One scrape result, ready to pass to `write_metrics`. A failed scrape or the
/// first sample after a server restart has `is_stale` set, causing the writer
/// to insert an explicit gap instead of carrying counters across boots.
#[derive(Debug, Clone, Serialize)]
pub struct ModelServerScrape {
    pub metric: NormalizedMetricRow,
    pub is_stale: bool,
    pub boot_changed: bool,
}

#[derive(Debug, Clone, Default)]
struct ServerState {
    boot_id: Option<String>,
    prompt_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ModelScraper {
    client: reqwest::Client,
    state: Arc<Mutex<HashMap<String, ServerState>>>,
}

impl Default for ModelScraper {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelScraper {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(SCRAPE_TIMEOUT)
            .connect_timeout(SCRAPE_TIMEOUT)
            .build()
            .expect("build model metrics client");
        Self::with_client(client)
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Fetch and normalize one server's `/metrics` endpoint.
    pub async fn scrape(&self, target: &ModelServerTarget) -> ModelServerScrape {
        let base_url = target.base_url.trim_end_matches('/');
        let key = format!("{}:{}:{}", target.node, target.port, target.model);
        let body = match self.client.get(format!("{base_url}/metrics")).send().await {
            Ok(response) if response.status().is_success() => response.text().await.ok(),
            _ => None,
        };

        let Some(body) = body else {
            let boot_id = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .and_then(|state| state.boot_id.clone());
            return stale_scrape(target, boot_id, false);
        };

        let parsed = parse_metrics(&body);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let previous = state.get(&key).cloned().unwrap_or_default();
        let counter_reset = decreased(parsed.prompt_tokens, previous.prompt_tokens)
            || decreased(parsed.output_tokens, previous.output_tokens);
        let reported_boot = parsed
            .process_start
            .map(|started| format!("process-start:{started:.6}"));
        let reported_change = previous.boot_id.is_some()
            && reported_boot.is_some()
            && previous.boot_id != reported_boot;
        let boot_changed = reported_change || counter_reset;
        let boot_id = if boot_changed && reported_boot == previous.boot_id {
            Some(format!("counter-reset:{}", uuid::Uuid::new_v4()))
        } else {
            reported_boot.or(previous.boot_id)
        };

        state.insert(
            key,
            ServerState {
                boot_id: boot_id.clone(),
                prompt_tokens: parsed.prompt_tokens,
                output_tokens: parsed.output_tokens,
            },
        );
        drop(state);

        if boot_changed {
            return stale_scrape(target, boot_id, true);
        }

        ModelServerScrape {
            metric: NormalizedMetricRow {
                recorded_at: Utc::now(),
                node: target.node.clone(),
                port: target.port,
                model: target.model.clone(),
                boot_id,
                batch_occupancy: parsed.batch_occupancy(),
                kv_cache_util: parsed.kv_cache_util.map(normalize_ratio),
                queue_depth: parsed.queue_depth,
                prompt_tokens_total: parsed.prompt_tokens,
                output_tokens_total: parsed.output_tokens,
            },
            is_stale: false,
            boot_changed: false,
        }
    }
}

fn stale_scrape(
    target: &ModelServerTarget,
    boot_id: Option<String>,
    boot_changed: bool,
) -> ModelServerScrape {
    ModelServerScrape {
        metric: NormalizedMetricRow {
            recorded_at: Utc::now(),
            node: target.node.clone(),
            port: target.port,
            model: target.model.clone(),
            boot_id,
            batch_occupancy: None,
            kv_cache_util: None,
            queue_depth: None,
            prompt_tokens_total: None,
            output_tokens_total: None,
        },
        is_stale: true,
        boot_changed,
    }
}

fn decreased(current: Option<i64>, previous: Option<i64>) -> bool {
    matches!((current, previous), (Some(current), Some(previous)) if current < previous)
}

#[derive(Default)]
struct ParsedMetrics {
    active: Option<f64>,
    batch_capacity: Option<f64>,
    batch_occupancy: Option<f64>,
    kv_cache_util: Option<f64>,
    queue_depth: Option<i64>,
    prompt_tokens: Option<i64>,
    output_tokens: Option<i64>,
    process_start: Option<f64>,
}

impl ParsedMetrics {
    fn batch_occupancy(&self) -> Option<f64> {
        self.batch_occupancy.map(normalize_ratio).or_else(|| {
            let capacity = self.batch_capacity?;
            (capacity > 0.0).then_some((self.active? / capacity).clamp(0.0, 1.0))
        })
    }
}

fn normalize_ratio(value: f64) -> f64 {
    if value > 1.0 { value / 100.0 } else { value }.clamp(0.0, 1.0)
}

fn add_i64(slot: &mut Option<i64>, value: f64) {
    if value < 0.0 || value > i64::MAX as f64 {
        return;
    }
    if let Some(sum) = slot.unwrap_or(0).checked_add(value as i64) {
        *slot = Some(sum);
    }
}

/// Parse only capacity/supply signals. Latency, request identifiers, and all
/// unrecognized series are intentionally ignored.
fn parse_metrics(body: &str) -> ParsedMetrics {
    let mut parsed = ParsedMetrics::default();
    for line in body.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((series, raw_value)) = line.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let name = series.split('{').next().unwrap_or(series);
        let Ok(value) = raw_value.trim().parse::<f64>() else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }

        match name {
            "llamacpp:batch_occupancy"
            | "llamacpp_batch_occupancy"
            | "vllm:batch_occupancy"
            | "vllm_batch_occupancy"
            | "batch_occupancy" => parsed.batch_occupancy = Some(value),
            "llamacpp:requests_processing"
            | "llamacpp_requests_processing"
            | "vllm:num_requests_running"
            | "vllm_num_requests_running" => {
                parsed.active = Some(parsed.active.unwrap_or(0.0) + value.max(0.0))
            }
            "llamacpp:max_batch_size"
            | "llamacpp_max_batch_size"
            | "vllm:max_num_seqs"
            | "vllm_max_num_seqs"
            | "max_batch_size" => parsed.batch_capacity = Some(value),
            "llamacpp:kv_cache_usage_ratio"
            | "llamacpp_kv_cache_usage_ratio"
            | "vllm:gpu_cache_usage_perc"
            | "vllm_gpu_cache_usage_perc"
            | "vllm:kv_cache_usage_perc"
            | "vllm_kv_cache_usage_perc"
            | "kv_cache_utilization"
            | "kv_cache_util"
                if value >= 0.0 =>
            {
                parsed.kv_cache_util =
                    Some(parsed.kv_cache_util.map_or(value, |old| old.max(value)))
            }
            "llamacpp:requests_deferred"
            | "llamacpp_requests_deferred"
            | "llamacpp:requests_waiting"
            | "llamacpp_requests_waiting"
            | "vllm:num_requests_waiting"
            | "vllm_num_requests_waiting"
            | "queue_depth" => add_i64(&mut parsed.queue_depth, value),
            "llamacpp:prompt_tokens_total"
            | "llamacpp_prompt_tokens_total"
            | "llamacpp:tokens_evaluated_total"
            | "llamacpp_tokens_evaluated_total"
            | "vllm:prompt_tokens_total"
            | "vllm_prompt_tokens_total"
            | "prompt_tokens_total" => add_i64(&mut parsed.prompt_tokens, value),
            "llamacpp:tokens_predicted_total"
            | "llamacpp_tokens_predicted_total"
            | "vllm:generation_tokens_total"
            | "vllm_generation_tokens_total"
            | "output_tokens_total" => add_i64(&mut parsed.output_tokens, value),
            "process_start_time_seconds" => parsed.process_start = Some(value),
            _ => {}
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ModelServerTarget {
        ModelServerTarget {
            base_url: "http://model".into(),
            node: "worker-1".into(),
            port: 8000,
            model: "test-model".into(),
        }
    }

    #[test]
    fn parses_llama_and_ignores_demand_side_series() {
        let parsed = parse_metrics(
            "llamacpp:requests_processing 2\nllamacpp:max_batch_size 8\n\
             llamacpp:kv_cache_usage_ratio 0.4\nllamacpp:requests_deferred 3\n\
             llamacpp:prompt_tokens_total 100\nllamacpp:tokens_predicted_total 40\n\
             request_latency_seconds_bucket{request_id=\"secret\",le=\"1\"} 99\n",
        );
        assert_eq!(parsed.batch_occupancy(), Some(0.25));
        assert_eq!(parsed.kv_cache_util, Some(0.4));
        assert_eq!(parsed.queue_depth, Some(3));
        assert_eq!(parsed.prompt_tokens, Some(100));
        assert_eq!(parsed.output_tokens, Some(40));
    }

    #[test]
    fn aggregates_labeled_vllm_counters() {
        let parsed = parse_metrics(
            "vllm:num_requests_waiting{model_name=\"a\"} 2\n\
             vllm:num_requests_waiting{model_name=\"b\"} 3\n\
             vllm:prompt_tokens_total{model_name=\"a\"} 10\n\
             vllm:prompt_tokens_total{model_name=\"b\"} 20\n\
             vllm:generation_tokens_total 7\nvllm:gpu_cache_usage_perc 75\n",
        );
        assert_eq!(parsed.queue_depth, Some(5));
        assert_eq!(parsed.prompt_tokens, Some(30));
        assert_eq!(parsed.output_tokens, Some(7));
        assert_eq!(parsed.kv_cache_util.map(normalize_ratio), Some(0.75));
    }

    #[test]
    fn stale_row_has_no_measurements() {
        let result = stale_scrape(&target(), Some("boot-1".into()), true);
        assert!(result.is_stale && result.boot_changed);
        assert_eq!(result.metric.boot_id.as_deref(), Some("boot-1"));
        assert_eq!(result.metric.prompt_tokens_total, None);
    }
}
