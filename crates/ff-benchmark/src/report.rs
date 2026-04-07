use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::collector::{MetricSummary, RequestSample, summarize, summarize_by_endpoint};
use crate::runner::{EndpointRunResult, ScenarioRun};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub scenario_name: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub endpoint_results: Vec<EndpointRunResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub report_id: Uuid,
    pub generated_at: DateTime<Utc>,
    pub scenario_reports: Vec<ScenarioReport>,
    pub overall_summary: MetricSummary,
    pub endpoint_summary: Vec<(String, MetricSummary)>,
}

impl BenchmarkReport {
    pub fn from_runs(runs: &[ScenarioRun]) -> Self {
        let scenario_reports = runs
            .iter()
            .map(|run| ScenarioReport {
                scenario_name: run.scenario.name.clone(),
                started_at: run.started_at,
                finished_at: run.finished_at,
                endpoint_results: run.endpoint_results.clone(),
            })
            .collect::<Vec<_>>();

        let all_samples = collect_samples(runs);
        let overall_summary = summarize(&all_samples);

        let endpoint_map = summarize_by_endpoint(&all_samples);
        let endpoint_summary = endpoint_map.into_iter().collect();

        Self {
            report_id: Uuid::new_v4(),
            generated_at: Utc::now(),
            scenario_reports,
            overall_summary,
            endpoint_summary,
        }
    }

    pub fn to_json_pretty(&self) -> anyhow::Result<String> {
        serde_json::to_string_pretty(self).context("failed to serialize benchmark report as JSON")
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# ForgeFleet Benchmark Report\n\n");
        out.push_str(&format!("- Report ID: `{}`\n", self.report_id));
        out.push_str(&format!("- Generated: {}\n\n", self.generated_at));

        out.push_str("## Overall Summary\n\n");
        out.push_str(&render_metrics(&self.overall_summary));

        out.push_str("\n## Endpoint Summary\n\n");
        if self.endpoint_summary.is_empty() {
            out.push_str("No endpoint samples available.\n");
        } else {
            for (endpoint, metrics) in &self.endpoint_summary {
                out.push_str(&format!("### {}\n\n", endpoint));
                out.push_str(&render_metrics(metrics));
                out.push('\n');
            }
        }

        out.push_str("\n## Scenario Details\n\n");
        if self.scenario_reports.is_empty() {
            out.push_str("No scenarios executed.\n");
        } else {
            for scenario in &self.scenario_reports {
                out.push_str(&format!("### {}\n\n", scenario.scenario_name));
                out.push_str(&format!("- Started: {}\n", scenario.started_at));
                out.push_str(&format!("- Finished: {}\n\n", scenario.finished_at));

                for endpoint_result in &scenario.endpoint_results {
                    out.push_str(&format!(
                        "- **{}** — p95: {} ms, err: {:.2}%\n",
                        endpoint_result.endpoint.id(),
                        format_opt(endpoint_result.metrics.latency_p95_ms),
                        endpoint_result.metrics.error_rate * 100.0,
                    ));
                }
                out.push('\n');
            }
        }

        out
    }
}

fn collect_samples(runs: &[ScenarioRun]) -> Vec<RequestSample> {
    let mut out = Vec::new();
    for run in runs {
        for endpoint in &run.endpoint_results {
            out.extend(endpoint.samples.clone());
        }
    }
    out
}

fn render_metrics(metrics: &MetricSummary) -> String {
    format!(
        "- Requests: {} (success {}, failed {})\n- Error rate: {:.2}%\n- Latency p50/p95/p99: {}/{}/{} ms\n- Queue avg/p95: {}/{} ms\n- Throughput: {} tok/s\n",
        metrics.total_requests,
        metrics.successful_requests,
        metrics.failed_requests,
        metrics.error_rate * 100.0,
        format_opt(metrics.latency_p50_ms),
        format_opt(metrics.latency_p95_ms),
        format_opt(metrics.latency_p99_ms),
        format_opt(metrics.queue_avg_ms),
        format_opt(metrics.queue_p95_ms),
        format_opt(metrics.tokens_per_second),
    )
}

fn format_opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}
