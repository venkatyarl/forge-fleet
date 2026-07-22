//! Self-heal requeue for work_items that terminally failed on TRANSIENT errors.
//!
//! The dispatch retry ladder (`work_item_dispatch::requeue_or_fail`) marks a
//! work_item terminal `failed` once its attempt budget is exhausted — even when
//! every attempt died on an INFRASTRUCTURE failure (backend spawn, heartbeat
//! takeover, DB pool, provider/network, host-resource exhaustion) rather than
//! anything wrong with the task itself. Those items are buildable once the
//! infra condition clears (creds fixed, node back online, rate-limit window
//! passed), so this sweep returns them to the ready pool for another try.
//!
//! Requeue restores FULL redispatch eligibility, mirroring what
//! `pg_reap_stale_work_item_leases` undoes on takeover: `pg_ready_work_items`
//! skips any item with an unreleased lease, and `pg_assign_work_item` can't
//! insert a second active lease past the partial-unique index — so flipping
//! `status` alone is NOT enough. Each requeue also releases active leases,
//! clears `assigned_to`/`assigned_computer`, fails live worktree rows, and
//! frees any slot still pointing at the item.

use anyhow::Result;
use sqlx::PgPool;
use tracing::info;

/// Error signatures marking a stored `last_error` as a TRANSIENT infrastructure
/// failure (vs a task-level failure — compile error, test failure, lint — that
/// retrying without a code change cannot fix). Shared with the dispatch retry
/// prompt: `work_item_dispatch::retry_error_is_actionable` treats exactly this
/// class as not-actionable. Signatures are consolidated from live dispatch
/// errors + an `ff council` (codex+kimi) pass; kept deliberately unambiguous so
/// a real Rust compile/test error is never matched.
pub const TRANSIENT_ERROR_SIGNATURES: &[&str] = &[
    // dispatch / backend spawn + routing
    "no dispatchable backend",
    "all backends failed on this node",
    "spawn \"",
    "command timed out",
    "timed out after",
    // heartbeat / lease lifecycle
    "stale-heartbeat",
    "heartbeat takeover",
    // datastore / pool
    "pool timed out",
    "pool timeout",
    "route deployments",
    // auth / provider / network (LLM endpoint or gh)
    "gh auth login",
    "bad credentials",
    "rate limit",
    "service unavailable",
    "internal server error",
    "connection refused",
    "network is unreachable",
    // host resource exhaustion
    "no space left",
    "cannot allocate memory",
    "too many open files",
    "resource temporarily unavailable",
    "worker died",
];

/// Whether a stored `last_error` matches a [`TRANSIENT_ERROR_SIGNATURES`]
/// infrastructure signature (case-insensitive substring match).
pub fn error_is_transient(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    TRANSIENT_ERROR_SIGNATURES
        .iter()
        .any(|sig| lower.contains(sig))
}

/// Total-attempt ceiling for self-heal requeues. MUST stay STRICTLY ABOVE the
/// dispatch/scheduler failure caps (`work_item_dispatch::MAX_DISPATCH_ATTEMPTS`
/// = `work_item_scheduler::MAX_BUILD_ATTEMPTS` = 5): a transiently-failed item
/// usually lands on `failed` AT that cap, so a ceiling at or below it would
/// never requeue anything. 8 = the 5 regular attempts + 3 self-heal retries;
/// past that the item stays terminally `failed` for a human.
pub const MAX_SELF_HEAL_ATTEMPTS: i32 = 8;

/// Max items returned to the ready pool per sweep (back-pressure: a fleet-wide
/// outage can terminally fail a large backlog at once; re-admit it gradually).
pub const SELF_HEAL_REQUEUE_BATCH: i64 = 16;

/// Skip items whose last lease released within this window. The transient
/// condition that failed them (dead creds, offline node, rate limit) rarely
/// clears in seconds, and the scheduler ticks every ~15s — without a cooldown
/// an item would burn its whole self-heal budget inside a single outage.
pub const SELF_HEAL_COOLDOWN_SECS: i64 = 600;

/// One self-heal sweep with the default knobs; called from the scheduler tick.
pub async fn requeue_transient_failures(pg: &PgPool) -> Result<u64> {
    requeue_transient_failures_with(
        pg,
        MAX_SELF_HEAL_ATTEMPTS,
        SELF_HEAL_REQUEUE_BATCH,
        SELF_HEAL_COOLDOWN_SECS,
    )
    .await
}

/// Requeue up to `batch` terminally-`failed` task work_items whose `last_error`
/// is transient (see [`TRANSIENT_ERROR_SIGNATURES`]) and whose `attempts` is
/// still under `max_attempts`, restoring full redispatch eligibility in one
/// transaction-equivalent statement. Returns the number of items requeued.
pub async fn requeue_transient_failures_with(
    pg: &PgPool,
    max_attempts: i32,
    batch: i64,
    cooldown_secs: i64,
) -> Result<u64> {
    // `%sig%` patterns for `LIKE ANY` — signatures contain no LIKE wildcards.
    let patterns: Vec<String> = TRANSIENT_ERROR_SIGNATURES
        .iter()
        .map(|sig| format!("%{sig}%"))
        .collect();

    let rows = sqlx::query_scalar::<_, uuid::Uuid>(
        "WITH candidates AS (
             SELECT w.id
               FROM work_items w
              WHERE w.status = 'failed'
                AND w.kind = 'task'
                AND COALESCE(w.attempts, 0) < $1
                -- Transient classification MUST sit here, BEFORE the LIMIT: a
                -- LIMIT over all failed items with classification applied
                -- afterwards lets a page of older non-transient failures starve
                -- every transient one behind it, forever.
                AND lower(COALESCE(w.last_error, '')) LIKE ANY($2)
                -- Cooldown: the most recent lease release approximates when the
                -- item failed (work_items has no failed-at stamp).
                AND NOT EXISTS (
                    SELECT 1 FROM work_item_leases lc
                     WHERE lc.work_item_id = w.id
                       AND lc.released_at > NOW() - make_interval(secs => $4))
              ORDER BY w.created_at ASC
              LIMIT $3
                FOR UPDATE SKIP LOCKED
         ), released_leases AS (
             UPDATE work_item_leases l
                SET lease_state = 'released',
                    released_at = NOW(),
                    release_reason = 'self-heal transient requeue'
               FROM candidates c
              WHERE l.work_item_id = c.id
                AND l.released_at IS NULL
         ), freed_slots AS (
             UPDATE sub_agents sa
                SET current_work_item_id = NULL,
                    status = 'idle'
               FROM candidates c
              WHERE sa.current_work_item_id = c.id
         ), failed_worktrees AS (
             UPDATE work_item_worktrees t
                SET status = 'failed'
               FROM candidates c
              WHERE t.work_item_id = c.id
                AND t.status IN ('creating', 'active')
         )
         UPDATE work_items w
            SET status = 'ready',
                attempts = COALESCE(w.attempts, 0) + 1,
                assigned_to = NULL,
                assigned_computer = NULL
           FROM candidates c
          WHERE w.id = c.id
      RETURNING w.id",
    )
    .bind(max_attempts)
    .bind(&patterns)
    .bind(batch)
    .bind(cooldown_secs as f64)
    .fetch_all(pg)
    .await?;

    if !rows.is_empty() {
        info!(
            requeued = rows.len(),
            max_attempts, batch, "self-heal: requeued transiently-failed work_items"
        );
    }
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::env;

    #[test]
    fn transient_classification_matrix() {
        for transient in [
            "connect: Connection refused (os error 111)",
            "codex: 429 Too Many Requests — rate limit exceeded",
            "stale-heartbeat takeover (attempt 3)",
            "pool timed out while waiting for an open connection",
            "No space left on device (os error 28)",
        ] {
            assert!(error_is_transient(transient), "{transient:?}");
        }
        for task_level in [
            "error[E0308]: mismatched types",
            "test result: FAILED. 1 passed; 2 failed",
            "cargo fmt --check found diffs",
        ] {
            assert!(!error_is_transient(task_level), "{task_level:?}");
        }
    }

    // -- DB tests: early-return (skip) when no Postgres is configured; CI's
    //    `cargo test --lib` has no database and must never panic here.

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_self_heal_requeue_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn create_temp_db() -> Option<(PgPool, PgPool, String)> {
        let (admin_url, db_url, db_name) = temp_db_urls()?;
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
        // Minimal slice of the live schema: only the tables + columns the
        // requeue statement touches (no cross-table FKs needed for the test).
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE work_items (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 kind TEXT NOT NULL DEFAULT 'task',
                 status TEXT NOT NULL,
                 attempts INT NOT NULL DEFAULT 0,
                 last_error TEXT,
                 assigned_to TEXT,
                 assigned_computer TEXT,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
             );
             CREATE TABLE work_item_leases (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 work_item_id UUID NOT NULL,
                 lease_state TEXT NOT NULL DEFAULT 'claimed',
                 released_at TIMESTAMPTZ,
                 release_reason TEXT
             );
             CREATE TABLE work_item_worktrees (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 work_item_id UUID NOT NULL,
                 status TEXT NOT NULL DEFAULT 'active'
             );
             CREATE TABLE sub_agents (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 current_work_item_id UUID,
                 status TEXT
             );",
        )
        .execute(&pool)
        .await
        .expect("create minimal work_item schema");
        Some((admin, pool, db_name))
    }

    async fn drop_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
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
        admin.close().await;
    }

    async fn insert_failed_item(
        pool: &PgPool,
        last_error: &str,
        attempts: i32,
        created_offset_secs: i64,
    ) -> uuid::Uuid {
        sqlx::query_scalar(
            "INSERT INTO work_items
                 (kind, status, attempts, last_error, assigned_to, assigned_computer, created_at)
             VALUES ('task', 'failed', $1, $2, 'slot-1', 'computer-1',
                     NOW() - make_interval(secs => $3))
          RETURNING id",
        )
        .bind(attempts)
        .bind(last_error)
        .bind(created_offset_secs as f64)
        .fetch_one(pool)
        .await
        .expect("insert failed work_item")
    }

    #[tokio::test]
    async fn requeue_clears_assignment_lease_worktree_and_slot() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!("skipping: FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL not set");
            return;
        };

        let transient =
            insert_failed_item(&pool, "dispatch: connection refused by endpoint", 5, 3600).await;
        let task_level = insert_failed_item(&pool, "error[E0308]: mismatched types", 5, 3600).await;
        let exhausted =
            insert_failed_item(&pool, "rate limit exceeded", MAX_SELF_HEAL_ATTEMPTS, 3600).await;
        let cooling = insert_failed_item(&pool, "service unavailable", 5, 3600).await;

        // Live residue on the transient item: an unreleased lease (blocks both
        // pg_ready_work_items and a new pg_assign_work_item lease), an active
        // worktree, and a slot still pointing at it.
        sqlx::query("INSERT INTO work_item_leases (work_item_id) VALUES ($1)")
            .bind(transient)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO work_item_worktrees (work_item_id, status) VALUES ($1, 'active')")
            .bind(transient)
            .execute(&pool)
            .await
            .unwrap();
        let slot: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO sub_agents (current_work_item_id, status)
             VALUES ($1, 'busy') RETURNING id",
        )
        .bind(transient)
        .fetch_one(&pool)
        .await
        .unwrap();
        // A lease released moments ago puts `cooling` inside the cooldown window.
        sqlx::query(
            "INSERT INTO work_item_leases (work_item_id, lease_state, released_at)
             VALUES ($1, 'failed', NOW() - INTERVAL '10 seconds')",
        )
        .bind(cooling)
        .execute(&pool)
        .await
        .unwrap();

        let requeued = requeue_transient_failures(&pool).await.expect("requeue");
        assert_eq!(requeued, 1);

        let row = sqlx::query(
            "SELECT status, attempts, assigned_to, assigned_computer
               FROM work_items WHERE id = $1",
        )
        .bind(transient)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("status"), "ready");
        assert_eq!(row.get::<i32, _>("attempts"), 6);
        assert_eq!(row.get::<Option<String>, _>("assigned_to"), None);
        assert_eq!(row.get::<Option<String>, _>("assigned_computer"), None);

        let lease = sqlx::query(
            "SELECT lease_state, released_at, release_reason
               FROM work_item_leases WHERE work_item_id = $1",
        )
        .bind(transient)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(lease.get::<String, _>("lease_state"), "released");
        assert!(
            lease
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("released_at")
                .is_some()
        );
        assert_eq!(
            lease.get::<Option<String>, _>("release_reason").as_deref(),
            Some("self-heal transient requeue")
        );

        let worktree_status: String =
            sqlx::query_scalar("SELECT status FROM work_item_worktrees WHERE work_item_id = $1")
                .bind(transient)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(worktree_status, "failed");

        let slot_row =
            sqlx::query("SELECT current_work_item_id, status FROM sub_agents WHERE id = $1")
                .bind(slot)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            slot_row.get::<Option<uuid::Uuid>, _>("current_work_item_id"),
            None
        );
        assert_eq!(
            slot_row.get::<Option<String>, _>("status").as_deref(),
            Some("idle")
        );

        // Task-level, attempt-exhausted, and cooling-down items stay failed.
        for (id, why) in [
            (task_level, "task-level error"),
            (exhausted, "attempts at ceiling"),
            (cooling, "inside cooldown window"),
        ] {
            let status: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(status, "failed", "{why} must not requeue");
        }

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn transient_requeue_not_starved_by_older_nontransient_failures() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!("skipping: FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL not set");
            return;
        };

        // Regression (retry attempt 2): a batch LIMIT applied before transient
        // classification returned only the OLDEST failed items — all
        // non-transient here — and the newer transient failure never requeued.
        let batch = 3i64;
        for i in 0..(batch + 2) {
            insert_failed_item(
                &pool,
                "error[E0308]: mismatched types",
                5,
                86_400 + i * 60, // older than the transient item below
            )
            .await;
        }
        let transient = insert_failed_item(&pool, "network is unreachable", 5, 3600).await;

        let requeued = requeue_transient_failures_with(&pool, MAX_SELF_HEAL_ATTEMPTS, batch, 600)
            .await
            .expect("requeue");
        assert_eq!(requeued, 1);

        let status: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = $1")
            .bind(transient)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "ready");

        drop_temp_db(admin, pool, &db_name).await;
    }
}
