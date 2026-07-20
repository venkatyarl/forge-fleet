//! Deduplication for alerts consumed by the gateway before Telegram delivery.

use ff_observability::alerting::{Alert, AlertEngine, AlertSeverity};

/// Tracks gateway alerts and decides which ones should be forwarded.
///
/// [`AlertEngine`] owns the deduplication window and occurrence counters. A
/// repeated alert is still recorded by the engine, but only a new alert is
/// returned to the caller for Telegram delivery.
#[derive(Debug, Clone)]
pub struct TelegramAlertTracker {
    engine: AlertEngine,
}

impl Default for TelegramAlertTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TelegramAlertTracker {
    /// Create a tracker using the observability engine's default dedup window.
    pub fn new() -> Self {
        Self {
            engine: AlertEngine::new(Vec::new()),
        }
    }

    /// Record an alert and return it only when it should be sent to Telegram.
    pub fn track_for_telegram(
        &self,
        metric: &str,
        severity: AlertSeverity,
        message: impl Into<String>,
        node: Option<String>,
        model_id: Option<String>,
    ) -> Option<Alert> {
        let alert = self
            .engine
            .fire_alert(metric, severity, message.into(), node, model_id);

        (alert.count == 1).then_some(alert)
    }

    /// Return the tracked alert state, including aggregated occurrence counts.
    pub fn active_alerts(&self) -> Vec<Alert> {
        self.engine.active_alerts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_alert_is_tracked_but_not_forwarded() {
        let tracker = TelegramAlertTracker::new();

        let first = tracker.track_for_telegram(
            "cpu_usage",
            AlertSeverity::Warning,
            "CPU usage is high",
            Some("taylor".into()),
            None,
        );
        let repeated = tracker.track_for_telegram(
            "cpu_usage",
            AlertSeverity::Warning,
            "CPU usage remains high",
            Some("taylor".into()),
            None,
        );

        assert!(first.is_some());
        assert!(repeated.is_none());
        let active = tracker.active_alerts();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].count, 2);
    }

    #[test]
    fn distinct_metric_node_or_model_is_forwarded() {
        let tracker = TelegramAlertTracker::new();

        for (metric, node, model) in [
            ("cpu_usage", Some("taylor".into()), None),
            ("memory_usage", Some("taylor".into()), None),
            ("cpu_usage", Some("veronica".into()), None),
            ("cpu_usage", Some("taylor".into()), Some("llama".into())),
        ] {
            assert!(
                tracker
                    .track_for_telegram(
                        metric,
                        AlertSeverity::Warning,
                        "threshold exceeded",
                        node,
                        model,
                    )
                    .is_some()
            );
        }
    }
}
