//! Pillar 4 — distributed concurrent development scheduler tick.
//!
//! Leader-only, serial. Each tick: reap stale leases (freeing slots + returning
//! their work_items to the ready pool), then assign `status='ready'` work_items
//! to free fleet slots via [`ff_db::pg_assign_work_item`] (one active lease per
//! item, enforced by a partial-unique index). Single-leader serial execution
//! means no cross-process race — no `FOR UPDATE SKIP LOCKED` needed.
//!
//! v1 = assignment (lease + slot reservation). The slot's agent loop picks up
//! its `current_work_item_id` to execute; the merge-queue drain + dispatch are
//! follow-ups. Only touches work_items explicitly flagged `status='ready'`, so
//! operator PM items (status 'idea' etc.) are never disturbed.
//!
//! Design: `.forgefleet/plans/DECISION-pillar4-canonical-home.md`.

use anyhow::Result;
use sqlx::PgPool;
use tracing::{info, warn};

/// Lease heartbeat deadline: a slot must heartbeat within this window or its
/// lease is reaped and the work_item re-queued. `pub(crate)` so the dispatch
/// path can keep the Lane-1 local-codegen timeout STRICTLY BELOW it (see
/// `work_item_dispatch::LANE1_TIMEOUT_SECS`) — a slow local lane must fail over
/// to the cloud backstop before this reaper can reclaim the lease.
pub(crate) const LEASE_STALE_SECS: i64 = 180;
/// Hard ceiling on lease HOLD time regardless of heartbeat — reclaims a wedged
/// dispatch that keeps its heartbeat fresh but makes no progress (the
/// "building forever, live heartbeat" wedge). Above a real build's Lane-2 cap
/// (~18.5 min).
const MAX_LEASE_DURATION_SECS: i64 = 25 * 60;
/// Lease lifetime granted at assignment (refreshed by heartbeats).
const LEASE_GRANT_SECS: i64 = 600;
/// Max assignments per tick (back-pressure; the rest wait for the next tick).
const MAX_ASSIGN_PER_TICK: i64 = 64;
/// Minimum age before an `in_progress` work_item with NO active lease is
/// considered orphaned and cancelled. Far above the lease/heartbeat windows so
/// a legitimately-leased item is never swept mid-assignment.
const ORPHAN_MIN_AGE_SECS: i64 = 3600;
/// Failure-convergence ceiling: after this many stalled/reaped attempts a
/// work_item is marked `failed` (with context) instead of re-queued forever.
/// A task the swarm genuinely can't build must STOP thrashing and surface for a
/// human or a retry-with-error-context — 3 tries is enough signal.
const MAX_BUILD_ATTEMPTS: i32 = 3;

/// One scheduler pass. Returns the number of work_items assigned this tick.
pub async fn evaluate_work_items(pg: &PgPool) -> Result<usize> {
    let reaped = ff_db::pg_reap_stale_work_item_leases(
        pg,
        LEASE_STALE_SECS,
        MAX_LEASE_DURATION_SECS,
        MAX_BUILD_ATTEMPTS,
    )
    .await?;
    if reaped > 0 {
        warn!(
            reaped,
            "work_item_scheduler: reaped stale leases (slots freed, items re-queued)"
        );
    }

    // Companion sweep: `in_progress` work_items with no active lease can't be
    // reaped by the lease sweep above (they have no lease row). Cancel the ones
    // older than ORPHAN_MIN_AGE_SECS so they stop polluting `in_progress`.
    let orphans = ff_db::pg_reap_orphaned_work_items(pg, ORPHAN_MIN_AGE_SECS).await?;
    if orphans > 0 {
        warn!(
            orphans,
            "work_item_scheduler: cancelled orphaned in_progress work_items (no active lease)"
        );
    }

    let ready = ff_db::pg_ready_work_items(pg, MAX_ASSIGN_PER_TICK).await?;
    if ready.is_empty() {
        return Ok(0);
    }

    // Slots that are free fleet-wide (a pinned item filters to its host).
    let global_free = ff_db::pg_free_slots(pg, None, MAX_ASSIGN_PER_TICK).await?;
    if global_free.is_empty() {
        info!(
            ready = ready.len(),
            "work_item_scheduler: items ready but no free slots"
        );
        return Ok(0);
    }

    // Prefer slots on computers with a LIVE agent-capable LLM endpoint so we
    // don't hand a build to a node whose model is already dead at tick time
    // (the E3 finding: a stale 'healthy' row wasted a ~6min lease cycle on
    // priya). This is a PREFERENCE, not a gate — `pop_slot` falls back to any
    // free slot if no viable one remains, so assignment never starves when the
    // deployment rows are momentarily stale (e.g. right after a deploy).
    let viable: std::collections::HashSet<uuid::Uuid> = match ff_db::pg_agent_viable_computer_ids(
        pg,
    )
    .await
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            warn!(error = %e, "work_item_scheduler: agent-viability lookup failed; assigning without preference");
            std::collections::HashSet::new()
        }
    };

    let mut pool: Vec<ff_db::FreeSlot> = global_free;
    let mut assigned = 0usize;
    let mut fallback_assigns = 0usize;
    for item in ready {
        // Honor a host pin by re-querying that host's free slots; else take from
        // the shared pool, preferring an agent-viable computer.
        let slot = if let Some(host) = item.assigned_computer.as_deref() {
            match ff_db::pg_free_slots(pg, Some(host), 1).await {
                Ok(mut v) => v.pop(),
                Err(e) => {
                    warn!(host, error = %e, "work_item_scheduler: pinned-slot lookup failed");
                    None
                }
            }
        } else {
            pop_slot(&mut pool, &viable, &mut fallback_assigns)
        };
        let Some(slot) = slot else { continue };

        match ff_db::pg_assign_work_item(
            pg,
            item.id,
            slot.sub_agent_id,
            slot.computer_id,
            LEASE_GRANT_SECS,
        )
        .await
        {
            Ok(true) => {
                assigned += 1;
                // Keep the shared pool consistent if a pinned assignment consumed
                // a slot that also sat in `pool`.
                pool.retain(|s| s.sub_agent_id != slot.sub_agent_id);
            }
            Ok(false) => { /* lost the race / already leased — skip */ }
            Err(e) => warn!(item = %item.id, error = %e, "work_item_scheduler: assign failed"),
        }
    }

    if assigned > 0 {
        info!(
            assigned,
            fallback_assigns, "work_item_scheduler: assigned work_items to fleet slots"
        );
    }
    if fallback_assigns > 0 {
        // Not silent: surface that we leased build work to nodes with no live
        // agent endpoint in the DB. Expected transiently after a deploy; if it
        // persists, agent-capability detection or the reconciler is lagging.
        warn!(
            fallback_assigns,
            "work_item_scheduler: assigned to non-agent-viable slots (no live agent endpoint); \
             self-heal will reclaim if the build stalls"
        );
    }
    Ok(assigned)
}

/// Take one free slot from `pool`, preferring a slot whose computer currently
/// has a live agent-capable LLM endpoint (`viable`). Falls back to any free slot
/// (bumping `fallback_assigns`) so assignment never starves when the deployment
/// rows are momentarily stale. Pure so the prefer-with-fallback rule is testable.
fn pop_slot(
    pool: &mut Vec<ff_db::FreeSlot>,
    viable: &std::collections::HashSet<uuid::Uuid>,
    fallback_assigns: &mut usize,
) -> Option<ff_db::FreeSlot> {
    if let Some(idx) = pool.iter().position(|s| viable.contains(&s.computer_id)) {
        return Some(pool.remove(idx));
    }
    // No viable slot left — fall back to any free slot (preserves prior pop()).
    let slot = pool.pop();
    if slot.is_some() {
        *fallback_assigns += 1;
    }
    slot
}

/// Spawn the leader-gated scheduler loop. The skip path reads the process-local
/// leader cache instead of probing Postgres.
pub fn spawn_work_item_scheduler(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if let Err(e) = evaluate_work_items(&pg).await {
                        warn!(error = %e, "work_item_scheduler tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("work_item_scheduler loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(computer: uuid::Uuid) -> ff_db::FreeSlot {
        ff_db::FreeSlot {
            sub_agent_id: uuid::Uuid::new_v4(),
            computer_id: computer,
        }
    }

    /// E3 finding: prefer an agent-viable computer's slot when one exists, so a
    /// build isn't handed to a node with no live LLM endpoint.
    #[test]
    fn pop_slot_prefers_a_viable_computer() {
        let dead = uuid::Uuid::new_v4();
        let live = uuid::Uuid::new_v4();
        // dead node's slot is "fresher" (would win a plain pop()) but has no LLM.
        let mut pool = vec![slot(live), slot(dead)];
        let viable: std::collections::HashSet<_> = [live].into_iter().collect();
        let mut fb = 0;
        let picked = pop_slot(&mut pool, &viable, &mut fb).unwrap();
        assert_eq!(picked.computer_id, live, "must pick the live-endpoint node");
        assert_eq!(fb, 0, "a viable pick is not a fallback");
        assert_eq!(pool.len(), 1, "exactly one slot consumed");
    }

    /// Safety: when NO slot is agent-viable (e.g. rows stale right after a
    /// deploy), assignment must still proceed via fallback rather than starve.
    #[test]
    fn pop_slot_falls_back_when_none_viable() {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let mut pool = vec![slot(a), slot(b)];
        let viable = std::collections::HashSet::new();
        let mut fb = 0;
        assert!(pop_slot(&mut pool, &viable, &mut fb).is_some());
        assert_eq!(fb, 1, "fallback assignment must be counted, not silent");
        // Empty pool yields None without bumping the fallback counter.
        let mut empty: Vec<ff_db::FreeSlot> = vec![];
        assert!(pop_slot(&mut empty, &viable, &mut fb).is_none());
        assert_eq!(fb, 1);
    }

    /// REGRESSION GUARD (reaper bug class #589/#590): same coupling as
    /// `lease_takeover` — the scheduler's own lease-reap window must clear at
    /// least two dispatch heartbeats so a live build's lease is never reclaimed.
    #[test]
    fn lease_stale_window_clears_two_heartbeats() {
        let cadence = crate::work_item_dispatch::HEARTBEAT_SECS as i64;
        assert!(
            LEASE_STALE_SECS >= 2 * cadence,
            "LEASE_STALE_SECS ({LEASE_STALE_SECS}) must be >= 2x the dispatch heartbeat ({cadence})"
        );
    }
}
