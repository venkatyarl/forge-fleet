//! Stuck-slot reaper for `sub_agents`.
//!
//! ## Role
//!
//! Runs on the leader every 10 minutes. Resets any stuck `sub_agents` row back
//! to `'idle'` so the dispatch queue can reuse the slot — when a worker crashes
//! mid-task or flips to `'error'` without a later cleanup, the slot is
//! effectively dead and would otherwise leak forever. Slots holding an ACTIVE
//! `work_item_leases` row are exempt: the lease lifecycle owns those (the
//! stale-lease reaper resets them when the lease dies), and each tick also
//! runs [`ff_db::queries::pg_reconcile_sub_agent_slots`] to re-derive
//! `status`/`current_work_item_id` from the lease table when they drift.
//!
//! ## Per-status staleness clock (the important part)
//!
//! `started_at` is the only clock available — there is NO periodic mid-task
//! heartbeat on `sub_agents` (every `UPDATE sub_agents` is a claim/release
//! state transition, so `last_heartbeat_at == started_at` for a busy slot).
//! That means a flat short timeout is WRONG for `'busy'`: a legitimately
//! long-running task (cold builds run ~45 min) whose `started_at` is older than
//! the timeout would be reset to `'idle'` mid-run, and the scheduler would then
//! dispatch a SECOND task onto the same slot (oversubscription) while the first
//! process is still alive. So we split the threshold:
//!
//! - `'error'` (dead) or NULL `started_at` (never really ran) → reset after
//!   [`ERROR_STALE_MINS`] (free dead slots quickly).
//! - `'busy'` with a `started_at` → reset only after the generous
//!   [`BUSY_STALE_MINS`] ceiling (a "hung task" guard that still gives real
//!   long tasks room to finish).
//!
//! The reap reason is surfaced via `tracing::info!` + the caller's audit trail,
//! not a per-row text column (the V23 table has no `last_error`).

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Minutes a dead/errored or never-started slot may sit before being freed.
const ERROR_STALE_MINS: i64 = 10;
/// Minutes a `'busy'` slot may run before it's assumed hung. Must exceed the
/// longest legitimate task (cold builds via the wave run ~45 min) so a live
/// slot is never reset mid-task (which would let the scheduler oversubscribe
/// it). 60 min is comfortably above 45 and well below "obviously wedged".
const BUSY_STALE_MINS: i64 = 60;

/// Staleness ceiling (minutes) for a reapable `status`. `'busy'` gets the long
/// ceiling; everything else we reap (`'error'`) gets the short one.
fn reap_threshold_mins(status: &str) -> i64 {
    match status {
        "busy" => BUSY_STALE_MINS,
        _ => ERROR_STALE_MINS,
    }
}

/// Pure mirror of the reaper's SQL WHERE clause (for tests + as the spec). A
/// slot is reaped when its status is reapable AND it either never meaningfully
/// started (NULL `started_at`) or its `started_at` is older than the per-status
/// threshold — UNLESS it holds an ACTIVE work_item lease. A live lease means
/// the lease lifecycle owns the slot: the stale-lease reaper
/// (`pg_reap_stale_work_item_leases`) resets it when the lease actually dies,
/// and wiping it here while a long build keeps heartbeating desyncs `busy`
/// from the lease table (observed 2026-07-20: 1 busy slot / 36 active leases).
/// `started_at_age_mins` is `None` for a NULL `started_at`.
#[allow(dead_code)] // pure mirror of the reaper SQL — the tested spec, not the impl
fn should_reap(status: &str, started_at_age_mins: Option<i64>, has_active_lease: bool) -> bool {
    if has_active_lease {
        return false;
    }
    if status != "error" && status != "busy" {
        return false;
    }
    match started_at_age_mins {
        None => true,
        Some(age) => age > reap_threshold_mins(status),
    }
}

/// Is this process currently the elected leader?
async fn is_leader(_pool: &PgPool, _my_name: &str) -> bool {
    crate::leader_cache::is_current_leader()
}

/// Background stuck-slot reaper.
pub struct SubAgentReaper {
    pool: PgPool,
    my_name: String,
}

impl SubAgentReaper {
    pub fn new(pool: PgPool, my_name: String) -> Self {
        Self { pool, my_name }
    }

    /// One tick: gate on leader, reset stuck rows, log each. Returns count.
    pub async fn run_once(&self) -> Result<usize> {
        if !is_leader(&self.pool, &self.my_name).await {
            return Ok(0);
        }

        // Per-status thresholds, interpolated from the consts so the SQL and
        // `should_reap` (the tested spec) can't drift. The values are i64
        // literals we control — no injection surface.
        let sql = format!(
            "UPDATE sub_agents AS s
                SET status               = 'idle',
                    current_work_item_id = NULL
               FROM computers c
              WHERE s.computer_id = c.id
                AND NOT EXISTS (
                     SELECT 1 FROM work_item_leases l
                      WHERE l.sub_agent_id = s.id AND l.released_at IS NULL)
                AND (
                     (s.status = 'error'
                        AND (s.started_at IS NULL
                             OR s.started_at < NOW() - INTERVAL '{ERROR_STALE_MINS} minutes'))
                  OR (s.status = 'busy'
                        AND (s.started_at IS NULL
                             OR s.started_at < NOW() - INTERVAL '{BUSY_STALE_MINS} minutes'))
                )
              RETURNING c.name AS computer_name, s.slot AS slot, s.status AS status"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .context("reap stuck sub_agents")?;

        for row in &rows {
            let computer: String = row.get("computer_name");
            let slot: i32 = row.get("slot");
            let prior: String = row.get("status");
            tracing::info!(
                computer = %computer,
                slot = slot,
                prior_status = %prior,
                threshold_mins = reap_threshold_mins(&prior),
                "reaped stuck sub_agent slot"
            );
        }

        // Reconcile the mutable slot columns from the lease source of truth:
        // relink slots whose ACTIVE lease says busy, free slots whose lease was
        // released without the matching slot update landing.
        let (relinked, freed) = ff_db::queries::pg_reconcile_sub_agent_slots(&self.pool)
            .await
            .context("reconcile sub_agent slots from leases")?;
        if relinked > 0 || freed > 0 {
            tracing::info!(relinked, freed, "reconciled sub_agent slots from leases");
        }

        Ok(rows.len() + relinked as usize + freed as usize)
    }

    /// Spawn the 10-minute tick. First tick fires ~120s after spawn so the
    /// daemon's other subsystems come up first.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let kickoff = Duration::from_secs(120);
            let interval = Duration::from_secs(600);

            tokio::select! {
                _ = tokio::time::sleep(kickoff) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                match self.run_once().await {
                    Ok(n) if n > 0 => tracing::info!(reaped = n, "sub-agent reaper tick"),
                    Ok(_) => tracing::debug!("sub-agent reaper tick: nothing to do"),
                    Err(e) => tracing::warn!(error = %e, "sub-agent reaper tick failed"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_long_task_is_protected_until_ceiling() {
        // THE BUG FIX: a live 'busy' slot running a 30-min task (well under the
        // 60-min ceiling) must NOT be reaped — reaping it mid-run would let the
        // scheduler oversubscribe the slot.
        assert!(!should_reap("busy", Some(30), false));
        assert!(!should_reap("busy", Some(BUSY_STALE_MINS), false)); // exactly at ceiling: not yet
        // A genuinely hung 'busy' slot past the ceiling IS reaped.
        assert!(should_reap("busy", Some(BUSY_STALE_MINS + 5), false));
        // A 'busy' slot that never recorded a start is dead → reaped.
        assert!(should_reap("busy", None, false));
    }

    #[test]
    fn active_lease_exempts_slot_from_reaping() {
        // A slot whose lease is still ACTIVE belongs to the lease lifecycle —
        // wiping it here while the build heartbeats its lease desyncs 'busy'
        // from work_item_leases (2026-07-20: 1 busy slot / 36 active leases).
        assert!(!should_reap("busy", Some(BUSY_STALE_MINS + 500), true));
        assert!(!should_reap("busy", None, true));
        assert!(!should_reap("error", Some(ERROR_STALE_MINS + 500), true));
    }

    #[test]
    fn error_slots_freed_quickly() {
        assert!(!should_reap("error", Some(5), false)); // within the short window
        assert!(should_reap("error", Some(ERROR_STALE_MINS + 1), false));
        assert!(should_reap("error", None, false));
    }

    #[test]
    fn non_reapable_status_never_reaped() {
        assert!(!should_reap("idle", None, false));
        assert!(!should_reap("idle", Some(999), false));
        assert!(!should_reap("planning", Some(999), false));
    }

    #[test]
    fn thresholds_are_distinct_and_busy_is_generous() {
        assert_eq!(reap_threshold_mins("error"), ERROR_STALE_MINS);
        assert_eq!(reap_threshold_mins("busy"), BUSY_STALE_MINS);
        // Busy ceiling must clear the ~45-min cold-build worst case.
        assert!(BUSY_STALE_MINS > 45);
        assert!(BUSY_STALE_MINS > ERROR_STALE_MINS);
    }
}
