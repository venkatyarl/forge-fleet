//! Batched node updates: per-node health gating before update + restart.
//!
//! A fleet-wide update must not race ahead of a bad rollout: nodes are
//! processed in fixed-size batches, and within a batch every node is
//! health-checked, updated, and restarted *concurrently* (`batch_size` is the
//! actual in-flight cap, not just a bookkeeping boundary). A batch must fully
//! settle — including its health checks — before the next batch starts, so
//! a bad build halts the rollout at a batch boundary instead of racing ahead
//! fleet-wide. This mirrors the stop-on-failure semantics
//! [`crate::deployer::DeploymentOrchestrator`] already applies to
//! percentage-based rollouts, but keyed on discrete nodes processed
//! `batch_size` at a time instead of traffic percentage.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::future::join_all;
use tracing::{info, warn};

use crate::git_utils::git_fetch_and_reset_hard;
use crate::health_gate::{HealthGate, HealthGateConfig, HealthGateEvaluation, HealthSnapshot};

use super::restart_forgefleetd_local;

/// Configuration for a batched node update run.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchUpdateConfig {
    /// Number of nodes health-checked/updated/restarted concurrently per
    /// batch. Must be > 0.
    pub batch_size: usize,
    /// Health gate thresholds evaluated before touching each node.
    pub health_gate: HealthGateConfig,
}

/// Per-node outcome of a batched update run.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeUpdateOutcome {
    /// Node identifier (hostname/address) as passed to `run_batched_update`.
    pub node: String,
    /// Health gate evaluation performed before the update was attempted.
    pub health: HealthGateEvaluation,
    /// What happened to this node.
    pub result: NodeUpdateResult,
}

/// Result of attempting to update and restart a single node.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeUpdateResult {
    /// Health gate passed, update applied, and `forgefleetd` restarted.
    Updated,
    /// Health gate failed; the node was left untouched.
    SkippedUnhealthy,
    /// Health gate passed but the update step failed.
    UpdateFailed(String),
    /// Update succeeded but the `forgefleetd` restart failed.
    RestartFailed(String),
}

/// Summary of a batched update run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BatchUpdateReport {
    /// Outcome for every node that was evaluated (in order).
    pub outcomes: Vec<NodeUpdateOutcome>,
    /// Index (0-based) of the batch after which the run stopped early due to
    /// a failure, or `None` if every batch completed.
    pub aborted_after_batch: Option<usize>,
}

impl BatchUpdateReport {
    /// `true` if every evaluated node updated and restarted cleanly.
    pub fn all_succeeded(&self) -> bool {
        self.aborted_after_batch.is_none()
            && self
                .outcomes
                .iter()
                .all(|o| o.result == NodeUpdateResult::Updated)
    }
}

/// Run a batched node update: nodes are grouped into fixed-size batches and,
/// within a batch, health-checked before the update and restart are applied
/// to that node.
///
/// Nodes within a batch are processed *concurrently* — `batch_size` bounds
/// how many nodes are in flight at once, not just where the abort check is
/// evaluated. If any node in a batch is unhealthy, fails to update, or fails
/// to restart, the rest of that batch still runs to completion (so the
/// caller gets a full picture of the batch), but no further batches are
/// started — the run stops with `aborted_after_batch` set to that batch's
/// index.
///
/// `health_check`, `update_node`, and `restart_node` are injected so this can
/// be driven by real per-node probes/SSH commands in production and by fakes
/// in tests, following the same closure-injection pattern as
/// [`super::restart_forgefleetd_with_drain`].
pub async fn run_batched_update<S, H, HFut, U, UFut, R, RFut>(
    nodes: &[S],
    config: &BatchUpdateConfig,
    health_check: H,
    update_node: U,
    restart_node: R,
) -> Result<BatchUpdateReport>
where
    S: AsRef<str>,
    H: Fn(&str) -> HFut,
    HFut: Future<Output = HealthSnapshot>,
    U: Fn(&str) -> UFut,
    UFut: Future<Output = Result<()>>,
    R: Fn(&str) -> RFut,
    RFut: Future<Output = Result<()>>,
{
    anyhow::ensure!(
        config.batch_size > 0,
        "batch_size must be greater than zero"
    );

    let mut outcomes = Vec::with_capacity(nodes.len());
    let mut aborted_after_batch = None;

    let health_check = &health_check;
    let update_node = &update_node;
    let restart_node = &restart_node;

    for (batch_index, batch) in nodes.chunks(config.batch_size).enumerate() {
        // Drive every node in this batch concurrently: `batch_size` is the
        // actual in-flight cap, not just a bookkeeping boundary. The next
        // batch is only started once every future here has resolved.
        let batch_outcomes = join_all(batch.iter().map(|node| {
            let node_name = node.as_ref().to_string();
            async move {
                let snapshot = health_check(&node_name).await;
                let health = HealthGate::evaluate(&config.health_gate, snapshot);

                if !health.passed() {
                    warn!(
                        node = node_name,
                        reasons = ?health.reasons,
                        "batched node update: pre-update health gate failed; skipping node"
                    );
                    return NodeUpdateOutcome {
                        node: node_name,
                        health,
                        result: NodeUpdateResult::SkippedUnhealthy,
                    };
                }

                let result = match update_node(&node_name).await {
                    Ok(()) => match restart_node(&node_name).await {
                        Ok(()) => {
                            info!(
                                node = node_name,
                                "batched node update: updated and restarted"
                            );
                            NodeUpdateResult::Updated
                        }
                        Err(err) => {
                            warn!(node = node_name, error = %err, "batched node update: restart failed");
                            NodeUpdateResult::RestartFailed(err.to_string())
                        }
                    },
                    Err(err) => {
                        warn!(node = node_name, error = %err, "batched node update: update failed");
                        NodeUpdateResult::UpdateFailed(err.to_string())
                    }
                };

                NodeUpdateOutcome {
                    node: node_name,
                    health,
                    result,
                }
            }
        }))
        .await;

        let batch_failed = batch_outcomes
            .iter()
            .any(|o| o.result != NodeUpdateResult::Updated);
        outcomes.extend(batch_outcomes);

        if batch_failed {
            warn!(
                batch = batch_index,
                "batched node update: batch had failures; not starting further batches"
            );
            aborted_after_batch = Some(batch_index);
            break;
        }
    }

    Ok(BatchUpdateReport {
        outcomes,
        aborted_after_batch,
    })
}

/// Probe a node's `forgefleetd` TCP address to build a real health snapshot.
///
/// This is a lightweight reachability probe (not a full metrics pull): it
/// gives [`run_batched_update`] a real default health check that does not
/// depend on a metrics backend. Callers with access to real success/error
/// rate telemetry should pass their own closure to `run_batched_update`
/// instead of this probe.
pub async fn probe_forgefleetd_health(addr: &str, timeout: Duration) -> HealthSnapshot {
    let started = Instant::now();

    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            HealthSnapshot::new(1.0, 0.0, elapsed_ms, 1.0, 1)
        }
        _ => HealthSnapshot::new(0.0, 1.0, timeout.as_millis() as u64, 0.0, 1),
    }
}

/// Update a node's checkout via `git fetch` + `reset --hard`.
///
/// Runs the blocking git plumbing in [`crate::git_utils::git_fetch_and_reset_hard`]
/// on a blocking thread so it composes with the async closures expected by
/// [`run_batched_update`].
pub async fn update_node_checkout(repo_path: &Path, remote_ref: &str) -> Result<()> {
    let repo_path = repo_path.to_path_buf();
    let remote_ref = remote_ref.to_string();

    tokio::task::spawn_blocking(move || git_fetch_and_reset_hard(&repo_path, &remote_ref))
        .await
        .context("update task panicked")?
}

/// Restart `forgefleetd` on the local node.
///
/// Thin re-export of [`super::restart_forgefleetd_local`] so callers can wire
/// up a full local `update_node` + `restart_node` pair from this module alone.
pub async fn restart_node_local(_node: &str) -> Result<()> {
    restart_forgefleetd_local().await
}

#[cfg(test)]
mod tests;
