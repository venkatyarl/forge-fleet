//! Durable pull consumer for the `FF_TASKS` JetStream stream.
//!
//! Follows the idempotent stream-setup pattern from
//! `ff_agent::nats_jetstream`: streams are created with
//! `get_or_create_stream` at startup and every transport failure is
//! logged, never panicked.
//!
//! Delivery semantics:
//! - explicit-ack **durable** pull consumer ([`DURABLE_NAME`]) so the
//!   scheduler resumes where it left off across gateway restarts;
//! - **bounded redelivery**: a failing task is NAK'd with a short delay
//!   and retried up to [`MAX_DELIVERIES`] total attempts;
//! - **DLQ**: after the final failed attempt the payload is republished
//!   to `ff.dlq.tasks.<tail>` (captured by the `FF_TASKS_DLQ` stream)
//!   and the original message is TERM'd so the server stops
//!   redelivering it.

use std::future::Future;
use std::time::Duration;

use async_nats::jetstream::consumer::AckPolicy;
use async_nats::jetstream::consumer::pull;
use async_nats::jetstream::stream::{Config as StreamConfig, RetentionPolicy, StorageType, Stream};
use async_nats::jetstream::{AckKind, Context, Message};
use futures::StreamExt;
use tracing::{error, info, warn};

/// Stream the scheduler consumes from.
pub const STREAM_FF_TASKS: &str = "FF_TASKS";
/// Subject space captured by [`STREAM_FF_TASKS`].
pub const FF_TASKS_SUBJECT_PREFIX: &str = "ff.tasks.";

/// Dead-letter stream. Lives on the distinct `ff.dlq.` prefix so DLQ
/// traffic can never overlap the live stream's subject filter.
pub const STREAM_FF_TASKS_DLQ: &str = "FF_TASKS_DLQ";
/// Subject prefix for dead-lettered tasks.
pub const DLQ_SUBJECT_PREFIX: &str = "ff.dlq.tasks.";

/// Durable consumer name — persists server-side across gateway restarts.
pub const DURABLE_NAME: &str = "ff-gateway-scheduler";

/// Maximum delivery attempts (first delivery + redeliveries) before a
/// task is dead-lettered.
pub const MAX_DELIVERIES: i64 = 5;

/// How long the server waits for an ack before redelivering.
const ACK_WAIT: Duration = Duration::from_secs(30);
/// Delay requested on NAK so a failing task is not retried in a hot loop.
const NAK_DELAY: Duration = Duration::from_secs(5);
/// Retention limits, matching `ff_agent::nats_jetstream` defaults.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_MAX_MSGS: i64 = 10_000_000;

/// What to do with a message after a handling attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Handled successfully — ack it.
    Ack,
    /// Failed with attempts remaining — NAK with delay for redelivery.
    Retry,
    /// Failed on the final allowed attempt — publish to the DLQ, then TERM.
    DeadLetter,
}

/// Decide a message's fate from the handler outcome and how many times
/// the server has delivered it (`delivered` is 1 on the first attempt).
pub fn disposition(handled_ok: bool, delivered: i64) -> Disposition {
    if handled_ok {
        Disposition::Ack
    } else if delivered >= MAX_DELIVERIES {
        Disposition::DeadLetter
    } else {
        Disposition::Retry
    }
}

/// Map a task subject to its dead-letter subject:
/// `ff.tasks.build.x` → `ff.dlq.tasks.build.x`.
pub fn dlq_subject(subject: &str) -> String {
    let tail = subject
        .strip_prefix(FF_TASKS_SUBJECT_PREFIX)
        .unwrap_or(subject);
    format!("{DLQ_SUBJECT_PREFIX}{tail}")
}

/// Idempotently create the `FF_TASKS` and `FF_TASKS_DLQ` streams and
/// return the task stream.
pub async fn ensure_streams(js: &Context) -> Result<Stream, async_nats::Error> {
    for (name, subject) in [
        (STREAM_FF_TASKS_DLQ, format!("{DLQ_SUBJECT_PREFIX}>")),
        (STREAM_FF_TASKS, format!("{FF_TASKS_SUBJECT_PREFIX}>")),
    ] {
        let cfg = StreamConfig {
            name: name.to_string(),
            subjects: vec![subject],
            retention: RetentionPolicy::Limits,
            max_age: DEFAULT_MAX_AGE,
            max_messages: DEFAULT_MAX_MSGS,
            storage: StorageType::File,
            ..Default::default()
        };
        js.get_or_create_stream(cfg).await?;
        info!(stream = name, "jetstream stream ready");
    }
    Ok(js.get_stream(STREAM_FF_TASKS).await?)
}

/// Run the scheduler consumer with the default task handler. Loops until
/// the underlying message stream ends; callers typically `tokio::spawn`
/// this after connecting NATS.
pub async fn run(client: &async_nats::Client) -> Result<(), async_nats::Error> {
    run_with_handler(client, handle_task).await
}

/// Run the durable pull consumer, feeding each message through `handler`.
/// A handler `Err` triggers bounded redelivery and, once
/// [`MAX_DELIVERIES`] is exhausted, dead-lettering.
pub async fn run_with_handler<F, Fut>(
    client: &async_nats::Client,
    handler: F,
) -> Result<(), async_nats::Error>
where
    F: Fn(String, Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let js = async_nats::jetstream::new(client.clone());
    let stream = ensure_streams(&js).await?;
    let consumer = stream
        .get_or_create_consumer(
            DURABLE_NAME,
            pull::Config {
                durable_name: Some(DURABLE_NAME.to_string()),
                description: Some("gateway task scheduler".to_string()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: ACK_WAIT,
                max_deliver: MAX_DELIVERIES,
                ..Default::default()
            },
        )
        .await?;
    info!(
        stream = STREAM_FF_TASKS,
        durable = DURABLE_NAME,
        "scheduler pull consumer ready"
    );

    let mut messages = consumer.messages().await?;
    while let Some(next) = messages.next().await {
        match next {
            Ok(msg) => process_message(&js, msg, &handler).await,
            Err(e) => warn!(error = %e, "scheduler consumer pull error"),
        }
    }
    Ok(())
}

/// Handle one delivery: run the handler, then ack / NAK / dead-letter
/// according to [`disposition`]. Ack failures are logged only — the
/// server will simply redeliver.
async fn process_message<F, Fut>(js: &Context, msg: Message, handler: &F)
where
    F: Fn(String, Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let subject = msg.subject.to_string();
    let delivered = msg.info().map(|i| i.delivered).unwrap_or(1);
    let result = handler(subject.clone(), msg.payload.to_vec()).await;

    match disposition(result.is_ok(), delivered) {
        Disposition::Ack => {
            if let Err(e) = msg.ack().await {
                warn!(subject, error = %e, "scheduler ack failed");
            }
        }
        Disposition::Retry => {
            warn!(
                subject,
                delivered,
                error = result.err().unwrap_or_default(),
                "task failed; NAK for redelivery"
            );
            if let Err(e) = msg.ack_with(AckKind::Nak(Some(NAK_DELAY))).await {
                warn!(subject, error = %e, "scheduler NAK failed");
            }
        }
        Disposition::DeadLetter => {
            error!(
                subject,
                delivered,
                error = result.err().unwrap_or_default(),
                "task exhausted deliveries; moving to DLQ"
            );
            dead_letter(js, &subject, msg.payload.to_vec()).await;
            if let Err(e) = msg.ack_with(AckKind::Term).await {
                warn!(subject, error = %e, "scheduler TERM failed");
            }
        }
    }
}

/// Republish an exhausted task to the DLQ stream. Best-effort: a DLQ
/// publish failure is logged but the message is still TERM'd, since
/// unbounded redelivery of a poison message is worse than losing it to
/// the (still stream-retained) original.
async fn dead_letter(js: &Context, subject: &str, payload: Vec<u8>) {
    let dlq = dlq_subject(subject);
    match js.publish(dlq.clone(), payload.into()).await {
        Ok(ack) => {
            if let Err(e) = ack.await {
                error!(subject = dlq, error = %e, "DLQ publish not acked");
            }
        }
        Err(e) => error!(subject = dlq, error = %e, "DLQ publish failed"),
    }
}

/// Default task handler: validate the JSON envelope and log the
/// dispatch. Real scheduling work hangs off this entry point; a payload
/// that can never parse is redelivered up to [`MAX_DELIVERIES`] times
/// and then lands in the DLQ as a poison message.
async fn handle_task(subject: String, payload: Vec<u8>) -> Result<(), String> {
    let task: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|e| format!("invalid task payload: {e}"))?;
    let id = task
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "task payload missing string `id`".to_string())?;
    info!(subject, task_id = id, "scheduler task received");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    #[test]
    fn success_always_acks() {
        assert_eq!(disposition(true, 1), Disposition::Ack);
        assert_eq!(disposition(true, MAX_DELIVERIES), Disposition::Ack);
    }

    #[test]
    fn failure_retries_until_max_deliveries() {
        for delivered in 1..MAX_DELIVERIES {
            assert_eq!(disposition(false, delivered), Disposition::Retry);
        }
        assert_eq!(disposition(false, MAX_DELIVERIES), Disposition::DeadLetter);
        assert_eq!(
            disposition(false, MAX_DELIVERIES + 1),
            Disposition::DeadLetter
        );
    }

    #[test]
    fn dlq_subject_swaps_prefix() {
        assert_eq!(dlq_subject("ff.tasks.build.x"), "ff.dlq.tasks.build.x");
        // Unexpected subjects still land under the DLQ prefix.
        assert_eq!(dlq_subject("other.subject"), "ff.dlq.tasks.other.subject");
    }

    #[test]
    fn handle_task_accepts_valid_envelope() {
        let payload = br#"{"id":"wi-123","kind":"build"}"#.to_vec();
        assert!(block_on(handle_task("ff.tasks.build".into(), payload)).is_ok());
    }

    #[test]
    fn handle_task_rejects_bad_payloads() {
        let bad_json = block_on(handle_task("ff.tasks.x".into(), b"not json".to_vec()));
        assert!(bad_json.unwrap_err().contains("invalid task payload"));

        let no_id = block_on(handle_task("ff.tasks.x".into(), b"{}".to_vec()));
        assert!(no_id.unwrap_err().contains("missing string `id`"));
    }
}
