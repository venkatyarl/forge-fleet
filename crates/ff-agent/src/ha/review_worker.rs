//! Distributed review worker — claims merge-queue entries for PR review.
//!
//! Any node may run one. Workers compete on unclaimed `work_item_merge_queue`
//! rows via `FOR UPDATE SKIP LOCKED`, so each row is reviewed by at most one
//! worker at a time with no leader election. A claim stamps
//! `review_claimed_at` + `review_claimed_by` and acts as a TTL lease: a worker
//! that dies mid-review never clears its stamp, so
//! [`release_expired_review_claims`] nulls claims older than the TTL and the
//! row becomes claimable again (same recovery model as the deferred-task
//! queue's claim protocol).

use anyhow::Result;
use sqlx::{PgPool, Row};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Default lease TTL for a review claim. Generous enough for the slow path
/// (480B review + cloud confirm of a rejection); an expired claim only means
/// the row is offered to another worker, not that the review is failed.
pub const DEFAULT_REVIEW_CLAIM_TTL_SECS: i64 = 900;

/// Merge-queue statuses whose rows are eligible for a review claim — the
/// non-terminal states where a PR exists and a verdict is still actionable.
const CLAIMABLE_STATUSES: &str = "('queued', 'ci_running', 'mergeable')";

/// A merge-queue entry claimed for review by this worker.
#[derive(Debug, Clone)]
pub struct ClaimedReviewItem {
    pub queue_id: uuid::Uuid,
    pub work_item_id: uuid::Uuid,
    pub project_id: String,
    pub pr_url: String,
    pub branch_name: String,
}

/// Atomically claim the next unclaimed merge-queue entry for review.
///
/// Single-statement CTE claim: the `FOR UPDATE SKIP LOCKED` select and the
/// `review_claimed_at` stamp commit together, so two workers can never claim
/// the same row — a locked candidate is skipped, not waited on. Rows without a
/// `pr_url` are ignored (nothing to review) rather than claimed and burned.
pub async fn claim_next_review_item(
    pool: &PgPool,
    worker_name: &str,
) -> Result<Option<ClaimedReviewItem>> {
    let row = sqlx::query(&format!(
        "WITH next AS (
             SELECT id FROM work_item_merge_queue
              WHERE status IN {CLAIMABLE_STATUSES}
                AND review_claimed_at IS NULL
                AND pr_url IS NOT NULL
              ORDER BY position ASC
              FOR UPDATE SKIP LOCKED
              LIMIT 1
         )
         UPDATE work_item_merge_queue AS q
            SET review_claimed_at = NOW(),
                review_claimed_by = $1
           FROM next
          WHERE q.id = next.id
      RETURNING q.id, q.work_item_id, q.project_id, q.pr_url, q.branch_name"
    ))
    .bind(worker_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ClaimedReviewItem {
        queue_id: r.get("id"),
        work_item_id: r.get("work_item_id"),
        project_id: r.get("project_id"),
        pr_url: r.get("pr_url"),
        branch_name: r.get("branch_name"),
    }))
}

/// Release a claim this worker holds (review finished or abandoned), making
/// the row immediately claimable again. Returns true if a claim was cleared.
pub async fn release_review_claim(pool: &PgPool, queue_id: uuid::Uuid) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE work_item_merge_queue \
            SET review_claimed_at = NULL, review_claimed_by = NULL \
          WHERE id = $1 AND review_claimed_at IS NOT NULL",
    )
    .bind(queue_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Release claims older than `ttl_secs` on still-claimable rows so work held
/// by a dead worker is re-offered to the fleet. Terminal rows keep their stamp
/// as a record of who reviewed them. Returns the number of leases released.
pub async fn release_expired_review_claims(pool: &PgPool, ttl_secs: i64) -> Result<u64> {
    let res = sqlx::query(&format!(
        "UPDATE work_item_merge_queue \
            SET review_claimed_at = NULL, review_claimed_by = NULL \
          WHERE review_claimed_at IS NOT NULL \
            AND review_claimed_at < NOW() - make_interval(secs => $1) \
            AND status IN {CLAIMABLE_STATUSES}"
    ))
    .bind(ttl_secs as f64)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// One worker tick: reap expired leases fleet-wide, then try to claim the next
/// reviewable entry for this worker. Returns the claimed item, if any.
pub async fn run_review_worker_tick(
    pool: &PgPool,
    worker_name: &str,
    ttl_secs: i64,
) -> Result<Option<ClaimedReviewItem>> {
    let released = release_expired_review_claims(pool, ttl_secs).await?;
    if released > 0 {
        tracing::info!(
            released,
            ttl_secs,
            "review_worker: released expired review leases"
        );
    }
    claim_next_review_item(pool, worker_name).await
}

/// Spawn the background claim loop. No leader gating: every node may run one;
/// SKIP LOCKED plus the `review_claimed_at` stamp keep claims exclusive.
pub fn spawn(
    pg: PgPool,
    worker_name: String,
    poll_interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(poll_interval.max(Duration::from_secs(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match run_review_worker_tick(&pg, &worker_name, DEFAULT_REVIEW_CLAIM_TTL_SECS)
                        .await
                    {
                        Ok(Some(item)) => {
                            tracing::info!(
                                queue_id = %item.queue_id,
                                work_item = %item.work_item_id,
                                pr = %item.pr_url,
                                "review_worker: claimed merge-queue entry for review"
                            );
                        }
                        Ok(None) => {
                            tracing::debug!("review_worker: nothing to claim");
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "review_worker tick failed");
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn db_env_missing() -> bool {
        // CI's `cargo test --lib` has no Postgres — DB tests must early-return.
        env::var("FORGEFLEET_POSTGRES_URL").is_err() && env::var("FORGEFLEET_DATABASE_URL").is_err()
    }

    fn temp_db_urls() -> (String, String, String) {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .expect("FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL must be set for DB tests");
        let (prefix, _) = base_url
            .rsplit_once('/')
            .expect("database URL must end with /<db>");
        let db_name = format!("ff_review_worker_{}", uuid::Uuid::new_v4().simple());
        (
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        )
    }

    async fn create_temp_db() -> (sqlx::PgPool, sqlx::PgPool, String) {
        let (admin_url, db_url, db_name) = temp_db_urls();
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&db_url)
            .await
            .expect("connect temp db");
        // Minimal work_item_merge_queue (FKs dropped) — just what the claim
        // protocol touches.
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE work_item_merge_queue (
                 id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 work_item_id      UUID NOT NULL,
                 project_id        TEXT NOT NULL,
                 position          BIGSERIAL,
                 status            TEXT NOT NULL DEFAULT 'queued',
                 branch_name       TEXT NOT NULL,
                 pr_url            TEXT,
                 review_claimed_at TIMESTAMPTZ,
                 review_claimed_by TEXT
             );",
        )
        .execute(&pool)
        .await
        .expect("create minimal merge queue schema");
        (admin, pool, db_name)
    }

    async fn drop_temp_db(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .ok();
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .ok();
    }

    async fn insert_entry(pool: &sqlx::PgPool, status: &str, pr_url: Option<&str>) -> uuid::Uuid {
        let row = sqlx::query(
            "INSERT INTO work_item_merge_queue \
                (work_item_id, project_id, status, branch_name, pr_url) \
             VALUES (gen_random_uuid(), 'proj-1', $1, 'ff/branch', $2) \
             RETURNING id",
        )
        .bind(status)
        .bind(pr_url)
        .fetch_one(pool)
        .await
        .expect("insert merge queue entry");
        row.get("id")
    }

    #[tokio::test]
    async fn claim_is_exclusive_and_skips_ineligible_rows() {
        if db_env_missing() {
            eprintln!("skipping review_worker DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        insert_entry(&pool, "merged", Some("https://github.com/x/y/pull/1")).await;
        insert_entry(&pool, "queued", None).await; // no PR — nothing to review
        let claimable = insert_entry(&pool, "queued", Some("https://github.com/x/y/pull/3")).await;

        let item = claim_next_review_item(&pool, "worker-a")
            .await
            .expect("claim")
            .expect("one claimable row");
        assert_eq!(item.queue_id, claimable);
        assert_eq!(item.pr_url, "https://github.com/x/y/pull/3");

        // Claimed row is invisible to a second worker.
        let second = claim_next_review_item(&pool, "worker-b")
            .await
            .expect("second claim");
        assert!(second.is_none());

        let claimed_by: Option<String> =
            sqlx::query_scalar("SELECT review_claimed_by FROM work_item_merge_queue WHERE id = $1")
                .bind(claimable)
                .fetch_one(&pool)
                .await
                .expect("read claim stamp");
        assert_eq!(claimed_by.as_deref(), Some("worker-a"));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn expired_lease_is_released_and_reclaimable() {
        if db_env_missing() {
            eprintln!("skipping review_worker DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        insert_entry(&pool, "queued", Some("https://github.com/x/y/pull/1")).await;
        let item = claim_next_review_item(&pool, "worker-a")
            .await
            .expect("claim")
            .expect("claimable row");

        // Fresh lease: nothing to release, row stays claimed.
        let released = release_expired_review_claims(&pool, DEFAULT_REVIEW_CLAIM_TTL_SECS)
            .await
            .expect("release pass");
        assert_eq!(released, 0);

        // Backdate the lease past the TTL — as if worker-a died mid-review.
        sqlx::query(
            "UPDATE work_item_merge_queue \
                SET review_claimed_at = NOW() - INTERVAL '1 hour' WHERE id = $1",
        )
        .bind(item.queue_id)
        .execute(&pool)
        .await
        .expect("backdate lease");

        let released = release_expired_review_claims(&pool, DEFAULT_REVIEW_CLAIM_TTL_SECS)
            .await
            .expect("release pass");
        assert_eq!(released, 1);

        let reclaimed = claim_next_review_item(&pool, "worker-b")
            .await
            .expect("reclaim")
            .expect("released row is claimable again");
        assert_eq!(reclaimed.queue_id, item.queue_id);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn explicit_release_reopens_claim() {
        if db_env_missing() {
            eprintln!("skipping review_worker DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        insert_entry(&pool, "ci_running", Some("https://github.com/x/y/pull/2")).await;
        let item = claim_next_review_item(&pool, "worker-a")
            .await
            .expect("claim")
            .expect("claimable row");

        assert!(
            release_review_claim(&pool, item.queue_id)
                .await
                .expect("release")
        );
        // Releasing an unclaimed row is a no-op.
        assert!(
            !release_review_claim(&pool, item.queue_id)
                .await
                .expect("second release")
        );

        let again = claim_next_review_item(&pool, "worker-b")
            .await
            .expect("reclaim")
            .expect("released row is claimable");
        assert_eq!(again.queue_id, item.queue_id);

        drop_temp_db(admin, pool, &db_name).await;
    }
}
