//! Integration tests for the Postgres migration runner.

use sqlx::postgres::PgPoolOptions;

fn db_url() -> Option<String> {
    std::env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        .ok()
}

fn temp_db_urls() -> Option<(String, String, String)> {
    let base_url = db_url()?;
    let (prefix, _) = base_url.rsplit_once('/')?;
    let db_name = format!("ff_migration_test_{}", uuid::Uuid::new_v4().simple());
    Some((
        format!("{prefix}/postgres"),
        format!("{prefix}/{db_name}"),
        db_name,
    ))
}

async fn create_temp_db() -> Option<(sqlx::PgPool, sqlx::PgPool, String)> {
    let (admin_url, db_url, db_name) = temp_db_urls()?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect to admin database");
    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin)
        .await
        .expect("create temp database");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await
        .expect("connect to temp database");
    Some((admin, pool, db_name))
}

async fn drop_temp_db(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
    pool.close().await;
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid)
           FROM pg_stat_activity
          WHERE datname = $1
            AND pid <> pg_backend_pid()",
    )
    .bind(db_name)
    .execute(&admin)
    .await;
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
        .execute(&admin)
        .await;
    admin.close().await;
}

/// Fresh Postgres bootstrap must apply every embedded migration, including the
/// v161 `canonical_github_alias` baseline, and must be safe to run again
/// without replay conflicts.
#[tokio::test]
async fn migration_fresh_bootstrap_starts_from_v161_baseline_and_is_idempotent() {
    let Some((admin, pool, db_name)) = create_temp_db().await else {
        eprintln!(
            "skipping migration fresh bootstrap test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
        );
        return;
    };

    let run = || async {
        ff_db::run_postgres_migrations(&pool)
            .await
            .expect("run postgres migrations")
    };

    // First run on a fresh DB: every pending migration should apply.
    let first_version = run().await;
    assert!(
        first_version >= 161,
        "expected fresh bootstrap to reach at least v161, got {first_version}"
    );

    // The v161 baseline must be recorded in the migrations tracking table.
    let v161_recorded: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _migrations WHERE version = 161)")
            .fetch_one(&pool)
            .await
            .expect("query _migrations for v161");
    assert!(
        v161_recorded,
        "v161 (canonical_github_alias) must be recorded in _migrations after fresh bootstrap"
    );

    // Second run must be a no-op and must not fail with a replay conflict on
    // the _migrations primary key.
    let second_version = run().await;
    assert_eq!(
        first_version, second_version,
        "re-running migrations on an up-to-date DB must be idempotent"
    );

    // The runner's reported version must agree with the tracking table.
    let max_version: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _migrations")
        .fetch_one(&pool)
        .await
        .expect("query max migration version");
    assert_eq!(
        max_version as u32, second_version,
        "_migrations table must agree with the runner's reported version"
    );

    drop_temp_db(admin, pool, &db_name).await;
}

/// V176 adds merge train tracking tables. Verify they are created by the
/// migration and support the expected insert patterns.
#[tokio::test]
async fn v176_merge_train_tables_are_created() {
    let Some((admin, pool, db_name)) = create_temp_db().await else {
        eprintln!("skipping v176 merge train test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
        return;
    };

    ff_db::run_postgres_migrations(&pool)
        .await
        .expect("run postgres migrations");

    // Insert a minimal project/work_item so foreign keys resolve.
    let project_id = "v176-test-project";
    sqlx::query("INSERT INTO projects (id, display_name, status) VALUES ($1, $2, 'active')")
        .bind(project_id)
        .bind("v176 test project")
        .execute(&pool)
        .await
        .expect("insert test project");

    let work_item_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO work_items (project_id, kind, title, created_by)
         VALUES ($1, 'task', 'v176 test', 'test') RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("insert test work item");

    let queue_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO work_item_merge_queue (work_item_id, project_id, branch_name)
         VALUES ($1, $2, 'v176-branch') RETURNING id",
    )
    .bind(work_item_id)
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("insert test merge queue entry");

    let train_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO merge_trains (project_id, base_branch, status)
         VALUES ($1, 'main', 'assembling') RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("insert merge train");

    let member_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO merge_train_members
         (train_id, work_item_id, queue_id, position, branch_name, status)
         VALUES ($1, $2, $3, 1, 'v176-branch', 'pending') RETURNING id",
    )
    .bind(train_id)
    .bind(work_item_id)
    .bind(queue_id)
    .fetch_one(&pool)
    .await
    .expect("insert merge train member");

    // Link the queue entry back to the train.
    sqlx::query("UPDATE work_item_merge_queue SET train_id = $1 WHERE id = $2")
        .bind(train_id)
        .bind(queue_id)
        .execute(&pool)
        .await
        .expect("link queue entry to train");

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM merge_train_members WHERE train_id = $1")
            .bind(train_id)
            .fetch_one(&pool)
            .await
            .expect("count merge train members");
    assert_eq!(count, 1, "expected one member in the train");

    let linked_train: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT train_id FROM work_item_merge_queue WHERE id = $1")
            .bind(queue_id)
            .fetch_one(&pool)
            .await
            .expect("read queue entry train_id");
    assert_eq!(
        linked_train,
        Some(train_id),
        "queue entry must reference its train"
    );

    // Clean up the member explicitly so the train can be dropped before the
    // work_item/queue rows (foreign keys are CASCADE, but the explicit delete
    // makes the test intent clear).
    sqlx::query("DELETE FROM merge_train_members WHERE id = $1")
        .bind(member_id)
        .execute(&pool)
        .await
        .expect("delete merge train member");

    drop_temp_db(admin, pool, &db_name).await;
}
