//! Health gate configuration and pass/fail evaluation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Runtime health snapshot for a rollout step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthSnapshot {
    /// Success rate from 0.0 to 1.0.
    pub success_rate: f64,
    /// Error rate from 0.0 to 1.0.
    pub error_rate: f64,
    /// p95 latency in milliseconds.
    pub p95_latency_ms: u64,
    /// Availability from 0.0 to 1.0.
    pub availability: f64,
    /// Number of samples represented by this snapshot.
    pub sample_size: u64,
    /// Snapshot timestamp.
    pub measured_at: DateTime<Utc>,
}

impl HealthSnapshot {
    /// Build a health snapshot with current timestamp.
    pub fn new(
        success_rate: f64,
        error_rate: f64,
        p95_latency_ms: u64,
        availability: f64,
        sample_size: u64,
    ) -> Self {
        Self {
            success_rate,
            error_rate,
            p95_latency_ms,
            availability,
            sample_size,
            measured_at: Utc::now(),
        }
    }
}

/// Threshold configuration for rollout health checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthGateConfig {
    /// Minimum success rate required.
    pub min_success_rate: f64,
    /// Maximum tolerated error rate.
    pub max_error_rate: f64,
    /// Maximum p95 latency in ms.
    pub max_p95_latency_ms: u64,
    /// Minimum availability required.
    pub min_availability: f64,
    /// Minimum sample size required to trust the signal.
    pub min_sample_size: u64,
}

impl Default for HealthGateConfig {
    fn default() -> Self {
        Self {
            min_success_rate: 0.97,
            max_error_rate: 0.03,
            max_p95_latency_ms: 2_000,
            min_availability: 0.995,
            min_sample_size: 100,
        }
    }
}

/// Result status of the health gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthGateStatus {
    /// All checks passed.
    Pass,
    /// One or more checks failed.
    Fail,
}

/// Detailed health gate evaluation output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthGateEvaluation {
    /// Gate status.
    pub status: HealthGateStatus,
    /// Failure reasons (empty when pass).
    pub reasons: Vec<String>,
    /// Snapshot evaluated.
    pub snapshot: HealthSnapshot,
    /// Config used to evaluate.
    pub config: HealthGateConfig,
}

impl HealthGateEvaluation {
    /// Returns true when health gate status is pass.
    pub fn passed(&self) -> bool {
        self.status == HealthGateStatus::Pass
    }
}

/// Health gate evaluator.
#[derive(Debug, Default)]
pub struct HealthGate;

impl HealthGate {
    /// Evaluate a health snapshot against gate thresholds.
    pub fn evaluate(config: &HealthGateConfig, snapshot: HealthSnapshot) -> HealthGateEvaluation {
        let mut reasons = Vec::new();

        if snapshot.sample_size < config.min_sample_size {
            reasons.push(format!(
                "insufficient sample size: {} < {}",
                snapshot.sample_size, config.min_sample_size
            ));
        }

        if snapshot.success_rate < config.min_success_rate {
            reasons.push(format!(
                "success rate below threshold: {:.4} < {:.4}",
                snapshot.success_rate, config.min_success_rate
            ));
        }

        if snapshot.error_rate > config.max_error_rate {
            reasons.push(format!(
                "error rate above threshold: {:.4} > {:.4}",
                snapshot.error_rate, config.max_error_rate
            ));
        }

        if snapshot.p95_latency_ms > config.max_p95_latency_ms {
            reasons.push(format!(
                "p95 latency above threshold: {}ms > {}ms",
                snapshot.p95_latency_ms, config.max_p95_latency_ms
            ));
        }

        if snapshot.availability < config.min_availability {
            reasons.push(format!(
                "availability below threshold: {:.4} < {:.4}",
                snapshot.availability, config.min_availability
            ));
        }

        let status = if reasons.is_empty() {
            HealthGateStatus::Pass
        } else {
            HealthGateStatus::Fail
        };

        HealthGateEvaluation {
            status,
            reasons,
            snapshot,
            config: config.clone(),
        }
    }
}
