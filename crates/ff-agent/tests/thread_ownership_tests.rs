//! Integration test for [`ff_agent::thread_ownership`] against a real
//! Postgres instance: single-writer acquire/reclaim, presence heartbeats,
//! causal sequencing, and redaction on `workstream_threads`.
//!
//! Skips (rather than panics) when neither `FORGEFLEET_POSTGRES_URL` nor
//! `FORGEFLEET_DATABASE_URL` is set, since CI's `cargo test --lib`/`--tests`
//! run has no database available.

use std::env;
use std::time::Duration;

use ff_agent::thread_ownership::{
    AcquireOutcome, heartbeat_presence, next_causal_seq, redact_thread, release_thread_owner,
    try_acquire_thread_owner,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn temp_db_urls(name_prefix: &str) -> Option<(String, String, String)> {
    let base_url = env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
        .ok()?;
    let (prefix, _) = base_url.rsplit_once('/')?;
    let db_name = format!("{name_prefix}_{}", Uuid::new_v4().simple());
    Some((
        format!("{prefix}/postgres"),
        format!("{prefix}/{db_name}"),
        db_name,
    ))
}

/// Minimal schema mirroring the live `workstream_threads` table plus the
/// concurrency-control columns added by `SCHEMA_V269_WORKSTREAM_THREAD_CONCURRENCY`.
async fn create_thread_ownership_test_db() -> Option<(PgPool, PgPool, String)> {
    let (admin_url, db_url, db_name) = temp_db_urls("ff_thread_owner_it")?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect admin db");
    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin)
        .await
        .expect("create temp db");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await
        .expect("connect temp db");
    sqlx::raw_sql(
        "CREATE EXTENSION IF NOT EXISTS pgcrypto;
         CREATE TABLE workstream_threads (
             id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
             workstream_id      UUID NOT NULL,
             label              TEXT,
             status             TEXT NOT NULL DEFAULT 'active',
             opened_by_session  TEXT,
             focus              TEXT,
             created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
             owner_session      TEXT,
             owner_acquired_at  TIMESTAMPTZ,
             last_seen_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
             causal_seq         BIGINT NOT NULL DEFAULT 0,
             redacted_at        TIMESTAMPTZ,
             redacted_reason    TEXT
         );",
    )
    .execute(&pool)
    .await
    .expect("create minimal workstream_threads schema");
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
    .expect("terminate temp db sessions");
    sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
        .execute(&admin)
        .await
        .expect("drop temp db");
    admin.close().await;
}

async fn insert_thread(pool: &PgPool, workstream_id: Uuid) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO workstream_threads (workstream_id, label) VALUES ($1, 'test-thread') \
         RETURNING id",
    )
    .bind(workstream_id)
    .fetch_one(pool)
    .await
    .expect("insert thread");
    id
}

#[tokio::test]
async fn single_writer_enforced_until_owner_releases_or_goes_stale() {
    let Some((admin, pool, db_name)) = create_thread_ownership_test_db().await else {
        eprintln!(
            "skipping thread_ownership integration test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let thread_id = insert_thread(&pool, Uuid::new_v4()).await;

    // First session acquires exclusively.
    let outcome = try_acquire_thread_owner(&pool, thread_id, "session-a", Duration::from_secs(60))
        .await
        .expect("acquire for session-a");
    assert_eq!(outcome, AcquireOutcome::Granted);

    // A second session must be refused while session-a's presence is fresh —
    // thread exclusivity / single-writer.
    let outcome = try_acquire_thread_owner(&pool, thread_id, "session-b", Duration::from_secs(60))
        .await
        .expect("attempt acquire for session-b");
    assert_eq!(
        outcome,
        AcquireOutcome::HeldByOther {
            owner_session: "session-a".to_string()
        }
    );

    // session-a re-acquiring (e.g. renewing before a write) is idempotent.
    let outcome = try_acquire_thread_owner(&pool, thread_id, "session-a", Duration::from_secs(60))
        .await
        .expect("re-acquire for session-a");
    assert_eq!(outcome, AcquireOutcome::Granted);

    // Once session-a releases, session-b can acquire.
    release_thread_owner(&pool, thread_id, "session-a")
        .await
        .expect("release session-a");
    let outcome = try_acquire_thread_owner(&pool, thread_id, "session-b", Duration::from_secs(60))
        .await
        .expect("acquire for session-b after release");
    assert_eq!(outcome, AcquireOutcome::Granted);

    drop_temp_db(admin, pool, &db_name).await;
}

#[tokio::test]
async fn stale_owner_presence_is_reclaimed() {
    let Some((admin, pool, db_name)) = create_thread_ownership_test_db().await else {
        eprintln!(
            "skipping thread_ownership integration test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let thread_id = insert_thread(&pool, Uuid::new_v4()).await;

    try_acquire_thread_owner(&pool, thread_id, "session-a", Duration::from_secs(60))
        .await
        .expect("acquire for session-a");

    // Force session-a's presence heartbeat into the past to simulate a dead
    // client that never released its lock.
    sqlx::query(
        "UPDATE workstream_threads SET last_seen_at = now() - INTERVAL '5 minutes' WHERE id = $1",
    )
    .bind(thread_id)
    .execute(&pool)
    .await
    .expect("age presence");

    // A short lease TTL means session-a's stale presence must be reclaimable.
    let outcome = try_acquire_thread_owner(&pool, thread_id, "session-b", Duration::from_secs(30))
        .await
        .expect("reclaim from stale owner");
    assert_eq!(outcome, AcquireOutcome::Granted);

    // session-a can no longer heartbeat — it lost ownership.
    let still_owner = heartbeat_presence(&pool, thread_id, "session-a")
        .await
        .expect("heartbeat attempt for evicted owner");
    assert!(!still_owner, "evicted owner must not be able to heartbeat");

    drop_temp_db(admin, pool, &db_name).await;
}

#[tokio::test]
async fn causal_seq_is_monotonic_per_thread() {
    let Some((admin, pool, db_name)) = create_thread_ownership_test_db().await else {
        eprintln!(
            "skipping thread_ownership integration test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let thread_id = insert_thread(&pool, Uuid::new_v4()).await;

    let first = next_causal_seq(&pool, thread_id)
        .await
        .expect("first causal seq");
    let second = next_causal_seq(&pool, thread_id)
        .await
        .expect("second causal seq");
    let third = next_causal_seq(&pool, thread_id)
        .await
        .expect("third causal seq");

    assert_eq!(first, 1);
    assert_eq!(second, 2);
    assert_eq!(third, 3);

    drop_temp_db(admin, pool, &db_name).await;
}

#[tokio::test]
async fn redaction_tombstones_content_and_releases_owner() {
    let Some((admin, pool, db_name)) = create_thread_ownership_test_db().await else {
        eprintln!(
            "skipping thread_ownership integration test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let thread_id = insert_thread(&pool, Uuid::new_v4()).await;
    try_acquire_thread_owner(&pool, thread_id, "session-a", Duration::from_secs(60))
        .await
        .expect("acquire before redaction");

    redact_thread(&pool, thread_id, "operator-requested scrub")
        .await
        .expect("redact thread");

    let row: (
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT label, focus, status, owner_session, redacted_reason \
               FROM workstream_threads WHERE id = $1",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("read redacted row");

    assert_eq!(row.0, None, "label must be scrubbed");
    assert_eq!(row.1, None, "focus must be scrubbed");
    assert_eq!(row.2, "redacted");
    assert_eq!(row.3, None, "redaction must release any owner");
    assert_eq!(row.4, Some("operator-requested scrub".to_string()));

    drop_temp_db(admin, pool, &db_name).await;
}
