//! Verification model for candidate fixes.
//!
//! Determines whether a proposed repair should be accepted, retried, or rolled back.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::repair::{RepairAction, RepairRisk};

/// Verification decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationOutcome {
    Passed,
    Failed,
    Inconclusive,
}

/// Signals collected after applying a repair action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationInput {
    pub build_passed: bool,
    pub tests_passed: bool,
    pub health_checks_passed: bool,
    pub error_rate_before: f32,
    pub error_rate_after: f32,
    pub regression_detected: bool,
    pub notes: Vec<String>,
}

impl VerificationInput {
    pub fn success(error_rate_before: f32, error_rate_after: f32) -> Self {
        Self {
            build_passed: true,
            tests_passed: true,
            health_checks_passed: true,
            error_rate_before,
            error_rate_after,
            regression_detected: false,
            notes: Vec::new(),
        }
    }
}

/// Final verification report for an attempted fix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub id: Uuid,
    pub action_id: Uuid,
    pub outcome: VerificationOutcome,
    pub confidence: f32,
    pub improvement_ratio: f32,
    pub rollback_recommended: bool,
    pub rationale: Vec<String>,
    pub verified_at: DateTime<Utc>,
}

/// Configurable scoring model for verification decisions.
#[derive(Debug, Clone)]
pub struct VerificationModel {
    /// Minimum relative error-rate reduction to consider the fix effective.
    pub minimum_improvement_ratio: f32,
}

impl Default for VerificationModel {
    fn default() -> Self {
        Self::new(0.2)
    }
}

impl VerificationModel {
    pub fn new(minimum_improvement_ratio: f32) -> Self {
        Self {
            minimum_improvement_ratio: minimum_improvement_ratio.clamp(0.01, 0.95),
        }
    }

    pub fn verify(&self, action: &RepairAction, input: &VerificationInput) -> VerificationReport {
        let mut rationale = Vec::new();

        let improvement_ratio =
            compute_improvement_ratio(input.error_rate_before, input.error_rate_after);
        if input.regression_detected {
            rationale.push("Regression detected after applying fix".to_string());
            return VerificationReport {
                id: Uuid::new_v4(),
                action_id: action.id,
                outcome: VerificationOutcome::Failed,
                confidence: 0.96,
                improvement_ratio,
                rollback_recommended: true,
                rationale,
                verified_at: Utc::now(),
            };
        }

        if !input.build_passed {
            rationale.push("Build failed after applying fix".to_string());
        }
        if !input.tests_passed {
            rationale.push("Test suite failed after applying fix".to_string());
        }

        if !input.build_passed || !input.tests_passed {
            let rollback = matches!(action.risk, RepairRisk::High | RepairRisk::Critical);
            return VerificationReport {
                id: Uuid::new_v4(),
                action_id: action.id,
                outcome: VerificationOutcome::Failed,
                confidence: 0.88,
                improvement_ratio,
                rollback_recommended: rollback,
                rationale,
                verified_at: Utc::now(),
            };
        }

        if !input.health_checks_passed {
            rationale.push("Health checks still unstable; need more observation".to_string());
            return VerificationReport {
                id: Uuid::new_v4(),
                action_id: action.id,
                outcome: VerificationOutcome::Inconclusive,
                confidence: 0.6,
                improvement_ratio,
                rollback_recommended: false,
                rationale,
                verified_at: Utc::now(),
            };
        }

        if improvement_ratio >= self.minimum_improvement_ratio {
            rationale.push(format!(
                "Error rate improved by {:.1}% (threshold {:.1}%)",
                improvement_ratio * 100.0,
                self.minimum_improvement_ratio * 100.0
            ));
            VerificationReport {
                id: Uuid::new_v4(),
                action_id: action.id,
                outcome: VerificationOutcome::Passed,
                confidence: (0.75 + improvement_ratio * 0.25).clamp(0.75, 0.99),
                improvement_ratio,
                rollback_recommended: false,
                rationale,
                verified_at: Utc::now(),
            }
        } else {
            rationale.push(format!(
                "Improvement {:.1}% below threshold {:.1}%",
                improvement_ratio * 100.0,
                self.minimum_improvement_ratio * 100.0
            ));
            VerificationReport {
                id: Uuid::new_v4(),
                action_id: action.id,
                outcome: VerificationOutcome::Inconclusive,
                confidence: 0.58,
                improvement_ratio,
                rollback_recommended: false,
                rationale,
                verified_at: Utc::now(),
            }
        }
    }

    pub fn should_rollback(&self, report: &VerificationReport, action: &RepairAction) -> bool {
        report.rollback_recommended
            || (report.outcome == VerificationOutcome::Failed
                && matches!(action.risk, RepairRisk::High | RepairRisk::Critical))
    }
}

fn compute_improvement_ratio(before: f32, after: f32) -> f32 {
    if before <= 0.0 {
        return if after <= 0.0 { 1.0 } else { 0.0 };
    }
    ((before - after) / before).clamp(-2.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repair::{RepairStatus, RepairStrategy};

    fn sample_action(risk: RepairRisk) -> RepairAction {
        RepairAction {
            id: Uuid::new_v4(),
            cause_fingerprint: "abc".to_string(),
            strategy: RepairStrategy::FixCompilation,
            description: "fix".to_string(),
            commands: vec!["cargo check".to_string()],
            confidence: 0.8,
            risk,
            status: RepairStatus::Applied,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn passes_when_error_rate_improves_enough() {
        let model = VerificationModel::new(0.25);
        let action = sample_action(RepairRisk::Low);
        let input = VerificationInput::success(0.4, 0.1);

        let report = model.verify(&action, &input);
        assert_eq!(report.outcome, VerificationOutcome::Passed);
        assert!(report.improvement_ratio >= 0.25);
        assert!(!report.rollback_recommended);
    }

    #[test]
    fn recommends_rollback_on_high_risk_failed_fix() {
        let model = VerificationModel::default();
        let action = sample_action(RepairRisk::High);
        let input = VerificationInput {
            build_passed: false,
            tests_passed: false,
            health_checks_passed: false,
            error_rate_before: 0.2,
            error_rate_after: 0.3,
            regression_detected: false,
            notes: vec![],
        };

        let report = model.verify(&action, &input);
        assert_eq!(report.outcome, VerificationOutcome::Failed);
        assert!(model.should_rollback(&report, &action));
    }
}
