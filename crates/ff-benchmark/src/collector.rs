use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Per-request telemetry emitted by the benchmark runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSample {
    pub timestamp: DateTime<Utc>,
    pub endpoint: String,
    pub scenario: String,
    pub model: String,
    pub success: bool,
    pub latency_ms: f64,
    pub queue_time_ms: Option<f64>,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub error: Option<String>,
}

/// Aggregated metrics for a scenario/endpoint pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSummary {
    pub total_requests: usize,
    pub successful_requests: usize,
    pub failed_requests: usize,
    pub error_rate: f64,
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
    pub latency_p99_ms: Option<f64>,
    pub queue_p95_ms: Option<f64>,
    pub queue_avg_ms: Option<f64>,
    pub tokens_per_second: Option<f64>,
    pub avg_total_tokens: Option<f64>,
}

impl MetricSummary {
    pub fn empty() -> Self {
        Self {
            total_requests: 0,
            successful_requests: 0,
            failed_requests: 0,
            error_rate: 0.0,
            latency_p50_ms: None,
            latency_p95_ms: None,
            latency_p99_ms: None,
            queue_p95_ms: None,
            queue_avg_ms: None,
            tokens_per_second: None,
            avg_total_tokens: None,
        }
    }
}

/// Compute percentile using linear interpolation between adjacent ranked samples.
pub fn percentile(values: &[f64], pct: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }

    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);

    if sorted.len() == 1 {
        return sorted.first().copied();
    }

    let clamped = pct.clamp(0.0, 100.0) / 100.0;
    let rank = clamped * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;

    if lower == upper {
        return sorted.get(lower).copied();
    }

    let weight = rank - lower as f64;
    let low = sorted[lower];
    let high = sorted[upper];
    Some(low + (high - low) * weight)
}

/// Aggregate request samples into benchmark metrics.
pub fn summarize(samples: &[RequestSample]) -> MetricSummary {
    if samples.is_empty() {
        return MetricSummary::empty();
    }

    let total_requests = samples.len();
    let successful_requests = samples.iter().filter(|s| s.success).count();
    let failed_requests = total_requests.saturating_sub(successful_requests);
    let error_rate = if total_requests == 0 {
        0.0
    } else {
        failed_requests as f64 / total_requests as f64
    };

    let latencies: Vec<f64> = samples
        .iter()
        .filter(|s| s.success)
        .map(|s| s.latency_ms)
        .collect();

    let queues: Vec<f64> = samples.iter().filter_map(|s| s.queue_time_ms).collect();

    let total_tokens_sum: u64 = samples
        .iter()
        .filter(|s| s.success)
        .map(|s| s.total_tokens as u64)
        .sum();

    let total_latency_secs: f64 = samples
        .iter()
        .filter(|s| s.success)
        .map(|s| s.latency_ms / 1000.0)
        .sum();

    let tokens_per_second = if total_latency_secs > 0.0 {
        Some(total_tokens_sum as f64 / total_latency_secs)
    } else {
        None
    };

    let avg_total_tokens = if successful_requests > 0 {
        Some(total_tokens_sum as f64 / successful_requests as f64)
    } else {
        None
    };

    let queue_avg_ms = if queues.is_empty() {
        None
    } else {
        Some(queues.iter().sum::<f64>() / queues.len() as f64)
    };

    MetricSummary {
        total_requests,
        successful_requests,
        failed_requests,
        error_rate,
        latency_p50_ms: percentile(&latencies, 50.0),
        latency_p95_ms: percentile(&latencies, 95.0),
        latency_p99_ms: percentile(&latencies, 99.0),
        queue_p95_ms: percentile(&queues, 95.0),
        queue_avg_ms,
        tokens_per_second,
        avg_total_tokens,
    }
}

/// Group samples by endpoint and summarize each group.
pub fn summarize_by_endpoint(samples: &[RequestSample]) -> DashMap<String, MetricSummary> {
    let grouped: DashMap<String, Vec<RequestSample>> = DashMap::new();

    for sample in samples {
        grouped
            .entry(sample.endpoint.clone())
            .or_default()
            .push(sample.clone());
    }

    let summaries = DashMap::new();
    for entry in &grouped {
        summaries.insert(entry.key().clone(), summarize(entry.value()));
    }

    summaries
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_returns_none_for_empty() {
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn percentile_interpolates_midpoints() {
        let values = [10.0, 20.0, 30.0, 40.0];
        let p50 = percentile(&values, 50.0).expect("p50 should exist");
        assert!((p50 - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_handles_high_quantiles() {
        let values = [1.0, 3.0, 9.0, 10.0, 11.0];
        let p95 = percentile(&values, 95.0).expect("p95 should exist");
        assert!((p95 - 10.8).abs() < 0.0001);

        let p99 = percentile(&values, 99.0).expect("p99 should exist");
        assert!((p99 - 10.96).abs() < 0.0001);
    }
}
