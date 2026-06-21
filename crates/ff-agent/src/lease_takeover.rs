//! Pillar 4 lease takeover.
//!
//! Leader-gated reclamation for active work_item leases whose heartbeat went
//! stale because the builder host crashed or stalled. The state transition
//! releases the lease, frees the sub-agent slot, marks any in-flight worktree
//! stale/failed, and returns the work_item to `ready` so the scheduler can lease
//! it to another fleet slot.

use anyhow::Result;
use sqlx::PgPool;
use std::time::Duration;
use tracing::{info, warn};

const STALE_HEARTBEAT_SECS: i64 = 5 * 60;

async fn is_current_leader(pg: &PgPool, worker_name: &str) -> bool {
    sqlx::query_scalar(
        r#"SELECT EXISTS (
               SELECT 1 FROM fleet_leader_state
                WHERE member_name = $1
                  AND heartbeat_at > NOW() - INTERVAL '60 seconds'
           )"#,
    )
    .bind(worker_name)
    .fetch_one(pg)
    .await
    .unwrap_or(false)
}

pub async fn evaluate_lease_takeover(pg: &PgPool, worker_name: &str) -> Result<usize> {
    if !is_current_leader(pg, worker_name).await {
        return Ok(0);
    }

    let reclaimed = ff_db::pg_reap_stale_work_item_leases(pg, STALE_HEARTBEAT_SECS).await? as usize;
    if reclaimed > 0 {
        warn!(
            reclaimed,
            "lease_takeover: reclaimed stale work_item leases"
        );
    }
    Ok(reclaimed)
}

pub fn spawn_lease_takeover(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = evaluate_lease_takeover(&pg, &worker_name).await {
                        warn!(error = %e, "lease_takeover tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("lease_takeover loop stopped");
    })
}
