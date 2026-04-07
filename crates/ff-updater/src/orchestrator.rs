//! Update orchestrator — full lifecycle state machine.
//!
//! Coordinates the complete update flow:
//! `Check → Build → Verify → Swap → Signal Restart`
//!
//! Fleet-aware: supports rolling updates so not all nodes update simultaneously.
//! Tracks state machine transitions for observability and recovery.

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::builder::{BuildResult, BuilderConfig, SourceBuilder};
use crate::canary::{CanaryOrchestrator, CanaryPolicy, FleetNode, RolloutProgress};
use crate::checker::{CheckResult, CheckerConfig, UpdateChecker};
use crate::error::{UpdateError, UpdateResult};
use crate::rollback::{RollbackConfig, RollbackManager, RollbackResult};
use crate::rollout::{RolloutController, RolloutPlan, RolloutStatus};
use crate::swapper::{BinarySwapper, SwapResult, SwapperConfig};
use crate::verifier::{BinaryVerifier, VerifierConfig, VerifyResult};

// ─── State machine ───────────────────────────────────────────────────────────

/// The current state of an update operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// No update in progress.
    Idle,
    /// Checking for available updates.
    Checking,
    /// Building new binary from source.
    Building,
    /// Verifying the newly built binary.
    Verifying,
    /// Swapping the binary (atomic replace).
    Swapping,
    /// Waiting for restart signal to be acknowledged.
    WaitingRestart,
    /// Update complete and successful.
    Complete,
    /// Update failed at some stage.
    Failed,
    /// Rolled back to previous version.
    RolledBack,
}

impl fmt::Display for UpdateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Checking => write!(f, "checking"),
            Self::Building => write!(f, "building"),
            Self::Verifying => write!(f, "verifying"),
            Self::Swapping => write!(f, "swapping"),
            Self::WaitingRestart => write!(f, "waiting_restart"),
            Self::Complete => write!(f, "complete"),
            Self::Failed => write!(f, "failed"),
            Self::RolledBack => write!(f, "rolled_back"),
        }
    }
}

// ─── Update record ───────────────────────────────────────────────────────────

/// Complete record of an update attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRecord {
    /// Unique ID for this update attempt.
    pub id: Uuid,

    /// Node this update is for.
    pub node_name: String,

    /// Current state.
    pub state: UpdateState,

    /// When the update started.
    pub started_at: DateTime<Utc>,

    /// When the update completed (or failed).
    pub completed_at: Option<DateTime<Utc>>,

    /// Check result (if check ran).
    pub check_result: Option<CheckResult>,

    /// Build result (if build ran).
    pub build_result: Option<BuildResult>,

    /// Verify result (if verify ran).
    pub verify_result: Option<VerifyResult>,

    /// Swap result (if swap ran).
    pub swap_result: Option<SwapResult>,

    /// Rollback result (if rollback was needed).
    pub rollback_result: Option<RollbackResult>,

    /// Error message if the update failed.
    pub error: Option<String>,
}

impl UpdateRecord {
    fn new(node_name: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            node_name: node_name.to_string(),
            state: UpdateState::Idle,
            started_at: Utc::now(),
            completed_at: None,
            check_result: None,
            build_result: None,
            verify_result: None,
            swap_result: None,
            rollback_result: None,
            error: None,
        }
    }

    fn fail(&mut self, error: String) {
        self.state = UpdateState::Failed;
        self.error = Some(error);
        self.completed_at = Some(Utc::now());
    }

    fn complete(&mut self) {
        self.state = UpdateState::Complete;
        self.completed_at = Some(Utc::now());
    }
}

// ─── Fleet coordination ──────────────────────────────────────────────────────

/// Rolling update strategy for fleet-wide updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollingUpdateConfig {
    /// Maximum number of nodes to update simultaneously (default: 1).
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Delay between starting updates on successive nodes (seconds, default: 60).
    #[serde(default = "default_stagger_delay")]
    pub stagger_delay_secs: u64,

    /// Whether to abort the entire rollout if one node fails (default: true).
    #[serde(default = "default_true")]
    pub abort_on_failure: bool,

    /// Ordered list of node names. Leader updates last.
    #[serde(default)]
    pub node_order: Vec<String>,
}

fn default_max_concurrent() -> usize {
    1
}
fn default_stagger_delay() -> u64 {
    60
}
fn default_true() -> bool {
    true
}

impl Default for RollingUpdateConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            stagger_delay_secs: 60,
            abort_on_failure: true,
            node_order: Vec::new(),
        }
    }
}

// ─── Orchestrator config ─────────────────────────────────────────────────────

/// Combined configuration for the update orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// This node's name.
    pub node_name: String,

    /// Checker configuration.
    pub checker: CheckerConfig,

    /// Builder configuration.
    pub builder: BuilderConfig,

    /// Verifier configuration.
    #[serde(default)]
    pub verifier: VerifierConfig,

    /// Swapper configuration.
    pub swapper: SwapperConfig,

    /// Rollback configuration.
    pub rollback: RollbackConfig,

    /// Rolling update configuration (fleet coordination).
    #[serde(default)]
    pub rolling: RollingUpdateConfig,

    /// Canary deployment policy.
    #[serde(default)]
    pub canary: CanaryPolicy,

    /// Whether to automatically trigger updates when detected (default: false).
    /// When false, the orchestrator reports availability but waits for explicit trigger.
    #[serde(default)]
    pub auto_update: bool,
}

// ─── Restart signal ──────────────────────────────────────────────────────────

/// How the orchestrator signals that a restart is needed.
/// The actual restart is handled by the parent process (systemd, launchd, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum RestartSignal {
    /// Write a file that the service supervisor watches.
    TouchFile(PathBuf),
    /// Exit with a specific code that the supervisor interprets as "restart me".
    ExitCode(i32),
    /// No restart needed (for testing).
    #[default]
    None,
}

// ─── Orchestrator ────────────────────────────────────────────────────────────

/// The update orchestrator — drives the full update lifecycle.
pub struct UpdateOrchestrator {
    config: OrchestratorConfig,
    current_update: Option<UpdateRecord>,
    history: Vec<UpdateRecord>,
    restart_signal: RestartSignal,
    canary: Option<CanaryOrchestrator>,
    rollout: Option<RolloutController>,
}

impl UpdateOrchestrator {
    pub fn new(config: OrchestratorConfig, restart_signal: RestartSignal) -> Self {
        Self {
            config,
            current_update: None,
            history: Vec::new(),
            restart_signal,
            canary: None,
            rollout: None,
        }
    }

    /// Current update state.
    pub fn state(&self) -> UpdateState {
        self.current_update
            .as_ref()
            .map(|u| u.state)
            .unwrap_or(UpdateState::Idle)
    }

    /// Whether an update is currently in progress.
    pub fn is_busy(&self) -> bool {
        matches!(
            self.state(),
            UpdateState::Checking
                | UpdateState::Building
                | UpdateState::Verifying
                | UpdateState::Swapping
                | UpdateState::WaitingRestart
        )
    }

    /// Get the current update record (if any).
    pub fn current_update(&self) -> Option<&UpdateRecord> {
        self.current_update.as_ref()
    }

    /// Get update history.
    pub fn history(&self) -> &[UpdateRecord] {
        &self.history
    }

    /// Borrow the canary orchestrator (if a canary deployment is in progress).
    pub fn canary(&self) -> Option<&CanaryOrchestrator> {
        self.canary.as_ref()
    }

    /// Borrow the rollout controller (if a rollout is in progress).
    pub fn rollout(&self) -> Option<&RolloutController> {
        self.rollout.as_ref()
    }

    /// Get current canary rollout progress (if any).
    pub fn canary_progress(&self) -> Option<&RolloutProgress> {
        self.canary.as_ref().map(|c| c.progress())
    }

    /// Get current rollout status (if any).
    pub fn rollout_status(&self) -> Option<&RolloutStatus> {
        self.rollout.as_ref().map(|r| r.status())
    }

    // ── Canary-aware fleet update ────────────────────────────────────

    /// Initiate a canary deployment across the fleet.
    ///
    /// This replaces the simple rolling update with:
    ///   1. Select canary node(s)
    ///   2. Update canary only
    ///   3. Monitor + health gates during bake time
    ///   4. If healthy → create a `RolloutPlan` for remaining nodes
    ///   5. If unhealthy → rollback canary, abort
    ///
    /// Returns the list of canary node names to update first.
    pub fn begin_canary_update(
        &mut self,
        fleet: &[FleetNode],
        target_version: &str,
    ) -> UpdateResult<Vec<String>> {
        if self.is_busy() {
            return Err(UpdateError::AlreadyInProgress {
                state: self.state().to_string(),
            });
        }

        let mut canary_orch =
            CanaryOrchestrator::new(self.config.canary.clone(), target_version.to_string());

        let canary_nodes = canary_orch.select_canary_nodes(fleet)?;
        canary_orch.begin_canary_deploy();

        info!(
            canary_nodes = ?canary_nodes,
            target_version,
            "canary deployment initiated"
        );

        // Prepare the rollout plan for after canary promotion.
        let plan = RolloutPlan::from_fleet(fleet, &canary_nodes, target_version.to_string());
        let rollout_ctrl = RolloutController::new(plan);

        self.canary = Some(canary_orch);
        self.rollout = Some(rollout_ctrl);

        Ok(canary_nodes)
    }

    /// Called by the fleet coordinator after a canary node has been updated.
    pub fn report_canary_node_updated(&mut self, node_name: &str) {
        if let Some(ref mut c) = self.canary {
            c.mark_node_updated(node_name);
        }
    }

    /// Evaluate canary health gates. Returns `true` if all canaries are healthy.
    pub async fn check_canary_health(&mut self, fleet: &[FleetNode]) -> bool {
        match self.canary.as_mut() {
            Some(c) => c.check_canary_health(fleet).await,
            None => false,
        }
    }

    /// Promote the canary and start the rolling update of remaining nodes.
    /// Returns the first batch of nodes to update.
    pub fn promote_and_start_rollout(&mut self) -> UpdateResult<Vec<String>> {
        let canary = self
            .canary
            .as_mut()
            .ok_or_else(|| UpdateError::FleetCoordination {
                reason: "no canary deployment in progress".into(),
            })?;

        canary.promote_canary();
        canary.begin_rollout();

        let rollout = self
            .rollout
            .as_mut()
            .ok_or_else(|| UpdateError::FleetCoordination {
                reason: "no rollout controller available".into(),
            })?;

        let batch = rollout.start()?;
        info!(batch = ?batch, "canary promoted — rolling out to remaining nodes");
        Ok(batch)
    }

    /// Report a rollout node as updated.
    pub fn report_rollout_node_updated(&mut self, node_name: &str) {
        if let Some(ref mut r) = self.rollout {
            r.mark_node_updated(node_name);
        }
    }

    /// Report a rollout node as healthy.
    pub fn report_rollout_node_healthy(&mut self, node_name: &str) {
        if let Some(ref mut r) = self.rollout {
            r.mark_node_healthy(node_name);
        }
    }

    /// Report a rollout node as failed.
    pub fn report_rollout_node_failed(&mut self, node_name: &str, error: String) {
        if let Some(ref mut r) = self.rollout {
            r.mark_node_failed(node_name, error);
        }
    }

    /// Get the next batch from the rollout controller.
    pub fn next_rollout_batch(&mut self) -> Vec<String> {
        match self.rollout.as_mut() {
            Some(r) => r.next_batch(),
            None => Vec::new(),
        }
    }

    /// Abort the canary rollout and mark the canary as rolled back.
    pub fn abort_canary_rollout(&mut self, failed_nodes: &[String]) {
        if let Some(ref mut c) = self.canary {
            c.mark_rolled_back(failed_nodes);
        }
        if let Some(ref mut r) = self.rollout {
            let _ = r.abort();
        }
        warn!("canary rollout aborted");
    }

    /// Mark the full canary+rollout as complete.
    pub fn complete_canary_rollout(&mut self) {
        if let Some(ref mut c) = self.canary {
            c.mark_complete();
        }
        info!("full canary rollout complete");
    }

    /// Pause the rollout (manual control).
    pub fn pause_rollout(&mut self) -> UpdateResult<()> {
        self.rollout
            .as_mut()
            .ok_or_else(|| UpdateError::FleetCoordination {
                reason: "no rollout in progress".into(),
            })?
            .pause()
    }

    /// Resume a paused rollout (manual control).
    pub fn resume_rollout(&mut self) -> UpdateResult<Vec<String>> {
        self.rollout
            .as_mut()
            .ok_or_else(|| UpdateError::FleetCoordination {
                reason: "no rollout in progress".into(),
            })?
            .resume()
    }

    /// Run the full update pipeline: check → build → verify → swap → restart.
    ///
    /// This is the main entry point for a self-update on this node.
    pub fn run_update(&mut self) -> UpdateResult<UpdateRecord> {
        if self.is_busy() {
            return Err(UpdateError::AlreadyInProgress {
                state: self.state().to_string(),
            });
        }

        let mut record = UpdateRecord::new(&self.config.node_name);
        record.state = UpdateState::Checking;
        info!(node = %self.config.node_name, "starting update pipeline");

        // ── Step 1: Check ────────────────────────────────────────────
        record.state = UpdateState::Checking;
        let check = {
            let mut checker = UpdateChecker::new(self.config.checker.clone());
            match checker.check_git() {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("check failed: {e}");
                    error!(%msg);
                    record.fail(msg);
                    self.archive_record(record.clone());
                    return Ok(record);
                }
            }
        };

        if !check.update_available {
            info!("no update available, already up to date");
            record.check_result = Some(check);
            record.complete();
            self.archive_record(record.clone());
            return Ok(record);
        }

        info!(
            commits_behind = check.commits_behind,
            remote_sha = %check.remote_sha,
            "update available"
        );
        record.check_result = Some(check);

        // ── Step 2: Build ────────────────────────────────────────────
        record.state = UpdateState::Building;
        let build = {
            let builder = SourceBuilder::new(self.config.builder.clone());
            match builder.build() {
                Ok(b) => b,
                Err(e) => {
                    let msg = format!("build error: {e}");
                    error!(%msg);
                    record.fail(msg);
                    self.archive_record(record.clone());
                    return Ok(record);
                }
            }
        };

        if !build.success {
            let msg = "build completed but was not successful".to_string();
            warn!(%msg);
            record.build_result = Some(build);
            record.fail(msg);
            self.archive_record(record.clone());
            return Ok(record);
        }

        let binary_path = build
            .binary_path
            .clone()
            .expect("successful build must have binary_path");
        record.build_result = Some(build);

        // ── Step 3: Verify ───────────────────────────────────────────
        record.state = UpdateState::Verifying;
        let verify = {
            let verifier = BinaryVerifier::new(self.config.verifier.clone());
            match verifier.verify(&binary_path) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("verify error: {e}");
                    error!(%msg);
                    record.fail(msg);
                    self.archive_record(record.clone());
                    return Ok(record);
                }
            }
        };

        if !verify.passed {
            let msg = format!("verification failed: {}", verify.summary);
            warn!(%msg);
            record.verify_result = Some(verify);
            record.fail(msg);
            self.archive_record(record.clone());
            return Ok(record);
        }

        record.verify_result = Some(verify);

        // ── Step 4: Swap ─────────────────────────────────────────────
        record.state = UpdateState::Swapping;
        let swap = {
            let swapper = BinarySwapper::new(self.config.swapper.clone());
            match swapper.swap(&binary_path) {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("swap error: {e}");
                    error!(%msg);
                    record.fail(msg);
                    self.archive_record(record.clone());
                    return Ok(record);
                }
            }
        };

        if !swap.success {
            let msg = format!("swap failed: {}", swap.summary);
            warn!(%msg);
            record.swap_result = Some(swap);
            record.fail(msg);
            self.archive_record(record.clone());
            return Ok(record);
        }

        record.swap_result = Some(swap);

        // ── Step 5: Signal restart ───────────────────────────────────
        record.state = UpdateState::WaitingRestart;
        if let Err(e) = self.signal_restart() {
            let msg = format!("restart signal failed: {e}");
            error!(%msg);
            // Not fatal — the binary is already swapped. The operator can restart manually.
            warn!("binary swapped but restart signal failed — manual restart needed");
        }

        record.complete();
        info!(
            id = %record.id,
            "update pipeline complete — restart pending"
        );

        self.archive_record(record.clone());
        Ok(record)
    }

    /// Trigger a rollback to the previous binary version.
    pub fn rollback(&mut self) -> UpdateResult<RollbackResult> {
        info!(node = %self.config.node_name, "triggering rollback");

        let mgr = RollbackManager::new(self.config.rollback.clone());
        let result = mgr.rollback()?;

        // Record in history
        if let Some(ref mut current) = self.current_update {
            current.rollback_result = Some(result.clone());
            current.state = UpdateState::RolledBack;
            current.completed_at = Some(Utc::now());
        }

        Ok(result)
    }

    /// Check if this node should update given its position in the rolling order.
    ///
    /// Returns `true` if this node is next in line, `false` if it should wait.
    pub fn is_my_turn(&self, completed_nodes: &[String]) -> bool {
        let order = &self.config.rolling.node_order;
        if order.is_empty() {
            // No ordering defined — go ahead
            return true;
        }

        let my_name = &self.config.node_name;
        let my_index = order.iter().position(|n| n == my_name);

        match my_index {
            None => {
                // Not in the order list — safe to update
                true
            }
            Some(idx) => {
                // All nodes before me must be in completed_nodes
                order[..idx].iter().all(|n| completed_nodes.contains(n))
            }
        }
    }

    // ── Internal ─────────────────────────────────────────────────────

    fn signal_restart(&self) -> UpdateResult<()> {
        match &self.restart_signal {
            RestartSignal::TouchFile(path) => {
                info!(path = %path.display(), "writing restart signal file");
                std::fs::write(path, Utc::now().to_rfc3339()).map_err(|e| {
                    UpdateError::Other(format!("failed to write restart signal: {e}"))
                })?;
            }
            RestartSignal::ExitCode(code) => {
                info!(code, "restart signal: exit code (deferred)");
                // The actual exit is handled by the caller
            }
            RestartSignal::None => {
                info!("no restart signal configured");
            }
        }
        Ok(())
    }

    fn archive_record(&mut self, record: UpdateRecord) {
        self.current_update = Some(record.clone());
        self.history.push(record);

        // Keep history bounded
        if self.history.len() > 100 {
            self.history.drain(0..50);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_state_display() {
        assert_eq!(UpdateState::Idle.to_string(), "idle");
        assert_eq!(UpdateState::Building.to_string(), "building");
        assert_eq!(UpdateState::RolledBack.to_string(), "rolled_back");
    }

    /// Helper to build a default OrchestratorConfig for tests.
    fn test_config(node_name: &str, rolling: RollingUpdateConfig) -> OrchestratorConfig {
        OrchestratorConfig {
            node_name: node_name.into(),
            checker: CheckerConfig::default(),
            builder: BuilderConfig::default(),
            verifier: VerifierConfig::default(),
            swapper: SwapperConfig::default(),
            rollback: RollbackConfig::default(),
            rolling,
            canary: CanaryPolicy::default(),
            auto_update: false,
        }
    }

    #[test]
    fn test_is_my_turn_no_order() {
        let config = RollingUpdateConfig::default();
        let orch = UpdateOrchestrator::new(test_config("taylor", config), RestartSignal::None);
        // No order defined → always my turn
        assert!(orch.is_my_turn(&[]));
    }

    #[test]
    fn test_is_my_turn_ordered() {
        let config = RollingUpdateConfig {
            node_order: vec!["james".into(), "marcus".into(), "taylor".into()],
            ..Default::default()
        };
        let orch = UpdateOrchestrator::new(test_config("taylor", config), RestartSignal::None);

        // None completed → not my turn
        assert!(!orch.is_my_turn(&[]));

        // Only james → not my turn
        assert!(!orch.is_my_turn(&["james".into()]));

        // james + marcus → my turn
        assert!(orch.is_my_turn(&["james".into(), "marcus".into()]));
    }

    #[test]
    fn test_update_record_new() {
        let record = UpdateRecord::new("taylor");
        assert_eq!(record.state, UpdateState::Idle);
        assert_eq!(record.node_name, "taylor");
        assert!(record.error.is_none());
    }

    #[test]
    fn test_orchestrator_idle_by_default() {
        let orch = UpdateOrchestrator::new(
            test_config("taylor", RollingUpdateConfig::default()),
            RestartSignal::None,
        );

        assert_eq!(orch.state(), UpdateState::Idle);
        assert!(!orch.is_busy());
        assert!(orch.current_update().is_none());
        assert!(orch.history().is_empty());
        assert!(orch.canary().is_none());
        assert!(orch.rollout().is_none());
    }

    #[test]
    fn test_begin_canary_update() {
        use crate::canary::{CanaryStage, FleetNode};

        let mut orch = UpdateOrchestrator::new(
            test_config("taylor", RollingUpdateConfig::default()),
            RestartSignal::None,
        );

        let fleet = vec![
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
                name: "taylor".into(),
                priority: 100,
                is_leader: true,
                health_url: "http://taylor/health".into(),
                current_version: Some("v1".into()),
            },
        ];

        let canaries = orch.begin_canary_update(&fleet, "v2").unwrap();
        assert_eq!(canaries, vec!["james"]);

        assert!(orch.canary().is_some());
        assert!(orch.rollout().is_some());
        assert_eq!(orch.canary().unwrap().stage(), CanaryStage::CanaryDeploying);
    }

    #[test]
    fn test_promote_and_rollout() {
        use crate::canary::{CanaryStage, FleetNode};
        use crate::rollout::RolloutState;

        let mut orch = UpdateOrchestrator::new(
            test_config("taylor", RollingUpdateConfig::default()),
            RestartSignal::None,
        );

        let fleet = vec![
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
                name: "taylor".into(),
                priority: 100,
                is_leader: true,
                health_url: "http://taylor/health".into(),
                current_version: Some("v1".into()),
            },
        ];

        orch.begin_canary_update(&fleet, "v2").unwrap();
        orch.report_canary_node_updated("james");

        // Canary should be in monitoring now
        assert_eq!(
            orch.canary().unwrap().stage(),
            CanaryStage::CanaryMonitoring
        );

        // Promote and start rollout
        let batch = orch.promote_and_start_rollout().unwrap();
        // Remaining: marcus, taylor (leader last)
        assert_eq!(batch, vec!["marcus"]);

        assert_eq!(orch.rollout().unwrap().state(), RolloutState::InProgress);
    }

    #[test]
    fn test_abort_canary_rollout() {
        use crate::canary::{CanaryStage, FleetNode};

        let mut orch = UpdateOrchestrator::new(
            test_config("taylor", RollingUpdateConfig::default()),
            RestartSignal::None,
        );

        let fleet = vec![
            FleetNode {
                name: "james".into(),
                priority: 10,
                is_leader: false,
                health_url: "http://james/health".into(),
                current_version: None,
            },
            FleetNode {
                name: "taylor".into(),
                priority: 100,
                is_leader: true,
                health_url: "http://taylor/health".into(),
                current_version: None,
            },
        ];

        orch.begin_canary_update(&fleet, "v2").unwrap();
        orch.report_canary_node_updated("james");

        // Simulate canary failure → abort
        orch.abort_canary_rollout(&["james".into()]);
        assert_eq!(orch.canary().unwrap().stage(), CanaryStage::RolledBack);
    }
}
