//! NATS JetStream durable stream setup.
//!
//! Creates idempotent JetStream streams for audit, cost, alerts, and tasks.
//! Call [`init_jetstream_streams`] once at daemon startup after [`init_nats`].

use std::time::Duration;
use tracing::{info, warn};

/// Stream names used across the fleet.
pub const STREAM_AUDIT: &str = "AUDIT";
pub const STREAM_COST: &str = "COST";
pub const STREAM_ALERTS: &str = "ALERTS";
pub const STREAM_TASKS: &str = "TASKS";
pub const STREAM_LOGS: &str = "LOGS";

/// Default retention: 30 days, max 10M messages per stream.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_MAX_MSGS: i64 = 10_000_000;

/// Idempotent stream creation. Logs on success, warns on failure.
/// Failure is non-fatal — the daemon continues with Redis/Postgres fallback.
pub async fn init_jetstream_streams(client: &async_nats::Client) {
    let js = async_nats::jetstream::new(client.clone());

    let streams = vec![
        (STREAM_AUDIT, vec!["fleet.audit.>"]),
        (STREAM_COST, vec!["fleet.cost.>"]),
        (STREAM_ALERTS, vec!["fleet.alerts.>"]),
        (STREAM_TASKS, vec!["fleet.tasks.>"]),
        (STREAM_LOGS, vec!["logs.>"]),
    ];

    for (name, subjects) in streams {
        let subjects: Vec<String> = subjects.into_iter().map(|s| s.to_string()).collect();
        let cfg = async_nats::jetstream::stream::Config {
            name: name.to_string(),
            subjects,
            retention: async_nats::jetstream::stream::RetentionPolicy::Limits,
            max_age: DEFAULT_MAX_AGE,
            max_messages: DEFAULT_MAX_MSGS,
            storage: async_nats::jetstream::stream::StorageType::File,
            ..Default::default()
        };

        match js.get_or_create_stream(cfg).await {
            Ok(mut stream) => {
                info!(stream = name, "jetstream stream ready");
                let _ = stream.info().await;
            }
            Err(e) => {
                warn!(stream = name, error = %e, "jetstream stream creation failed");
            }
        }
    }
}

/// Publish a message to a JetStream stream with durability guarantees.
/// Falls back to fire-and-forget NATS publish if JetStream is unavailable.
pub async fn publish_js(client: &async_nats::Client, subject: impl Into<String>, payload: Vec<u8>) {
    let js = async_nats::jetstream::new(client.clone());
    let subject = subject.into();
    match js.publish(subject.clone(), payload.clone().into()).await {
        Ok(ack) => {
            if let Err(e) = ack.await {
                warn!(subject, error = %e, "jetstream ack failed");
            }
        }
        Err(e) => {
            warn!(subject, error = %e, "jetstream publish failed, falling back to NATS");
            crate::nats_client::publish_raw(subject, payload).await;
        }
    }
}
