//! Node quarantine system for ForgeFleet.
//!
//! Isolates flaky nodes from routing after repeated health-check failures.
//! Quarantined nodes are temporarily removed from load-balancing but stay in the
//! registry so they can be restored when healthy again.
//!
//! Features:
//! - Manual quarantine with reason and duration
//! - Auto-quarantine based on configurable failure thresholds
//! - Automatic release after duration expires
//! - Manual early release

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

// ─── Policy ──────────────────────────────────────────────────────────────────

/// Configurable thresholds for auto-quarantine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantinePolicy {
    /// Number of health-check failures within `window` that triggers quarantine.
    pub failure_threshold: u32,
    /// Time window over which failures are counted.
    pub window: Duration,
    /// How long a node stays quarantined before auto-release.
    pub default_quarantine_duration: Duration,
}

impl Default for QuarantinePolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            window: Duration::from_secs(5 * 60), // 5 minutes
            default_quarantine_duration: Duration::from_secs(10 * 60), // 10 minutes
        }
    }
}

// ─── Quarantine Entry ────────────────────────────────────────────────────────

/// Record of a quarantined node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineEntry {
    /// Node name.
    pub node: String,
    /// Why this node was quarantined.
    pub reason: String,
    /// When quarantine started.
    pub quarantined_at: DateTime<Utc>,
    /// When this node should be auto-released.
    pub release_at: DateTime<Utc>,
    /// Whether this was auto-triggered (vs. manual).
    pub auto: bool,
}

impl QuarantineEntry {
    /// Has the quarantine duration expired?
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.release_at
    }

    /// Remaining quarantine time (zero if expired).
    pub fn remaining(&self) -> Duration {
        let delta = self.release_at.signed_duration_since(Utc::now());
        if delta.num_milliseconds() <= 0 {
            Duration::ZERO
        } else {
            delta.to_std().unwrap_or(Duration::ZERO)
        }
    }
}

// ─── Failure Tracker (for auto-quarantine) ───────────────────────────────────

/// Tracks timestamped failures for a single node within a sliding window.
#[derive(Debug, Clone, Default)]
struct FailureTracker {
    /// Timestamps of each recorded failure.
    timestamps: Vec<DateTime<Utc>>,
}

impl FailureTracker {
    /// Record a failure at `now` and prune old entries outside `window`.
    fn record(&mut self, now: DateTime<Utc>, window: Duration) {
        self.timestamps.push(now);
        let cutoff =
            now - chrono::Duration::from_std(window).unwrap_or(chrono::Duration::seconds(300));
        self.timestamps.retain(|t| *t >= cutoff);
    }

    /// Count of failures within the window.
    fn count(&self) -> u32 {
        self.timestamps.len() as u32
    }

    /// Clear history (e.g. after quarantine).
    fn clear(&mut self) {
        self.timestamps.clear();
    }
}

// ─── Node Quarantine ─────────────────────────────────────────────────────────

/// Thread-safe quarantine manager.
#[derive(Clone)]
pub struct NodeQuarantine {
    policy: QuarantinePolicy,
    /// Currently quarantined nodes.
    quarantined: Arc<RwLock<HashMap<String, QuarantineEntry>>>,
    /// Per-node failure trackers for auto-quarantine.
    trackers: Arc<RwLock<HashMap<String, FailureTracker>>>,
}

impl NodeQuarantine {
    /// Create a new quarantine manager with the given policy.
    pub fn new(policy: QuarantinePolicy) -> Self {
        Self {
            policy,
            quarantined: Arc::new(RwLock::new(HashMap::new())),
            trackers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create with default policy.
    pub fn with_defaults() -> Self {
        Self::new(QuarantinePolicy::default())
    }

    // ── Manual quarantine ────────────────────────────────────────────────

    /// Manually quarantine a node for the given duration.
    pub async fn quarantine_node(&self, name: &str, reason: &str, duration: Duration) {
        let now = Utc::now();
        let release_at =
            now + chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::seconds(600));

        let entry = QuarantineEntry {
            node: name.to_string(),
            reason: reason.to_string(),
            quarantined_at: now,
            release_at,
            auto: false,
        };

        warn!(
            node = name,
            reason = reason,
            release_at = %release_at,
            "node manually quarantined"
        );

        self.quarantined
            .write()
            .await
            .insert(name.to_string(), entry);
    }

    /// Manually release a node from quarantine.
    pub async fn release_node(&self, name: &str) -> bool {
        let removed = self.quarantined.write().await.remove(name).is_some();
        if removed {
            info!(node = name, "node manually released from quarantine");
            // Clear failure tracker so it gets a fresh start.
            self.trackers.write().await.remove(name);
        }
        removed
    }

    // ── Auto-quarantine ──────────────────────────────────────────────────

    /// Record a health-check failure for a node.
    ///
    /// If the node exceeds the failure threshold within the window, it is
    /// automatically quarantined. Returns `true` if the node was just quarantined.
    pub async fn record_failure(&self, name: &str) -> bool {
        // Already quarantined? Skip tracking.
        if self.is_quarantined(name).await {
            return false;
        }

        let now = Utc::now();
        let mut trackers = self.trackers.write().await;
        let tracker = trackers.entry(name.to_string()).or_default();
        tracker.record(now, self.policy.window);

        if tracker.count() >= self.policy.failure_threshold {
            // Auto-quarantine.
            let release_at = now
                + chrono::Duration::from_std(self.policy.default_quarantine_duration)
                    .unwrap_or(chrono::Duration::seconds(600));

            let reason = format!(
                "auto: {} failures in {}s window",
                tracker.count(),
                self.policy.window.as_secs()
            );

            warn!(
                node = name,
                failures = tracker.count(),
                window_secs = self.policy.window.as_secs(),
                "auto-quarantining node"
            );

            tracker.clear();

            // Need to drop trackers lock before acquiring quarantined lock to avoid
            // potential deadlocks (both are RwLock, but let's be explicit).
            drop(trackers);

            let entry = QuarantineEntry {
                node: name.to_string(),
                reason,
                quarantined_at: now,
                release_at,
                auto: true,
            };

            self.quarantined
                .write()
                .await
                .insert(name.to_string(), entry);

            return true;
        }

        false
    }

    /// Record a health-check success for a node. Clears failure history.
    pub async fn record_success(&self, name: &str) {
        let mut trackers = self.trackers.write().await;
        if let Some(tracker) = trackers.get_mut(name) {
            tracker.clear();
        }
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// Check if a node is currently quarantined (auto-releases expired entries).
    pub async fn is_quarantined(&self, name: &str) -> bool {
        // First check without write lock.
        {
            let map = self.quarantined.read().await;
            match map.get(name) {
                Some(entry) if !entry.is_expired() => return true,
                Some(_) => { /* expired — need to remove */ }
                None => return false,
            }
        }

        // Remove expired entry.
        let mut map = self.quarantined.write().await;
        if let Some(entry) = map.get(name) {
            if entry.is_expired() {
                info!(node = name, "quarantine expired, auto-releasing");
                map.remove(name);
                // Also clear tracker.
                self.trackers.write().await.remove(name);
                return false;
            }
            true
        } else {
            false
        }
    }

    /// List all currently quarantined nodes (pruning expired ones).
    pub async fn list_quarantined(&self) -> Vec<QuarantineEntry> {
        self.prune_expired().await;
        let map = self.quarantined.read().await;
        map.values().cloned().collect()
    }

    /// Number of currently quarantined nodes.
    pub async fn quarantined_count(&self) -> usize {
        self.prune_expired().await;
        self.quarantined.read().await.len()
    }

    /// Get quarantine info for a specific node.
    pub async fn get_quarantine_info(&self, name: &str) -> Option<QuarantineEntry> {
        if self.is_quarantined(name).await {
            self.quarantined.read().await.get(name).cloned()
        } else {
            None
        }
    }

    // ── Internal ─────────────────────────────────────────────────────────

    /// Remove all expired quarantine entries.
    async fn prune_expired(&self) {
        let mut map = self.quarantined.write().await;
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, entry)| entry.is_expired())
            .map(|(name, _)| name.clone())
            .collect();

        for name in &expired {
            info!(node = name, "quarantine expired, auto-releasing");
            map.remove(name);
        }
        drop(map);

        if !expired.is_empty() {
            let mut trackers = self.trackers.write().await;
            for name in &expired {
                trackers.remove(name);
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_policy() -> QuarantinePolicy {
        QuarantinePolicy {
            failure_threshold: 3,
            window: Duration::from_secs(60),
            default_quarantine_duration: Duration::from_millis(100), // fast for tests
        }
    }

    #[tokio::test]
    async fn test_manual_quarantine_and_release() {
        let q = NodeQuarantine::new(fast_policy());

        assert!(!q.is_quarantined("taylor").await);

        q.quarantine_node("taylor", "testing", Duration::from_secs(60))
            .await;
        assert!(q.is_quarantined("taylor").await);

        let entries = q.list_quarantined().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node, "taylor");
        assert!(!entries[0].auto);

        assert!(q.release_node("taylor").await);
        assert!(!q.is_quarantined("taylor").await);
        assert!(q.list_quarantined().await.is_empty());
    }

    #[tokio::test]
    async fn test_release_nonexistent_returns_false() {
        let q = NodeQuarantine::new(fast_policy());
        assert!(!q.release_node("ghost").await);
    }

    #[tokio::test]
    async fn test_auto_quarantine_on_threshold() {
        let q = NodeQuarantine::new(fast_policy());

        assert!(!q.record_failure("james").await);
        assert!(!q.record_failure("james").await);
        // 3rd failure → auto-quarantine
        assert!(q.record_failure("james").await);

        assert!(q.is_quarantined("james").await);
        let info = q.get_quarantine_info("james").await.unwrap();
        assert!(info.auto);
        assert!(info.reason.contains("auto:"));
    }

    #[tokio::test]
    async fn test_success_clears_failure_tracker() {
        let q = NodeQuarantine::new(fast_policy());

        q.record_failure("marcus").await;
        q.record_failure("marcus").await;
        q.record_success("marcus").await; // reset

        // Should not quarantine after 1 more failure (only 1 since reset)
        assert!(!q.record_failure("marcus").await);
        assert!(!q.is_quarantined("marcus").await);
    }

    #[tokio::test]
    async fn test_auto_release_after_duration() {
        let q = NodeQuarantine::new(fast_policy());

        q.quarantine_node("sophie", "flaky", Duration::from_millis(50))
            .await;
        assert!(q.is_quarantined("sophie").await);

        tokio::time::sleep(Duration::from_millis(60)).await;

        // Should auto-release
        assert!(!q.is_quarantined("sophie").await);
    }

    #[tokio::test]
    async fn test_list_prunes_expired() {
        let q = NodeQuarantine::new(fast_policy());

        q.quarantine_node("a", "test", Duration::from_millis(30))
            .await;
        q.quarantine_node("b", "test", Duration::from_secs(300))
            .await;

        assert_eq!(q.list_quarantined().await.len(), 2);

        tokio::time::sleep(Duration::from_millis(40)).await;

        let remaining = q.list_quarantined().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].node, "b");
    }

    #[tokio::test]
    async fn test_quarantined_count() {
        let q = NodeQuarantine::new(fast_policy());

        q.quarantine_node("a", "test", Duration::from_secs(60))
            .await;
        q.quarantine_node("b", "test", Duration::from_secs(60))
            .await;
        assert_eq!(q.quarantined_count().await, 2);
    }

    #[tokio::test]
    async fn test_no_double_quarantine_tracking() {
        let q = NodeQuarantine::new(fast_policy());

        // Manually quarantine first
        q.quarantine_node("x", "manual", Duration::from_secs(60))
            .await;

        // Failures shouldn't re-quarantine (already quarantined)
        assert!(!q.record_failure("x").await);
        assert!(!q.record_failure("x").await);
        assert!(!q.record_failure("x").await);
    }

    #[tokio::test]
    async fn test_remaining_duration() {
        let entry = QuarantineEntry {
            node: "test".to_string(),
            reason: "test".to_string(),
            quarantined_at: Utc::now(),
            release_at: Utc::now() + chrono::Duration::seconds(60),
            auto: false,
        };
        assert!(entry.remaining() > Duration::ZERO);
        assert!(entry.remaining() <= Duration::from_secs(60));

        let expired = QuarantineEntry {
            node: "test".to_string(),
            reason: "test".to_string(),
            quarantined_at: Utc::now() - chrono::Duration::seconds(120),
            release_at: Utc::now() - chrono::Duration::seconds(60),
            auto: false,
        };
        assert_eq!(expired.remaining(), Duration::ZERO);
        assert!(expired.is_expired());
    }
}
