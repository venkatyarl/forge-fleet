//! `ff-deploy` — deployment/release orchestration primitives for ForgeFleet.
//!
//! This crate provides:
//! - release domain models (`release`)
//! - deploy target resolution with retry (`resolution`)
//! - rollout strategy + planning (`strategy`, `rollout`)
//! - health gate evaluation (`health_gate`)
//! - rollback decisioning and planning (`rollback`)
//! - deployment orchestration interfaces (`deployer`)

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

pub mod config;
pub mod daemon;
pub mod deploy;
#[cfg(test)]
mod deploy_tests;
pub mod deployer;
pub mod git_utils;
pub mod health_gate;
pub mod node;
pub mod release;
pub mod resolution;
pub mod rollback;
pub mod rollout;
pub mod strategy;

pub use config::DeployConfig;
pub use daemon::{ActiveLease, RestartReport, restart_with_lease_drain};
pub use deploy::{
    LeaseSource, RestartCoordinator, git_fetch_and_reset_hard, git_stash_dirty_tree,
    git_tree_is_dirty,
};
pub use deployer::{DeploymentAdapter, DeploymentOrchestrator, DeploymentReport, StepOutcome};

pub use health_gate::{
    HealthGate, HealthGateConfig, HealthGateEvaluation, HealthGateStatus, HealthSnapshot,
};
pub use node::batch::{
    BatchUpdateConfig, BatchUpdateReport, NodeUpdateOutcome, NodeUpdateResult,
    probe_forgefleetd_health, restart_node_local, run_batched_update, update_node_checkout,
};
pub use node::{
    drain_active_work_item_leases, forgefleetd_restart_command, requeue_claimed_items,
    restart_forgefleetd_local, restart_forgefleetd_local_with_drain,
    restart_forgefleetd_with_drain,
};
pub use release::{ReleaseChannel, ReleaseManifest, ReleaseRecord, ReleaseState};
pub use resolution::{
    ResolutionError, ResolutionRetryPolicy, ResolvedTarget, TargetLike,
    resolve_all_with_retry_async, resolve_with_retry, resolve_with_retry_async,
};
pub use rollback::{
    RollbackAction, RollbackCause, RollbackContext, RollbackDecider, RollbackDecision,
    RollbackPlan, RollbackPlanner, RollbackSeverity, RollbackStep,
};
pub use rollout::{RolloutError, RolloutPhase, RolloutPlan, RolloutPlanner, RolloutStep};
pub use strategy::{CanaryStrategy, FullStrategy, RolloutStrategy, StagedStrategy, StrategyError};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The legacy shell version reconciler is retired. Deployment convergence must
/// be driven through [`run_native_deploy_tick`].
pub const SHELL_RECONCILER_ENABLED: bool = false;

/// A node participating in a native deployment tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDeployTarget {
    pub name: String,
    pub architecture: String,
}

/// Result of deploying one target during a native tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeDeployOutcome {
    Restarted,
    BuildFailed(String),
    TransferFailed(String),
    CleanupFailed(String),
    RestartFailed(String),
}

/// Per-target report returned by [`run_native_deploy_tick`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDeployResult {
    pub target: NativeDeployTarget,
    pub outcome: NativeDeployOutcome,
}

/// Complete report for one native deployment tick.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NativeDeployReport {
    pub results: Vec<NativeDeployResult>,
}

impl NativeDeployReport {
    pub fn converged(&self) -> bool {
        self.results
            .iter()
            .all(|result| result.outcome == NativeDeployOutcome::Restarted)
    }
}

/// Boxed future used by [`NativeDeployOperations`].
pub type NativeDeployFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Native operations required by one deployment tick.
///
/// `restart_with_drain` must drain the target's active leases before it
/// restarts the daemon. Keeping that operation explicit prevents the native
/// tick from accidentally falling back to the retired shell reconciler.
pub trait NativeDeployOperations {
    type Artifact: Clone + Send + Sync;

    fn build<'a>(
        &'a self,
        builder: &'a NativeDeployTarget,
    ) -> NativeDeployFuture<'a, Self::Artifact>;

    fn transfer<'a>(
        &'a self,
        artifact: &'a Self::Artifact,
        target: &'a NativeDeployTarget,
    ) -> NativeDeployFuture<'a, ()>;

    fn cleanup<'a>(
        &'a self,
        artifact: &'a Self::Artifact,
        target: &'a NativeDeployTarget,
    ) -> NativeDeployFuture<'a, ()>;

    fn restart_with_drain<'a>(
        &'a self,
        target: &'a NativeDeployTarget,
    ) -> NativeDeployFuture<'a, ()>;
}

/// Run one shell-free deployment convergence tick.
///
/// Targets are grouped by architecture and the first target in each sorted
/// group builds exactly once. That artifact is transferred to every target in
/// the group. Cleanup is attempted after every transfer, including failed
/// transfers, and restart-with-drain only runs after both transfer and cleanup
/// succeed.
pub async fn run_native_deploy_tick<O>(
    targets: impl IntoIterator<Item = NativeDeployTarget>,
    operations: &O,
) -> NativeDeployReport
where
    O: NativeDeployOperations + Sync,
{
    let mut groups = BTreeMap::<String, Vec<NativeDeployTarget>>::new();
    for target in targets {
        groups
            .entry(target.architecture.trim().to_ascii_lowercase())
            .or_default()
            .push(target);
    }

    let mut report = NativeDeployReport::default();
    for targets in groups.values_mut() {
        targets.sort_by(|left, right| left.name.cmp(&right.name));
        let builder = targets
            .first()
            .expect("architecture groups are never empty");
        let artifact = match operations.build(builder).await {
            Ok(artifact) => artifact,
            Err(error) => {
                let error = error.to_string();
                report
                    .results
                    .extend(targets.iter().cloned().map(|target| NativeDeployResult {
                        target,
                        outcome: NativeDeployOutcome::BuildFailed(error.clone()),
                    }));
                continue;
            }
        };

        for target in targets.iter() {
            let transfer = operations.transfer(&artifact, target).await;
            let cleanup = operations.cleanup(&artifact, target).await;
            let outcome = match (transfer, cleanup) {
                (Err(error), _) => NativeDeployOutcome::TransferFailed(error.to_string()),
                (Ok(()), Err(error)) => NativeDeployOutcome::CleanupFailed(error.to_string()),
                (Ok(()), Ok(())) => match operations.restart_with_drain(target).await {
                    Ok(()) => NativeDeployOutcome::Restarted,
                    Err(error) => NativeDeployOutcome::RestartFailed(error.to_string()),
                },
            };
            report.results.push(NativeDeployResult {
                target: target.clone(),
                outcome,
            });
        }
    }

    report
}

#[cfg(test)]
mod native_tick_tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingOperations {
        calls: Mutex<Vec<String>>,
    }

    impl NativeDeployOperations for RecordingOperations {
        type Artifact = String;

        fn build<'a>(
            &'a self,
            builder: &'a NativeDeployTarget,
        ) -> NativeDeployFuture<'a, Self::Artifact> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("build:{}", builder.name));
                Ok(builder.architecture.clone())
            })
        }

        fn transfer<'a>(
            &'a self,
            _artifact: &'a Self::Artifact,
            target: &'a NativeDeployTarget,
        ) -> NativeDeployFuture<'a, ()> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("transfer:{}", target.name));
                Ok(())
            })
        }

        fn cleanup<'a>(
            &'a self,
            _artifact: &'a Self::Artifact,
            target: &'a NativeDeployTarget,
        ) -> NativeDeployFuture<'a, ()> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("cleanup:{}", target.name));
                Ok(())
            })
        }

        fn restart_with_drain<'a>(
            &'a self,
            target: &'a NativeDeployTarget,
        ) -> NativeDeployFuture<'a, ()> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("restart:{}", target.name));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn builds_once_per_arch_then_transfers_cleans_and_restarts() {
        let operations = RecordingOperations::default();
        let report = run_native_deploy_tick(
            [
                NativeDeployTarget {
                    name: "b".into(),
                    architecture: "x86_64".into(),
                },
                NativeDeployTarget {
                    name: "a".into(),
                    architecture: "x86_64".into(),
                },
                NativeDeployTarget {
                    name: "c".into(),
                    architecture: "aarch64".into(),
                },
            ],
            &operations,
        )
        .await;

        assert!(report.converged());
        assert_eq!(
            *operations.calls.lock().unwrap(),
            vec![
                "build:c",
                "transfer:c",
                "cleanup:c",
                "restart:c",
                "build:a",
                "transfer:a",
                "cleanup:a",
                "restart:a",
                "transfer:b",
                "cleanup:b",
                "restart:b",
            ]
        );
    }
}
