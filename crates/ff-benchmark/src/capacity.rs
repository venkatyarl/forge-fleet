use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::runner::ScenarioRun;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaturationThresholds {
    pub max_p95_latency_ms: f64,
    pub max_error_rate: f64,
    pub min_tokens_per_second: f64,
    pub max_queue_p95_ms: f64,
}

impl Default for SaturationThresholds {
    fn default() -> Self {
        Self {
            max_p95_latency_ms: 3500.0,
            max_error_rate: 0.02,
            min_tokens_per_second: 30.0,
            max_queue_p95_ms: 500.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityRecommendation {
    pub scenario: String,
    pub endpoint: String,
    pub current_concurrency: u32,
    pub recommended_concurrency: u32,
    pub severity: RecommendationSeverity,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityPlan {
    pub generated_at: DateTime<Utc>,
    pub thresholds: SaturationThresholds,
    pub recommendations: Vec<CapacityRecommendation>,
}

impl CapacityPlan {
    pub fn has_blockers(&self) -> bool {
        self.recommendations
            .iter()
            .any(|r| r.severity == RecommendationSeverity::Critical)
    }
}

/// Build capacity recommendations from benchmark results.
pub fn plan_capacity(runs: &[ScenarioRun], thresholds: SaturationThresholds) -> CapacityPlan {
    let mut recommendations = Vec::new();

    for run in runs {
        for endpoint_result in &run.endpoint_results {
            let metrics = &endpoint_result.metrics;
            let current = run.scenario.concurrency.max(1);
            let mut severity = RecommendationSeverity::Info;
            let mut reason = String::from("Within thresholds");
            let mut recommended = current;

            if metrics.error_rate > thresholds.max_error_rate {
                severity = RecommendationSeverity::Critical;
                reason = format!(
                    "error rate {:.2}% exceeded max {:.2}%",
                    metrics.error_rate * 100.0,
                    thresholds.max_error_rate * 100.0
                );
                recommended = (current as f64 * 0.5).ceil() as u32;
            } else if metrics
                .latency_p95_ms
                .is_some_and(|v| v > thresholds.max_p95_latency_ms)
            {
                severity = RecommendationSeverity::Warning;
                reason = format!(
                    "p95 latency {}ms exceeded threshold {}ms",
                    metrics
                        .latency_p95_ms
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "n/a".to_string()),
                    thresholds.max_p95_latency_ms,
                );
                recommended = (current as f64 * 0.75).ceil() as u32;
            } else if metrics
                .queue_p95_ms
                .is_some_and(|v| v > thresholds.max_queue_p95_ms)
            {
                severity = RecommendationSeverity::Warning;
                reason = format!(
                    "queue p95 {}ms exceeded threshold {}ms",
                    metrics
                        .queue_p95_ms
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "n/a".to_string()),
                    thresholds.max_queue_p95_ms,
                );
                recommended = (current as f64 * 0.8).ceil() as u32;
            } else if metrics
                .tokens_per_second
                .is_some_and(|v| v < thresholds.min_tokens_per_second)
            {
                severity = RecommendationSeverity::Warning;
                reason = format!(
                    "throughput {} tok/s below threshold {} tok/s",
                    metrics
                        .tokens_per_second
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "n/a".to_string()),
                    thresholds.min_tokens_per_second,
                );
                recommended = (current as f64 * 0.9).ceil() as u32;
            } else if current >= 2 {
                severity = RecommendationSeverity::Info;
                reason = "Headroom available; safe to increase concurrency modestly".to_string();
                recommended = current + 1;
            }

            recommendations.push(CapacityRecommendation {
                scenario: run.scenario.name.clone(),
                endpoint: endpoint_result.endpoint.id(),
                current_concurrency: current,
                recommended_concurrency: recommended.max(1),
                severity,
                reason,
            });
        }
    }

    CapacityPlan {
        generated_at: Utc::now(),
        thresholds,
        recommendations,
    }
}
