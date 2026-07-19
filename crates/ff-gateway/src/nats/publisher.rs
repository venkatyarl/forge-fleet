//! JetStream publisher for gateway task events.
//!
//! Emits events onto the `FF_TASKS` stream consumed by the durable
//! scheduler pull consumer in [`crate::nats::consumer::scheduler`].
//! All publishes are best-effort: failures are logged and dropped so
//! callers never fail because NATS is unavailable.

use serde_json::json;
use tracing::warn;

/// Subject prefix for task-ready notifications on the `FF_TASKS` stream.
pub const FF_TASKS_READY_SUBJECT_PREFIX: &str = "ff.tasks.ready.";

/// JetStream publisher for task lifecycle events.
#[derive(Clone)]
pub struct TaskPublisher {
    js: async_nats::jetstream::Context,
}

impl TaskPublisher {
    /// Create a new publisher bound to the given JetStream context.
    pub fn new(js: async_nats::jetstream::Context) -> Self {
        Self { js }
    }

    /// Publish a task-ready event to `ff.tasks.ready.<task_id>`.
    ///
    /// The message is durably published through JetStream so the scheduler
    /// consumer can pull it. Serialization and transport failures are
    /// swallowed and logged only.
    pub async fn publish_task_ready(&self, task_id: impl AsRef<str>) {
        let task_id = task_id.as_ref();
        let subject = format!("{FF_TASKS_READY_SUBJECT_PREFIX}{task_id}");
        let payload = json!({ "task_id": task_id, "event": "ready" });

        let bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(_) => return,
        };

        match self.js.publish(subject.clone(), bytes.into()).await {
            Ok(ack) => {
                if let Err(e) = ack.await {
                    warn!(subject, error = %e, "task ready jetstream ack failed");
                }
            }
            Err(e) => {
                warn!(subject, error = %e, "task ready jetstream publish failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_subject_includes_task_id() {
        assert_eq!(
            format!("{FF_TASKS_READY_SUBJECT_PREFIX}wi-123"),
            "ff.tasks.ready.wi-123"
        );
    }
}
