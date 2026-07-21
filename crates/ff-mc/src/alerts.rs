//! Alert aggregation for Mission Control.
//!
//! Alerts produced by ff-mc are forwarded to the observability layer. To avoid
//! spamming that layer with identical repeated alerts, this module collapses
//! duplicates into a single [`AggregatedAlert`] that carries a `count`.

use chrono::{DateTime, Utc};
use ff_observability::{Alert, AlertSeverity};
use std::collections::HashMap;

/// Key used to decide whether two alerts are the same repeated occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AlertKey {
    rule_id: String,
    severity: AlertSeverity,
    node: Option<String>,
    model_id: Option<String>,
    message: String,
}

impl From<&Alert> for AlertKey {
    fn from(a: &Alert) -> Self {
        Self {
            rule_id: a.rule_id.clone(),
            severity: a.severity,
            node: a.node.clone(),
            model_id: a.model_id.clone(),
            message: a.message.clone(),
        }
    }
}

/// An alert bundled with the number of times it has been seen.
#[derive(Debug, Clone)]
pub struct AggregatedAlert {
    /// Representative alert instance.
    pub alert: Alert,
    /// How many times this alert has repeated (including the first occurrence).
    pub count: usize,
    /// Earliest `fired_at` among the collapsed alerts.
    pub first_fired_at: DateTime<Utc>,
    /// Latest `fired_at` among the collapsed alerts.
    pub last_fired_at: DateTime<Utc>,
}

impl AggregatedAlert {
    /// Human-readable message, appending the repeat count when > 1.
    pub fn message_with_count(&self) -> String {
        if self.count <= 1 {
            self.alert.message.clone()
        } else {
            format!("{} (x{})", self.alert.message, self.count)
        }
    }
}

/// Stateful alert aggregator.
///
/// Repeated calls to [`record`](Self::record) with alerts that share the same
/// rule, severity, node, model and message are collapsed into one
/// [`AggregatedAlert`] whose `count` is incremented.
#[derive(Debug, Default)]
pub struct AlertAggregator {
    index: HashMap<AlertKey, usize>,
    alerts: Vec<AggregatedAlert>,
}

impl AlertAggregator {
    /// Create an empty aggregator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a single alert.
    pub fn record(&mut self, alert: Alert) {
        let key = AlertKey::from(&alert);
        if let Some(idx) = self.index.get(&key).copied() {
            let bucket = &mut self.alerts[idx];
            bucket.count += 1;
            if alert.fired_at > bucket.last_fired_at {
                bucket.last_fired_at = alert.fired_at;
            }
        } else {
            let fired_at = alert.fired_at;
            let idx = self.alerts.len();
            self.alerts.push(AggregatedAlert {
                alert,
                count: 1,
                first_fired_at: fired_at,
                last_fired_at: fired_at,
            });
            self.index.insert(key, idx);
        }
    }

    /// Current aggregated alerts, in first-seen order.
    pub fn aggregated(&self) -> Vec<AggregatedAlert> {
        self.alerts.clone()
    }

    /// Drain and return all aggregated alerts, resetting the aggregator.
    pub fn flush(&mut self) -> Vec<AggregatedAlert> {
        self.index.clear();
        std::mem::take(&mut self.alerts)
    }

    /// Number of distinct aggregated alerts currently held.
    pub fn len(&self) -> usize {
        self.alerts.len()
    }

    /// Whether the aggregator is empty.
    pub fn is_empty(&self) -> bool {
        self.alerts.is_empty()
    }
}

/// Aggregate a batch of alerts in one shot.
///
/// Alerts are grouped by `(rule_id, severity, node, model_id, message)` and
/// returned in first-seen order.
pub fn aggregate_alerts(alerts: impl IntoIterator<Item = Alert>) -> Vec<AggregatedAlert> {
    let mut aggregator = AlertAggregator::new();
    for alert in alerts {
        aggregator.record(alert);
    }
    aggregator.aggregated()
}

/// Aggregate a batch of alerts and pass each [`AggregatedAlert`] to `sink`.
///
/// `sink` is the integration point for the observability layer: callers can
/// forward the de-duplicated alerts to `ff_observability` logging, events, or
/// the alerting engine.
pub fn send_aggregated(
    alerts: impl IntoIterator<Item = Alert>,
    mut sink: impl FnMut(&AggregatedAlert),
) {
    for aggregated in aggregate_alerts(alerts) {
        sink(&aggregated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ff_observability::AlertSeverity;
    use uuid::Uuid;

    fn make_alert(
        rule_id: &str,
        severity: AlertSeverity,
        node: Option<&str>,
        model_id: Option<&str>,
        message: &str,
    ) -> Alert {
        Alert {
            id: Uuid::new_v4(),
            last_sent: None,
            rule_id: rule_id.into(),
            severity,
            message: message.into(),
            node: node.map(Into::into),
            model_id: model_id.map(Into::into),
            fired_at: Utc::now(),
            resolved_at: None,
            acknowledged: false,
            count: 1,
        }
    }

    #[test]
    fn repeated_alerts_are_collapsed_with_count() {
        let alerts = vec![
            make_alert(
                "high_cpu",
                AlertSeverity::Warning,
                Some("taylor"),
                None,
                "CPU at 95%",
            ),
            make_alert(
                "high_cpu",
                AlertSeverity::Warning,
                Some("taylor"),
                None,
                "CPU at 95%",
            ),
            make_alert(
                "high_cpu",
                AlertSeverity::Warning,
                Some("taylor"),
                None,
                "CPU at 95%",
            ),
        ];

        let aggregated = aggregate_alerts(alerts);
        assert_eq!(aggregated.len(), 1);
        assert_eq!(aggregated[0].count, 3);
        assert!(aggregated[0].message_with_count().contains("(x3)"));
    }

    #[test]
    fn distinct_nodes_remain_separate() {
        let alerts = vec![
            make_alert(
                "high_cpu",
                AlertSeverity::Warning,
                Some("taylor"),
                None,
                "CPU high",
            ),
            make_alert(
                "high_cpu",
                AlertSeverity::Warning,
                Some("james"),
                None,
                "CPU high",
            ),
        ];

        let aggregated = aggregate_alerts(alerts);
        assert_eq!(aggregated.len(), 2);
        assert!(aggregated.iter().all(|a| a.count == 1));
    }

    #[test]
    fn different_severities_are_not_collapsed() {
        let alerts = vec![
            make_alert(
                "node_down",
                AlertSeverity::Warning,
                Some("taylor"),
                None,
                "node unreachable",
            ),
            make_alert(
                "node_down",
                AlertSeverity::Critical,
                Some("taylor"),
                None,
                "node unreachable",
            ),
        ];

        let aggregated = aggregate_alerts(alerts);
        assert_eq!(aggregated.len(), 2);
    }

    #[test]
    fn flush_clears_aggregator() {
        let mut aggregator = AlertAggregator::new();
        aggregator.record(make_alert(
            "high_cpu",
            AlertSeverity::Warning,
            Some("taylor"),
            None,
            "CPU high",
        ));
        aggregator.record(make_alert(
            "high_cpu",
            AlertSeverity::Warning,
            Some("taylor"),
            None,
            "CPU high",
        ));

        let flushed = aggregator.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].count, 2);
        assert!(aggregator.is_empty());
        assert_eq!(aggregator.len(), 0);
    }

    #[test]
    fn send_aggregated_invokes_sink() {
        let alerts = vec![
            make_alert(
                "disk_low",
                AlertSeverity::Warning,
                Some("taylor"),
                None,
                "disk low",
            ),
            make_alert(
                "disk_low",
                AlertSeverity::Warning,
                Some("taylor"),
                None,
                "disk low",
            ),
        ];

        let mut received = Vec::new();
        send_aggregated(alerts, |agg| received.push(agg.count));
        assert_eq!(received, vec![2]);
    }
}
