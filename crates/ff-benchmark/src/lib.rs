//! `ff-benchmark` — fleet benchmarking, regression detection, and capacity planning.
//!
//! This crate provides:
//! - scenario definitions for latency/throughput/long-context/routing tests
//! - async benchmark runner for model endpoints
//! - metrics collection (p50/p95/p99, tok/s, error, queue)
//! - report generation (JSON + Markdown)
//! - capacity recommendations and saturation checks
//! - baseline-vs-candidate regression detection

pub mod capacity;
pub mod collector;
pub mod regression;
pub mod report;
pub mod runner;
pub mod scenarios;

pub use capacity::{
    CapacityPlan, CapacityRecommendation, RecommendationSeverity, SaturationThresholds,
    plan_capacity,
};
pub use collector::{MetricSummary, RequestSample, percentile, summarize, summarize_by_endpoint};
pub use regression::{
    RegressionIssue, RegressionMetric, RegressionReport, RegressionSeverity, RegressionThresholds,
    detect_regressions,
};
pub use report::{BenchmarkReport, ScenarioReport};
pub use runner::{
    BenchmarkEndpoint, BenchmarkRunner, BenchmarkRunnerError, EndpointRunResult, RunnerConfig,
    RunnerResult, ScenarioRun,
};
pub use scenarios::{BenchmarkRequest, BenchmarkScenario, ScenarioKind};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
