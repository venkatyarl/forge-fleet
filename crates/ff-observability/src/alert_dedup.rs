//! Metric-and-node aware alert deduplication.
//!
//! Repeated alerts for the same metric and node are collapsed into a single
//! [`Alert`] whose [`count`](Alert::count) tracks how many times the alert has
//! fired within the deduplication window.
//!
//! This is intentionally separate from the generic TTL suppression in
//! [`crate::alerts`] and the rule-based [`crate::alerting::AlertEngine`]. It
//! provides a reusable tracker that downstream components can use when they
//! already know the metric/node an alert belongs to.

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

use crate::alerting::Alert;

const DEFAULT_DEDUP_WINDOW_SECS: i64 = 300;

/// Key used to group repeated alerts.
///
/// Two alerts with the same `metric` and `node` are considered duplicates.
/// A `None` node represents a fleet-wide or global alert.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AlertDedupKey {
    pub metric: String,
    pub node: Option<String>,
}

impl AlertDedupKey {
    /// Create a key from a metric name and optional node.
    pub fn new(metric: impl Into<String>, node: Option<String>) -> Self {
        Self {
            metric: metric.into(),
            node,
        }
    }

    /// Render the key as `metric:node` (or `metric:` when node is absent).
    ///
    /// This is useful for prefix-based resolution.
    pub fn as_string(&self) -> String {
        format!("{}:{}", self.metric, self.node.as_deref().unwrap_or("*"))
    }
}

impl fmt::Display for AlertDedupKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_string())
    }
}

#[derive(Debug, Clone)]
struct DedupEntry {
    alert: Alert,
    last_seen_at: DateTime<Utc>,
}

/// Tracks active alerts and collapses repeats by metric and node.
#[derive(Debug, Clone)]
pub struct AlertDedupTracker {
    active: Arc<DashMap<AlertDedupKey, DedupEntry>>,
    window: chrono::Duration,
}

impl AlertDedupTracker {
    /// Create a tracker with the given deduplication window.
    pub fn new(window: chrono::Duration) -> Self {
        Self {
            active: Arc::new(DashMap::new()),
            window,
        }
    }

    /// Create a tracker with the default 5-minute deduplication window.
    pub fn with_default_window() -> Self {
        Self::new(chrono::Duration::seconds(DEFAULT_DEDUP_WINDOW_SECS))
    }

    /// Record an alert, collapsing it with an existing alert for the same
    /// metric/node when the previous occurrence is still inside the window.
    ///
    /// The first value returned is the alert to emit (with an up-to-date
    /// [`count`](Alert::count)). The second value is `true` only when this is a
    /// brand-new deduplicated alert.
    pub fn record(&self, alert: Alert) -> (Alert, bool) {
        self.record_by(alert.rule_id.clone(), alert.node.clone(), alert)
    }

    /// Record an alert using an explicit metric/node key.
    ///
    /// This is useful when the alert's [`rule_id`](Alert::rule_id) or
    /// [`node`](Alert::node) do not match the desired deduplication grouping.
    pub fn record_by(
        &self,
        metric: impl Into<String>,
        node: Option<String>,
        alert: Alert,
    ) -> (Alert, bool) {
        let now = Utc::now();
        let key = AlertDedupKey::new(metric, node);

        match self.active.entry(key) {
            Entry::Occupied(mut entry) if now - entry.get().last_seen_at <= self.window => {
                let entry = entry.get_mut();
                entry.alert.count = entry.alert.count.saturating_add(1);
                entry.last_seen_at = now;
                (entry.alert.clone(), false)
            }
            Entry::Occupied(mut entry) => {
                entry.insert(DedupEntry {
                    alert: alert.clone(),
                    last_seen_at: now,
                });
                (alert, true)
            }
            Entry::Vacant(entry) => {
                entry.insert(DedupEntry {
                    alert: alert.clone(),
                    last_seen_at: now,
                });
                (alert, true)
            }
        }
    }

    /// Return all currently active deduplicated alerts.
    pub fn active_alerts(&self) -> Vec<Alert> {
        self.active
            .iter()
            .map(|e| e.value().alert.clone())
            .collect()
    }

    /// Return the number of active deduplicated alerts.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Resolve the active alert matching `metric` and `node`, if any.
    ///
    /// Returns `true` if an active alert was resolved.
    pub fn resolve(&self, metric: &str, node: Option<&str>) -> bool {
        let key = AlertDedupKey::new(metric, node.map(String::from));
        self.active
            .remove(&key)
            .map(|(_, mut entry)| {
                entry.alert.resolve();
                entry.alert
            })
            .is_some()
    }

    /// Resolve every active alert whose string-form key starts with `prefix`.
    pub fn resolve_by_prefix(&self, prefix: &str) {
        let keys: Vec<AlertDedupKey> = self
            .active
            .iter()
            .filter(|e| e.key().as_string().starts_with(prefix))
            .map(|e| e.key().clone())
            .collect();

        for key in keys {
            if let Some((_, mut entry)) = self.active.remove(&key) {
                entry.alert.resolve();
            }
        }
    }

    /// Change the deduplication window.
    ///
    /// Existing active alerts keep their current window; only new occurrences
    /// are compared against the updated window.
    pub fn set_window(&mut self, window: chrono::Duration) {
        self.window = window;
    }

    /// Current deduplication window.
    pub fn window(&self) -> chrono::Duration {
        self.window
    }
}

impl Default for AlertDedupTracker {
    fn default() -> Self {
        Self::with_default_window()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerting::AlertSeverity;

    fn sample_alert(rule_id: &str, node: Option<&str>, message: &str) -> Alert {
        Alert {
            id: uuid::Uuid::new_v4(),
            rule_id: rule_id.to_string(),
            severity: AlertSeverity::Warning,
            message: message.to_string(),
            node: node.map(String::from),
            model_id: None,
            fired_at: Utc::now(),
            resolved_at: None,
            acknowledged: false,
            count: 1,
        }
    }

    #[test]
    fn collapses_repeated_alerts_by_metric_and_node() {
        let tracker = AlertDedupTracker::with_default_window();

        let first = tracker.record(sample_alert("high_cpu", Some("node-a"), "cpu high"));
        assert!(first.1);
        assert_eq!(first.0.count, 1);

        let repeated = tracker.record(sample_alert("high_cpu", Some("node-a"), "cpu still high"));
        assert!(!repeated.1);
        assert_eq!(repeated.0.count, 2);
        assert_eq!(repeated.0.id, first.0.id);

        let again = tracker.record(sample_alert("high_cpu", Some("node-a"), "cpu very high"));
        assert!(!again.1);
        assert_eq!(again.0.count, 3);

        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn treats_different_nodes_and_metrics_as_separate_alerts() {
        let tracker = AlertDedupTracker::with_default_window();

        tracker.record(sample_alert("high_cpu", Some("node-a"), "cpu high"));
        tracker.record(sample_alert("high_cpu", Some("node-b"), "cpu high"));
        tracker.record(sample_alert("high_memory", Some("node-a"), "memory high"));
        tracker.record(sample_alert("high_memory", None, "fleet memory high"));

        assert_eq!(tracker.active_count(), 4);
    }

    #[test]
    fn starts_a_new_alert_after_window_expires() {
        let tracker = AlertDedupTracker::new(chrono::Duration::zero());

        let first = tracker.record(sample_alert("high_cpu", Some("node-a"), "cpu high"));
        std::thread::sleep(std::time::Duration::from_millis(1));
        let second = tracker.record(sample_alert("high_cpu", Some("node-a"), "cpu high"));

        assert_ne!(second.0.id, first.0.id);
        assert_eq!(second.0.count, 1);
        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn resolves_exact_and_prefix_matches() {
        let tracker = AlertDedupTracker::with_default_window();

        tracker.record(sample_alert("high_cpu", Some("node-a"), "cpu high"));
        tracker.record(sample_alert("high_cpu", Some("node-b"), "cpu high"));
        tracker.record(sample_alert("high_memory", Some("node-a"), "memory high"));

        assert!(tracker.resolve("high_cpu", Some("node-a")));
        assert!(!tracker.resolve("high_cpu", Some("node-a")));

        tracker.resolve_by_prefix("high_cpu:");
        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn record_by_uses_explicit_key() {
        let tracker = AlertDedupTracker::with_default_window();

        let alert = sample_alert("custom_rule", Some("node-a"), "boom");
        let (emitted, is_new) = tracker.record_by("disk_full", Some("node-a".to_string()), alert);
        assert!(is_new);
        assert_eq!(emitted.rule_id, "custom_rule");

        let repeated = tracker.record_by(
            "disk_full",
            Some("node-a".to_string()),
            sample_alert("custom_rule", Some("node-a"), "boom again"),
        );
        assert!(!repeated.1);
        assert_eq!(repeated.0.count, 2);
    }
}
