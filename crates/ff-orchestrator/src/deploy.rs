//! Graceful fleet-deploy orchestration.
//!
//! Sequences the update+restart portion of a graceful rollout — the middle of
//! the `drain → update → restart → re-enable` sequence — into **health-gated
//! batches**. The concrete drain/re-enable that BRACKET a run and the real
//! per-batch update+restart + health-check operations are supplied by the
//! caller: `ff fleet deploy` drains its targets, hands the grouped plans to
//! [`GracefulDeployOrchestrator::run`], then re-enables them, while this module
//! owns only the batching cadence and the fail-safe abort policy.
//!
//! Concretely, the live command wires it as:
//! ```text
//! drain_deploy_targets(..)           // Phase 1: drain (real, Postgres + HA lease release)
//!   -> GracefulDeployOrchestrator::run(ops, group_plans)
//!        for each batch: ops.deploy_batch(..)   // Phase 2+3: update + restart (real, SSH)
//!                        ops.health_check(..)    // gate: post-restart convergence (real)
//! restore_deploy_targets(..)         // Phase 4: re-enable (real, always runs)
//! ```
//!
//! The module is kept free of Postgres/SSH so it unit-tests without a database
//! or a fleet; the live implementation lives behind [`GracefulDeployOps`].

use async_trait::async_trait;

/// Verdict for a just-restarted batch's health check — the gate between
/// batches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchHealth {
    /// Every unit in the batch passed its post-restart health check.
    Healthy,
    /// At least one unit failed; `reason` is surfaced to the operator and, when
    /// [`GracefulDeployConfig::abort_on_unhealthy`] is set, halts the rollout.
    Unhealthy { reason: String },
}

impl BatchHealth {
    pub fn is_healthy(&self) -> bool {
        matches!(self, BatchHealth::Healthy)
    }
}

/// Rollout policy for the batched update+restart phase.
#[derive(Debug, Clone)]
pub struct GracefulDeployConfig {
    /// Units updated+restarted together before the next health gate. Clamped to
    /// at least 1 (a zero batch would never make progress).
    pub batch_size: usize,
    /// Halt the rollout when a batch fails its health check (canary safety),
    /// leaving the not-yet-deployed units untouched for a later re-run rather
    /// than rolling a possibly-bad build across the rest of the fleet.
    pub abort_on_unhealthy: bool,
}

impl Default for GracefulDeployConfig {
    fn default() -> Self {
        Self {
            batch_size: 1,
            abort_on_unhealthy: true,
        }
    }
}

impl GracefulDeployConfig {
    fn effective_batch_size(&self) -> usize {
        self.batch_size.max(1)
    }
}

/// The real per-batch update+restart and health-check operations, injected by
/// the caller. `ff fleet deploy` implements this over live SSH + Postgres;
/// tests implement it in-memory.
#[async_trait]
pub trait GracefulDeployOps: Sync {
    /// One deployable unit — opaque here (e.g. an `(os, arch)` group plan).
    type Unit: Send;
    /// Per-unit outcome the caller reports (e.g. one host's deploy result).
    type Outcome: Send;

    /// Update + restart every unit in `batch`, returning their outcomes. Must
    /// not panic on a single-unit failure — encode the failure in the returned
    /// outcome so [`GracefulDeployOps::health_check`] can see it.
    async fn deploy_batch(&self, batch: Vec<Self::Unit>) -> Vec<Self::Outcome>;

    /// Assess a just-deployed batch's post-restart health (e.g. every host
    /// converged on the new SHA). This is the gate between batches.
    async fn health_check(&self, outcomes: &[Self::Outcome]) -> BatchHealth;
}

/// Outcome of a batched rollout.
pub struct GracefulDeployRun<U, O> {
    /// Per-unit outcomes for every batch that ran, in order.
    pub outcomes: Vec<O>,
    /// Units never deployed because an unhealthy batch aborted the rollout.
    /// Empty on a clean run; the caller surfaces these so a halted rollout
    /// isn't reported as full coverage.
    pub skipped: Vec<U>,
    /// Reason the rollout halted early, if it did.
    pub aborted: Option<String>,
    /// Number of health-gated waves that actually ran.
    pub batches_deployed: usize,
}

/// Drives the batched, health-gated update+restart phase of a graceful deploy.
///
/// This type owns only the *cadence and safety policy*; the real work is done
/// through the injected [`GracefulDeployOps`], and drain/re-enable bracket the
/// run in the caller (see the module docs).
pub struct GracefulDeployOrchestrator {
    config: GracefulDeployConfig,
}

impl GracefulDeployOrchestrator {
    pub fn new(config: GracefulDeployConfig) -> Self {
        Self { config }
    }

    /// Update+restart `units` in `batch_size` waves, running the health gate
    /// after each. With [`GracefulDeployConfig::abort_on_unhealthy`], the first
    /// unhealthy wave stops the rollout and the remaining units are returned in
    /// [`GracefulDeployRun::skipped`] (the caller re-enables its targets
    /// regardless — this method never touches drain/re-enable state).
    pub async fn run<O: GracefulDeployOps>(
        &self,
        ops: &O,
        units: Vec<O::Unit>,
    ) -> GracefulDeployRun<O::Unit, O::Outcome> {
        let batch_size = self.config.effective_batch_size();
        let mut remaining = units.into_iter();
        let mut outcomes: Vec<O::Outcome> = Vec::new();
        let mut aborted: Option<String> = None;
        let mut batches_deployed = 0usize;

        loop {
            let batch: Vec<O::Unit> = remaining.by_ref().take(batch_size).collect();
            if batch.is_empty() {
                break;
            }
            // Phase 2+3: update + restart this wave, then gate on its health.
            let batch_outcomes = ops.deploy_batch(batch).await;
            let health = ops.health_check(&batch_outcomes).await;
            outcomes.extend(batch_outcomes);
            batches_deployed += 1;

            if let BatchHealth::Unhealthy { reason } = health {
                if self.config.abort_on_unhealthy {
                    aborted = Some(reason);
                    break;
                }
            }
        }

        let skipped: Vec<O::Unit> = remaining.collect();
        GracefulDeployRun {
            outcomes,
            skipped,
            aborted,
            batches_deployed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory ops: units and outcomes are both `u32`; each batch echoes its
    /// units, and any batch index in `unhealthy_batches` reports unhealthy.
    struct MockOps {
        unhealthy_batches: Vec<usize>,
        batches_seen: Mutex<Vec<Vec<u32>>>,
        health_calls: Mutex<usize>,
    }

    impl MockOps {
        fn new(unhealthy_batches: Vec<usize>) -> Self {
            Self {
                unhealthy_batches,
                batches_seen: Mutex::new(Vec::new()),
                health_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl GracefulDeployOps for MockOps {
        type Unit = u32;
        type Outcome = u32;

        async fn deploy_batch(&self, batch: Vec<u32>) -> Vec<u32> {
            self.batches_seen.lock().unwrap().push(batch.clone());
            batch
        }

        async fn health_check(&self, _outcomes: &[u32]) -> BatchHealth {
            let mut idx = self.health_calls.lock().unwrap();
            let this = *idx;
            *idx += 1;
            if self.unhealthy_batches.contains(&this) {
                BatchHealth::Unhealthy {
                    reason: format!("batch {this} unhealthy"),
                }
            } else {
                BatchHealth::Healthy
            }
        }
    }

    #[tokio::test]
    async fn deploys_every_unit_when_healthy() {
        let ops = MockOps::new(vec![]);
        let orch = GracefulDeployOrchestrator::new(GracefulDeployConfig {
            batch_size: 2,
            abort_on_unhealthy: true,
        });
        let run = orch.run(&ops, vec![1, 2, 3, 4, 5]).await;
        assert_eq!(run.outcomes, vec![1, 2, 3, 4, 5]);
        assert!(run.skipped.is_empty());
        assert!(run.aborted.is_none());
        assert_eq!(run.batches_deployed, 3);
        assert_eq!(
            *ops.batches_seen.lock().unwrap(),
            vec![vec![1, 2], vec![3, 4], vec![5]]
        );
    }

    #[tokio::test]
    async fn aborts_and_skips_remaining_on_unhealthy() {
        let ops = MockOps::new(vec![0]);
        let orch = GracefulDeployOrchestrator::new(GracefulDeployConfig {
            batch_size: 2,
            abort_on_unhealthy: true,
        });
        let run = orch.run(&ops, vec![1, 2, 3, 4, 5]).await;
        // Only the first (unhealthy) wave ran; the rest are left untouched.
        assert_eq!(run.outcomes, vec![1, 2]);
        assert_eq!(run.skipped, vec![3, 4, 5]);
        assert!(run.aborted.is_some());
        assert_eq!(run.batches_deployed, 1);
    }

    #[tokio::test]
    async fn continues_through_unhealthy_when_abort_disabled() {
        let ops = MockOps::new(vec![0]);
        let orch = GracefulDeployOrchestrator::new(GracefulDeployConfig {
            batch_size: 2,
            abort_on_unhealthy: false,
        });
        let run = orch.run(&ops, vec![1, 2, 3, 4, 5]).await;
        assert_eq!(run.outcomes, vec![1, 2, 3, 4, 5]);
        assert!(run.skipped.is_empty());
        assert!(run.aborted.is_none());
        assert_eq!(run.batches_deployed, 3);
    }

    #[tokio::test]
    async fn zero_batch_size_is_clamped_to_one() {
        let ops = MockOps::new(vec![]);
        let orch = GracefulDeployOrchestrator::new(GracefulDeployConfig {
            batch_size: 0,
            abort_on_unhealthy: true,
        });
        let run = orch.run(&ops, vec![1, 2, 3]).await;
        assert_eq!(run.batches_deployed, 3);
        assert_eq!(
            *ops.batches_seen.lock().unwrap(),
            vec![vec![1], vec![2], vec![3]]
        );
    }

    #[tokio::test]
    async fn empty_units_deploys_nothing() {
        let ops = MockOps::new(vec![]);
        let orch = GracefulDeployOrchestrator::new(GracefulDeployConfig::default());
        let run = orch.run(&ops, Vec::<u32>::new()).await;
        assert!(run.outcomes.is_empty());
        assert_eq!(run.batches_deployed, 0);
        assert!(run.aborted.is_none());
    }
}
