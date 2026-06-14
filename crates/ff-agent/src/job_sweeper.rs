//! Stale-job sweeper — mark jobs stuck in `running` status for too long as failed.
//!
//! Jobs can get stuck in `running` if the process crashes mid-execution, if a
//! download stalls without HTTP error, or if an SSH connection drops after a
//! `deferred_tasks` claim. The sweeper recovers them so operators (and the
//! adaptive router) don't keep waiting on work that will never finish.

use chrono::{DateTime, Duration, Utc};

/// Summary of a sweep pass.
#[derive(Debug, Clone, Default)]
pub struct SweepSummary {
    pub jobs_failed: usize,
    pub deferred_failed: usize,
}

/// Configuration for what counts as "stale".
#[derive(Debug, Clone)]
pub struct SweepPolicy {
    /// A model_job is stale if `started_at` is older than this with status=running.
    pub job_stale_after: Duration,
    /// A deferred task is stale if `claimed_at` is older than this with status=running.
    pub deferred_stale_after: Duration,
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self {
            // Downloads can legitimately take 30+ min for big models; be conservative.
            job_stale_after: Duration::hours(2),
            // Shell deferred tasks should finish in minutes; if they don't, something's wrong.
            deferred_stale_after: Duration::minutes(30),
        }
    }
}

/// Run one sweep pass. Marks stale jobs as failed with a descriptive error.
/// Returns counts.
pub async fn sweep_stale(
    pool: &sqlx::PgPool,
    policy: &SweepPolicy,
) -> Result<SweepSummary, String> {
    let now: DateTime<Utc> = Utc::now();
    let mut summary = SweepSummary::default();

    // ── fleet_model_jobs ─────────────────────────────────────────────────
    let job_cutoff = now - policy.job_stale_after;
    let jobs = ff_db::pg_list_jobs(pool, Some("running"), 1000)
        .await
        .map_err(|e| format!("pg_list_jobs: {e}"))?;
    for j in &jobs {
        let started = j.started_at.unwrap_or(j.created_at);
        if started < job_cutoff {
            let elapsed = now - started;
            let err = format!(
                "job marked failed by sweeper — stuck in 'running' for {}h{}m",
                elapsed.num_hours(),
                elapsed.num_minutes() % 60
            );
            if let Err(e) = ff_db::pg_update_job_progress(
                pool,
                &j.id,
                Some("failed"),
                None,
                None,
                None,
                None,
                Some(&err),
            )
            .await
            {
                tracing::warn!("pg_update_job_progress({}): {e}", j.id);
            } else {
                summary.jobs_failed += 1;
            }
        }
    }

    // ── deferred_tasks ───────────────────────────────────────────────────
    // deferred_tasks has status='running' for claimed tasks. If claimed_at is
    // old, something crashed — reset to pending or mark failed (depending on attempts).
    let def_cutoff = now - policy.deferred_stale_after;
    let stuck_deferred = sqlx::query(
        "SELECT id, attempts, max_attempts, claimed_at FROM deferred_tasks
          WHERE status = 'running' AND claimed_at < $1
          LIMIT 500",
    )
    .bind(def_cutoff)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("list stuck deferred: {e}"))?;

    for row in &stuck_deferred {
        let id: sqlx::types::Uuid = row.get("id");
        let attempts: i32 = row.get("attempts");
        let max_attempts: i32 = row.get("max_attempts");
        let error_msg = "worker died mid-run (sweeper recovery)";
        let new_status = if attempts >= max_attempts {
            "failed"
        } else {
            "pending"
        };
        let update = sqlx::query(
            "UPDATE deferred_tasks
                SET status = $1,
                    last_error = $2,
                    claimed_by = NULL,
                    claimed_at = NULL,
                    next_attempt_at = NOW() + INTERVAL '2 minutes'
              WHERE id = $3",
        )
        .bind(new_status)
        .bind(error_msg)
        .bind(id)
        .execute(pool)
        .await;
        if let Err(e) = update {
            tracing::warn!("pg sweep update {id}: {e}");
        } else {
            summary.deferred_failed += 1;
        }
    }

    Ok(summary)
}

// Add the trait import that the raw query rows need.
use sqlx::Row as _;

#[cfg(test)]
mod tests {
    use super::*;

    /// These thresholds were proven in production for months under the legacy
    /// `ff daemon` and are preserved verbatim by the relocation into
    /// forgefleetd. The deferred threshold MUST stay generous enough not to
    /// reap a legitimately long-running deferred task; the model-job threshold
    /// covers multi-GB HF downloads. Pin them so a careless edit can't silently
    /// weaken recovery (too short → kills live work; too long → orphans leak).
    #[test]
    fn default_policy_thresholds_are_stable() {
        let p = SweepPolicy::default();
        assert_eq!(p.job_stale_after, Duration::hours(2));
        assert_eq!(p.deferred_stale_after, Duration::minutes(30));
    }

    #[test]
    fn sweep_interval_and_leader_window_are_sane() {
        assert_eq!(SWEEP_INTERVAL, std::time::Duration::from_secs(300));
        assert_eq!(LEADER_FRESH_SECS, 60);
    }
}

/// How often the sweeper runs in production. Mirrors the 5-minute cadence the
/// legacy `ff daemon` used so orphaned `running` rows are recovered promptly.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// The leader's Postgres `heartbeat_at` must be fresher than this for us to
/// consider ourselves the live leader. Matches the 60s window the other
/// leader-gated forgefleetd ticks (amcheck, summary-refresh) use.
const LEADER_FRESH_SECS: i64 = 60;

/// Production stale-job sweeper tick for `forgefleetd`.
///
/// Recovers `fleet_model_jobs` and `deferred_tasks` rows stuck in `running`
/// (process crashed, download stalled, or — the case that motivated moving this
/// here — a worker was restarted mid-task by the upgrade wave and left its own
/// `claimed_by=self` rows orphaned forever). Until 2026-06-14 this only ran
/// inside the legacy `ff daemon`; the legacy-daemon reaper (PR #298) disabled
/// every legacy `ff daemon` fleet-wide, which silently killed the only host of
/// this sweep and let orphaned `deferred_tasks` leak. Per
/// [`feedback_two_daemons`] production ticks must live in `src/main.rs`
/// (forgefleetd), so the sweep is relocated here with the SAME
/// [`SweepPolicy::default`] thresholds it has used in production for months —
/// a pure relocation, not a policy change.
///
/// Leader-gated on every fire (NOT at spawn): `sweep_stale` is a fleet-wide DB
/// operation, so only the live leader runs it — avoiding N redundant sweeps and
/// duplicate logging. Safe to start on every daemon; followers no-op.
pub struct StaleJobSweeperTick {
    pg: sqlx::PgPool,
    my_name: String,
    policy: SweepPolicy,
}

impl StaleJobSweeperTick {
    pub fn new(pg: sqlx::PgPool, my_name: String) -> Self {
        Self {
            pg,
            my_name,
            policy: SweepPolicy::default(),
        }
    }

    /// Are we the live leader right now? True iff the `fleet_leader_state`
    /// singleton names us AND its heartbeat is fresh.
    async fn is_live_leader(&self) -> bool {
        match ff_db::leader_state::pg_get_current_leader(&self.pg).await {
            Ok(Some(leader)) => {
                let fresh = Utc::now()
                    .signed_duration_since(leader.heartbeat_at)
                    .num_seconds()
                    < LEADER_FRESH_SECS;
                leader.member_name == self.my_name && fresh
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "stale-job sweeper: failed to read leader state");
                false
            }
        }
    }

    /// Spawn the 5-minute sweep loop. Leadership is gated inside the loop on
    /// every fire, so this is safe to start on every daemon.
    pub fn spawn(
        self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_live_leader().await {
                            continue;
                        }
                        match sweep_stale(&self.pg, &self.policy).await {
                            Ok(s) if s.jobs_failed + s.deferred_failed > 0 => tracing::info!(
                                jobs_failed = s.jobs_failed,
                                deferred_failed = s.deferred_failed,
                                "stale-job sweeper: recovered stuck running rows"
                            ),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "stale-job sweeper: pass failed"),
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            tracing::info!("stale-job sweeper shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}
