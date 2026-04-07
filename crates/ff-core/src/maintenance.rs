//! Maintenance mode management for ForgeFleet nodes.
//!
//! When a node enters maintenance mode:
//! 1. It stops accepting new tasks immediately
//! 2. Active tasks are given a configurable drain timeout to complete
//! 3. The node remains in the registry (visible to dashboards) but is excluded
//!    from routing and scheduling
//! 4. On exit, the node resumes accepting work
//!
//! Supports:
//! - Manual enter/exit
//! - Scheduled maintenance windows (enter at time X, exit at time Y)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

// ─── Maintenance State ───────────────────────────────────────────────────────

/// Lifecycle phase of a node in maintenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenancePhase {
    /// Tasks are being drained — no new work accepted, waiting for in-flight tasks.
    Draining,
    /// Fully in maintenance — no tasks running or accepted.
    Active,
}

impl std::fmt::Display for MaintenancePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draining => write!(f, "Draining"),
            Self::Active => write!(f, "Active (maintenance)"),
        }
    }
}

// ─── Maintenance Entry ───────────────────────────────────────────────────────

/// Record of a node in maintenance mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceEntry {
    /// Node name.
    pub node: String,
    /// Current phase.
    pub phase: MaintenancePhase,
    /// When maintenance was entered.
    pub entered_at: DateTime<Utc>,
    /// How long to wait for in-flight tasks before forcing maintenance.
    pub drain_timeout: Duration,
    /// When draining should be considered complete (entered_at + drain_timeout).
    pub drain_deadline: DateTime<Utc>,
    /// Optional reason for maintenance.
    pub reason: Option<String>,
}

impl MaintenanceEntry {
    /// Has the drain timeout elapsed?
    pub fn drain_expired(&self) -> bool {
        Utc::now() >= self.drain_deadline
    }

    /// How long until drain deadline (zero if expired).
    pub fn drain_remaining(&self) -> Duration {
        let delta = self.drain_deadline.signed_duration_since(Utc::now());
        if delta.num_milliseconds() <= 0 {
            Duration::ZERO
        } else {
            delta.to_std().unwrap_or(Duration::ZERO)
        }
    }
}

// ─── Scheduled Window ────────────────────────────────────────────────────────

/// A scheduled maintenance window — node should enter maintenance at `start`
/// and exit at `end`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceWindow {
    /// Node name.
    pub node: String,
    /// When to enter maintenance.
    pub start: DateTime<Utc>,
    /// When to exit maintenance.
    pub end: DateTime<Utc>,
    /// Drain timeout to use when entering.
    pub drain_timeout: Duration,
    /// Optional reason.
    pub reason: Option<String>,
}

impl MaintenanceWindow {
    /// Is the current time within this window?
    pub fn is_active(&self) -> bool {
        let now = Utc::now();
        now >= self.start && now < self.end
    }

    /// Has this window passed entirely?
    pub fn is_past(&self) -> bool {
        Utc::now() >= self.end
    }

    /// Has this window not started yet?
    pub fn is_upcoming(&self) -> bool {
        Utc::now() < self.start
    }
}

// ─── Maintenance Manager ─────────────────────────────────────────────────────

/// Thread-safe maintenance mode manager.
#[derive(Clone)]
pub struct MaintenanceManager {
    /// Nodes currently in maintenance (or draining).
    entries: Arc<RwLock<HashMap<String, MaintenanceEntry>>>,
    /// Scheduled maintenance windows.
    windows: Arc<RwLock<Vec<MaintenanceWindow>>>,
}

impl MaintenanceManager {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            windows: Arc::new(RwLock::new(Vec::new())),
        }
    }

    // ── Enter / Exit ─────────────────────────────────────────────────────

    /// Enter maintenance mode for a node with a drain timeout.
    ///
    /// The node transitions through Draining → Active. While draining, the node
    /// should finish in-flight tasks. After `drain_timeout`, it is considered
    /// fully in maintenance.
    pub async fn maintenance_enter(
        &self,
        node: &str,
        drain_timeout: Duration,
        reason: Option<String>,
    ) {
        let now = Utc::now();
        let drain_deadline = now
            + chrono::Duration::from_std(drain_timeout).unwrap_or(chrono::Duration::seconds(300));

        let entry = MaintenanceEntry {
            node: node.to_string(),
            phase: MaintenancePhase::Draining,
            entered_at: now,
            drain_timeout,
            drain_deadline,
            reason: reason.clone(),
        };

        warn!(
            node = node,
            drain_timeout_secs = drain_timeout.as_secs(),
            reason = reason.as_deref().unwrap_or("none"),
            "entering maintenance mode"
        );

        self.entries.write().await.insert(node.to_string(), entry);
    }

    /// Exit maintenance mode for a node.
    ///
    /// Returns `true` if the node was in maintenance, `false` if it wasn't.
    pub async fn maintenance_exit(&self, node: &str) -> bool {
        let removed = self.entries.write().await.remove(node).is_some();
        if removed {
            info!(node = node, "exiting maintenance mode, resuming work");
        }
        removed
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// Is a node in maintenance mode (either draining or fully active)?
    pub async fn is_in_maintenance(&self, node: &str) -> bool {
        self.entries.read().await.contains_key(node)
    }

    /// Is a node currently draining (but not yet fully in maintenance)?
    pub async fn is_draining(&self, node: &str) -> bool {
        let entries = self.entries.read().await;
        matches!(
            entries.get(node),
            Some(entry) if entry.phase == MaintenancePhase::Draining && !entry.drain_expired()
        )
    }

    /// Should a node accept new tasks?
    ///
    /// Returns `false` if the node is in any maintenance phase.
    pub async fn should_accept_work(&self, node: &str) -> bool {
        !self.is_in_maintenance(node).await
    }

    /// Get maintenance info for a specific node.
    pub async fn get_info(&self, node: &str) -> Option<MaintenanceEntry> {
        self.entries.read().await.get(node).cloned()
    }

    /// List all nodes currently in maintenance.
    pub async fn list_maintenance(&self) -> Vec<MaintenanceEntry> {
        self.entries.read().await.values().cloned().collect()
    }

    /// Number of nodes in maintenance.
    pub async fn maintenance_count(&self) -> usize {
        self.entries.read().await.len()
    }

    // ── Drain Progression ────────────────────────────────────────────────

    /// Advance any draining nodes to Active if their drain deadline has passed.
    ///
    /// Call this periodically (e.g. on heartbeat tick). Returns names of nodes
    /// that transitioned from Draining to Active.
    pub async fn tick_drain_progress(&self) -> Vec<String> {
        let mut transitioned = Vec::new();
        let mut entries = self.entries.write().await;

        for (name, entry) in entries.iter_mut() {
            if entry.phase == MaintenancePhase::Draining && entry.drain_expired() {
                info!(node = name, "drain complete, entering full maintenance");
                entry.phase = MaintenancePhase::Active;
                transitioned.push(name.clone());
            }
        }

        transitioned
    }

    // ── Scheduled Windows ────────────────────────────────────────────────

    /// Schedule a maintenance window.
    pub async fn schedule_window(&self, window: MaintenanceWindow) {
        info!(
            node = %window.node,
            start = %window.start,
            end = %window.end,
            "scheduling maintenance window"
        );
        self.windows.write().await.push(window);
    }

    /// List all scheduled windows (pruning past ones).
    pub async fn list_windows(&self) -> Vec<MaintenanceWindow> {
        let mut windows = self.windows.write().await;
        windows.retain(|w| !w.is_past());
        windows.clone()
    }

    /// Check scheduled windows and enter/exit maintenance as needed.
    ///
    /// Call this periodically. Returns `(entered, exited)` — names of nodes
    /// that were auto-entered or auto-exited.
    pub async fn tick_scheduled_windows(&self) -> (Vec<String>, Vec<String>) {
        let mut entered = Vec::new();
        let mut exited = Vec::new();

        let windows = self.windows.read().await.clone();

        for window in &windows {
            if window.is_active() && !self.is_in_maintenance(&window.node).await {
                // Time to enter maintenance.
                self.maintenance_enter(&window.node, window.drain_timeout, window.reason.clone())
                    .await;
                entered.push(window.node.clone());
            } else if window.is_past() && self.is_in_maintenance(&window.node).await {
                // Window ended — exit maintenance.
                self.maintenance_exit(&window.node).await;
                exited.push(window.node.clone());
            }
        }

        // Prune past windows.
        self.windows.write().await.retain(|w| !w.is_past());

        (entered, exited)
    }

    /// Cancel a scheduled window for a node (removes all windows for that node).
    pub async fn cancel_windows(&self, node: &str) -> usize {
        let mut windows = self.windows.write().await;
        let before = windows.len();
        windows.retain(|w| w.node != node);
        before - windows.len()
    }
}

impl Default for MaintenanceManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_enter_and_exit_maintenance() {
        let mgr = MaintenanceManager::new();

        assert!(mgr.should_accept_work("taylor").await);

        mgr.maintenance_enter("taylor", Duration::from_millis(50), Some("upgrade".into()))
            .await;

        assert!(!mgr.should_accept_work("taylor").await);
        assert!(mgr.is_in_maintenance("taylor").await);
        assert!(mgr.is_draining("taylor").await);

        let info = mgr.get_info("taylor").await.unwrap();
        assert_eq!(info.phase, MaintenancePhase::Draining);
        assert_eq!(info.reason, Some("upgrade".into()));

        assert!(mgr.maintenance_exit("taylor").await);
        assert!(mgr.should_accept_work("taylor").await);
    }

    #[tokio::test]
    async fn test_exit_nonexistent_returns_false() {
        let mgr = MaintenanceManager::new();
        assert!(!mgr.maintenance_exit("ghost").await);
    }

    #[tokio::test]
    async fn test_drain_progresses_to_active() {
        let mgr = MaintenanceManager::new();

        mgr.maintenance_enter("james", Duration::from_millis(30), None)
            .await;

        assert_eq!(mgr.tick_drain_progress().await, Vec::<String>::new());

        tokio::time::sleep(Duration::from_millis(40)).await;

        let transitioned = mgr.tick_drain_progress().await;
        assert_eq!(transitioned, vec!["james".to_string()]);

        let info = mgr.get_info("james").await.unwrap();
        assert_eq!(info.phase, MaintenancePhase::Active);

        // Still in maintenance (Active phase).
        assert!(!mgr.should_accept_work("james").await);
    }

    #[tokio::test]
    async fn test_list_maintenance() {
        let mgr = MaintenanceManager::new();
        mgr.maintenance_enter("a", Duration::from_secs(60), None)
            .await;
        mgr.maintenance_enter("b", Duration::from_secs(60), None)
            .await;

        let list = mgr.list_maintenance().await;
        assert_eq!(list.len(), 2);
        assert_eq!(mgr.maintenance_count().await, 2);
    }

    #[tokio::test]
    async fn test_maintenance_entry_drain_remaining() {
        let entry = MaintenanceEntry {
            node: "test".to_string(),
            phase: MaintenancePhase::Draining,
            entered_at: Utc::now(),
            drain_timeout: Duration::from_secs(60),
            drain_deadline: Utc::now() + chrono::Duration::seconds(60),
            reason: None,
        };
        assert!(entry.drain_remaining() > Duration::ZERO);
        assert!(!entry.drain_expired());

        let expired_entry = MaintenanceEntry {
            node: "test".to_string(),
            phase: MaintenancePhase::Draining,
            entered_at: Utc::now() - chrono::Duration::seconds(120),
            drain_timeout: Duration::from_secs(60),
            drain_deadline: Utc::now() - chrono::Duration::seconds(60),
            reason: None,
        };
        assert_eq!(expired_entry.drain_remaining(), Duration::ZERO);
        assert!(expired_entry.drain_expired());
    }

    #[tokio::test]
    async fn test_schedule_window() {
        let mgr = MaintenanceManager::new();

        let window = MaintenanceWindow {
            node: "taylor".to_string(),
            start: Utc::now() - chrono::Duration::seconds(10),
            end: Utc::now() + chrono::Duration::seconds(300),
            drain_timeout: Duration::from_secs(30),
            reason: Some("scheduled upgrade".into()),
        };

        mgr.schedule_window(window).await;

        // Should enter maintenance when we tick.
        let (entered, exited) = mgr.tick_scheduled_windows().await;
        assert_eq!(entered, vec!["taylor".to_string()]);
        assert!(exited.is_empty());
        assert!(mgr.is_in_maintenance("taylor").await);
    }

    #[tokio::test]
    async fn test_schedule_window_past_exits() {
        let mgr = MaintenanceManager::new();

        // Window that already passed.
        let window = MaintenanceWindow {
            node: "james".to_string(),
            start: Utc::now() - chrono::Duration::seconds(200),
            end: Utc::now() - chrono::Duration::seconds(100),
            drain_timeout: Duration::from_secs(10),
            reason: None,
        };
        mgr.schedule_window(window).await;

        // Manually put in maintenance (as if entered during window).
        mgr.maintenance_enter("james", Duration::from_millis(10), None)
            .await;

        let (entered, exited) = mgr.tick_scheduled_windows().await;
        assert!(entered.is_empty());
        assert_eq!(exited, vec!["james".to_string()]);
        assert!(!mgr.is_in_maintenance("james").await);
    }

    #[tokio::test]
    async fn test_cancel_windows() {
        let mgr = MaintenanceManager::new();

        mgr.schedule_window(MaintenanceWindow {
            node: "taylor".to_string(),
            start: Utc::now() + chrono::Duration::seconds(100),
            end: Utc::now() + chrono::Duration::seconds(200),
            drain_timeout: Duration::from_secs(30),
            reason: None,
        })
        .await;

        mgr.schedule_window(MaintenanceWindow {
            node: "taylor".to_string(),
            start: Utc::now() + chrono::Duration::seconds(300),
            end: Utc::now() + chrono::Duration::seconds(400),
            drain_timeout: Duration::from_secs(30),
            reason: None,
        })
        .await;

        assert_eq!(mgr.cancel_windows("taylor").await, 2);
        assert!(mgr.list_windows().await.is_empty());
    }

    #[tokio::test]
    async fn test_window_state_checks() {
        let future = MaintenanceWindow {
            node: "test".into(),
            start: Utc::now() + chrono::Duration::seconds(100),
            end: Utc::now() + chrono::Duration::seconds(200),
            drain_timeout: Duration::from_secs(30),
            reason: None,
        };
        assert!(future.is_upcoming());
        assert!(!future.is_active());
        assert!(!future.is_past());

        let active = MaintenanceWindow {
            node: "test".into(),
            start: Utc::now() - chrono::Duration::seconds(50),
            end: Utc::now() + chrono::Duration::seconds(50),
            drain_timeout: Duration::from_secs(30),
            reason: None,
        };
        assert!(!active.is_upcoming());
        assert!(active.is_active());
        assert!(!active.is_past());

        let past = MaintenanceWindow {
            node: "test".into(),
            start: Utc::now() - chrono::Duration::seconds(200),
            end: Utc::now() - chrono::Duration::seconds(100),
            drain_timeout: Duration::from_secs(30),
            reason: None,
        };
        assert!(!past.is_upcoming());
        assert!(!past.is_active());
        assert!(past.is_past());
    }

    #[tokio::test]
    async fn test_is_draining_false_after_deadline() {
        let mgr = MaintenanceManager::new();
        mgr.maintenance_enter("x", Duration::from_millis(20), None)
            .await;
        assert!(mgr.is_draining("x").await);

        tokio::time::sleep(Duration::from_millis(30)).await;

        // Drain timeout passed — still in maintenance but not "draining".
        assert!(!mgr.is_draining("x").await);
        assert!(mgr.is_in_maintenance("x").await);
    }
}
