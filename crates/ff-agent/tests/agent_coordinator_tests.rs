//! Integration test for [`ff_agent::sub_agent_reaper`] against a real Postgres
//! instance: a `'busy'` slot whose heartbeat/`started_at` has gone stale past
//! the reaper's ceiling must be reset to `'idle'`, while a fresh busy slot and
//! a busy slot backed by an ACTIVE lease must be left alone.
//!
//! Skips (rather than panics) when neither `FORGEFLEET_POSTGRES_URL` nor
//! `FORGEFLEET_DATABASE_URL` is set, since CI's `cargo test --lib`/`--tests`
//! run has no database available.

use std::env;
use std::time::Duration;

use ff_agent::agent_coordinator::reap_stale_busy_slots;
use ff_agent::leader_cache::{LeaderCache, LeaderInfo};
use ff_agent::sub_agent_reaper::SubAgentReaper;
use ff_agent::work_item_dispatch::run_git;
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
             id                UUID PRIMARY KEY,
             status            TEXT NOT NULL DEFAULT 'ready',
             assigned_to       TEXT,
             assigned_computer TEXT
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
         );
         CREATE TABLE work_item_worktrees (
             id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
             work_item_id  UUID NOT NULL REFERENCES work_items(id),
             computer_id   UUID NOT NULL REFERENCES computers(id),
             sub_agent_id  UUID REFERENCES sub_agents(id),
             repo_path     TEXT NOT NULL,
             worktree_path TEXT NOT NULL,
             base_branch   TEXT NOT NULL,
             task_branch   TEXT NOT NULL,
             status        TEXT NOT NULL DEFAULT 'creating',
             created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
             cleaned_at    TIMESTAMPTZ
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

#[tokio::test]
async fn stale_slot_reaper_requeues_orphaned_work_item() {
    let Some((admin, pool, db_name)) = create_reaper_test_db().await else {
        eprintln!(
            "skipping stale slot reaper integration test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let computer = Uuid::new_v4();
    sqlx::query("INSERT INTO computers (id, name) VALUES ($1, 'testbox')")
        .bind(computer)
        .execute(&pool)
        .await
        .expect("insert computer");

    // slot 0: busy with a stale heartbeat and an in-flight (non-terminal) work
    // item — the reaper must free the slot AND re-queue the item as 'ready'.
    let stale_slot = Uuid::new_v4();
    let orphaned_item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO work_items (id, status, assigned_to, assigned_computer)
         VALUES ($1, 'building', 'sub-agent-testbox:0', 'testbox')",
    )
    .bind(orphaned_item)
    .execute(&pool)
    .await
    .expect("insert orphaned work item");
    sqlx::query(
        "INSERT INTO sub_agents
             (id, computer_id, slot, status, current_work_item_id, started_at, last_heartbeat_at)
         VALUES ($1, $2, 0, 'busy', $3, NOW() - INTERVAL '90 minutes', NOW() - INTERVAL '90 minutes')",
    )
    .bind(stale_slot)
    .bind(computer)
    .bind(orphaned_item)
    .execute(&pool)
    .await
    .expect("insert stale busy slot");

    // slot 1: busy with a FRESH heartbeat — must not be touched.
    let fresh_slot = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sub_agents (id, computer_id, slot, status, started_at, last_heartbeat_at)
         VALUES ($1, $2, 1, 'busy', NOW() - INTERVAL '2 minutes', NOW() - INTERVAL '2 minutes')",
    )
    .bind(fresh_slot)
    .bind(computer)
    .execute(&pool)
    .await
    .expect("insert fresh busy slot");

    // slot 2: stale heartbeat but an ACTIVE lease — the lease lifecycle owns
    // it, so the reaper must leave both the slot and its item alone.
    let leased_slot = Uuid::new_v4();
    let leased_item = Uuid::new_v4();
    sqlx::query("INSERT INTO work_items (id, status) VALUES ($1, 'building')")
        .bind(leased_item)
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
    .bind(leased_item)
    .execute(&pool)
    .await
    .expect("insert leased busy slot");
    sqlx::query(
        "INSERT INTO work_item_leases (work_item_id, sub_agent_id, computer_id)
         VALUES ($1, $2, $3)",
    )
    .bind(leased_item)
    .bind(leased_slot)
    .bind(computer)
    .execute(&pool)
    .await
    .expect("insert active lease");

    // 60-min ceiling, same value the daemon's 60s tick passes.
    let reaped = reap_stale_busy_slots(&pool, 3600)
        .await
        .expect("run stale slot reap");
    assert_eq!(
        reaped.len(),
        1,
        "exactly the lease-less stale slot: {reaped:?}"
    );
    assert_eq!(reaped[0].sub_agent_id, stale_slot);
    assert_eq!(reaped[0].work_item_id, Some(orphaned_item));
    assert!(
        reaped[0].requeued,
        "in-flight orphaned item must be re-queued"
    );

    let slot_row = sqlx::query("SELECT status, current_work_item_id FROM sub_agents WHERE id = $1")
        .bind(stale_slot)
        .fetch_one(&pool)
        .await
        .expect("read reaped slot");
    assert_eq!(slot_row.get::<String, _>("status"), "idle");
    assert_eq!(
        slot_row.get::<Option<Uuid>, _>("current_work_item_id"),
        None
    );

    let item_row =
        sqlx::query("SELECT status, assigned_to, assigned_computer FROM work_items WHERE id = $1")
            .bind(orphaned_item)
            .fetch_one(&pool)
            .await
            .expect("read requeued item");
    assert_eq!(item_row.get::<String, _>("status"), "ready");
    assert_eq!(item_row.get::<Option<String>, _>("assigned_to"), None);
    assert_eq!(item_row.get::<Option<String>, _>("assigned_computer"), None);

    let fresh_status: String = sqlx::query("SELECT status FROM sub_agents WHERE id = $1")
        .bind(fresh_slot)
        .fetch_one(&pool)
        .await
        .expect("read fresh slot")
        .get("status");
    assert_eq!(
        fresh_status, "busy",
        "fresh-heartbeat slot must not be reaped"
    );

    let leased_status: String = sqlx::query("SELECT status FROM sub_agents WHERE id = $1")
        .bind(leased_slot)
        .fetch_one(&pool)
        .await
        .expect("read leased slot")
        .get("status");
    assert_eq!(
        leased_status, "busy",
        "actively-leased slot must not be reaped"
    );

    drop_temp_db(admin, pool, &db_name).await;
}

/// Sets up a minimal real git repo with `main` checked out and committed,
/// then branches off `task_branch` with an uncommitted change — mimicking a
/// clone-direct slot clone left mid-build by a hung/crashed task.
fn init_clone_direct_repo(repo: &std::path::Path, task_branch: &str) {
    run_git(repo, ["init"], Duration::from_secs(10)).unwrap();
    run_git(
        repo,
        ["config", "user.name", "Test"],
        Duration::from_secs(10),
    )
    .unwrap();
    run_git(
        repo,
        ["config", "user.email", "test@example.com"],
        Duration::from_secs(10),
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "base").unwrap();
    run_git(repo, ["add", "-A"], Duration::from_secs(10)).unwrap();
    run_git(repo, ["commit", "-m", "base"], Duration::from_secs(10)).unwrap();
    run_git(repo, ["branch", "-M", "main"], Duration::from_secs(10)).unwrap();
    run_git(
        repo,
        ["checkout", "-b", task_branch],
        Duration::from_secs(10),
    )
    .unwrap();
    std::fs::write(repo.join("scratch.txt"), "half-finished build output").unwrap();
}

#[tokio::test]
async fn stale_slot_reaper_resets_orphaned_clone_direct_worktree() {
    let Some((admin, pool, db_name)) = create_reaper_test_db().await else {
        eprintln!(
            "skipping stale slot worktree cleanup test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let computer = Uuid::new_v4();
    sqlx::query("INSERT INTO computers (id, name) VALUES ($1, 'testbox')")
        .bind(computer)
        .execute(&pool)
        .await
        .expect("insert computer");

    let stale_slot = Uuid::new_v4();
    let orphaned_item = Uuid::new_v4();
    sqlx::query("INSERT INTO work_items (id, status) VALUES ($1, 'building')")
        .bind(orphaned_item)
        .execute(&pool)
        .await
        .expect("insert orphaned work item");
    sqlx::query(
        "INSERT INTO sub_agents
             (id, computer_id, slot, status, current_work_item_id, started_at, last_heartbeat_at)
         VALUES ($1, $2, 0, 'busy', $3, NOW() - INTERVAL '90 minutes', NOW() - INTERVAL '90 minutes')",
    )
    .bind(stale_slot)
    .bind(computer)
    .bind(orphaned_item)
    .execute(&pool)
    .await
    .expect("insert stale busy slot");

    let repo_tmp = tempfile::tempdir().unwrap();
    let repo_path = repo_tmp.path();
    let task_branch = "feature/orphaned-task-abcd";
    init_clone_direct_repo(repo_path, task_branch);
    let repo_path_str = repo_path.to_string_lossy().to_string();

    let worktree_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO work_item_worktrees
             (id, work_item_id, computer_id, sub_agent_id, repo_path, worktree_path,
              base_branch, task_branch, status)
         VALUES ($1, $2, $3, $4, $5, $5, 'main', $6, 'active')",
    )
    .bind(worktree_id)
    .bind(orphaned_item)
    .bind(computer)
    .bind(stale_slot)
    .bind(&repo_path_str)
    .bind(task_branch)
    .execute(&pool)
    .await
    .expect("insert clone-direct worktree row");

    let reaped = reap_stale_busy_slots(&pool, 3600)
        .await
        .expect("run stale slot reap");
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].sub_agent_id, stale_slot);

    // Clone-direct: the slot's persistent clone is reset in place, never deleted.
    assert!(
        repo_path.exists(),
        "clone-direct repo directory must be preserved for reuse"
    );
    let branches = run_git(
        repo_path,
        ["branch", "--list", task_branch],
        Duration::from_secs(10),
    )
    .expect("list branches");
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "abandoned task branch must be deleted from the slot's clone"
    );

    let worktree_row =
        sqlx::query("SELECT status, cleaned_at FROM work_item_worktrees WHERE id = $1")
            .bind(worktree_id)
            .fetch_one(&pool)
            .await
            .expect("read worktree row");
    assert_eq!(worktree_row.get::<String, _>("status"), "cleaned");
    assert!(
        worktree_row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("cleaned_at")
            .is_some(),
        "cleaned_at must be stamped"
    );

    drop_temp_db(admin, pool, &db_name).await;
}

#[tokio::test]
async fn stale_slot_reaper_removes_legacy_worktree_and_build_artifacts() {
    let Some((admin, pool, db_name)) = create_reaper_test_db().await else {
        eprintln!(
            "skipping stale slot legacy worktree cleanup test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let computer = Uuid::new_v4();
    sqlx::query("INSERT INTO computers (id, name) VALUES ($1, 'testbox')")
        .bind(computer)
        .execute(&pool)
        .await
        .expect("insert computer");

    let stale_slot = Uuid::new_v4();
    let orphaned_item = Uuid::new_v4();
    sqlx::query("INSERT INTO work_items (id, status) VALUES ($1, 'building')")
        .bind(orphaned_item)
        .execute(&pool)
        .await
        .expect("insert orphaned work item");
    sqlx::query(
        "INSERT INTO sub_agents
             (id, computer_id, slot, status, current_work_item_id, started_at, last_heartbeat_at)
         VALUES ($1, $2, 0, 'busy', $3, NOW() - INTERVAL '90 minutes', NOW() - INTERVAL '90 minutes')",
    )
    .bind(stale_slot)
    .bind(computer)
    .bind(orphaned_item)
    .execute(&pool)
    .await
    .expect("insert stale busy slot");

    // Legacy pre-clone-direct layout: a detached worktree dir sitting inside
    // the shared repo clone, left with a leftover `target/` build artifact.
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo_path = repo_tmp.path();
    let task_branch = "feature/legacy-orphan-abcd";
    init_clone_direct_repo(repo_path, task_branch);
    // Roll the shared clone back to main; the "worktree" below stands in for
    // the separate detached-worktree directory from the legacy layout.
    run_git(repo_path, ["checkout", "main"], Duration::from_secs(10)).unwrap();

    let worktree_dir = repo_tmp.path().join("legacy-worktree");
    std::fs::create_dir_all(worktree_dir.join("target")).unwrap();
    std::fs::write(worktree_dir.join("target").join("build.bin"), "junk").unwrap();
    std::fs::write(worktree_dir.join("scratch.txt"), "leftover").unwrap();

    let worktree_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO work_item_worktrees
             (id, work_item_id, computer_id, sub_agent_id, repo_path, worktree_path,
              base_branch, task_branch, status)
         VALUES ($1, $2, $3, $4, $5, $6, 'main', $7, 'active')",
    )
    .bind(worktree_id)
    .bind(orphaned_item)
    .bind(computer)
    .bind(stale_slot)
    .bind(repo_path.to_string_lossy().to_string())
    .bind(worktree_dir.to_string_lossy().to_string())
    .bind(task_branch)
    .execute(&pool)
    .await
    .expect("insert legacy worktree row");

    let reaped = reap_stale_busy_slots(&pool, 3600)
        .await
        .expect("run stale slot reap");
    assert_eq!(reaped.len(), 1);

    assert!(
        !worktree_dir.exists(),
        "legacy detached worktree directory (and its build artifacts) must be removed"
    );

    let worktree_row = sqlx::query("SELECT status FROM work_item_worktrees WHERE id = $1")
        .bind(worktree_id)
        .fetch_one(&pool)
        .await
        .expect("read worktree row");
    assert_eq!(worktree_row.get::<String, _>("status"), "cleaned");

    drop_temp_db(admin, pool, &db_name).await;
}
