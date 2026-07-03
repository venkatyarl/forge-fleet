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
/// Failure-convergence ceiling — must match `work_item_scheduler::MAX_BUILD_ATTEMPTS`.
/// After this many reaped attempts the reaper marks the item `failed` instead of
/// re-queuing it forever (the escalation ladder takes over from there).
const MAX_BUILD_ATTEMPTS: i32 = 3;

pub async fn evaluate_lease_takeover(pg: &PgPool, _worker_name: &str) -> Result<usize> {
    if !crate::leader_cache::is_current_leader() {
        return Ok(0);
    }

    let reclaimed =
        ff_db::pg_reap_stale_work_item_leases(pg, STALE_HEARTBEAT_SECS, MAX_BUILD_ATTEMPTS).await?
            as usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// REGRESSION GUARD (reaper bug class #589/#590): the lease reaper reclaims
    /// a work_item lease whose `heartbeat_at` is older than STALE_HEARTBEAT_SECS.
    /// The dispatch loop bumps that heartbeat every
    /// `work_item_dispatch::HEARTBEAT_SECS`, so the window MUST clear at least
    /// two beats or a live build (which heartbeats fine) gets its lease yanked
    /// and re-leased — a duplicate build. Couple the consts.
    #[test]
    fn stale_window_clears_two_heartbeats() {
        let cadence = crate::work_item_dispatch::HEARTBEAT_SECS as i64;
        assert!(
            STALE_HEARTBEAT_SECS >= 2 * cadence,
            "STALE_HEARTBEAT_SECS ({STALE_HEARTBEAT_SECS}) must be >= 2x the dispatch heartbeat ({cadence})"
        );
    }
}
