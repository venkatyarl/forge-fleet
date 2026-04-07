//! Rollout planning for canary, staged, and full deployment strategies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::strategy::{RolloutStrategy, StrategyError};

/// Rollout step phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutPhase {
    /// Canary step.
    Canary,
    /// Staged step with 1-based stage index.
    Stage { stage: usize },
    /// Full rollout step.
    Full,
}

/// A single rollout step in the execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutStep {
    /// Step index (0-based).
    pub index: usize,
    /// Rollout phase.
    pub phase: RolloutPhase,
    /// Cumulative deployment target in percent.
    pub target_percent: u8,
    /// Cumulative target count of nodes/instances.
    pub target_count: usize,
    /// Pause duration after this step, if any.
    pub pause_after_secs: u64,
}

/// Complete rollout plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutPlan {
    /// Unique plan id.
    pub id: Uuid,
    /// Strategy used to generate the plan.
    pub strategy: RolloutStrategy,
    /// Total deployment targets (nodes/instances).
    pub total_targets: usize,
    /// Ordered rollout steps.
    pub steps: Vec<RolloutStep>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

impl RolloutPlan {
    /// Final target percentage reached by this plan.
    pub fn final_percent(&self) -> Option<u8> {
        self.steps.last().map(|s| s.target_percent)
    }
}

/// Planner for rollout strategies.
#[derive(Debug, Default)]
pub struct RolloutPlanner;

impl RolloutPlanner {
    /// Build a rollout plan for a strategy and target population.
    pub fn build(
        strategy: RolloutStrategy,
        total_targets: usize,
    ) -> Result<RolloutPlan, RolloutError> {
        strategy.validate().map_err(RolloutError::InvalidStrategy)?;

        if total_targets == 0 {
            return Err(RolloutError::InvalidTargetCount(total_targets));
        }

        let progression = strategy.progression();
        let pause = strategy.pause_secs();

        let mut steps = Vec::with_capacity(progression.len());
        let mut last_count = 0usize;

        for (idx, pct) in progression.into_iter().enumerate() {
            let mut target_count = ceil_percent(total_targets, pct);

            // Ensure strictly increasing cumulative counts when possible.
            if target_count <= last_count && last_count < total_targets {
                target_count = (last_count + 1).min(total_targets);
            }
            if pct == 100 {
                target_count = total_targets;
            }

            let phase = phase_for(&strategy, idx, pct);
            let pause_after_secs = if pct < 100 { pause } else { 0 };

            steps.push(RolloutStep {
                index: idx,
                phase,
                target_percent: pct,
                target_count,
                pause_after_secs,
            });

            last_count = target_count;
        }

        Ok(RolloutPlan {
            id: Uuid::new_v4(),
            strategy,
            total_targets,
            steps,
            created_at: Utc::now(),
        })
    }
}

fn phase_for(strategy: &RolloutStrategy, idx: usize, pct: u8) -> RolloutPhase {
    match strategy {
        RolloutStrategy::Canary(_) => {
            if pct == 100 {
                RolloutPhase::Full
            } else {
                RolloutPhase::Canary
            }
        }
        RolloutStrategy::Staged(_) => {
            if pct == 100 {
                RolloutPhase::Full
            } else {
                RolloutPhase::Stage { stage: idx + 1 }
            }
        }
        RolloutStrategy::Full(_) => RolloutPhase::Full,
    }
}

fn ceil_percent(total: usize, percent: u8) -> usize {
    if percent == 0 {
        return 0;
    }
    (total * percent as usize).div_ceil(100)
}

/// Rollout planning error.
#[derive(Debug, Error)]
pub enum RolloutError {
    /// Strategy failed validation.
    #[error("invalid rollout strategy: {0}")]
    InvalidStrategy(#[from] StrategyError),
    /// No targets supplied.
    #[error("invalid target count: {0} (must be > 0)")]
    InvalidTargetCount(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::{CanaryStrategy, FullStrategy, StagedStrategy};

    #[test]
    fn canary_plan_progresses_and_finishes_at_100() {
        let strategy = RolloutStrategy::Canary(CanaryStrategy {
            initial_percent: 10,
            step_percent: 20,
            pause_secs: 30,
            max_error_rate: 0.02,
        });

        let plan = RolloutPlanner::build(strategy, 10).expect("plan should build");

        let percents: Vec<u8> = plan.steps.iter().map(|s| s.target_percent).collect();
        assert_eq!(percents, vec![10, 30, 50, 70, 90, 100]);

        assert_eq!(plan.steps[0].target_count, 1);
        assert_eq!(plan.steps.last().expect("has last").target_count, 10);
        assert_eq!(plan.final_percent(), Some(100));
    }

    #[test]
    fn staged_plan_maps_stage_phases() {
        let strategy = RolloutStrategy::Staged(StagedStrategy {
            progression: vec![25, 50, 100],
            pause_secs: 60,
            max_error_rate: 0.03,
        });

        let plan = RolloutPlanner::build(strategy, 8).expect("plan should build");

        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].phase, RolloutPhase::Stage { stage: 1 });
        assert_eq!(plan.steps[1].phase, RolloutPhase::Stage { stage: 2 });
        assert_eq!(plan.steps[2].phase, RolloutPhase::Full);

        let counts: Vec<usize> = plan.steps.iter().map(|s| s.target_count).collect();
        assert_eq!(counts, vec![2, 4, 8]);
    }

    #[test]
    fn full_plan_single_step() {
        let strategy = RolloutStrategy::Full(FullStrategy::default());
        let plan = RolloutPlanner::build(strategy, 3).expect("plan should build");

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].phase, RolloutPhase::Full);
        assert_eq!(plan.steps[0].target_percent, 100);
        assert_eq!(plan.steps[0].target_count, 3);
    }

    #[test]
    fn rollout_rejects_zero_targets() {
        let strategy = RolloutStrategy::Full(FullStrategy::default());
        let err = RolloutPlanner::build(strategy, 0).expect_err("must reject zero targets");

        assert!(matches!(err, RolloutError::InvalidTargetCount(0)));
    }
}
