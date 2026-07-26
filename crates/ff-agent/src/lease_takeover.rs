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

/// Hard ceiling on how long a single lease may be HELD regardless of heartbeat.
/// The stale-heartbeat reaper cannot reclaim a wedged dispatch whose daemon keeps
/// the heartbeat fresh (the "building forever with a live heartbeat" wedge —
/// observed 2026-07-06: 24 min, 0 output). This age cap reclaims it so the slot
/// self-heals. Set well above a real build (Lane-2 dispatch caps at ~18.5 min).
const MAX_LEASE_DURATION_SECS: i64 = 45 * 60;
/// Failure-convergence ceiling — must match `work_item_scheduler::MAX_BUILD_ATTEMPTS`.
/// After this many reaped attempts the reaper marks the item `failed` instead of
/// re-queuing it forever. MUST stay strictly above
/// `ff_routing_policy::LOCAL_LANE_MAX_TRIES` (=3) so the escalation ladder gets
/// cloud attempts (3 local + 2 cloud) before the hard fail — see the detailed
/// note on `work_item_scheduler::MAX_BUILD_ATTEMPTS`.
const MAX_BUILD_ATTEMPTS: i32 = 5;

pub async fn evaluate_lease_takeover(pg: &PgPool, _worker_name: &str) -> Result<usize> {
    if !crate::leader_cache::is_current_leader() {
        return Ok(0);
    }

    let stale_heartbeat_secs = crate::work_item_scheduler::lease_stale_secs(pg).await;
    let reclaimed = ff_db::pg_reap_stale_work_item_leases(
        pg,
        stale_heartbeat_secs,
        MAX_LEASE_DURATION_SECS,
        MAX_BUILD_ATTEMPTS,
    )
    .await? as usize;
    if reclaimed > 0 {
        warn!(
            reclaimed,
            "lease_takeover: reclaimed stale/overlong work_item leases"
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
    /// REGRESSION GUARD (reaper bug class #589/#590): the measured lease reaper
    /// floor must clear at least two dispatch heartbeats. The runtime window may
    /// grow above this from successful build data, but it must never shrink below
    /// a live build's normal heartbeat cadence.
    #[test]
    fn stale_window_floor_clears_two_heartbeats() {
        let cadence = crate::work_item_dispatch::HEARTBEAT_SECS as i64;
        assert!(
            crate::work_item_scheduler::MIN_LEASE_STALE_SECS >= 2 * cadence,
            "MIN_LEASE_STALE_SECS must be >= 2x the dispatch heartbeat ({cadence})"
        );
    }
}
