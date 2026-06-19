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
/// lease is reaped and the work_item re-queued.
const LEASE_STALE_SECS: i64 = 180;
/// Lease lifetime granted at assignment (refreshed by heartbeats).
const LEASE_GRANT_SECS: i64 = 600;
/// Max assignments per tick (back-pressure; the rest wait for the next tick).
const MAX_ASSIGN_PER_TICK: i64 = 64;

/// One scheduler pass. Returns the number of work_items assigned this tick.
pub async fn evaluate_work_items(pg: &PgPool) -> Result<usize> {
    let reaped = ff_db::pg_reap_stale_work_item_leases(pg, LEASE_STALE_SECS).await?;
    if reaped > 0 {
        warn!(
            reaped,
            "work_item_scheduler: reaped stale leases (slots freed, items re-queued)"
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

    let mut pool: Vec<ff_db::FreeSlot> = global_free;
    let mut assigned = 0usize;
    for item in ready {
        // Honor a host pin by re-querying that host's free slots; else take from
        // the shared pool.
        let slot = if let Some(host) = item.assigned_computer.as_deref() {
            match ff_db::pg_free_slots(pg, Some(host), 1).await {
                Ok(mut v) => v.pop(),
                Err(e) => {
                    warn!(host, error = %e, "work_item_scheduler: pinned-slot lookup failed");
                    None
                }
            }
        } else {
            pool.pop()
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
            "work_item_scheduler: assigned work_items to fleet slots"
        );
    }
    Ok(assigned)
}

/// Spawn the leader-gated scheduler loop. Mirrors `scheduler_tick`'s leader
/// check against `fleet_leader_state`.
pub fn spawn_work_item_scheduler(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"SELECT EXISTS (
                               SELECT 1 FROM fleet_leader_state
                                WHERE member_name = $1
                                  AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                           )"#,
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);
                    if !is_leader {
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
