//! Rule-based alerting — node down, model unavailable, high load, custom rules.
//!
//! The [`AlertEngine`] evaluates [`AlertRule`]s against current fleet state
//! (metrics, events, node status) and produces [`Alert`]s. Alerts can be
//! consumed by notification systems (ff-notify) or the dashboard.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use ff_core::config::AlertDeduplicationConfig;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::metrics::MetricsCollector;

// ─── Alert Severity ──────────────────────────────────────────────────────────

/// How critical an alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    /// Informational — notable but not actionable.
    Info,
    /// Warning — may require attention soon.
    Warning,
    /// Critical — immediate attention required.
    Critical,
}

impl std::fmt::Display for AlertSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Info => write!(f, "INFO"),
            Self::Warning => write!(f, "WARNING"),
            Self::Critical => write!(f, "CRITICAL"),
        }
    }
}

// ─── Alert ───────────────────────────────────────────────────────────────────

/// A fired alert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// Unique alert instance ID.
    pub id: Uuid,
    /// Which rule produced this alert.
    pub rule_id: String,
    /// Severity.
    pub severity: AlertSeverity,
    /// Human-readable message.
    pub message: String,
    /// Node that triggered the alert, if applicable.
    pub node: Option<String>,
    /// Model that triggered the alert, if applicable.
    pub model_id: Option<String>,
    /// When the alert fired.
    pub fired_at: DateTime<Utc>,
    /// When the alert was resolved (None = still active).
    pub resolved_at: Option<DateTime<Utc>>,
    /// Whether this alert has been acknowledged by an operator.
    pub acknowledged: bool,
    /// Number of occurrences aggregated into this alert.
    #[serde(default = "default_alert_count")]
    pub count: u64,
}

const fn default_alert_count() -> u64 {
    1
}

impl Alert {
    /// Whether this alert is still active (not resolved).
    pub fn is_active(&self) -> bool {
        self.resolved_at.is_none()
    }

    /// Resolve this alert.
    pub fn resolve(&mut self) {
        self.resolved_at = Some(Utc::now());
    }

    /// Acknowledge this alert.
    pub fn acknowledge(&mut self) {
        self.acknowledged = true;
    }
}

// ─── Alert Rule ──────────────────────────────────────────────────────────────

/// The condition type for an alert rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertCondition {
    /// Node has not sent a heartbeat within `timeout_secs`.
    NodeDown { timeout_secs: u64 },
    /// A model endpoint is unreachable or unhealthy.
    ModelDown,
    /// Node CPU exceeds threshold.
    HighCpu { threshold_percent: f64 },
    /// Node memory utilization exceeds threshold (0.0–1.0).
    HighMemory { threshold_ratio: f64 },
    /// Model error rate exceeds threshold (0.0–1.0).
    HighErrorRate { threshold_ratio: f64 },
    /// Model p95 latency exceeds threshold.
    HighLatency { threshold_ms: f64 },
}

/// A declarative alert rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    /// Unique rule ID.
    pub id: String,
    /// Human-readable rule name.
    pub name: String,
    /// Severity when this rule fires.
    pub severity: AlertSeverity,
    /// The condition to evaluate.
    pub condition: AlertCondition,
    /// Optional: only apply to specific nodes.
    pub target_nodes: Option<Vec<String>>,
    /// Optional: only apply to specific models.
    pub target_models: Option<Vec<String>>,
    /// Whether this rule is enabled.
    pub enabled: bool,
}

impl AlertRule {
    /// Create a standard "node down" rule.
    pub fn node_down(timeout_secs: u64) -> Self {
        Self {
            id: "node_down".to_string(),
            name: "Node Down".to_string(),
            severity: AlertSeverity::Critical,
            condition: AlertCondition::NodeDown { timeout_secs },
            target_nodes: None,
            target_models: None,
            enabled: true,
        }
    }

    /// Create a standard "model down" rule.
    pub fn model_down() -> Self {
        Self {
            id: "model_down".to_string(),
            name: "Model Down".to_string(),
            severity: AlertSeverity::Critical,
            condition: AlertCondition::ModelDown,
            target_nodes: None,
            target_models: None,
            enabled: true,
        }
    }

    /// Create a standard "high CPU" rule.
    pub fn high_cpu(threshold_percent: f64) -> Self {
        Self {
            id: "high_cpu".to_string(),
            name: "High CPU Usage".to_string(),
            severity: AlertSeverity::Warning,
            condition: AlertCondition::HighCpu { threshold_percent },
            target_nodes: None,
            target_models: None,
            enabled: true,
        }
    }

    /// Create a standard "high memory" rule.
    pub fn high_memory(threshold_ratio: f64) -> Self {
        Self {
            id: "high_memory".to_string(),
            name: "High Memory Usage".to_string(),
            severity: AlertSeverity::Warning,
            condition: AlertCondition::HighMemory { threshold_ratio },
            target_nodes: None,
            target_models: None,
            enabled: true,
        }
    }

    /// Create a standard "high error rate" rule.
    pub fn high_error_rate(threshold_ratio: f64) -> Self {
        Self {
            id: "high_error_rate".to_string(),
            name: "High Model Error Rate".to_string(),
            severity: AlertSeverity::Warning,
            condition: AlertCondition::HighErrorRate { threshold_ratio },
            target_nodes: None,
            target_models: None,
            enabled: true,
        }
    }
}

// ─── Alert Engine ────────────────────────────────────────────────────────────

/// The alert evaluation engine.
///
/// Holds configured rules and active alerts. Call [`evaluate`] periodically
/// (e.g. every 15s) to check rules against current metrics.
#[derive(Debug, Clone)]
pub struct AlertEngine {
    /// Configured alert rules.
    rules: Vec<AlertRule>,
    /// Currently active (unfired or unresolved) alerts, keyed by a dedup key.
    active_alerts: Arc<DashMap<String, ActiveAlert>>,
    /// History of all alerts (bounded).
    history: Arc<DashMap<Uuid, Alert>>,
    /// How long repeated alerts are aggregated.
    dedup_window: chrono::Duration,
    /// Occurrence at which matching alerts begin to be collapsed.
    dedup_threshold_count: u64,
}

#[derive(Debug, Clone)]
struct ActiveAlert {
    alert: Alert,
    last_seen_at: DateTime<Utc>,
}

const DEFAULT_DEDUP_WINDOW_SECS: i64 = 300;

impl AlertEngine {
    /// Create a new alert engine with the given rules.
    pub fn new(rules: Vec<AlertRule>) -> Self {
        Self::with_dedup_window(rules, chrono::Duration::seconds(DEFAULT_DEDUP_WINDOW_SECS))
    }

    /// Create an alert engine with a custom deduplication window.
    pub fn with_dedup_window(rules: Vec<AlertRule>, dedup_window: chrono::Duration) -> Self {
        Self {
            rules,
            active_alerts: Arc::new(DashMap::new()),
            history: Arc::new(DashMap::new()),
            dedup_window,
            dedup_threshold_count: 2,
        }
    }

    /// Create an alert engine using the deduplication settings from `fleet.toml`.
    pub fn with_deduplication_config(
        rules: Vec<AlertRule>,
        config: &AlertDeduplicationConfig,
    ) -> Self {
        let window_secs = i64::try_from(config.window_secs).unwrap_or(i64::MAX);
        let mut engine = Self::with_dedup_window(rules, chrono::Duration::seconds(window_secs));
        engine.dedup_threshold_count = config.threshold_count.max(2);
        engine
    }

    /// Create an engine with sensible default rules for ForgeFleet.
    ///
    /// When called from a tokio runtime context this also spawns the history
    /// pruner so the `history` DashMap doesn't grow unbounded. Tests that
    /// construct an engine outside of a runtime get a no-pruner instance.
    pub fn with_defaults() -> Self {
        let engine = Self::new(vec![
            AlertRule::node_down(60),
            AlertRule::model_down(),
            AlertRule::high_cpu(90.0),
            AlertRule::high_memory(0.9),
            AlertRule::high_error_rate(0.1),
        ]);
        if tokio::runtime::Handle::try_current().is_ok() {
            std::mem::drop(Self::spawn_history_pruner(
                engine.history_handle(),
                std::time::Duration::from_secs(300),
                chrono::Duration::days(7),
                10_000,
            ));
        }
        engine
    }

    /// Add a rule dynamically.
    pub fn add_rule(&mut self, rule: AlertRule) {
        self.rules.push(rule);
    }

    /// Evaluate all rules against current metrics.
    ///
    /// Returns newly fired alerts (alerts that transitioned from inactive to active).
    pub fn evaluate(&self, metrics: &MetricsCollector) -> Vec<Alert> {
        let mut new_alerts = Vec::new();

        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }

            match &rule.condition {
                AlertCondition::HighCpu { threshold_percent } => {
                    for entry in metrics.node_metrics.iter() {
                        let nm = entry.value();
                        if let Some(ref targets) = rule.target_nodes
                            && !targets.contains(&nm.worker_name)
                        {
                            continue;
                        }
                        if nm.cpu_percent >= *threshold_percent {
                            let dedup_key = format!("{}:{}", rule.id, nm.worker_name);
                            let alert = Alert {
                                id: Uuid::new_v4(),
                                rule_id: rule.id.clone(),
                                severity: rule.severity,
                                message: format!(
                                    "{}: CPU at {:.1}% on {}",
                                    rule.name, nm.cpu_percent, nm.worker_name
                                ),
                                node: Some(nm.worker_name.clone()),
                                model_id: None,
                                fired_at: Utc::now(),
                                resolved_at: None,
                                acknowledged: false,
                                count: 1,
                            };
                            let (alert, is_new) = self.record_alert(dedup_key, alert);
                            if is_new {
                                new_alerts.push(alert);
                            }
                        }
                    }
                }
                AlertCondition::HighMemory { threshold_ratio } => {
                    for entry in metrics.node_metrics.iter() {
                        let nm = entry.value();
                        if let Some(ref targets) = rule.target_nodes
                            && !targets.contains(&nm.worker_name)
                        {
                            continue;
                        }
                        let util = nm.memory_utilization();
                        if util >= *threshold_ratio {
                            let dedup_key = format!("{}:{}", rule.id, nm.worker_name);
                            let alert = Alert {
                                id: Uuid::new_v4(),
                                rule_id: rule.id.clone(),
                                severity: rule.severity,
                                message: format!(
                                    "{}: memory at {:.0}% on {}",
                                    rule.name,
                                    util * 100.0,
                                    nm.worker_name
                                ),
                                node: Some(nm.worker_name.clone()),
                                model_id: None,
                                fired_at: Utc::now(),
                                resolved_at: None,
                                acknowledged: false,
                                count: 1,
                            };
                            let (alert, is_new) = self.record_alert(dedup_key, alert);
                            if is_new {
                                new_alerts.push(alert);
                            }
                        }
                    }
                }
                AlertCondition::HighErrorRate { threshold_ratio } => {
                    for entry in metrics.model_metrics.iter() {
                        let mm = entry.value();
                        if let Some(ref targets) = rule.target_models
                            && !targets.contains(&mm.model_id)
                        {
                            continue;
                        }
                        if mm.error_rate() >= *threshold_ratio {
                            let dedup_key =
                                format!("{}:{}@{}", rule.id, mm.model_id, mm.worker_name);
                            let alert = Alert {
                                id: Uuid::new_v4(),
                                rule_id: rule.id.clone(),
                                severity: rule.severity,
                                message: format!(
                                    "{}: error rate {:.1}% for {} on {}",
                                    rule.name,
                                    mm.error_rate() * 100.0,
                                    mm.model_id,
                                    mm.worker_name
                                ),
                                node: Some(mm.worker_name.clone()),
                                model_id: Some(mm.model_id.clone()),
                                fired_at: Utc::now(),
                                resolved_at: None,
                                acknowledged: false,
                                count: 1,
                            };
                            let (alert, is_new) = self.record_alert(dedup_key, alert);
                            if is_new {
                                new_alerts.push(alert);
                            }
                        }
                    }
                }
                // NodeDown and ModelDown require external liveness checks
                // (from ff-discovery or ff-mesh heartbeats), not just metrics.
                // The engine exposes fire_alert() for those subsystems to call.
                AlertCondition::NodeDown { .. }
                | AlertCondition::ModelDown
                | AlertCondition::HighLatency { .. } => {}
            }
        }

        new_alerts
    }

    /// Manually fire an alert (e.g. from external health checks).
    pub fn fire_alert(
        &self,
        rule_id: &str,
        severity: AlertSeverity,
        message: String,
        node: Option<String>,
        model_id: Option<String>,
    ) -> Alert {
        let dedup_key = format!(
            "{}:{}:{}",
            rule_id,
            node.as_deref().unwrap_or("*"),
            model_id.as_deref().unwrap_or("*")
        );
        let alert = Alert {
            id: Uuid::new_v4(),
            rule_id: rule_id.to_string(),
            severity,
            message,
            node,
            model_id,
            fired_at: Utc::now(),
            resolved_at: None,
            acknowledged: false,
            count: 1,
        };
        self.record_alert(dedup_key, alert).0
    }

    fn record_alert(&self, dedup_key: String, alert: Alert) -> (Alert, bool) {
        let now = Utc::now();
        match self.active_alerts.entry(dedup_key) {
            Entry::Occupied(mut entry) if now - entry.get().last_seen_at <= self.dedup_window => {
                let active = entry.get_mut();
                active.alert.count = active.alert.count.saturating_add(1);
                active.last_seen_at = now;
                self.history.insert(active.alert.id, active.alert.clone());
                let should_emit = active.alert.count < self.dedup_threshold_count;
                (active.alert.clone(), should_emit)
            }
            Entry::Occupied(mut entry) => {
                let active = ActiveAlert {
                    alert: alert.clone(),
                    last_seen_at: now,
                };
                entry.insert(active);
                self.history.insert(alert.id, alert.clone());
                (alert, true)
            }
            Entry::Vacant(entry) => {
                entry.insert(ActiveAlert {
                    alert: alert.clone(),
                    last_seen_at: now,
                });
                self.history.insert(alert.id, alert.clone());
                (alert, true)
            }
        }
    }

    /// Resolve all active alerts matching a dedup key prefix.
    pub fn resolve_by_prefix(&self, prefix: &str) {
        let keys: Vec<String> = self
            .active_alerts
            .iter()
            .filter(|e| e.key().starts_with(prefix))
            .map(|e| e.key().clone())
            .collect();

        for key in keys {
            if let Some((_, mut active)) = self.active_alerts.remove(&key) {
                active.alert.resolve();
                self.history.insert(active.alert.id, active.alert);
            }
        }
    }

    /// Get all currently active (unresolved) alerts.
    pub fn active_alerts(&self) -> Vec<Alert> {
        self.active_alerts
            .iter()
            .map(|e| e.value().alert.clone())
            .collect()
    }

    /// Get active alerts with a minimum severity.
    pub fn active_alerts_min_severity(&self, min: AlertSeverity) -> Vec<Alert> {
        self.active_alerts
            .iter()
            .filter(|e| e.value().alert.severity >= min)
            .map(|e| e.value().alert.clone())
            .collect()
    }

    /// Get alert history.
    pub fn alert_history(&self) -> Vec<Alert> {
        self.history.iter().map(|e| e.value().clone()).collect()
    }

    /// Count of active alerts.
    pub fn active_count(&self) -> usize {
        self.active_alerts.len()
    }

    /// Drop alerts from history older than `max_age`. Also caps total entries at
    /// `max_entries` (drops oldest by `fired_at` once the cap is exceeded).
    /// Returns the number of entries removed.
    pub fn prune_history(&self, max_age: chrono::Duration, max_entries: usize) -> usize {
        prune_history_map(&self.history, max_age, max_entries)
    }

    /// Clone the internal history `Arc` so a background pruner task can
    /// operate on it without needing to hold a reference to the engine itself.
    pub fn history_handle(&self) -> Arc<DashMap<Uuid, Alert>> {
        Arc::clone(&self.history)
    }

    /// Spawn a background task that periodically prunes the alert history
    /// DashMap. Pass `engine.history_handle()` as the first argument.
    ///
    /// The history grows on every fire_alert / resolve transition; without
    /// this task it accumulates for the lifetime of the daemon.
    pub fn spawn_history_pruner(
        history: Arc<DashMap<Uuid, Alert>>,
        interval: std::time::Duration,
        max_age: chrono::Duration,
        max_entries: usize,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Skip the immediate first tick so we don't churn on startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let removed = prune_history_map(&history, max_age, max_entries);
                if removed > 0 {
                    tracing::debug!(removed, "pruned alert history");
                }
            }
        })
    }
}

/// Shared pruning logic — drops entries older than `max_age`, then enforces a
/// hard cap of `max_entries` by dropping the oldest by `fired_at`.
fn prune_history_map(
    history: &DashMap<Uuid, Alert>,
    max_age: chrono::Duration,
    max_entries: usize,
) -> usize {
    let cutoff = Utc::now() - max_age;
    let mut removed = 0;
    let stale: Vec<Uuid> = history
        .iter()
        .filter(|e| e.value().fired_at < cutoff)
        .map(|e| *e.key())
        .collect();
    for id in stale {
        if history.remove(&id).is_some() {
            removed += 1;
        }
    }
    if history.len() > max_entries {
        let mut by_age: Vec<(Uuid, chrono::DateTime<Utc>)> = history
            .iter()
            .map(|e| (*e.key(), e.value().fired_at))
            .collect();
        by_age.sort_by_key(|(_, t)| *t);
        let overflow = history.len() - max_entries;
        for (id, _) in by_age.into_iter().take(overflow) {
            if history.remove(&id).is_some() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsCollector, NodeMetrics};
    use chrono::Utc;

    fn hot_node(name: &str, cpu: f64) -> NodeMetrics {
        NodeMetrics {
            worker_name: name.to_string(),
            cpu_percent: cpu,
            memory_used_gib: 10.0,
            memory_total_gib: 64.0,
            gpu_percent: None,
            gpu_memory_used_gib: None,
            disk_percent: 50.0,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
            active_inference_count: 1,
            load_avg_1m: cpu / 10.0,
            sampled_at: Utc::now(),
        }
    }

    #[test]
    fn test_default_rules() {
        let engine = AlertEngine::with_defaults();
        assert_eq!(engine.rules.len(), 5);
    }

    #[test]
    fn test_high_cpu_alert() {
        let engine = AlertEngine::new(vec![AlertRule::high_cpu(90.0)]);
        let metrics = MetricsCollector::new();
        metrics.record_node(hot_node("taylor", 95.0));
        metrics.record_node(hot_node("james", 50.0));

        let alerts = engine.evaluate(&metrics);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("taylor"));
        assert_eq!(engine.active_count(), 1);

        // Evaluating again should NOT duplicate.
        let alerts2 = engine.evaluate(&metrics);
        assert_eq!(alerts2.len(), 0);
        assert_eq!(engine.active_count(), 1);
        assert_eq!(engine.active_alerts()[0].count, 2);
    }

    #[test]
    fn test_resolve_alerts() {
        let engine = AlertEngine::new(vec![AlertRule::high_cpu(90.0)]);
        let metrics = MetricsCollector::new();
        metrics.record_node(hot_node("taylor", 95.0));

        engine.evaluate(&metrics);
        assert_eq!(engine.active_count(), 1);

        engine.resolve_by_prefix("high_cpu:taylor");
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn test_fire_manual_alert() {
        let engine = AlertEngine::with_defaults();
        let alert = engine.fire_alert(
            "node_down",
            AlertSeverity::Critical,
            "Node james is unreachable".into(),
            Some("james".into()),
            None,
        );
        assert!(alert.is_active());
        assert_eq!(engine.active_count(), 1);
    }

    #[test]
    fn test_repeated_manual_alerts_are_aggregated_by_metric_and_node() {
        let engine = AlertEngine::new(Vec::new());

        let first = engine.fire_alert(
            "node_down",
            AlertSeverity::Critical,
            "Node james is unreachable".into(),
            Some("james".into()),
            None,
        );
        let repeated = engine.fire_alert(
            "node_down",
            AlertSeverity::Critical,
            "Node james is unreachable".into(),
            Some("james".into()),
            None,
        );
        engine.fire_alert(
            "node_down",
            AlertSeverity::Critical,
            "Node taylor is unreachable".into(),
            Some("taylor".into()),
            None,
        );
        engine.fire_alert(
            "high_cpu",
            AlertSeverity::Warning,
            "CPU is high on james".into(),
            Some("james".into()),
            None,
        );

        assert_eq!(repeated.id, first.id);
        assert_eq!(repeated.count, 2);
        assert_eq!(engine.active_count(), 3);
        let james_node_down = engine
            .active_alerts()
            .into_iter()
            .find(|alert| alert.id == first.id)
            .expect("the deduplicated alert should remain active");
        assert_eq!(james_node_down.count, 2);
        let history = engine.alert_history();
        assert_eq!(history.len(), 3);
        assert_eq!(
            history
                .iter()
                .find(|alert| alert.id == first.id)
                .expect("history should update the collapsed alert")
                .count,
            2
        );
    }

    #[test]
    fn test_new_alert_fires_while_existing_alert_is_deduplicated() {
        let engine = AlertEngine::new(vec![AlertRule::high_cpu(90.0)]);
        let metrics = MetricsCollector::new();
        metrics.record_node(hot_node("james", 95.0));

        let first = engine.evaluate(&metrics);
        assert_eq!(first.len(), 1);

        metrics.record_node(hot_node("taylor", 96.0));
        let new_alerts = engine.evaluate(&metrics);

        assert_eq!(new_alerts.len(), 1);
        assert_eq!(new_alerts[0].node.as_deref(), Some("taylor"));
        assert_eq!(new_alerts[0].count, 1);
        assert_eq!(engine.active_count(), 2);
        let james = engine
            .active_alerts()
            .into_iter()
            .find(|alert| alert.node.as_deref() == Some("james"))
            .expect("the original alert should remain active");
        assert_eq!(james.id, first[0].id);
        assert_eq!(james.count, 2);
    }

    #[test]
    fn test_alert_after_dedup_window_is_new() {
        let engine = AlertEngine::with_dedup_window(Vec::new(), chrono::Duration::zero());
        let first = engine.fire_alert(
            "node_down",
            AlertSeverity::Critical,
            "Node james is unreachable".into(),
            Some("james".into()),
            None,
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
        let second = engine.fire_alert(
            "node_down",
            AlertSeverity::Critical,
            "Node james is unreachable".into(),
            Some("james".into()),
            None,
        );

        assert_ne!(second.id, first.id);
        assert_eq!(second.count, 1);
        assert_eq!(engine.active_count(), 1);
        assert_eq!(engine.alert_history().len(), 2);
    }

    #[test]
    fn test_configured_threshold_delays_collapsing() {
        let config = AlertDeduplicationConfig {
            window_secs: 300,
            threshold_count: 3,
        };
        let engine =
            AlertEngine::with_deduplication_config(vec![AlertRule::high_cpu(90.0)], &config);
        let metrics = MetricsCollector::new();
        metrics.record_node(hot_node("taylor", 95.0));

        assert_eq!(engine.evaluate(&metrics).len(), 1);
        assert_eq!(engine.evaluate(&metrics).len(), 1);
        assert!(engine.evaluate(&metrics).is_empty());
        assert_eq!(engine.active_alerts()[0].count, 3);
    }
}
