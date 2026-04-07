//! Rule-based alerting — node down, model unavailable, high load, custom rules.
//!
//! The [`AlertEngine`] evaluates [`AlertRule`]s against current fleet state
//! (metrics, events, node status) and produces [`Alert`]s. Alerts can be
//! consumed by notification systems (ff-notify) or the dashboard.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
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
    active_alerts: Arc<DashMap<String, Alert>>,
    /// History of all alerts (bounded).
    history: Arc<DashMap<Uuid, Alert>>,
}

impl AlertEngine {
    /// Create a new alert engine with the given rules.
    pub fn new(rules: Vec<AlertRule>) -> Self {
        Self {
            rules,
            active_alerts: Arc::new(DashMap::new()),
            history: Arc::new(DashMap::new()),
        }
    }

    /// Create an engine with sensible default rules for ForgeFleet.
    pub fn with_defaults() -> Self {
        Self::new(vec![
            AlertRule::node_down(60),
            AlertRule::model_down(),
            AlertRule::high_cpu(90.0),
            AlertRule::high_memory(0.9),
            AlertRule::high_error_rate(0.1),
        ])
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
                            && !targets.contains(&nm.node_name)
                        {
                            continue;
                        }
                        if nm.cpu_percent >= *threshold_percent {
                            let dedup_key = format!("{}:{}", rule.id, nm.node_name);
                            if !self.active_alerts.contains_key(&dedup_key) {
                                let alert = Alert {
                                    id: Uuid::new_v4(),
                                    rule_id: rule.id.clone(),
                                    severity: rule.severity,
                                    message: format!(
                                        "{}: CPU at {:.1}% on {}",
                                        rule.name, nm.cpu_percent, nm.node_name
                                    ),
                                    node: Some(nm.node_name.clone()),
                                    model_id: None,
                                    fired_at: Utc::now(),
                                    resolved_at: None,
                                    acknowledged: false,
                                };
                                self.active_alerts.insert(dedup_key, alert.clone());
                                self.history.insert(alert.id, alert.clone());
                                new_alerts.push(alert);
                            }
                        }
                    }
                }
                AlertCondition::HighMemory { threshold_ratio } => {
                    for entry in metrics.node_metrics.iter() {
                        let nm = entry.value();
                        if let Some(ref targets) = rule.target_nodes
                            && !targets.contains(&nm.node_name)
                        {
                            continue;
                        }
                        let util = nm.memory_utilization();
                        if util >= *threshold_ratio {
                            let dedup_key = format!("{}:{}", rule.id, nm.node_name);
                            if !self.active_alerts.contains_key(&dedup_key) {
                                let alert = Alert {
                                    id: Uuid::new_v4(),
                                    rule_id: rule.id.clone(),
                                    severity: rule.severity,
                                    message: format!(
                                        "{}: memory at {:.0}% on {}",
                                        rule.name,
                                        util * 100.0,
                                        nm.node_name
                                    ),
                                    node: Some(nm.node_name.clone()),
                                    model_id: None,
                                    fired_at: Utc::now(),
                                    resolved_at: None,
                                    acknowledged: false,
                                };
                                self.active_alerts.insert(dedup_key, alert.clone());
                                self.history.insert(alert.id, alert.clone());
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
                            let dedup_key = format!("{}:{}@{}", rule.id, mm.model_id, mm.node_name);
                            if !self.active_alerts.contains_key(&dedup_key) {
                                let alert = Alert {
                                    id: Uuid::new_v4(),
                                    rule_id: rule.id.clone(),
                                    severity: rule.severity,
                                    message: format!(
                                        "{}: error rate {:.1}% for {} on {}",
                                        rule.name,
                                        mm.error_rate() * 100.0,
                                        mm.model_id,
                                        mm.node_name
                                    ),
                                    node: Some(mm.node_name.clone()),
                                    model_id: Some(mm.model_id.clone()),
                                    fired_at: Utc::now(),
                                    resolved_at: None,
                                    acknowledged: false,
                                };
                                self.active_alerts.insert(dedup_key, alert.clone());
                                self.history.insert(alert.id, alert.clone());
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
        };
        self.active_alerts.insert(dedup_key, alert.clone());
        self.history.insert(alert.id, alert.clone());
        alert
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
            if let Some((_, mut alert)) = self.active_alerts.remove(&key) {
                alert.resolve();
                self.history.insert(alert.id, alert);
            }
        }
    }

    /// Get all currently active (unresolved) alerts.
    pub fn active_alerts(&self) -> Vec<Alert> {
        self.active_alerts
            .iter()
            .map(|e| e.value().clone())
            .collect()
    }

    /// Get active alerts with a minimum severity.
    pub fn active_alerts_min_severity(&self, min: AlertSeverity) -> Vec<Alert> {
        self.active_alerts
            .iter()
            .filter(|e| e.value().severity >= min)
            .map(|e| e.value().clone())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsCollector, NodeMetrics};
    use chrono::Utc;

    fn hot_node(name: &str, cpu: f64) -> NodeMetrics {
        NodeMetrics {
            node_name: name.to_string(),
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
}
