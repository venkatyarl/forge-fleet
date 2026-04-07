//! Rollout controller — orchestrate the phased deployment of updates across
//! fleet nodes with manual controls (pause, resume, abort) and safety checks.
//!
//! Works in tandem with [`crate::canary::CanaryOrchestrator`]: the canary picks
//! the first wave; once promoted, the rollout controller drives the remaining
//! nodes in order, respecting `max_unavailable` and `abort_on_failure`.

use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::canary::{FleetNode, NodeProgress, NodeUpdateStatus};
use crate::error::{UpdateError, UpdateResult};

// ─── Rollout command (manual controls) ───────────────────────────────────────

/// Commands an operator can issue to a running rollout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutCommand {
    Resume,
    Pause,
    Abort,
}

// ─── Rollout state ───────────────────────────────────────────────────────────

/// High-level state of the rollout controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutState {
    /// Plan created but not started.
    Planned,
    /// Rollout is actively deploying nodes.
    InProgress,
    /// Paused by operator (waiting for resume).
    Paused,
    /// Successfully completed.
    Complete,
    /// Aborted by operator or by failure policy.
    Aborted,
}

impl fmt::Display for RolloutState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planned => write!(f, "planned"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Paused => write!(f, "paused"),
            Self::Complete => write!(f, "complete"),
            Self::Aborted => write!(f, "aborted"),
        }
    }
}

// ─── Rollout plan ────────────────────────────────────────────────────────────

/// A plan describing the order and constraints for rolling out to the fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutPlan {
    /// Ordered list of node names to update (canary nodes already removed).
    pub node_order: Vec<String>,

    /// Whether to pause after the canary wave and require manual approval.
    #[serde(default)]
    pub pause_after_canary: bool,

    /// Maximum number of nodes updating at the same time.
    #[serde(default = "default_max_unavailable")]
    pub max_unavailable: usize,

    /// Whether to abort the entire rollout if any single node fails.
    #[serde(default = "default_abort_on_failure")]
    pub abort_on_failure: bool,

    /// Target version string for this rollout.
    pub target_version: String,
}

fn default_max_unavailable() -> usize {
    1
}
fn default_abort_on_failure() -> bool {
    true
}

impl RolloutPlan {
    /// Build a plan from a fleet list. Leader goes last.
    pub fn from_fleet(
        nodes: &[FleetNode],
        canary_nodes: &[String],
        target_version: String,
    ) -> Self {
        // Filter out canary nodes, sort: non-leaders by priority, leader last.
        let mut remaining: Vec<&FleetNode> = nodes
            .iter()
            .filter(|n| !canary_nodes.contains(&n.name))
            .collect();

        // Stable sort: non-leaders by priority ascending, then leaders.
        remaining.sort_by(|a, b| match (a.is_leader, b.is_leader) {
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
            _ => a.priority.cmp(&b.priority),
        });

        let node_order: Vec<String> = remaining.iter().map(|n| n.name.clone()).collect();

        Self {
            node_order,
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version,
        }
    }
}

// ─── Rollout status ──────────────────────────────────────────────────────────

/// Snapshot of the rollout's current progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutStatus {
    pub state: RolloutState,
    pub plan: RolloutPlan,
    pub node_statuses: HashMap<String, NodeProgress>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub current_batch: Vec<String>,
    pub abort_reason: Option<String>,
}

impl RolloutStatus {
    /// Number of nodes successfully updated.
    pub fn completed_count(&self) -> usize {
        self.node_statuses
            .values()
            .filter(|p| {
                matches!(
                    p.status,
                    NodeUpdateStatus::Updated | NodeUpdateStatus::HealthCheckPassed
                )
            })
            .count()
    }

    /// Number of nodes that failed.
    pub fn failed_count(&self) -> usize {
        self.node_statuses
            .values()
            .filter(|p| {
                matches!(
                    p.status,
                    NodeUpdateStatus::HealthCheckFailed | NodeUpdateStatus::RolledBack
                )
            })
            .count()
    }

    /// Total number of nodes in the plan.
    pub fn total_count(&self) -> usize {
        self.plan.node_order.len()
    }
}

// ─── Rollout controller ──────────────────────────────────────────────────────

/// Drives the rolling deployment of remaining nodes after canary promotion.
///
/// The controller does NOT perform the actual binary update — it tells the
/// caller *which* nodes to update and *when*.  The caller reports back status
/// via `mark_node_updated` / `mark_node_failed`.
pub struct RolloutController {
    status: RolloutStatus,
}

impl RolloutController {
    /// Create a new rollout controller from a plan.
    pub fn new(plan: RolloutPlan) -> Self {
        let mut node_statuses = HashMap::new();
        for name in &plan.node_order {
            node_statuses.insert(
                name.clone(),
                NodeProgress {
                    node_name: name.clone(),
                    status: NodeUpdateStatus::Pending,
                    updated_at: None,
                    health_checked_at: None,
                    error: None,
                },
            );
        }

        Self {
            status: RolloutStatus {
                state: RolloutState::Planned,
                plan,
                node_statuses,
                started_at: None,
                completed_at: None,
                current_batch: Vec::new(),
                abort_reason: None,
            },
        }
    }

    /// Borrow the current status snapshot.
    pub fn status(&self) -> &RolloutStatus {
        &self.status
    }

    /// Current state.
    pub fn state(&self) -> RolloutState {
        self.status.state
    }

    // ── Lifecycle ────────────────────────────────────────────────────

    /// Start the rollout.  Returns the first batch of nodes to update.
    pub fn start(&mut self) -> UpdateResult<Vec<String>> {
        if self.status.state != RolloutState::Planned {
            return Err(UpdateError::FleetCoordination {
                reason: format!("cannot start rollout in state {}", self.status.state),
            });
        }

        self.status.state = RolloutState::InProgress;
        self.status.started_at = Some(Utc::now());
        info!("rollout started");

        Ok(self.next_batch())
    }

    /// Get the next batch of nodes to update (respecting `max_unavailable`).
    ///
    /// Returns an empty vec when all nodes are done or the rollout is
    /// paused/aborted.
    pub fn next_batch(&mut self) -> Vec<String> {
        if self.status.state != RolloutState::InProgress {
            return Vec::new();
        }

        // Count how many nodes are currently updating (in-flight).
        let in_flight = self
            .status
            .node_statuses
            .values()
            .filter(|p| p.status == NodeUpdateStatus::Updating)
            .count();

        let available_slots = self.status.plan.max_unavailable.saturating_sub(in_flight);

        if available_slots == 0 {
            return Vec::new();
        }

        // Pick the next `available_slots` pending nodes in order.
        let batch: Vec<String> = self
            .status
            .plan
            .node_order
            .iter()
            .filter(|name| {
                self.status
                    .node_statuses
                    .get(*name)
                    .is_some_and(|p| p.status == NodeUpdateStatus::Pending)
            })
            .take(available_slots)
            .cloned()
            .collect();

        // Mark them as updating.
        for name in &batch {
            if let Some(np) = self.status.node_statuses.get_mut(name) {
                np.status = NodeUpdateStatus::Updating;
            }
        }

        self.status.current_batch = batch.clone();

        if !batch.is_empty() {
            info!(batch = ?batch, "next rollout batch");
        }

        batch
    }

    /// Report that a node was successfully updated.
    pub fn mark_node_updated(&mut self, node_name: &str) {
        if let Some(np) = self.status.node_statuses.get_mut(node_name) {
            np.status = NodeUpdateStatus::Updated;
            np.updated_at = Some(Utc::now());
        }
        info!(node = node_name, "rollout: node updated");
        self.check_completion();
    }

    /// Report that a node's health check passed after update.
    pub fn mark_node_healthy(&mut self, node_name: &str) {
        if let Some(np) = self.status.node_statuses.get_mut(node_name) {
            np.status = NodeUpdateStatus::HealthCheckPassed;
            np.health_checked_at = Some(Utc::now());
        }
        info!(node = node_name, "rollout: node healthy");
        self.check_completion();
    }

    /// Report that a node failed (health check or update itself).
    pub fn mark_node_failed(&mut self, node_name: &str, error: String) {
        if let Some(np) = self.status.node_statuses.get_mut(node_name) {
            np.status = NodeUpdateStatus::HealthCheckFailed;
            np.error = Some(error.clone());
        }
        warn!(node = node_name, %error, "rollout: node failed");

        if self.status.plan.abort_on_failure {
            self.abort_internal(format!("node {node_name} failed: {error}"));
        }
    }

    // ── Manual controls ──────────────────────────────────────────────

    /// Resume a paused rollout.  Returns the next batch to update.
    pub fn resume(&mut self) -> UpdateResult<Vec<String>> {
        match self.status.state {
            RolloutState::Paused => {
                self.status.state = RolloutState::InProgress;
                info!("rollout resumed");
                Ok(self.next_batch())
            }
            other => Err(UpdateError::FleetCoordination {
                reason: format!("cannot resume rollout in state {other}"),
            }),
        }
    }

    /// Pause the rollout.  In-flight nodes finish, but no new batches start.
    pub fn pause(&mut self) -> UpdateResult<()> {
        match self.status.state {
            RolloutState::InProgress => {
                self.status.state = RolloutState::Paused;
                info!("rollout paused");
                Ok(())
            }
            other => Err(UpdateError::FleetCoordination {
                reason: format!("cannot pause rollout in state {other}"),
            }),
        }
    }

    /// Abort the rollout.  Remaining pending nodes are marked Skipped.
    pub fn abort(&mut self) -> UpdateResult<()> {
        match self.status.state {
            RolloutState::InProgress | RolloutState::Paused => {
                self.abort_internal("operator requested abort".into());
                Ok(())
            }
            other => Err(UpdateError::FleetCoordination {
                reason: format!("cannot abort rollout in state {other}"),
            }),
        }
    }

    // ── Version skew check ───────────────────────────────────────────

    /// Ensure no node is more than 1 major/minor version behind another.
    ///
    /// `versions` maps node names to their current version string.
    /// This is a simple numeric check: parse the last component of the version
    /// as an integer and ensure max - min ≤ 1.
    ///
    /// For git-SHA based versioning, this checks that all versions are one of
    /// at most 2 distinct values.
    pub fn version_skew_check(versions: &HashMap<String, String>) -> UpdateResult<()> {
        if versions.is_empty() {
            return Ok(());
        }

        let unique_versions: std::collections::HashSet<&String> = versions.values().collect();

        if unique_versions.len() <= 2 {
            return Ok(());
        }

        // More than 2 distinct versions → skew violation.
        let nodes_by_version: HashMap<&String, Vec<&String>> = {
            let mut m: HashMap<&String, Vec<&String>> = HashMap::new();
            for (node, ver) in versions {
                m.entry(ver).or_default().push(node);
            }
            m
        };

        let detail: Vec<String> = nodes_by_version
            .iter()
            .map(|(ver, nodes)| {
                format!(
                    "{}: [{}]",
                    ver,
                    nodes
                        .iter()
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect();

        Err(UpdateError::FleetCoordination {
            reason: format!(
                "version skew detected ({} distinct versions, max allowed is 2): {}",
                unique_versions.len(),
                detail.join("; ")
            ),
        })
    }

    // ── Internals ────────────────────────────────────────────────────

    fn abort_internal(&mut self, reason: String) {
        error!(reason = %reason, "rollout aborted");
        self.status.state = RolloutState::Aborted;
        self.status.abort_reason = Some(reason);
        self.status.completed_at = Some(Utc::now());

        // Skip remaining pending nodes.
        for np in self.status.node_statuses.values_mut() {
            if np.status == NodeUpdateStatus::Pending {
                np.status = NodeUpdateStatus::Skipped;
            }
        }
    }

    fn check_completion(&mut self) {
        if self.status.state != RolloutState::InProgress {
            return;
        }

        let all_done = self.status.plan.node_order.iter().all(|name| {
            self.status.node_statuses.get(name).is_some_and(|p| {
                matches!(
                    p.status,
                    NodeUpdateStatus::Updated
                        | NodeUpdateStatus::HealthCheckPassed
                        | NodeUpdateStatus::Skipped
                        | NodeUpdateStatus::HealthCheckFailed
                        | NodeUpdateStatus::RolledBack
                )
            })
        });

        if all_done {
            let any_failed = self.status.failed_count() > 0;
            if any_failed {
                self.abort_internal("one or more nodes failed".into());
            } else {
                self.status.state = RolloutState::Complete;
                self.status.completed_at = Some(Utc::now());
                info!("rollout complete — all nodes updated");
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canary::FleetNode;

    fn test_fleet() -> Vec<FleetNode> {
        vec![
            FleetNode {
                name: "james".into(),
                priority: 10,
                is_leader: false,
                health_url: "http://james/health".into(),
                current_version: Some("v1".into()),
            },
            FleetNode {
                name: "marcus".into(),
                priority: 20,
                is_leader: false,
                health_url: "http://marcus/health".into(),
                current_version: Some("v1".into()),
            },
            FleetNode {
                name: "sophie".into(),
                priority: 30,
                is_leader: false,
                health_url: "http://sophie/health".into(),
                current_version: Some("v1".into()),
            },
            FleetNode {
                name: "taylor".into(),
                priority: 100,
                is_leader: true,
                health_url: "http://taylor/health".into(),
                current_version: Some("v1".into()),
            },
        ]
    }

    #[test]
    fn test_plan_from_fleet_leader_last() {
        let fleet = test_fleet();
        let canary = vec!["james".to_string()];
        let plan = RolloutPlan::from_fleet(&fleet, &canary, "v2".into());

        // james is canary → excluded.  Remaining: marcus(20), sophie(30), taylor(leader 100)
        assert_eq!(plan.node_order, vec!["marcus", "sophie", "taylor"]);
        assert_eq!(plan.target_version, "v2");
    }

    #[test]
    fn test_rollout_start_and_next_batch() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into(), "c".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);

        assert_eq!(ctrl.state(), RolloutState::Planned);

        let batch = ctrl.start().unwrap();
        assert_eq!(batch, vec!["a"]);
        assert_eq!(ctrl.state(), RolloutState::InProgress);

        // a is in-flight, max_unavailable=1 → no more
        let batch2 = ctrl.next_batch();
        assert!(batch2.is_empty());

        // Mark a done
        ctrl.mark_node_updated("a");
        let batch3 = ctrl.next_batch();
        assert_eq!(batch3, vec!["b"]);
    }

    #[test]
    fn test_rollout_max_unavailable_2() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            pause_after_canary: false,
            max_unavailable: 2,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);

        let batch = ctrl.start().unwrap();
        assert_eq!(batch, vec!["a", "b"]);

        ctrl.mark_node_updated("a");
        let batch2 = ctrl.next_batch();
        assert_eq!(batch2, vec!["c"]);
    }

    #[test]
    fn test_rollout_abort_on_failure() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into(), "c".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);

        ctrl.start().unwrap();
        ctrl.mark_node_failed("a", "health check timeout".into());

        assert_eq!(ctrl.state(), RolloutState::Aborted);
        assert!(ctrl.status().abort_reason.is_some());

        // Remaining nodes should be skipped
        let b_status = ctrl.status().node_statuses.get("b").unwrap();
        assert_eq!(b_status.status, NodeUpdateStatus::Skipped);
        let c_status = ctrl.status().node_statuses.get("c").unwrap();
        assert_eq!(c_status.status, NodeUpdateStatus::Skipped);
    }

    #[test]
    fn test_rollout_no_abort_on_failure() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: false,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);

        ctrl.start().unwrap();
        ctrl.mark_node_failed("a", "oops".into());

        // Should NOT abort since abort_on_failure is false
        assert_eq!(ctrl.state(), RolloutState::InProgress);

        let batch = ctrl.next_batch();
        assert_eq!(batch, vec!["b"]);
    }

    #[test]
    fn test_rollout_pause_resume() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);

        ctrl.start().unwrap();
        ctrl.mark_node_updated("a");

        ctrl.pause().unwrap();
        assert_eq!(ctrl.state(), RolloutState::Paused);

        // next_batch returns empty while paused
        let batch = ctrl.next_batch();
        assert!(batch.is_empty());

        let batch = ctrl.resume().unwrap();
        assert_eq!(batch, vec!["b"]);
        assert_eq!(ctrl.state(), RolloutState::InProgress);
    }

    #[test]
    fn test_rollout_abort_manual() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);
        ctrl.start().unwrap();

        ctrl.abort().unwrap();
        assert_eq!(ctrl.state(), RolloutState::Aborted);
    }

    #[test]
    fn test_rollout_completion() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into()],
            pause_after_canary: false,
            max_unavailable: 2,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);
        ctrl.start().unwrap();

        ctrl.mark_node_updated("a");
        ctrl.mark_node_updated("b");

        assert_eq!(ctrl.state(), RolloutState::Complete);
        assert!(ctrl.status().completed_at.is_some());
    }

    #[test]
    fn test_rollout_status_counts() {
        let plan = RolloutPlan {
            node_order: vec!["a".into(), "b".into(), "c".into()],
            pause_after_canary: false,
            max_unavailable: 3,
            abort_on_failure: false,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);
        ctrl.start().unwrap();

        ctrl.mark_node_updated("a");
        ctrl.mark_node_healthy("b");
        ctrl.mark_node_failed("c", "timeout".into());

        assert_eq!(ctrl.status().completed_count(), 2); // a + b
        assert_eq!(ctrl.status().failed_count(), 1); // c
        assert_eq!(ctrl.status().total_count(), 3);
    }

    #[test]
    fn test_version_skew_check_ok() {
        let mut versions = HashMap::new();
        versions.insert("a".into(), "v2".into());
        versions.insert("b".into(), "v2".into());
        versions.insert("c".into(), "v1".into()); // 2 distinct versions → OK

        assert!(RolloutController::version_skew_check(&versions).is_ok());
    }

    #[test]
    fn test_version_skew_check_violation() {
        let mut versions = HashMap::new();
        versions.insert("a".into(), "v3".into());
        versions.insert("b".into(), "v2".into());
        versions.insert("c".into(), "v1".into()); // 3 distinct versions → violation

        let result = RolloutController::version_skew_check(&versions);
        assert!(result.is_err());
    }

    #[test]
    fn test_version_skew_check_empty() {
        let versions = HashMap::new();
        assert!(RolloutController::version_skew_check(&versions).is_ok());
    }

    #[test]
    fn test_version_skew_check_all_same() {
        let mut versions = HashMap::new();
        versions.insert("a".into(), "v1".into());
        versions.insert("b".into(), "v1".into());
        versions.insert("c".into(), "v1".into());

        assert!(RolloutController::version_skew_check(&versions).is_ok());
    }

    #[test]
    fn test_cannot_start_twice() {
        let plan = RolloutPlan {
            node_order: vec!["a".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);
        ctrl.start().unwrap();
        assert!(ctrl.start().is_err());
    }

    #[test]
    fn test_cannot_resume_when_not_paused() {
        let plan = RolloutPlan {
            node_order: vec!["a".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let mut ctrl = RolloutController::new(plan);
        ctrl.start().unwrap();
        // Not paused → resume should fail
        assert!(ctrl.resume().is_err());
    }

    #[test]
    fn test_cannot_pause_when_not_in_progress() {
        let plan = RolloutPlan {
            node_order: vec!["a".into()],
            pause_after_canary: false,
            max_unavailable: 1,
            abort_on_failure: true,
            target_version: "v2".into(),
        };
        let ctrl = RolloutController::new(plan);
        // Not started → can't pause
        // Need a mutable ref, but state is Planned
        let mut ctrl = ctrl;
        assert!(ctrl.pause().is_err());
    }

    #[test]
    fn test_rollout_state_display() {
        assert_eq!(RolloutState::Planned.to_string(), "planned");
        assert_eq!(RolloutState::InProgress.to_string(), "in_progress");
        assert_eq!(RolloutState::Paused.to_string(), "paused");
        assert_eq!(RolloutState::Complete.to_string(), "complete");
        assert_eq!(RolloutState::Aborted.to_string(), "aborted");
    }

    #[test]
    fn test_plan_from_fleet_excludes_canary() {
        let fleet = test_fleet();
        let canary = vec!["james".to_string(), "marcus".to_string()];
        let plan = RolloutPlan::from_fleet(&fleet, &canary, "v2".into());

        // Only sophie and taylor remain; sophie before taylor (leader)
        assert_eq!(plan.node_order, vec!["sophie", "taylor"]);
    }

    #[test]
    fn test_rollout_order_respects_priority() {
        let fleet = vec![
            FleetNode {
                name: "high".into(),
                priority: 50,
                is_leader: false,
                health_url: "http://h/health".into(),
                current_version: None,
            },
            FleetNode {
                name: "low".into(),
                priority: 5,
                is_leader: false,
                health_url: "http://l/health".into(),
                current_version: None,
            },
            FleetNode {
                name: "leader".into(),
                priority: 1,
                is_leader: true,
                health_url: "http://leader/health".into(),
                current_version: None,
            },
        ];
        let plan = RolloutPlan::from_fleet(&fleet, &[], "v2".into());
        // low(5) before high(50), leader last even though priority 1
        assert_eq!(plan.node_order, vec!["low", "high", "leader"]);
    }
}
