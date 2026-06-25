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
    /// `research_sessions` rows recovered (stuck in a non-terminal status).
    pub research_sessions_failed: usize,
    /// `research_subtasks` rows recovered (stuck in `running`).
    pub research_subtasks_failed: usize,
    /// `fleet_model_deployments` rows flipped `healthy` → `unhealthy` because
    /// their `last_health_at` went stale (the node wedged/went offline and its
    /// per-node reconciler stopped refreshing them).
    pub deployments_marked_unhealthy: usize,
    /// Terminal (`completed`/`cancelled`/`failed`) `deferred_tasks` rows deleted
    /// past the retention window — keeps the queue table from growing unbounded.
    pub deferred_pruned: usize,
}

/// Configuration for what counts as "stale".
#[derive(Debug, Clone)]
pub struct SweepPolicy {
    /// A model_job is stale if `started_at` is older than this with status=running.
    pub job_stale_after: Duration,
    /// A deferred task is stale if `claimed_at` is older than this with status=running.
    pub deferred_stale_after: Duration,
    /// A research session is stale if `created_at` is older than this while its
    /// status is still non-terminal (`planning`/`dispatching`). Unlike
    /// `deferred_tasks`, research sub-agents run *inside* the foreground
    /// `ff research` process — there is no worker to re-claim them — so if that
    /// process dies the session and its `running` subtasks are orphaned forever.
    pub research_stale_after: Duration,
    /// A `healthy` deployment is stale if its `last_health_at` is older than
    /// this. The per-node `deployment_reconciler` refreshes `last_health_at`
    /// every ~60s; a host that wedges/goes offline can NEVER flip its own
    /// deployments to unhealthy, so the row lingers as `healthy` with a frozen
    /// `last_health_at` and every observability surface (pulse, dashboards,
    /// `ff fleet route`) keeps reporting a dead endpoint as live. The live
    /// dispatch pickers already refuse such rows past
    /// `DISPATCH_HEALTH_MAX_AGE_SEC` (300s, PR #332) — this is the slower,
    /// persistent data-correctness flip, deliberately 2× the dispatch floor so
    /// a brief reconciler blip never flaps the stored state. A recovered node's
    /// own reconciler re-flips it `healthy` with a fresh `last_health_at`, so
    /// the flip is self-correcting.
    pub deployment_health_stale_after: Duration,
    /// Terminal (`completed`/`cancelled`/`failed`) `deferred_tasks` rows older
    /// than this are deleted. The table is an operational work queue, not a
    /// history store — without a retention cap it grows forever (48k rows /
    /// 6 weeks observed, ~450 HA-backup rsync rows/day alone), bloating the
    /// table and slowing every `pg_list_deferred` / claim scan (the worker, the
    /// autoscaler enqueue guard, version-check). 14 days keeps ample recent
    /// failure history for debugging while bounding growth.
    pub terminal_retention: Duration,
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self {
            // Downloads can legitimately take 30+ min for big models; be conservative.
            job_stale_after: Duration::hours(2),
            // Shell deferred tasks should finish in minutes; if they don't, something's wrong.
            deferred_stale_after: Duration::minutes(30),
            // A live `ff research` run (planner + N parallel sub-agents at the
            // default depth on slow local models) can take a while; 1h is well
            // past any legitimate run, so only genuinely orphaned sessions
            // (process killed/crashed) qualify. Aged off `created_at`, so a
            // session stuck in `planning` because the planner itself died is
            // recovered too.
            research_stale_after: Duration::hours(1),
            // 2× the live-dispatch freshness floor (DISPATCH_HEALTH_MAX_AGE_SEC
            // = 300s, PR #332): dispatch stops routing to a stale endpoint
            // first (cheap, reversible, no stored change), and only if it stays
            // stale this long does the persistent healthy→unhealthy flip fire —
            // so a transient one-tick reconciler miss never flaps the data.
            deployment_health_stale_after: Duration::minutes(10),
            // Keep two weeks of terminal queue history, then prune.
            terminal_retention: Duration::days(14),
        }
    }
}

/// Statuses that are TERMINAL (the task is done and will never run again) and
/// therefore safe to prune past the retention window. Deliberately excludes
/// `running`/`pending`/`dispatchable`/`claimed` (live work the sweeper recovers
/// rather than deletes).
pub fn is_prunable_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "cancelled" | "failed")
}

/// Max terminal rows deleted per sweep pass — bounds the DELETE so draining a
/// large backlog never takes a long lock; steady-state churn is well under this.
const TERMINAL_PRUNE_BATCH: i64 = 5000;

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

    // ── deferred_tasks retention prune ───────────────────────────────────
    // Terminal rows (completed/cancelled/failed) are never otherwise deleted,
    // so the queue table grows forever. Delete those older than the retention
    // window, bounded to TERMINAL_PRUNE_BATCH per pass (ctid sub-select) so
    // draining a large backlog never holds a long lock. Aged off created_at
    // (every row has it; completed_at can be NULL for sweeper-failed rows).
    let prune_cutoff = now - policy.terminal_retention;
    match sqlx::query(
        "DELETE FROM deferred_tasks WHERE ctid IN (
             SELECT ctid FROM deferred_tasks
              WHERE status IN ('completed', 'cancelled', 'failed')
                AND created_at < $1
              LIMIT $2)",
    )
    .bind(prune_cutoff)
    .bind(TERMINAL_PRUNE_BATCH)
    .execute(pool)
    .await
    {
        Ok(r) => summary.deferred_pruned = r.rows_affected() as usize,
        Err(e) => tracing::warn!("deferred_tasks retention prune: {e}"),
    }

    // ── research_sessions / research_subtasks ────────────────────────────
    // `ff research` decomposes + dispatches + synthesizes all inside ONE
    // foreground process: the sub-agent loops are not daemon-managed, so if that
    // process is killed/crashes, the session is left in a non-terminal status
    // (`planning` if the planner died, `dispatching` after) and its sub-agents'
    // rows stay `running` forever — no worker ever re-claims them. (Observed:
    // 25-day-old `planning` sessions accumulating.) Recover both, gated on the
    // SESSION's `created_at` so never-started `pending` subtasks are covered too.
    let research_cutoff = now - policy.research_stale_after;

    // 1) Fail the orphaned sub-agent rows of stale sessions first, so a
    //    re-run/inspection sees consistent terminal state.
    let subtasks = sqlx::query(
        "UPDATE research_subtasks st
            SET status = 'failed',
                completed_at = NOW(),
                error = COALESCE(st.error, 'reaped by sweeper — research orchestrator process died (stuck running)')
           FROM research_sessions s
          WHERE st.session_id = s.id
            AND st.status = 'running'
            AND s.status NOT IN ('done', 'failed')
            AND s.created_at < $1",
    )
    .bind(research_cutoff)
    .execute(pool)
    .await;
    match subtasks {
        Ok(r) => summary.research_subtasks_failed = r.rows_affected() as usize,
        Err(e) => tracing::warn!("pg sweep research_subtasks: {e}"),
    }

    // 2) Fail the stale sessions themselves.
    let sessions = sqlx::query(
        "UPDATE research_sessions
            SET status = 'failed',
                completed_at = NOW(),
                error = COALESCE(error, 'reaped by sweeper — orchestrator process died before synthesis (stuck in non-terminal status)')
          WHERE status NOT IN ('done', 'failed')
            AND created_at < $1",
    )
    .bind(research_cutoff)
    .execute(pool)
    .await;
    match sessions {
        Ok(r) => summary.research_sessions_failed = r.rows_affected() as usize,
        Err(e) => tracing::warn!("pg sweep research_sessions: {e}"),
    }

    // ── fleet_model_deployments (stale-healthy → unhealthy) ──────────────
    // A wedged/offline node's per-node reconciler stops refreshing
    // `last_health_at`, so the deployment is stuck reporting `healthy` to every
    // observability surface even though nothing answers there. Flip the
    // provably-stale ones to `unhealthy` so the stored state matches reality;
    // the node's own reconciler re-flips it back with a fresh timestamp on
    // recovery, so this is self-correcting (and only ever touches rows the live
    // dispatch pickers were already skipping). Rows with a NULL `last_health_at`
    // were never datable — leave them for the dispatch NULL-passes / reconciler.
    let deploy_cutoff = now - policy.deployment_health_stale_after;
    let deployments = sqlx::query(
        "UPDATE fleet_model_deployments
            SET health_status = 'unhealthy'
          WHERE health_status = 'healthy'
            AND last_health_at IS NOT NULL
            AND last_health_at < $1",
    )
    .bind(deploy_cutoff)
    .execute(pool)
    .await;
    match deployments {
        Ok(r) => summary.deployments_marked_unhealthy = r.rows_affected() as usize,
        Err(e) => tracing::warn!("pg sweep fleet_model_deployments: {e}"),
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
        // Research sessions: generous enough not to reap a live `ff research`
        // run, short enough that orphans (process killed) clear within the hour.
        assert_eq!(p.research_stale_after, Duration::hours(1));
        // Deployment health flip: must stay strictly LONGER than the live
        // dispatch freshness floor (DISPATCH_HEALTH_MAX_AGE_SEC = 300s) so the
        // cheap reversible router skip always fires before the persistent
        // healthy→unhealthy write — otherwise a transient reconciler blip
        // flaps the stored state.
        assert_eq!(p.deployment_health_stale_after, Duration::minutes(10));
        assert!(
            p.deployment_health_stale_after.num_seconds()
                > ff_db::queries::DISPATCH_HEALTH_MAX_AGE_SEC as i64
        );
        // Retention must be long enough to keep useful recent history but
        // finite so the queue table can't grow unbounded.
        assert_eq!(p.terminal_retention, Duration::days(14));
    }

    #[test]
    fn only_terminal_statuses_are_prunable() {
        // Live/recoverable states must NEVER be deleted by the retention prune —
        // the sweeper recovers those, it doesn't drop them.
        for s in ["completed", "cancelled", "failed"] {
            assert!(is_prunable_terminal_status(s), "{s} should be prunable");
        }
        for s in ["pending", "running", "dispatchable", "claimed"] {
            assert!(!is_prunable_terminal_status(s), "{s} must NOT be prunable");
        }
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

/// Max research sessions to auto-recover per sweep pass. Each recovery is one
/// gateway synthesis call, so this caps the LLM work a single tick can drive;
/// a backlog drains across successive 5-minute passes.
const RESEARCH_AUTO_RECOVER_LIMIT: i64 = 3;

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
                            Ok(s) if s.jobs_failed
                                + s.deferred_failed
                                + s.research_sessions_failed
                                + s.research_subtasks_failed
                                + s.deployments_marked_unhealthy
                                + s.deferred_pruned > 0 => tracing::info!(
                                jobs_failed = s.jobs_failed,
                                deferred_failed = s.deferred_failed,
                                research_sessions_failed = s.research_sessions_failed,
                                research_subtasks_failed = s.research_subtasks_failed,
                                deployments_marked_unhealthy = s.deployments_marked_unhealthy,
                                deferred_pruned = s.deferred_pruned,
                                "stale-job sweeper: recovered stuck running rows + pruned terminal queue"
                            ),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "stale-job sweeper: pass failed"),
                        }

                        // Autonomously finish research runs the sweep just (or
                        // previously) reaped to `failed`: their sub-agent work
                        // survives in `research_subtasks`, so synthesize the
                        // report without waiting for an operator to run
                        // `ff research --recover`. Bounded per session.
                        match crate::research::auto_recover_stale(
                            &self.pg,
                            crate::research::MAX_AUTO_RECOVER_ATTEMPTS,
                            RESEARCH_AUTO_RECOVER_LIMIT,
                        )
                        .await
                        {
                            Ok(r) if r.attempted > 0 => tracing::info!(
                                attempted = r.attempted,
                                recovered = r.recovered,
                                failed = r.failed,
                                "stale-job sweeper: auto-recovered reaped research sessions"
                            ),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "auto-recover research: pass failed"),
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
