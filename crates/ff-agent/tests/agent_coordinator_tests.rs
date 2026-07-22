//! Integration test for [`ff_agent::sub_agent_reaper`] against a real Postgres
//! instance: a `'busy'` slot whose heartbeat/`started_at` has gone stale past
//! the reaper's ceiling must be reset to `'idle'`, while a fresh busy slot and
//! a busy slot backed by an ACTIVE lease must be left alone.
//!
//! Skips (rather than panics) when neither `FORGEFLEET_POSTGRES_URL` nor
//! `FORGEFLEET_DATABASE_URL` is set, since CI's `cargo test --lib`/`--tests`
//! run has no database available.

use std::env;

use ff_agent::leader_cache::{LeaderCache, LeaderInfo};
use ff_agent::sub_agent_reaper::SubAgentReaper;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
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

/// Minimal slot/lease schema mirroring `SCHEMA_V23_SUB_AGENTS` — just enough
/// for `SubAgentReaper::run_once` (and the lease reconcile it also runs) to
/// operate against.
async fn create_reaper_test_db() -> Option<(PgPool, PgPool, String)> {
    let (admin_url, db_url, db_name) = temp_db_urls("ff_slot_reaper_it")?;
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
         CREATE TABLE computers (
             id   UUID PRIMARY KEY,
             name TEXT NOT NULL
         );
         CREATE TABLE work_items (
             id     UUID PRIMARY KEY,
             status TEXT NOT NULL DEFAULT 'ready'
         );
         CREATE TABLE sub_agents (
             id                   UUID PRIMARY KEY,
             computer_id          UUID NOT NULL REFERENCES computers(id),
             slot                 INT NOT NULL,
             status               TEXT NOT NULL DEFAULT 'idle',
             current_work_item_id UUID REFERENCES work_items(id),
             started_at           TIMESTAMPTZ,
             workspace_dir        TEXT NOT NULL DEFAULT '',
             last_heartbeat_at    TIMESTAMPTZ
         );
         CREATE TABLE work_item_leases (
             id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
             work_item_id     UUID NOT NULL REFERENCES work_items(id),
             sub_agent_id     UUID REFERENCES sub_agents(id),
             computer_id      UUID NOT NULL REFERENCES computers(id),
             lease_state      TEXT NOT NULL DEFAULT 'claimed',
             lease_expires_at TIMESTAMPTZ NOT NULL DEFAULT NOW() + INTERVAL '1 hour',
             heartbeat_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
             created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
             released_at      TIMESTAMPTZ,
             release_reason   TEXT
         );",
    )
    .execute(&pool)
    .await
    .expect("create minimal slot/lease schema");
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

#[tokio::test]
async fn reaper_resets_stale_busy_slots_with_stale_heartbeats() {
    let Some((admin, pool, db_name)) = create_reaper_test_db().await else {
        eprintln!(
            "skipping sub-agent reaper integration test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let computer = Uuid::new_v4();
    sqlx::query("INSERT INTO computers (id, name) VALUES ($1, 'testbox')")
        .bind(computer)
        .execute(&pool)
        .await
        .expect("insert computer");

    // slot 0: 'busy' with a stale heartbeat/started_at well past the 60-min
    // ceiling and no active lease — the reaper must reset it to idle.
    let stale_slot = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sub_agents (id, computer_id, slot, status, started_at, last_heartbeat_at)
         VALUES ($1, $2, 0, 'busy', NOW() - INTERVAL '90 minutes', NOW() - INTERVAL '90 minutes')",
    )
    .bind(stale_slot)
    .bind(computer)
    .execute(&pool)
    .await
    .expect("insert stale busy slot");

    // slot 1: 'busy' but still well within the ceiling — a legitimately
    // running task that must NOT be reaped mid-run.
    let fresh_slot = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sub_agents (id, computer_id, slot, status, started_at, last_heartbeat_at)
         VALUES ($1, $2, 1, 'busy', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes')",
    )
    .bind(fresh_slot)
    .bind(computer)
    .execute(&pool)
    .await
    .expect("insert fresh busy slot");

    // slot 2: 'busy' with a stale started_at/heartbeat, but it holds an ACTIVE
    // lease — the lease lifecycle owns it, so the reaper must leave it alone.
    let leased_slot = Uuid::new_v4();
    let leased_work_item = Uuid::new_v4();
    sqlx::query("INSERT INTO work_items (id, status) VALUES ($1, 'building')")
        .bind(leased_work_item)
        .execute(&pool)
        .await
        .expect("insert leased work item");
    sqlx::query(
        "INSERT INTO sub_agents
             (id, computer_id, slot, status, current_work_item_id, started_at, last_heartbeat_at)
         VALUES ($1, $2, 2, 'busy', $3, NOW() - INTERVAL '90 minutes', NOW() - INTERVAL '90 minutes')",
    )
    .bind(leased_slot)
    .bind(computer)
    .bind(leased_work_item)
    .execute(&pool)
    .await
    .expect("insert leased busy slot");
    sqlx::query(
        "INSERT INTO work_item_leases (work_item_id, sub_agent_id, computer_id)
         VALUES ($1, $2, $3)",
    )
    .bind(leased_work_item)
    .bind(leased_slot)
    .bind(computer)
    .execute(&pool)
    .await
    .expect("insert active lease");

    // The reaper gates on leader election via a process-global cache; mark
    // this process as leader so `run_once` actually does its sweep.
    LeaderCache::global()
        .update_state(true, LeaderInfo::default())
        .await;

    let reaper = SubAgentReaper::new(pool.clone(), "test-node".to_string());
    let reaped = reaper.run_once().await.expect("run reaper tick");
    assert!(
        reaped >= 1,
        "expected at least the stale busy slot to be reaped, got {reaped}"
    );

    let rows =
        sqlx::query("SELECT slot, status, current_work_item_id FROM sub_agents ORDER BY slot")
            .fetch_all(&pool)
            .await
            .expect("read slots back");

    let stale: (String, Option<Uuid>) = (
        rows[0].get::<String, _>("status"),
        rows[0].get::<Option<Uuid>, _>("current_work_item_id"),
    );
    assert_eq!(stale.0, "idle", "stale busy slot must be reset to idle");
    assert_eq!(stale.1, None, "reset slot must clear current_work_item_id");

    assert_eq!(
        rows[1].get::<String, _>("status"),
        "busy",
        "a busy slot within the staleness ceiling must not be reaped"
    );

    assert_eq!(
        rows[2].get::<String, _>("status"),
        "busy",
        "a busy slot with an ACTIVE lease must not be reaped"
    );
    assert_eq!(
        rows[2].get::<Option<Uuid>, _>("current_work_item_id"),
        Some(leased_work_item)
    );

    drop_temp_db(admin, pool, &db_name).await;
}
