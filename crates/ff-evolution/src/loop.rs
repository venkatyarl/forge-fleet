//! Evolution loop state machine.
//!
//! Flow: observe → analyze → propose → apply → verify.

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::analyzer::{AnalysisReport, FailureAnalyzer, FailureObservation};
use crate::backlog::BacklogService;
use crate::learning::{LearningOutcome, LearningStore};
use crate::repair::{RepairAction, RepairPlanner, RepairStatus};
use crate::verification::{
    VerificationInput, VerificationModel, VerificationOutcome, VerificationReport,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionPhase {
    Idle,
    Observing,
    Analyzing,
    Proposing,
    Applying,
    Verifying,
    Completed,
    Suppressed,
    RolledBack,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionState {
    pub run_id: Uuid,
    pub phase: EvolutionPhase,
    pub history: Vec<EvolutionPhase>,
    pub iteration: u64,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub observation: Option<FailureObservation>,
    pub analysis: Option<AnalysisReport>,
    pub proposals: Vec<RepairAction>,
    pub selected_action: Option<RepairAction>,
    pub verification: Option<VerificationReport>,
}

impl EvolutionState {
    pub fn new(iteration: u64) -> Self {
        let now = Utc::now();
        Self {
            run_id: Uuid::new_v4(),
            phase: EvolutionPhase::Idle,
            history: vec![EvolutionPhase::Idle],
            iteration,
            started_at: now,
            updated_at: now,
            observation: None,
            analysis: None,
            proposals: Vec::new(),
            selected_action: None,
            verification: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionRun {
    pub state: EvolutionState,
    pub durable_backlog_items_created: usize,
}

#[derive(Debug, Error)]
pub enum EvolutionError {
    #[error("invalid transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: EvolutionPhase,
        to: EvolutionPhase,
    },

    #[error("missing required state component: {0}")]
    MissingState(&'static str),
}

/// Autonomous engine orchestrating analysis, repair, verification, and learning.
#[derive(Clone)]
pub struct EvolutionEngine {
    pub analyzer: FailureAnalyzer,
    pub planner: RepairPlanner,
    pub verifier: VerificationModel,
    pub learning: LearningStore,
    pub backlog: BacklogService,
    interval: Duration,
}

impl std::fmt::Debug for EvolutionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvolutionEngine")
            .field("analyzer", &self.analyzer)
            .field("planner", &self.planner)
            .field("verifier", &self.verifier)
            .field("learning", &self.learning)
            .field("backlog", &self.backlog)
            .field("interval", &self.interval)
            .finish()
    }
}

impl Default for EvolutionEngine {
    fn default() -> Self {
        Self {
            analyzer: FailureAnalyzer::new(),
            planner: RepairPlanner::new(),
            verifier: VerificationModel::default(),
            learning: LearningStore::default(),
            backlog: BacklogService::default(),
            interval: Duration::from_secs(60),
        }
    }
}

impl EvolutionEngine {
    pub fn new(
        analyzer: FailureAnalyzer,
        planner: RepairPlanner,
        verifier: VerificationModel,
        learning: LearningStore,
        backlog: BacklogService,
    ) -> Self {
        Self {
            analyzer,
            planner,
            verifier,
            learning,
            backlog,
            interval: Duration::from_secs(60),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Run one full evolution cycle on an observation.
    pub fn run_once(
        &self,
        observation: FailureObservation,
        verification_input: VerificationInput,
    ) -> Result<EvolutionRun, EvolutionError> {
        let mut state = EvolutionState::new(1);

        self.transition(&mut state, EvolutionPhase::Observing)?;
        state.observation = Some(observation);

        self.transition(&mut state, EvolutionPhase::Analyzing)?;
        let analysis = self.analyzer.analyze(
            state
                .observation
                .as_ref()
                .ok_or(EvolutionError::MissingState("observation"))?,
        );
        let promoted = self.backlog.ingest_report(&analysis);
        state.analysis = Some(analysis.clone());

        self.transition(&mut state, EvolutionPhase::Proposing)?;
        let mut proposals = self.planner.propose_actions(&analysis);

        // Suppress repeatedly failing strategies.
        proposals.retain(|action| {
            !self
                .learning
                .is_suppressed(&action.cause_fingerprint, action.strategy, Utc::now())
        });

        if proposals.is_empty() {
            self.transition(&mut state, EvolutionPhase::Suppressed)?;
            return Ok(EvolutionRun {
                state,
                durable_backlog_items_created: promoted.len(),
            });
        }

        state.proposals = proposals.clone();
        let mut selected = self
            .planner
            .best_candidate(&proposals)
            .ok_or(EvolutionError::MissingState("repair proposal"))?;

        self.transition(&mut state, EvolutionPhase::Applying)?;
        selected.status = RepairStatus::Applied;
        self.planner.track(selected.clone());
        state.selected_action = Some(selected.clone());

        self.transition(&mut state, EvolutionPhase::Verifying)?;
        let verification = self.verifier.verify(&selected, &verification_input);
        state.verification = Some(verification.clone());

        match verification.outcome {
            VerificationOutcome::Passed => {
                self.planner
                    .update_status(selected.id, RepairStatus::Verified);
                self.learning.record(
                    selected.cause_fingerprint.clone(),
                    selected.strategy,
                    LearningOutcome::Success,
                    "repair verified",
                    verification.improvement_ratio,
                );
                self.transition(&mut state, EvolutionPhase::Completed)?;
            }
            VerificationOutcome::Failed => {
                self.planner
                    .update_status(selected.id, RepairStatus::Failed);
                self.learning.record(
                    selected.cause_fingerprint.clone(),
                    selected.strategy,
                    LearningOutcome::Failure,
                    "repair failed verification",
                    -0.25,
                );

                if self.verifier.should_rollback(&verification, &selected) {
                    self.planner
                        .update_status(selected.id, RepairStatus::RolledBack);
                    self.learning.record(
                        selected.cause_fingerprint.clone(),
                        selected.strategy,
                        LearningOutcome::RolledBack,
                        "rollback triggered by verifier",
                        -0.3,
                    );
                    self.transition(&mut state, EvolutionPhase::RolledBack)?;
                } else {
                    self.transition(&mut state, EvolutionPhase::Failed)?;
                }
            }
            VerificationOutcome::Inconclusive => {
                self.learning.record(
                    selected.cause_fingerprint.clone(),
                    selected.strategy,
                    LearningOutcome::Failure,
                    "verification inconclusive; treat as soft failure for suppression stats",
                    -0.05,
                );
                self.transition(&mut state, EvolutionPhase::Failed)?;
            }
        }

        Ok(EvolutionRun {
            state,
            durable_backlog_items_created: promoted.len(),
        })
    }

    /// Run periodic cycles from an external observation source.
    pub async fn run_periodic<F, Fut>(
        &self,
        mut fetch: F,
        max_cycles: usize,
    ) -> anyhow::Result<Vec<EvolutionRun>>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Option<(FailureObservation, VerificationInput)>>,
    {
        let mut ticker = tokio::time::interval(self.interval);
        let mut runs = Vec::new();

        for _ in 0..max_cycles {
            ticker.tick().await;
            if let Some((observation, verification_input)) = fetch().await {
                let run = self.run_once(observation, verification_input)?;
                runs.push(run);
            }
        }

        Ok(runs)
    }

    fn transition(
        &self,
        state: &mut EvolutionState,
        next: EvolutionPhase,
    ) -> Result<(), EvolutionError> {
        if !is_valid_transition(state.phase, next) {
            return Err(EvolutionError::InvalidTransition {
                from: state.phase,
                to: next,
            });
        }

        state.phase = next;
        state.history.push(next);
        state.updated_at = Utc::now();
        Ok(())
    }
}

fn is_valid_transition(from: EvolutionPhase, to: EvolutionPhase) -> bool {
    match from {
        EvolutionPhase::Idle => matches!(to, EvolutionPhase::Observing),
        EvolutionPhase::Observing => matches!(to, EvolutionPhase::Analyzing),
        EvolutionPhase::Analyzing => matches!(to, EvolutionPhase::Proposing),
        EvolutionPhase::Proposing => {
            matches!(
                to,
                EvolutionPhase::Applying | EvolutionPhase::Suppressed | EvolutionPhase::Failed
            )
        }
        EvolutionPhase::Applying => {
            matches!(to, EvolutionPhase::Verifying | EvolutionPhase::Failed)
        }
        EvolutionPhase::Verifying => {
            matches!(
                to,
                EvolutionPhase::Completed | EvolutionPhase::RolledBack | EvolutionPhase::Failed
            )
        }
        EvolutionPhase::Completed
        | EvolutionPhase::Suppressed
        | EvolutionPhase::RolledBack
        | EvolutionPhase::Failed => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::FailureSource;
    use crate::learning::SuppressionPolicy;
    use crate::repair::RepairStrategy;

    #[test]
    fn loop_transitions_to_completed_on_verified_fix() {
        let engine = EvolutionEngine::default();

        let obs = FailureObservation::new(
            FailureSource::Build,
            "cargo check failed",
            "error: cannot find type `X` in this scope\nfailed to compile",
        );

        let run = engine
            .run_once(obs, VerificationInput::success(0.4, 0.05))
            .expect("run should succeed");

        assert_eq!(run.state.phase, EvolutionPhase::Completed);
        assert_eq!(
            run.state.history,
            vec![
                EvolutionPhase::Idle,
                EvolutionPhase::Observing,
                EvolutionPhase::Analyzing,
                EvolutionPhase::Proposing,
                EvolutionPhase::Applying,
                EvolutionPhase::Verifying,
                EvolutionPhase::Completed,
            ]
        );
    }

    #[test]
    fn loop_stops_at_suppressed_when_strategy_is_blocked() {
        let mut engine = EvolutionEngine::default();
        engine.learning = LearningStore::new(SuppressionPolicy {
            max_failures: 1,
            cooldown_minutes: 120,
            failure_lookback_minutes: 240,
        });

        let obs = FailureObservation::new(
            FailureSource::Build,
            "compile issue",
            "error: cannot find function `foo`\nfailed to compile",
        );

        // Prime suppression for the same fingerprint + strategy.
        let report = engine.analyzer.analyze(&obs);
        let fp = report.primary.expect("primary cause").fingerprint;
        engine.learning.record(
            fp.clone(),
            RepairStrategy::FixCompilation,
            LearningOutcome::Failure,
            "previous failure",
            -0.3,
        );

        let run = engine
            .run_once(obs, VerificationInput::success(0.3, 0.2))
            .expect("run should complete in suppressed state");

        assert_eq!(run.state.phase, EvolutionPhase::Suppressed);
        assert_eq!(
            run.state.history,
            vec![
                EvolutionPhase::Idle,
                EvolutionPhase::Observing,
                EvolutionPhase::Analyzing,
                EvolutionPhase::Proposing,
                EvolutionPhase::Suppressed,
            ]
        );
    }
}
