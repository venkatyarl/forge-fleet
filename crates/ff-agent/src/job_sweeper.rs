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
                pool, &j.id, Some("failed"), None, None, None, None, Some(&err),
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
        let new_status = if attempts >= max_attempts { "failed" } else { "pending" };
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
