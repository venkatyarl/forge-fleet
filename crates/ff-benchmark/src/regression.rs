use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::runner::ScenarioRun;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegressionMetric {
    LatencyP95,
    LatencyP99,
    ErrorRate,
    TokensPerSecond,
    QueueP95,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegressionSeverity {
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionThresholds {
    pub latency_p95_increase_pct: f64,
    pub latency_p99_increase_pct: f64,
    pub error_rate_increase_pct: f64,
    pub tokens_per_second_drop_pct: f64,
    pub queue_p95_increase_pct: f64,
}

impl Default for RegressionThresholds {
    fn default() -> Self {
        Self {
            latency_p95_increase_pct: 10.0,
            latency_p99_increase_pct: 15.0,
            error_rate_increase_pct: 20.0,
            tokens_per_second_drop_pct: 10.0,
            queue_p95_increase_pct: 15.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionIssue {
    pub scenario: String,
    pub endpoint: String,
    pub metric: RegressionMetric,
    pub baseline: f64,
    pub current: f64,
    pub delta_pct: f64,
    pub severity: RegressionSeverity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionReport {
    pub detected_at: DateTime<Utc>,
    pub total_comparisons: usize,
    pub issues: Vec<RegressionIssue>,
}

impl RegressionReport {
    pub fn has_regressions(&self) -> bool {
        !self.issues.is_empty()
    }
}

/// Compare benchmark runs and flag statistically meaningful regressions.
pub fn detect_regressions(
    baseline_runs: &[ScenarioRun],
    candidate_runs: &[ScenarioRun],
    thresholds: &RegressionThresholds,
) -> RegressionReport {
    let baseline = index_runs(baseline_runs);
    let candidate = index_runs(candidate_runs);

    let mut issues = Vec::new();
    let mut total_comparisons = 0usize;

    for (key, base_metrics) in baseline {
        let Some(candidate_metrics) = candidate.get(&key) else {
            continue;
        };

        total_comparisons += 1;
        let (scenario, endpoint) = key;

        compare_higher_is_worse(
            &mut issues,
            &scenario,
            &endpoint,
            RegressionMetric::LatencyP95,
            base_metrics.latency_p95_ms,
            candidate_metrics.latency_p95_ms,
            thresholds.latency_p95_increase_pct,
        );

        compare_higher_is_worse(
            &mut issues,
            &scenario,
            &endpoint,
            RegressionMetric::LatencyP99,
            base_metrics.latency_p99_ms,
            candidate_metrics.latency_p99_ms,
            thresholds.latency_p99_increase_pct,
        );

        compare_higher_is_worse(
            &mut issues,
            &scenario,
            &endpoint,
            RegressionMetric::QueueP95,
            base_metrics.queue_p95_ms,
            candidate_metrics.queue_p95_ms,
            thresholds.queue_p95_increase_pct,
        );

        compare_higher_is_worse(
            &mut issues,
            &scenario,
            &endpoint,
            RegressionMetric::ErrorRate,
            Some(base_metrics.error_rate),
            Some(candidate_metrics.error_rate),
            thresholds.error_rate_increase_pct,
        );

        compare_lower_is_worse(
            &mut issues,
            &scenario,
            &endpoint,
            RegressionMetric::TokensPerSecond,
            base_metrics.tokens_per_second,
            candidate_metrics.tokens_per_second,
            thresholds.tokens_per_second_drop_pct,
        );
    }

    RegressionReport {
        detected_at: Utc::now(),
        total_comparisons,
        issues,
    }
}

fn compare_higher_is_worse(
    issues: &mut Vec<RegressionIssue>,
    scenario: &str,
    endpoint: &str,
    metric: RegressionMetric,
    baseline: Option<f64>,
    current: Option<f64>,
    threshold_pct: f64,
) {
    let (Some(base), Some(curr)) = (baseline, current) else {
        return;
    };

    if base <= 0.0 {
        return;
    }

    let delta_pct = ((curr - base) / base) * 100.0;
    if delta_pct >= threshold_pct {
        let severity = if delta_pct >= threshold_pct * 2.0 {
            RegressionSeverity::Critical
        } else {
            RegressionSeverity::Warning
        };

        issues.push(RegressionIssue {
            scenario: scenario.to_string(),
            endpoint: endpoint.to_string(),
            metric,
            baseline: base,
            current: curr,
            delta_pct,
            severity,
        });
    }
}

fn compare_lower_is_worse(
    issues: &mut Vec<RegressionIssue>,
    scenario: &str,
    endpoint: &str,
    metric: RegressionMetric,
    baseline: Option<f64>,
    current: Option<f64>,
    threshold_pct: f64,
) {
    let (Some(base), Some(curr)) = (baseline, current) else {
        return;
    };

    if base <= 0.0 {
        return;
    }

    let delta_pct = ((base - curr) / base) * 100.0;
    if delta_pct >= threshold_pct {
        let severity = if delta_pct >= threshold_pct * 2.0 {
            RegressionSeverity::Critical
        } else {
            RegressionSeverity::Warning
        };

        issues.push(RegressionIssue {
            scenario: scenario.to_string(),
            endpoint: endpoint.to_string(),
            metric,
            baseline: base,
            current: curr,
            delta_pct,
            severity,
        });
    }
}

fn index_runs(runs: &[ScenarioRun]) -> HashMap<(String, String), crate::collector::MetricSummary> {
    let mut out = HashMap::new();

    for run in runs {
        for endpoint in &run.endpoint_results {
            out.insert(
                (run.scenario.name.clone(), endpoint.endpoint.id()),
                endpoint.metrics.clone(),
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::{
        collector::MetricSummary,
        regression::{RegressionMetric, RegressionThresholds, detect_regressions},
        runner::{BenchmarkEndpoint, EndpointRunResult, ScenarioRun},
        scenarios::{BenchmarkRequest, BenchmarkScenario, ScenarioKind},
    };

    fn fixture_run(
        scenario_name: &str,
        endpoint_name: &str,
        p95: f64,
        error_rate: f64,
        tps: f64,
    ) -> ScenarioRun {
        let scenario = BenchmarkScenario {
            id: Uuid::new_v4(),
            name: scenario_name.to_string(),
            description: "fixture".to_string(),
            kind: ScenarioKind::Latency,
            iterations: 10,
            concurrency: 2,
            warmup_requests: 0,
            target_tier: None,
            target_model: Some("qwen".to_string()),
            routing_models: vec![],
            request: BenchmarkRequest {
                prompt: "ping".to_string(),
                max_tokens: 8,
                temperature: 0.0,
            },
        };

        ScenarioRun {
            run_id: Uuid::new_v4(),
            scenario,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            endpoint_results: vec![EndpointRunResult {
                endpoint: BenchmarkEndpoint {
                    node_name: endpoint_name.to_string(),
                    base_url: "http://localhost:8080".to_string(),
                    default_model: Some("qwen".to_string()),
                    api_key: None,
                },
                started_at: Utc::now(),
                finished_at: Utc::now(),
                metrics: MetricSummary {
                    total_requests: 10,
                    successful_requests: 10,
                    failed_requests: 0,
                    error_rate,
                    latency_p50_ms: Some(p95 * 0.7),
                    latency_p95_ms: Some(p95),
                    latency_p99_ms: Some(p95 * 1.2),
                    queue_p95_ms: Some(150.0),
                    queue_avg_ms: Some(90.0),
                    tokens_per_second: Some(tps),
                    avg_total_tokens: Some(42.0),
                },
                samples: Vec::new(),
            }],
        }
    }

    #[test]
    fn detects_latency_and_throughput_regression() {
        let baseline = vec![fixture_run("latency", "taylor", 1000.0, 0.01, 90.0)];
        let candidate = vec![fixture_run("latency", "taylor", 1250.0, 0.01, 72.0)];

        let report = detect_regressions(&baseline, &candidate, &RegressionThresholds::default());

        assert!(report.has_regressions());
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.metric == RegressionMetric::LatencyP95)
        );
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.metric == RegressionMetric::TokensPerSecond)
        );
    }

    #[test]
    fn no_regression_when_changes_within_threshold() {
        let baseline = vec![fixture_run("latency", "taylor", 1000.0, 0.01, 100.0)];
        let candidate = vec![fixture_run("latency", "taylor", 1060.0, 0.011, 95.0)];

        let report = detect_regressions(&baseline, &candidate, &RegressionThresholds::default());

        assert!(!report.has_regressions());
    }
}
