//! JetStream stream configuration for the `FF_TASKS` work queue.
//!
//! Defines the durable `FF_TASKS` stream consumed by the gateway scheduler.
//! The stream uses work-queue retention so messages are removed once explicitly
//! acked, file-backed storage for durability, and requires explicit consumer
//! acknowledgements.

use std::time::Duration;

use async_nats::jetstream::stream::{Config as StreamConfig, RetentionPolicy, StorageType};

/// Name of the JetStream stream that holds fleet task notifications.
pub const STREAM_FF_TASKS: &str = "FF_TASKS";

/// Subject space captured by [`STREAM_FF_TASKS`].
pub const FF_TASKS_SUBJECT_PREFIX: &str = "ff.tasks.";

/// Default retention: 30 days, max 10M messages per stream.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_MAX_MSGS: i64 = 10_000_000;

/// Build the `FF_TASKS` JetStream stream configuration.
///
/// - Work-queue retention: messages are deleted after an explicit ack.
/// - File-backed storage for durability across restarts.
/// - Consumers must ack explicitly (`no_ack: false`).
pub fn ff_tasks_stream_config() -> StreamConfig {
    StreamConfig {
        name: STREAM_FF_TASKS.to_string(),
        subjects: vec![format!("{FF_TASKS_SUBJECT_PREFIX}>")],
        retention: RetentionPolicy::WorkQueue,
        max_age: DEFAULT_MAX_AGE,
        max_messages: DEFAULT_MAX_MSGS,
        storage: StorageType::File,
        no_ack: false,
        ..Default::default()
    }
}

/// Idempotently create the `FF_TASKS` stream and return it.
pub async fn ensure_ff_tasks_stream(
    js: &async_nats::jetstream::Context,
) -> Result<async_nats::jetstream::stream::Stream, async_nats::Error> {
    js.get_or_create_stream(ff_tasks_stream_config()).await?;
    Ok(js.get_stream(STREAM_FF_TASKS).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ff_tasks_config_uses_work_queue_retention() {
        let cfg = ff_tasks_stream_config();
        assert_eq!(cfg.name, STREAM_FF_TASKS);
        assert_eq!(cfg.subjects, vec!["ff.tasks.>"]);
        assert!(matches!(cfg.retention, RetentionPolicy::WorkQueue));
        assert!(matches!(cfg.storage, StorageType::File));
        assert!(!cfg.no_ack);
    }
}
