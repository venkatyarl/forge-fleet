//! Canary deployment — update a small subset of nodes first, verify health,
//! then proceed with the full rollout (or rollback on failure).
//!
//! The canary strategy adds a "bake time" between updating the first node(s) and
//! rolling out to the rest of the fleet, giving operators confidence that a new
//! version is safe before committing.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::error::{UpdateError, UpdateResult};

// ─── Canary stage ────────────────────────────────────────────────────────────

/// Progression of a canary deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryStage {
    /// Canary has not been started yet.
    NotStarted,
    /// Canary node(s) are being updated.
    CanaryDeploying,
    /// Canary node(s) updated — bake-time monitoring in progress.
    CanaryMonitoring,
    /// Canary passed all health gates — ready to roll out.
    CanaryPassed,
    /// Remaining nodes are being updated (rolling).
    RollingOut,
    /// All nodes updated successfully.
    Complete,
    /// Canary failed — rolled back.
    RolledBack,
}

impl fmt::Display for CanaryStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted => write!(f, "not_started"),
            Self::CanaryDeploying => write!(f, "canary_deploying"),
            Self::CanaryMonitoring => write!(f, "canary_monitoring"),
            Self::CanaryPassed => write!(f, "canary_passed"),
            Self::RollingOut => write!(f, "rolling_out"),
            Self::Complete => write!(f, "complete"),
            Self::RolledBack => write!(f, "rolled_back"),
        }
    }
}

// ─── Node info ───────────────────────────────────────────────────────────────

/// Minimal information about a fleet node needed for canary selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetNode {
    /// Node name (e.g. "james", "marcus").
    pub name: String,

    /// Priority — lower numbers update first.  Leader should have the highest.
    pub priority: u32,

    /// Whether this node is the current leader.
    pub is_leader: bool,

    /// HTTP health endpoint (e.g. `http://192.168.5.101:51800/health`).
    pub health_url: String,

    /// Current running version string (git SHA or semver).
    pub current_version: Option<String>,
}

// ─── Per-node status ─────────────────────────────────────────────────────────

/// Status of an individual node during the canary/rollout process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeUpdateStatus {
    Pending,
    Updating,
    Updated,
    HealthCheckPassed,
    HealthCheckFailed,
    RolledBack,
    Skipped,
}

impl fmt::Display for NodeUpdateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Updating => write!(f, "updating"),
            Self::Updated => write!(f, "updated"),
            Self::HealthCheckPassed => write!(f, "health_check_passed"),
            Self::HealthCheckFailed => write!(f, "health_check_failed"),
            Self::RolledBack => write!(f, "rolled_back"),
            Self::Skipped => write!(f, "skipped"),
        }
    }
}

/// Tracks the state of a single node during rollout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeProgress {
    pub node_name: String,
    pub status: NodeUpdateStatus,
    pub updated_at: Option<DateTime<Utc>>,
    pub health_checked_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

// ─── Health gate ─────────────────────────────────────────────────────────────

/// A configurable health check that must pass before proceeding past the canary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthGate {
    /// Human-readable name (e.g. "http_health", "version_match").
    pub name: String,

    /// The kind of check to perform.
    pub kind: HealthGateKind,
}

/// What kind of health gate check to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthGateKind {
    /// HTTP GET must return 200.
    HttpStatus { url: String },

    /// HTTP GET to a version endpoint must contain the expected version string.
    VersionMatch {
        url: String,
        expected_version: String,
    },

    /// Custom shell command must exit 0.
    Command { command: String },
}

/// Result of evaluating one health gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthGateResult {
    pub gate_name: String,
    pub passed: bool,
    pub detail: String,
    pub checked_at: DateTime<Utc>,
}

// ─── Canary policy ───────────────────────────────────────────────────────────

/// Governs how many nodes participate in the canary wave and how long to bake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryPolicy {
    /// Number of nodes to include in the canary wave.
    /// When `canary_count` is 0, we use `canary_percentage` instead.
    #[serde(default = "default_canary_count")]
    pub canary_count: usize,

    /// Percentage of fleet to include if `canary_count` is 0 (0.0–1.0).
    #[serde(default)]
    pub canary_percentage: f64,

    /// How long (seconds) to monitor canary nodes before proceeding.
    #[serde(default = "default_bake_time")]
    pub bake_time_secs: u64,

    /// Interval (seconds) between health checks during the bake window.
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,

    /// Health gates that must ALL pass for the canary to be considered healthy.
    #[serde(default)]
    pub health_gates: Vec<HealthGate>,

    /// HTTP request timeout for health checks (seconds).
    #[serde(default = "default_health_timeout")]
    pub health_timeout_secs: u64,

    /// Number of consecutive health-check failures before declaring canary failed.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
}

fn default_canary_count() -> usize {
    1
}
fn default_bake_time() -> u64 {
    300 // 5 minutes
}
fn default_check_interval() -> u64 {
    15
}
fn default_health_timeout() -> u64 {
    10
}
fn default_failure_threshold() -> u32 {
    3
}

impl Default for CanaryPolicy {
    fn default() -> Self {
        Self {
            canary_count: 1,
            canary_percentage: 0.0,
            bake_time_secs: default_bake_time(),
            check_interval_secs: default_check_interval(),
            health_gates: Vec::new(),
            health_timeout_secs: default_health_timeout(),
            failure_threshold: default_failure_threshold(),
        }
    }
}

// ─── Rollout progress ────────────────────────────────────────────────────────

/// Aggregate rollout progress across all nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutProgress {
    pub stage: CanaryStage,
    pub canary_nodes: Vec<String>,
    pub remaining_nodes: Vec<String>,
    pub node_statuses: HashMap<String, NodeProgress>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub target_version: String,
    pub gate_results: Vec<HealthGateResult>,
}

impl RolloutProgress {
    /// How many nodes have been successfully updated.
    pub fn updated_count(&self) -> usize {
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

    /// How many nodes are still pending.
    pub fn pending_count(&self) -> usize {
        self.node_statuses
            .values()
            .filter(|p| p.status == NodeUpdateStatus::Pending)
            .count()
    }

    /// How many nodes failed.
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
}

// ─── Canary orchestrator ─────────────────────────────────────────────────────

/// Drives the canary deployment lifecycle.
///
/// The caller is responsible for actually performing the binary update on each
/// node (via SSH, fleet RPC, etc.).  `CanaryOrchestrator` decides *which* nodes
/// to update and *when*, and runs health gates to decide whether to proceed or
/// rollback.
pub struct CanaryOrchestrator {
    policy: CanaryPolicy,
    progress: RolloutProgress,
}

impl CanaryOrchestrator {
    /// Create a new canary orchestrator for a fleet update to `target_version`.
    pub fn new(policy: CanaryPolicy, target_version: String) -> Self {
        Self {
            policy,
            progress: RolloutProgress {
                stage: CanaryStage::NotStarted,
                canary_nodes: Vec::new(),
                remaining_nodes: Vec::new(),
                node_statuses: HashMap::new(),
                started_at: None,
                completed_at: None,
                target_version,
                gate_results: Vec::new(),
            },
        }
    }

    /// Current canary stage.
    pub fn stage(&self) -> CanaryStage {
        self.progress.stage
    }

    /// Borrow the full rollout progress for reporting.
    pub fn progress(&self) -> &RolloutProgress {
        &self.progress
    }

    /// The policy driving this canary deployment.
    pub fn policy(&self) -> &CanaryPolicy {
        &self.policy
    }

    // ── Step 1: Select canary nodes ──────────────────────────────────

    /// Pick which nodes will be canaries.
    ///
    /// Rules:
    /// - Lowest-priority nodes first (NOT the leader).
    /// - Respects `canary_count` (or `canary_percentage` if count is 0).
    /// - Returns the ordered list of canary node names.
    pub fn select_canary_nodes(&mut self, fleet: &[FleetNode]) -> UpdateResult<Vec<String>> {
        if fleet.is_empty() {
            return Err(UpdateError::FleetCoordination {
                reason: "no fleet nodes provided".into(),
            });
        }

        // Sort non-leader nodes by priority (ascending = lowest first).
        let mut candidates: Vec<&FleetNode> = fleet.iter().filter(|n| !n.is_leader).collect();
        candidates.sort_by_key(|n| n.priority);

        if candidates.is_empty() {
            return Err(UpdateError::FleetCoordination {
                reason: "no non-leader nodes available for canary".into(),
            });
        }

        let count = if self.policy.canary_count > 0 {
            self.policy.canary_count
        } else {
            let pct = self.policy.canary_percentage.clamp(0.0, 1.0);
            (fleet.len() as f64 * pct).ceil().max(1.0) as usize
        };

        let count = count.min(candidates.len());

        let canary_names: Vec<String> =
            candidates[..count].iter().map(|n| n.name.clone()).collect();

        // Remaining = everyone not in canary set (including leader, which goes last).
        let remaining_names: Vec<String> = fleet
            .iter()
            .filter(|n| !canary_names.contains(&n.name))
            .map(|n| n.name.clone())
            .collect();

        // Initialise per-node progress.
        for node in fleet {
            self.progress.node_statuses.insert(
                node.name.clone(),
                NodeProgress {
                    node_name: node.name.clone(),
                    status: NodeUpdateStatus::Pending,
                    updated_at: None,
                    health_checked_at: None,
                    error: None,
                },
            );
        }

        self.progress.canary_nodes = canary_names.clone();
        self.progress.remaining_nodes = remaining_names;
        self.progress.started_at = Some(Utc::now());
        self.progress.stage = CanaryStage::NotStarted;

        info!(
            canary = ?canary_names,
            remaining = ?self.progress.remaining_nodes,
            "canary nodes selected"
        );

        Ok(canary_names)
    }

    // ── Step 2: Mark canary deploy started ───────────────────────────

    /// Signal that canary node updates have been kicked off.
    pub fn begin_canary_deploy(&mut self) {
        self.progress.stage = CanaryStage::CanaryDeploying;
        for name in &self.progress.canary_nodes {
            if let Some(np) = self.progress.node_statuses.get_mut(name) {
                np.status = NodeUpdateStatus::Updating;
            }
        }
        info!(stage = %self.progress.stage, "canary deploy started");
    }

    /// Mark a single canary node as having been updated (binary swapped).
    pub fn mark_node_updated(&mut self, node_name: &str) {
        if let Some(np) = self.progress.node_statuses.get_mut(node_name) {
            np.status = NodeUpdateStatus::Updated;
            np.updated_at = Some(Utc::now());
        }
        info!(node = node_name, "node marked as updated");

        // If all canary nodes are updated → transition to monitoring.
        let all_canary_updated = self.progress.canary_nodes.iter().all(|n| {
            self.progress.node_statuses.get(n).is_some_and(|p| {
                matches!(
                    p.status,
                    NodeUpdateStatus::Updated | NodeUpdateStatus::HealthCheckPassed
                )
            })
        });

        if all_canary_updated && self.progress.stage == CanaryStage::CanaryDeploying {
            self.progress.stage = CanaryStage::CanaryMonitoring;
            info!("all canary nodes updated — entering monitoring/bake phase");
        }
    }

    // ── Step 3: Health gate evaluation ───────────────────────────────

    /// Evaluate all configured health gates for a single node.
    ///
    /// This is an async operation because it performs HTTP requests.
    pub async fn evaluate_health_gates(
        &self,
        node: &FleetNode,
        expected_version: &str,
    ) -> Vec<HealthGateResult> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.policy.health_timeout_secs))
            .build()
            .unwrap_or_default();

        let mut results = Vec::new();

        // If no explicit gates configured, use the default HTTP health check.
        if self.policy.health_gates.is_empty() {
            let res = Self::check_http_health(&client, &node.health_url).await;
            results.push(res);

            // Also do a version match if we have a version endpoint.
            // Convention: health_url + "/version" or health_url itself returns version
            // We'll use the health URL and check the body for the version string.
            let ver_res =
                Self::check_version_match(&client, &node.health_url, expected_version).await;
            results.push(ver_res);

            return results;
        }

        for gate in &self.policy.health_gates {
            let res = match &gate.kind {
                HealthGateKind::HttpStatus { url } => Self::check_http_health(&client, url).await,
                HealthGateKind::VersionMatch {
                    url,
                    expected_version: ver,
                } => Self::check_version_match(&client, url, ver).await,
                HealthGateKind::Command { command } => Self::check_command(command),
            };
            let res = HealthGateResult {
                gate_name: gate.name.clone(),
                ..res
            };
            results.push(res);
        }

        results
    }

    /// Check whether ALL health gates pass for all canary nodes.
    pub async fn check_canary_health(&mut self, fleet: &[FleetNode]) -> bool {
        let canary_names = self.progress.canary_nodes.clone();
        let version = self.progress.target_version.clone();
        let mut all_healthy = true;

        for name in &canary_names {
            let Some(node) = fleet.iter().find(|n| &n.name == name) else {
                warn!(node = name, "canary node not found in fleet list");
                all_healthy = false;
                continue;
            };

            let gate_results = self.evaluate_health_gates(node, &version).await;
            let node_healthy = gate_results.iter().all(|r| r.passed);

            if let Some(np) = self.progress.node_statuses.get_mut(name) {
                np.health_checked_at = Some(Utc::now());
                if node_healthy {
                    np.status = NodeUpdateStatus::HealthCheckPassed;
                } else {
                    np.status = NodeUpdateStatus::HealthCheckFailed;
                    np.error = Some(
                        gate_results
                            .iter()
                            .filter(|r| !r.passed)
                            .map(|r| format!("{}: {}", r.gate_name, r.detail))
                            .collect::<Vec<_>>()
                            .join("; "),
                    );
                    all_healthy = false;
                }
            }

            self.progress.gate_results.extend(gate_results);
        }

        all_healthy
    }

    // ── Step 4: Stage transitions ────────────────────────────────────

    /// Call after bake time + health checks pass to advance to rolling out.
    pub fn promote_canary(&mut self) {
        if self.progress.stage == CanaryStage::CanaryMonitoring {
            self.progress.stage = CanaryStage::CanaryPassed;
            info!("canary promoted — health gates passed, ready for full rollout");
        }
    }

    /// Transition from CanaryPassed → RollingOut.
    pub fn begin_rollout(&mut self) {
        if self.progress.stage == CanaryStage::CanaryPassed {
            self.progress.stage = CanaryStage::RollingOut;
            // Remaining nodes stay Pending — the RolloutController will drive them.
            info!(
                remaining = self.progress.remaining_nodes.len(),
                "rolling out to remaining nodes"
            );
        }
    }

    /// Mark the entire deployment as complete.
    pub fn mark_complete(&mut self) {
        self.progress.stage = CanaryStage::Complete;
        self.progress.completed_at = Some(Utc::now());
        info!("canary rollout complete");
    }

    /// Mark canary as rolled back (called after rollback is performed).
    pub fn mark_rolled_back(&mut self, failed_nodes: &[String]) {
        self.progress.stage = CanaryStage::RolledBack;
        self.progress.completed_at = Some(Utc::now());
        for name in failed_nodes {
            if let Some(np) = self.progress.node_statuses.get_mut(name) {
                np.status = NodeUpdateStatus::RolledBack;
            }
        }
        warn!(
            nodes = ?failed_nodes,
            "canary rolled back"
        );
    }

    // ── Health check helpers ─────────────────────────────────────────

    async fn check_http_health(client: &reqwest::Client, url: &str) -> HealthGateResult {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => HealthGateResult {
                gate_name: "http_health".into(),
                passed: true,
                detail: format!("HTTP {} from {url}", resp.status()),
                checked_at: Utc::now(),
            },
            Ok(resp) => HealthGateResult {
                gate_name: "http_health".into(),
                passed: false,
                detail: format!("HTTP {} from {url}", resp.status()),
                checked_at: Utc::now(),
            },
            Err(e) => HealthGateResult {
                gate_name: "http_health".into(),
                passed: false,
                detail: format!("request failed: {e}"),
                checked_at: Utc::now(),
            },
        }
    }

    async fn check_version_match(
        client: &reqwest::Client,
        url: &str,
        expected: &str,
    ) -> HealthGateResult {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                let matched = body.contains(expected);
                HealthGateResult {
                    gate_name: "version_match".into(),
                    passed: matched,
                    detail: if matched {
                        format!("version {expected} found in response")
                    } else {
                        format!(
                            "expected {expected}, got: {}",
                            body.chars().take(200).collect::<String>()
                        )
                    },
                    checked_at: Utc::now(),
                }
            }
            Ok(resp) => HealthGateResult {
                gate_name: "version_match".into(),
                passed: false,
                detail: format!("HTTP {} — cannot check version", resp.status()),
                checked_at: Utc::now(),
            },
            Err(e) => HealthGateResult {
                gate_name: "version_match".into(),
                passed: false,
                detail: format!("request failed: {e}"),
                checked_at: Utc::now(),
            },
        }
    }

    fn check_command(command: &str) -> HealthGateResult {
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
        {
            Ok(output) if output.status.success() => HealthGateResult {
                gate_name: "command".into(),
                passed: true,
                detail: format!("command succeeded: {command}"),
                checked_at: Utc::now(),
            },
            Ok(output) => HealthGateResult {
                gate_name: "command".into(),
                passed: false,
                detail: format!(
                    "exit code {}: {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr)
                ),
                checked_at: Utc::now(),
            },
            Err(e) => HealthGateResult {
                gate_name: "command".into(),
                passed: false,
                detail: format!("failed to run command: {e}"),
                checked_at: Utc::now(),
            },
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fleet() -> Vec<FleetNode> {
        vec![
            FleetNode {
                name: "james".into(),
                priority: 10,
                is_leader: false,
                health_url: "http://192.168.5.101:51800/health".into(),
                current_version: Some("abc123".into()),
            },
            FleetNode {
                name: "marcus".into(),
                priority: 20,
                is_leader: false,
                health_url: "http://192.168.5.102:51800/health".into(),
                current_version: Some("abc123".into()),
            },
            FleetNode {
                name: "sophie".into(),
                priority: 30,
                is_leader: false,
                health_url: "http://192.168.5.103:51800/health".into(),
                current_version: Some("abc123".into()),
            },
            FleetNode {
                name: "taylor".into(),
                priority: 100,
                is_leader: true,
                health_url: "http://192.168.5.100:51800/health".into(),
                current_version: Some("abc123".into()),
            },
        ]
    }

    #[test]
    fn test_select_canary_single_node() {
        let mut orch = CanaryOrchestrator::new(CanaryPolicy::default(), "def456".into());
        let fleet = test_fleet();

        let canaries = orch.select_canary_nodes(&fleet).unwrap();

        // Default count = 1, lowest priority non-leader = james (10)
        assert_eq!(canaries, vec!["james"]);
        assert_eq!(orch.progress().canary_nodes, vec!["james"]);
        // Remaining should be marcus, sophie, taylor
        assert_eq!(orch.progress().remaining_nodes.len(), 3);
        assert!(orch.progress().remaining_nodes.contains(&"taylor".into()));
        assert!(orch.progress().remaining_nodes.contains(&"marcus".into()));
        assert!(orch.progress().remaining_nodes.contains(&"sophie".into()));
    }

    #[test]
    fn test_select_canary_multiple_nodes() {
        let policy = CanaryPolicy {
            canary_count: 2,
            ..Default::default()
        };
        let mut orch = CanaryOrchestrator::new(policy, "def456".into());
        let fleet = test_fleet();

        let canaries = orch.select_canary_nodes(&fleet).unwrap();

        // Lowest 2 by priority: james (10), marcus (20)
        assert_eq!(canaries, vec!["james", "marcus"]);
        assert_eq!(orch.progress().remaining_nodes.len(), 2);
    }

    #[test]
    fn test_select_canary_by_percentage() {
        let policy = CanaryPolicy {
            canary_count: 0,
            canary_percentage: 0.5, // 50% of 4 nodes = 2
            ..Default::default()
        };
        let mut orch = CanaryOrchestrator::new(policy, "def456".into());
        let fleet = test_fleet();

        let canaries = orch.select_canary_nodes(&fleet).unwrap();

        // 50% of 4 = 2 nodes, but only 3 candidates (non-leader)
        assert_eq!(canaries.len(), 2);
        assert_eq!(canaries[0], "james");
        assert_eq!(canaries[1], "marcus");
    }

    #[test]
    fn test_select_canary_excludes_leader() {
        let policy = CanaryPolicy {
            canary_count: 10, // More than available non-leaders
            ..Default::default()
        };
        let mut orch = CanaryOrchestrator::new(policy, "def456".into());
        let fleet = test_fleet();

        let canaries = orch.select_canary_nodes(&fleet).unwrap();

        // Should cap at 3 (all non-leaders) — leader is excluded
        assert_eq!(canaries.len(), 3);
        assert!(!canaries.contains(&"taylor".into()));
    }

    #[test]
    fn test_select_canary_empty_fleet() {
        let mut orch = CanaryOrchestrator::new(CanaryPolicy::default(), "def456".into());
        let result = orch.select_canary_nodes(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_select_canary_all_leaders() {
        let fleet = vec![FleetNode {
            name: "taylor".into(),
            priority: 100,
            is_leader: true,
            health_url: "http://localhost/health".into(),
            current_version: None,
        }];
        let mut orch = CanaryOrchestrator::new(CanaryPolicy::default(), "def456".into());
        let result = orch.select_canary_nodes(&fleet);
        assert!(result.is_err());
    }

    #[test]
    fn test_stage_transitions() {
        let mut orch = CanaryOrchestrator::new(CanaryPolicy::default(), "def456".into());
        let fleet = test_fleet();

        assert_eq!(orch.stage(), CanaryStage::NotStarted);

        orch.select_canary_nodes(&fleet).unwrap();
        orch.begin_canary_deploy();
        assert_eq!(orch.stage(), CanaryStage::CanaryDeploying);

        // Mark canary node as updated → should auto-transition to monitoring
        orch.mark_node_updated("james");
        assert_eq!(orch.stage(), CanaryStage::CanaryMonitoring);

        orch.promote_canary();
        assert_eq!(orch.stage(), CanaryStage::CanaryPassed);

        orch.begin_rollout();
        assert_eq!(orch.stage(), CanaryStage::RollingOut);

        orch.mark_complete();
        assert_eq!(orch.stage(), CanaryStage::Complete);
    }

    #[test]
    fn test_rollback_transition() {
        let mut orch = CanaryOrchestrator::new(CanaryPolicy::default(), "def456".into());
        let fleet = test_fleet();

        orch.select_canary_nodes(&fleet).unwrap();
        orch.begin_canary_deploy();
        orch.mark_node_updated("james");
        assert_eq!(orch.stage(), CanaryStage::CanaryMonitoring);

        // Simulate rollback
        orch.mark_rolled_back(&["james".into()]);
        assert_eq!(orch.stage(), CanaryStage::RolledBack);

        let james_status = orch.progress().node_statuses.get("james").unwrap();
        assert_eq!(james_status.status, NodeUpdateStatus::RolledBack);
    }

    #[test]
    fn test_rollout_progress_counts() {
        let mut orch = CanaryOrchestrator::new(
            CanaryPolicy {
                canary_count: 2,
                ..Default::default()
            },
            "def456".into(),
        );
        let fleet = test_fleet();

        orch.select_canary_nodes(&fleet).unwrap();

        // All should be pending initially
        assert_eq!(orch.progress().pending_count(), 4);
        assert_eq!(orch.progress().updated_count(), 0);
        assert_eq!(orch.progress().failed_count(), 0);

        orch.begin_canary_deploy();
        orch.mark_node_updated("james");
        assert_eq!(orch.progress().updated_count(), 1);

        orch.mark_node_updated("marcus");
        assert_eq!(orch.progress().updated_count(), 2);
        assert_eq!(orch.progress().pending_count(), 2);
    }

    #[test]
    fn test_health_gate_result_creation() {
        let result = HealthGateResult {
            gate_name: "http_health".into(),
            passed: true,
            detail: "HTTP 200".into(),
            checked_at: Utc::now(),
        };
        assert!(result.passed);
        assert_eq!(result.gate_name, "http_health");
    }

    #[test]
    fn test_canary_stage_display() {
        assert_eq!(CanaryStage::NotStarted.to_string(), "not_started");
        assert_eq!(CanaryStage::CanaryDeploying.to_string(), "canary_deploying");
        assert_eq!(
            CanaryStage::CanaryMonitoring.to_string(),
            "canary_monitoring"
        );
        assert_eq!(CanaryStage::CanaryPassed.to_string(), "canary_passed");
        assert_eq!(CanaryStage::RollingOut.to_string(), "rolling_out");
        assert_eq!(CanaryStage::Complete.to_string(), "complete");
        assert_eq!(CanaryStage::RolledBack.to_string(), "rolled_back");
    }

    #[test]
    fn test_node_update_status_display() {
        assert_eq!(NodeUpdateStatus::Pending.to_string(), "pending");
        assert_eq!(NodeUpdateStatus::Updating.to_string(), "updating");
        assert_eq!(NodeUpdateStatus::Updated.to_string(), "updated");
        assert_eq!(
            NodeUpdateStatus::HealthCheckPassed.to_string(),
            "health_check_passed"
        );
        assert_eq!(
            NodeUpdateStatus::HealthCheckFailed.to_string(),
            "health_check_failed"
        );
        assert_eq!(NodeUpdateStatus::RolledBack.to_string(), "rolled_back");
        assert_eq!(NodeUpdateStatus::Skipped.to_string(), "skipped");
    }

    #[test]
    fn test_default_canary_policy() {
        let policy = CanaryPolicy::default();
        assert_eq!(policy.canary_count, 1);
        assert_eq!(policy.bake_time_secs, 300);
        assert_eq!(policy.check_interval_secs, 15);
        assert_eq!(policy.health_timeout_secs, 10);
        assert_eq!(policy.failure_threshold, 3);
        assert!(policy.health_gates.is_empty());
    }

    #[test]
    fn test_canary_percentage_rounding() {
        // 30% of 4 nodes = 1.2 → ceil to 2
        let policy = CanaryPolicy {
            canary_count: 0,
            canary_percentage: 0.3,
            ..Default::default()
        };
        let mut orch = CanaryOrchestrator::new(policy, "def456".into());
        let fleet = test_fleet();
        let canaries = orch.select_canary_nodes(&fleet).unwrap();
        assert_eq!(canaries.len(), 2);
    }

    #[test]
    fn test_priority_ordering() {
        // Ensure the lowest-priority node goes first
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
                name: "mid".into(),
                priority: 25,
                is_leader: false,
                health_url: "http://m/health".into(),
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
        let mut orch = CanaryOrchestrator::new(CanaryPolicy::default(), "v1".into());
        let canaries = orch.select_canary_nodes(&fleet).unwrap();
        // Lowest priority non-leader = "low" (5)
        assert_eq!(canaries, vec!["low"]);
    }
}
