//! Deduplicated forwarding of orchestrator alerts to downstream services.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use ff_observability::{Alert, AlertDeduplicationState};

/// A downstream service that receives orchestrator alerts.
#[async_trait]
pub trait AlertSink: Send + Sync {
    async fn send(&self, alert: &Alert) -> Result<()>;
}

/// Forwards each logical alert once per deduplication window.
pub struct AlertForwarder {
    deduplication: Arc<AlertDeduplicationState>,
    sinks: Vec<Arc<dyn AlertSink>>,
}

impl AlertForwarder {
    pub fn new(
        deduplication: Arc<AlertDeduplicationState>,
        sinks: Vec<Arc<dyn AlertSink>>,
    ) -> Self {
        Self {
            deduplication,
            sinks,
        }
    }

    /// Pass an alert through the deduplication filter, then send it to every sink.
    ///
    /// Returns `true` when the alert passed the filter. Duplicate alerts return
    /// `false` without contacting any downstream service.
    pub async fn forward(&self, alert: &Alert) -> Result<bool> {
        if !self.deduplication.should_emit(deduplication_key(alert)) {
            return Ok(false);
        }

        for sink in &self.sinks {
            sink.send(alert).await?;
        }
        Ok(true)
    }
}

fn deduplication_key(alert: &Alert) -> String {
    format!(
        "{}:{}:{}",
        alert.rule_id,
        alert.node.as_deref().unwrap_or_default(),
        alert.model_id.as_deref().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use chrono::Utc;
    use ff_observability::AlertSeverity;
    use uuid::Uuid;

    use super::*;

    #[derive(Default)]
    struct CountingSink(AtomicUsize);

    #[async_trait]
    impl AlertSink for CountingSink {
        async fn send(&self, _alert: &Alert) -> Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn alert(node: &str) -> Alert {
        Alert {
            id: Uuid::new_v4(),
            rule_id: "node-down".into(),
            severity: AlertSeverity::Critical,
            message: "node is unavailable".into(),
            node: Some(node.into()),
            model_id: None,
            fired_at: Utc::now(),
            resolved_at: None,
            acknowledged: false,
            count: 1,
        }
    }

    #[tokio::test]
    async fn suppresses_duplicate_alerts_before_forwarding() {
        let sink = Arc::new(CountingSink::default());
        let forwarder = AlertForwarder::new(
            Arc::new(AlertDeduplicationState::new(Duration::from_secs(60))),
            vec![sink.clone()],
        );

        assert!(forwarder.forward(&alert("worker-a")).await.unwrap());
        assert!(!forwarder.forward(&alert("worker-a")).await.unwrap());
        assert_eq!(sink.0.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn forwards_distinct_alert_keys_to_all_sinks() {
        let first = Arc::new(CountingSink::default());
        let second = Arc::new(CountingSink::default());
        let forwarder = AlertForwarder::new(
            Arc::new(AlertDeduplicationState::new(Duration::from_secs(60))),
            vec![first.clone(), second.clone()],
        );

        assert!(forwarder.forward(&alert("worker-a")).await.unwrap());
        assert!(forwarder.forward(&alert("worker-b")).await.unwrap());
        assert_eq!(first.0.load(Ordering::Relaxed), 2);
        assert_eq!(second.0.load(Ordering::Relaxed), 2);
    }
}
