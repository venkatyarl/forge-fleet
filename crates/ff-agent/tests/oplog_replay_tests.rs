use ff_agent::oplog_replay::ReplayController;
use sqlx::{PgPool, postgres::PgPoolOptions};

fn db_url() -> Option<String> {
    std::env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        .ok()
}

fn temp_db_urls() -> Option<(String, String, String)> {
    let base_url = db_url()?;
    let (prefix, _) = base_url.rsplit_once('/')?;
    let db_name = format!("ff_oplog_replay_test_{}", uuid::Uuid::new_v4().simple());
    Some((
        format!("{prefix}/postgres"),
        format!("{prefix}/{db_name}"),
        db_name,
    ))
}

async fn create_temp_db() -> Option<(PgPool, PgPool, String)> {
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
    ff_db::run_postgres_migrations(&pool)
        .await
        .expect("run migrations");
    Some((admin, pool, db_name))
}

async fn drop_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
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

#[tokio::test]
async fn replay_merges_lww_and_union_end_to_end() {
    let Some((admin, pool, db_name)) = create_temp_db().await else {
        eprintln!("skipping OpLog replay integration test: no Postgres URL configured");
        return;
    };

    sqlx::query(
        "INSERT INTO isolated_node_oplog
            (node_id, sequence, entity_type, entity_id, field_name, merge_strategy,
             value, observed_at, writer_id)
         VALUES
            ('node-a', 1, 'candidate', 'cand-1', 'status', 'LWW',
             '\"screen\"'::jsonb, '2026-07-27T10:00:00Z', 'node-a'),
            ('node-a', 2, 'candidate', 'cand-1', 'tags', 'UNION',
             '[\"union\", \"remote\"]'::jsonb, '2026-07-27T10:01:00Z', 'node-a'),
            ('node-a', 3, 'candidate', 'cand-1', 'status', 'LWW',
             '\"offer\"'::jsonb, '2026-07-27T10:02:00Z', 'node-a'),
            ('node-a', 4, 'candidate', 'cand-1', 'tags', 'UNION',
             '[\"remote\", \"senior\"]'::jsonb, '2026-07-27T10:03:00Z', 'node-a')",
    )
    .execute(&pool)
    .await
    .expect("seed isolated oplog");

    let controller = ReplayController::new(pool.clone()).with_batch_size(10);
    let report = controller
        .replay_node("node-a")
        .await
        .expect("replay node-a");
    assert_eq!(report.applied, 4);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.last_sequence, 4);
    assert_eq!(report.state_version, 1);

    let status: (serde_json::Value, i64) = sqlx::query_as(
        "SELECT value, version
           FROM oplog_shared_state
          WHERE entity_type = 'candidate'
            AND entity_id = 'cand-1'
            AND field_name = 'status'",
    )
    .fetch_one(&pool)
    .await
    .expect("read status state");
    assert_eq!(status.0, serde_json::json!("offer"));
    assert_eq!(status.1, 2);

    let tags: (serde_json::Value, i64) = sqlx::query_as(
        "SELECT value, version
           FROM oplog_shared_state
          WHERE entity_type = 'candidate'
            AND entity_id = 'cand-1'
            AND field_name = 'tags'",
    )
    .fetch_one(&pool)
    .await
    .expect("read tags state");
    assert_eq!(tags.0, serde_json::json!(["remote", "senior", "union"]));
    assert_eq!(tags.1, 2);

    let second = controller
        .replay_node("node-a")
        .await
        .expect("second replay is idempotent");
    assert_eq!(second.applied, 0);
    assert_eq!(second.last_sequence, 4);
    assert_eq!(second.state_version, 1);

    drop_temp_db(admin, pool, &db_name).await;
}

#[tokio::test]
async fn replay_gap_records_failed_checkpoint_without_partial_apply() {
    let Some((admin, pool, db_name)) = create_temp_db().await else {
        eprintln!("skipping OpLog replay gap test: no Postgres URL configured");
        return;
    };

    sqlx::query(
        "INSERT INTO isolated_node_oplog
            (node_id, sequence, entity_type, entity_id, field_name, merge_strategy,
             value, observed_at, writer_id)
         VALUES
            ('node-gap', 2, 'candidate', 'cand-2', 'status', 'LWW',
             '\"screen\"'::jsonb, '2026-07-27T10:00:00Z', 'node-gap')",
    )
    .execute(&pool)
    .await
    .expect("seed gapped oplog");

    let err = ReplayController::new(pool.clone())
        .replay_node("node-gap")
        .await
        .expect_err("gap should fail replay");
    assert!(err.to_string().contains("oplog gap"));

    let checkpoint: (i64, String, i64, Option<String>) = sqlx::query_as(
        "SELECT last_sequence, state, state_version, last_error
           FROM oplog_replay_checkpoints
          WHERE node_id = 'node-gap'",
    )
    .fetch_one(&pool)
    .await
    .expect("read failed checkpoint");
    assert_eq!(checkpoint.0, 0);
    assert_eq!(checkpoint.1, "failed");
    assert_eq!(checkpoint.2, 1);
    assert!(
        checkpoint
            .3
            .as_deref()
            .is_some_and(|error| error.contains("expected sequence 1"))
    );

    let applied_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM oplog_replay_applied WHERE node_id = 'node-gap'")
            .fetch_one(&pool)
            .await
            .expect("count applied rows");
    assert_eq!(applied_count, 0);

    drop_temp_db(admin, pool, &db_name).await;
}
