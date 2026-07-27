//! Leader-gated Jira queue ingestion tick.
//!
//! Wraps [`crate::ha::jira_ingest::run_jira_ingest_tick`] in a background loop
//! that only executes on the current fleet leader.

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Duration, interval};
use tracing::{info, warn};

/// Spawn the Jira ingestion tick. It wakes every `interval_secs` and, only if
/// this node is the current leader, polls the configured Jira queue and
/// upserts matching work items.
pub fn spawn_jira_ingestion_tick(
    pg: PgPool,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }

                    if let Err(err) = crate::ha::jira_ingest::run_jira_ingest_tick(&pg).await {
                        warn!(error = %err, "jira ingestion tick failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("jira ingestion tick shutting down");
                        break;
                    }
                }
            }
        }
    })
}
