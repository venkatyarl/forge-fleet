//! Rollback decisioning and rollback plan construction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::health_gate::HealthGateEvaluation;

/// Why a rollback was triggered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackCause {
    /// Operator requested manual rollback.
    Manual,
    /// Health gate evaluation failed.
    HealthGateFailed,
    /// Deployment execution error occurred.
    DeploymentError,
}

/// Severity of rollback condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackSeverity {
    /// Non-urgent rollback recommendation.
    Low,
    /// Elevated risk, rollback strongly advised.
    Medium,
    /// High-risk condition, rollback required.
    High,
    /// Critical outage-level condition.
    Critical,
}

/// Context used to decide rollback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackContext {
    /// Release id under evaluation.
    pub release_id: Uuid,
    /// Service name.
    pub service: String,
    /// Version currently being deployed.
    pub from_version: String,
    /// Last known good version.
    pub to_version: Option<String>,
    /// Percent currently deployed for the new version.
    pub deployed_percent: u8,
    /// Optional health gate result.
    pub health: Option<HealthGateEvaluation>,
    /// Optional deployment execution error.
    pub deployment_error: Option<String>,
    /// Operator manual rollback request.
    pub manual_rollback: bool,
}

/// Rollback decision output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackDecision {
    /// Whether rollback should execute.
    pub should_rollback: bool,
    /// Classified severity.
    pub severity: RollbackSeverity,
    /// Trigger causes.
    pub causes: Vec<RollbackCause>,
    /// Human-readable reasons.
    pub reasons: Vec<String>,
}

impl RollbackDecision {
    /// Decision representing no rollback required.
    pub fn no_rollback() -> Self {
        Self {
            should_rollback: false,
            severity: RollbackSeverity::Low,
            causes: Vec::new(),
            reasons: Vec::new(),
        }
    }
}

/// Rollback action type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackAction {
    /// Stop sending traffic to the candidate version.
    DisableCandidateTraffic,
    /// Repoint deployment to stable version.
    RestoreStableVersion,
    /// Validate post-rollback health.
    VerifyHealth,
}

/// A single rollback step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackStep {
    /// Step order (0-based).
    pub order: usize,
    /// Concrete action.
    pub action: RollbackAction,
    /// Action details.
    pub description: String,
}

/// Rollback execution plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackPlan {
    /// Rollback plan id.
    pub id: Uuid,
    /// Release id associated with this rollback.
    pub release_id: Uuid,
    /// Service name.
    pub service: String,
    /// Version being rolled back.
    pub from_version: String,
    /// Stable version restored.
    pub to_version: String,
    /// Ordered rollback actions.
    pub steps: Vec<RollbackStep>,
    /// Plan creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Stateless rollback decision engine.
#[derive(Debug, Clone)]
pub struct RollbackDecider {
    /// Percent deployed that escalates failed health to high severity.
    pub high_risk_percent: u8,
}

impl Default for RollbackDecider {
    fn default() -> Self {
        Self {
            high_risk_percent: 50,
        }
    }
}

impl RollbackDecider {
    /// Determine if a rollback should occur.
    pub fn decide(&self, ctx: &RollbackContext) -> RollbackDecision {
        let mut causes = Vec::new();
        let mut reasons = Vec::new();
        let mut severity = RollbackSeverity::Low;

        if ctx.manual_rollback {
            causes.push(RollbackCause::Manual);
            reasons.push("manual rollback requested by operator".to_string());
            severity = RollbackSeverity::High;
        }

        if let Some(err) = &ctx.deployment_error {
            causes.push(RollbackCause::DeploymentError);
            reasons.push(format!("deployment execution error: {err}"));
            severity = RollbackSeverity::Critical;
        }

        if let Some(health) = &ctx.health
            && !health.passed()
        {
            causes.push(RollbackCause::HealthGateFailed);
            reasons.extend(health.reasons.iter().cloned());

            severity = if ctx.deployed_percent >= self.high_risk_percent {
                RollbackSeverity::Critical
            } else {
                RollbackSeverity::High
            };
        }

        if causes.is_empty() {
            RollbackDecision::no_rollback()
        } else {
            RollbackDecision {
                should_rollback: true,
                severity,
                causes,
                reasons,
            }
        }
    }
}

/// Rollback plan builder.
#[derive(Debug, Default)]
pub struct RollbackPlanner;

impl RollbackPlanner {
    /// Build rollback plan from context + decision.
    pub fn build(ctx: &RollbackContext, decision: &RollbackDecision) -> Option<RollbackPlan> {
        if !decision.should_rollback {
            return None;
        }

        let target_version = ctx
            .to_version
            .clone()
            .unwrap_or_else(|| "unknown-stable".to_string());

        let steps = vec![
            RollbackStep {
                order: 0,
                action: RollbackAction::DisableCandidateTraffic,
                description: format!(
                    "disable traffic to candidate version {} at {}% rollout",
                    ctx.from_version, ctx.deployed_percent
                ),
            },
            RollbackStep {
                order: 1,
                action: RollbackAction::RestoreStableVersion,
                description: format!(
                    "restore stable version {} for service {}",
                    target_version, ctx.service
                ),
            },
            RollbackStep {
                order: 2,
                action: RollbackAction::VerifyHealth,
                description: "run health checks and confirm rollback stability".to_string(),
            },
        ];

        Some(RollbackPlan {
            id: Uuid::new_v4(),
            release_id: ctx.release_id,
            service: ctx.service.clone(),
            from_version: ctx.from_version.clone(),
            to_version: target_version,
            steps,
            created_at: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health_gate::{HealthGate, HealthGateConfig, HealthSnapshot};

    fn context_with_health_fail(deployed_percent: u8) -> RollbackContext {
        let cfg = HealthGateConfig {
            min_success_rate: 0.95,
            max_error_rate: 0.02,
            max_p95_latency_ms: 1500,
            min_availability: 0.99,
            min_sample_size: 50,
        };

        let snapshot = HealthSnapshot::new(0.90, 0.10, 2_500, 0.90, 120);
        let health = HealthGate::evaluate(&cfg, snapshot);

        RollbackContext {
            release_id: Uuid::new_v4(),
            service: "gateway".to_string(),
            from_version: "v2.0.0".to_string(),
            to_version: Some("v1.9.4".to_string()),
            deployed_percent,
            health: Some(health),
            deployment_error: None,
            manual_rollback: false,
        }
    }

    #[test]
    fn decider_requests_rollback_when_health_fails() {
        let ctx = context_with_health_fail(20);
        let decider = RollbackDecider::default();

        let decision = decider.decide(&ctx);

        assert!(decision.should_rollback);
        assert!(decision.causes.contains(&RollbackCause::HealthGateFailed));
        assert_eq!(decision.severity, RollbackSeverity::High);
        assert!(!decision.reasons.is_empty());
    }

    #[test]
    fn decider_escalates_at_high_rollout_percent() {
        let ctx = context_with_health_fail(80);
        let decider = RollbackDecider {
            high_risk_percent: 50,
        };

        let decision = decider.decide(&ctx);

        assert!(decision.should_rollback);
        assert_eq!(decision.severity, RollbackSeverity::Critical);
    }

    #[test]
    fn rollback_plan_contains_restore_step() {
        let ctx = context_with_health_fail(60);
        let decider = RollbackDecider::default();
        let decision = decider.decide(&ctx);

        let plan = RollbackPlanner::build(&ctx, &decision).expect("rollback plan expected");

        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[1].action, RollbackAction::RestoreStableVersion);
        assert_eq!(plan.to_version, "v1.9.4");
    }

    #[test]
    fn planner_returns_none_when_no_rollback_needed() {
        let ctx = RollbackContext {
            release_id: Uuid::new_v4(),
            service: "gateway".to_string(),
            from_version: "v2.0.0".to_string(),
            to_version: Some("v1.9.4".to_string()),
            deployed_percent: 10,
            health: None,
            deployment_error: None,
            manual_rollback: false,
        };

        let decision = RollbackDecider::default().decide(&ctx);
        let plan = RollbackPlanner::build(&ctx, &decision);

        assert!(!decision.should_rollback);
        assert!(plan.is_none());
    }
}
