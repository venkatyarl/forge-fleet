//! Queue / host invariant checks for the Pillar-4 work_item pipeline.
//!
//! As the planner emits many tasks and the scheduler fans them across the
//! fleet, silent failures (dead builders, orphaned worktrees, expired-but-
//! unreleased leases, duplicate claims) accumulate and stall autonomy without
//! any loud error. [`check_fleet_health`] surfaces those invariant violations
//! from live Postgres so a tick / `ff` verb / operator can spot (and later
//! auto-repair) a wedged queue.
//!
//! This is READ-ONLY: it counts violations, it does not mutate. Repair is a
//! separate concern (the reapers already exist for some of these).

use sqlx::PgPool;

/// The heartbeat cadence the dispatch loop bumps a live lease at. A builder is
/// considered dead when its lease heartbeat is older than
/// [`STUCK_BUILD_MULTIPLIER`] × this. Mirrors
/// `work_item_dispatch::HEARTBEAT_SECS` (kept as a local const to avoid a
/// cross-module coupling for a health check).
const HEARTBEAT_SECS: i64 = 45;
/// A `building` work_item whose lease last heartbeat is older than
/// `HEARTBEAT_SECS * this` is treated as a dead/stuck builder.
const STUCK_BUILD_MULTIPLIER: i64 = 3;

/// Snapshot of Pillar-4 queue/host invariant violations. `healthy` is true iff
/// every count is zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct FleetHealthReport {
    /// Leases past `lease_expires_at` that were never `released_at` — the slot
    /// is silently held by an expired claim.
    pub stale_claims: i64,
    /// `building` work_items whose live lease stopped heart-beating (dead
    /// builder — the process died but the row still says building).
    pub stuck_building: i64,
    /// Worktrees still `active`/`creating` whose work_item already reached a
    /// terminal state (done/failed/cancelled) — leaked disk + git worktrees.
    pub orphaned_worktrees: i64,
    /// A work_item with more than one un-released lease (duplicate claim — two
    /// builders racing the same item).
    pub duplicate_leases: i64,
    /// True iff all four counts are zero.
    pub healthy: bool,
    /// One-line human summary.
    pub summary: String,
}

impl FleetHealthReport {
    /// Recompute `healthy` from the four counts (all zero ⇒ healthy).
    pub fn is_healthy(&self) -> bool {
        self.stale_claims == 0
            && self.stuck_building == 0
            && self.orphaned_worktrees == 0
            && self.duplicate_leases == 0
    }
}

/// Query live Postgres for Pillar-4 queue/host invariant violations. Never
/// mutates. Any single sub-query that errors is surfaced in the summary rather
/// than failing the whole check.
pub async fn check_fleet_health(pool: &PgPool) -> FleetHealthReport {
    let stale_claims = scalar(
        pool,
        "SELECT count(*) FROM work_item_leases \
          WHERE released_at IS NULL AND lease_expires_at < NOW()",
    )
    .await;

    // A dead builder: a 'building' work_item whose live (un-released) lease
    // hasn't heart-beat within STUCK_BUILD_MULTIPLIER * HEARTBEAT_SECS.
    let stuck_secs = HEARTBEAT_SECS * STUCK_BUILD_MULTIPLIER;
    let stuck_building = scalar(
        pool,
        &format!(
            "SELECT count(*) FROM work_items w \
               JOIN work_item_leases l \
                 ON l.work_item_id = w.id AND l.released_at IS NULL \
              WHERE w.status = 'building' \
                AND l.heartbeat_at < NOW() - make_interval(secs => {stuck_secs})"
        ),
    )
    .await;

    let orphaned_worktrees = scalar(
        pool,
        "SELECT count(*) FROM work_item_worktrees wt \
           JOIN work_items w ON w.id = wt.work_item_id \
          WHERE wt.status IN ('active', 'creating') \
            AND w.status IN ('done', 'failed', 'cancelled')",
    )
    .await;

    let duplicate_leases = scalar(
        pool,
        "SELECT COALESCE(count(*), 0) FROM ( \
            SELECT work_item_id FROM work_item_leases \
             WHERE released_at IS NULL \
             GROUP BY work_item_id HAVING count(*) > 1 \
         ) d",
    )
    .await;

    let mut report = FleetHealthReport {
        stale_claims,
        stuck_building,
        orphaned_worktrees,
        duplicate_leases,
        healthy: false,
        summary: String::new(),
    };
    report.healthy = report.is_healthy();
    report.summary = if report.healthy {
        "pipeline healthy: no stale claims, stuck builds, orphaned worktrees, or duplicate leases"
            .to_string()
    } else {
        format!(
            "pipeline issues: {} stale claim(s), {} stuck build(s), {} orphaned worktree(s), {} duplicate lease(s)",
            report.stale_claims,
            report.stuck_building,
            report.orphaned_worktrees,
            report.duplicate_leases
        )
    };
    report
}

/// Run a `SELECT count(*)`-style scalar query; return -1 on error so a broken
/// sub-query is visibly non-zero (unhealthy) rather than silently 0 (healthy).
async fn scalar(pool: &PgPool, sql: &str) -> i64 {
    match sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, sql, "fleet_health_check: sub-query failed");
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_only_when_all_zero() {
        let ok = FleetHealthReport {
            stale_claims: 0,
            stuck_building: 0,
            orphaned_worktrees: 0,
            duplicate_leases: 0,
            healthy: false,
            summary: String::new(),
        };
        assert!(ok.is_healthy());
    }

    #[test]
    fn any_nonzero_is_unhealthy() {
        let bases = [
            FleetHealthReport {
                stale_claims: 1,
                ..Default::default()
            },
            FleetHealthReport {
                stuck_building: 2,
                ..Default::default()
            },
            FleetHealthReport {
                orphaned_worktrees: 3,
                ..Default::default()
            },
            FleetHealthReport {
                duplicate_leases: 1,
                ..Default::default()
            },
            // A failed sub-query (-1) must also read as unhealthy.
            FleetHealthReport {
                stale_claims: -1,
                ..Default::default()
            },
        ];
        for r in bases {
            assert!(!r.is_healthy(), "expected unhealthy: {r:?}");
        }
    }
}
