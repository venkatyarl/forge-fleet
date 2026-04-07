//! Deployment orchestration interfaces and default workflow runner.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::health_gate::{HealthGate, HealthGateConfig, HealthGateEvaluation, HealthSnapshot};
use crate::release::{ReleaseRecord, ReleaseState};
use crate::rollback::{
    RollbackContext, RollbackDecider, RollbackDecision, RollbackPlan, RollbackPlanner,
};
use crate::rollout::{RolloutPlan, RolloutPlanner, RolloutStep};

/// Adapter that performs concrete deployment actions.
pub trait DeploymentAdapter {
    /// Called before rollout begins.
    fn begin(&self, release: &ReleaseRecord, plan: &RolloutPlan) -> Result<()>;

    /// Execute an individual rollout step and return latest health snapshot.
    fn apply_step(&self, release: &ReleaseRecord, step: &RolloutStep) -> Result<HealthSnapshot>;

    /// Called after successful rollout.
    fn finalize(&self, release: &ReleaseRecord) -> Result<()>;

    /// Execute rollback actions.
    fn rollback(&self, release: &ReleaseRecord, plan: &RollbackPlan) -> Result<()>;
}

/// Outcome for each executed rollout step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepOutcome {
    /// Step metadata.
    pub step: RolloutStep,
    /// Health-gate evaluation for step.
    pub health: HealthGateEvaluation,
}

/// Result summary for an orchestrated deployment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentReport {
    /// Final release state.
    pub final_state: ReleaseState,
    /// Built rollout plan.
    pub rollout_plan: RolloutPlan,
    /// Outcomes for executed steps.
    pub step_outcomes: Vec<StepOutcome>,
    /// Rollback decision if triggered.
    pub rollback_decision: Option<RollbackDecision>,
    /// Rollback plan if generated.
    pub rollback_plan: Option<RollbackPlan>,
    /// Start timestamp.
    pub started_at: DateTime<Utc>,
    /// End timestamp.
    pub finished_at: DateTime<Utc>,
}

/// Default deployment orchestrator.
#[derive(Debug, Clone, Default)]
pub struct DeploymentOrchestrator {
    /// Health gate configuration used for every step.
    pub health_config: HealthGateConfig,
    /// Rollback decision policy.
    pub rollback_decider: RollbackDecider,
}

impl DeploymentOrchestrator {
    /// Execute a release rollout using the provided adapter.
    pub fn execute<A: DeploymentAdapter>(
        &self,
        adapter: &A,
        release: &mut ReleaseRecord,
        total_targets: usize,
    ) -> Result<DeploymentReport> {
        let started_at = Utc::now();
        let rollout_plan = RolloutPlanner::build(release.strategy.clone(), total_targets)
            .context("failed to build rollout plan")?;

        release.mark_started();
        info!(
            service = %release.manifest.service,
            version = %release.manifest.version,
            strategy = %release.strategy.name(),
            steps = rollout_plan.steps.len(),
            "starting deployment rollout"
        );

        adapter
            .begin(release, &rollout_plan)
            .context("deployment adapter begin failed")?;

        let mut step_outcomes = Vec::with_capacity(rollout_plan.steps.len());

        for step in &rollout_plan.steps {
            let snapshot = adapter
                .apply_step(release, step)
                .with_context(|| format!("failed to apply rollout step {}", step.index))?;

            let gate = HealthGate::evaluate(&self.health_config, snapshot);
            let passed = gate.passed();
            step_outcomes.push(StepOutcome {
                step: step.clone(),
                health: gate.clone(),
            });

            if !passed {
                warn!(
                    service = %release.manifest.service,
                    step = step.index,
                    target_percent = step.target_percent,
                    reasons = ?gate.reasons,
                    "health gate failed; preparing rollback"
                );

                let rollback_ctx = RollbackContext {
                    release_id: release.manifest.id,
                    service: release.manifest.service.clone(),
                    from_version: release.manifest.version.clone(),
                    to_version: release.manifest.previous_version.clone(),
                    deployed_percent: step.target_percent,
                    health: Some(gate),
                    deployment_error: None,
                    manual_rollback: false,
                };

                let decision = self.rollback_decider.decide(&rollback_ctx);
                let rollback_plan = RollbackPlanner::build(&rollback_ctx, &decision);

                if let Some(plan) = &rollback_plan {
                    adapter
                        .rollback(release, plan)
                        .context("deployment adapter rollback failed")?;
                    release.mark_rolled_back("rollback executed due to failed health gate");
                } else {
                    release.mark_failed("health gate failed and rollback plan was unavailable");
                }

                return Ok(DeploymentReport {
                    final_state: release.state,
                    rollout_plan,
                    step_outcomes,
                    rollback_decision: Some(decision),
                    rollback_plan,
                    started_at,
                    finished_at: Utc::now(),
                });
            }
        }

        adapter
            .finalize(release)
            .context("deployment adapter finalize failed")?;

        release.mark_succeeded();

        Ok(DeploymentReport {
            final_state: release.state,
            rollout_plan,
            step_outcomes,
            rollback_decision: None,
            rollback_plan: None,
            started_at,
            finished_at: Utc::now(),
        })
    }
}
