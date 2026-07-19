//! NATS-based task wake-up listener for the HA task agent.
//!
//! During the rollout from PostgreSQL LISTEN/NOTIFY to NATS JetStream,
//! producers dual-emit task-insert notifications on both channels.  This
//! module consumes the NATS side, waiting on the `FF_TASKS` stream's
//! `fleet.tasks.inserted` subject and waking the worker tick immediately
//! when a message arrives.
//!
//! NATS is optional: if the global NATS client is unavailable, the listener
//! returns an error so the caller can fall back to its polling interval.

use futures::StreamExt;
use tracing::debug;

use crate::nats_jetstream::FF_TASKS_INSERTED_SUBJECT;

/// Wait for the next task-inserted message on the NATS `FF_TASKS` stream.
///
/// Returns as soon as a message arrives so the caller can tick immediately.
/// If NATS is unavailable or the subscription cannot be created, returns an
/// error so the caller can fall back to polling.
pub async fn listen_for_tasks() -> anyhow::Result<()> {
    let Some(client) = crate::nats_client::get_nats().await.cloned() else {
        return Err(anyhow::anyhow!("NATS client not initialized"));
    };

    let mut subscriber = client.subscribe(FF_TASKS_INSERTED_SUBJECT).await?;

    // Block until at least one notification arrives.  Core-NATS delivery is
    // best-effort; the remaining LISTEN/NOTIFY path plus the polling interval
    // cover any dropped notification during the rollout.
    let _ = subscriber.next().await;
    debug!("woken by NATS FF_TASKS notification");
    Ok(())
}
