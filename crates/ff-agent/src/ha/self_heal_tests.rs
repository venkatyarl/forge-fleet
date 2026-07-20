use std::env;

use chrono::{DateTime, Utc};
use sqlx::Row;

use super::self_heal::rearm_self_heal_task;

#[tokio::test]
async fn same_bug_signature_rearms_multiple_times() {
    let database_url = match env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
    {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping self-heal re-arm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL");
            return;
        }
    };

    // A single-connection pool keeps this temporary table session-local and
    // avoids touching the real fleet_tasks table.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    sqlx::raw_sql(
        "CREATE TEMPORARY TABLE fleet_tasks (
             id UUID PRIMARY KEY,
             task_type TEXT NOT NULL,
             summary TEXT NOT NULL,
             payload JSONB NOT NULL DEFAULT '{}'::jsonb,
             priority INT NOT NULL DEFAULT 50,
             status TEXT NOT NULL DEFAULT 'pending',
             created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
             completed_at TIMESTAMPTZ,
             task_class TEXT,
             dedup_signature TEXT UNIQUE
         );",
    )
    .execute(&pool)
    .await
    .expect("create temporary fleet_tasks table");

    let signature = "same-recurring-bug";
    let task_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fleet_tasks
             (id, task_type, summary, payload, status, completed_at, task_class, dedup_signature)
         VALUES ($1, 'self_heal_writer', $2, '{\"status\":\"failed\"}', 'failed',
                 NOW() - INTERVAL '1 hour', 'self_heal', $2)",
    )
    .bind(task_id)
    .bind(signature)
    .execute(&pool)
    .await
    .expect("insert first bug report");

    for report_number in 1..=2 {
        let rearmed = rearm_self_heal_task(
            &pool,
            signature,
            "T2",
            report_number,
            Some(std::time::Duration::ZERO),
        )
        .await
        .expect("re-arm recurring self-heal task");
        assert!(rearmed, "report {report_number} should re-arm the task");

        let row = sqlx::query(
            "SELECT id, status, completed_at,
                    (payload->>'report_count')::int AS report_count
               FROM fleet_tasks
              WHERE dedup_signature = $1",
        )
        .bind(signature)
        .fetch_one(&pool)
        .await
        .expect("fetch re-armed task");
        assert_eq!(row.get::<uuid::Uuid, _>("id"), task_id);
        assert_eq!(row.get::<String, _>("status"), "pending");
        assert!(
            row.get::<Option<DateTime<Utc>>, _>("completed_at")
                .is_none()
        );

        let expected_reports: i32 = (1..=report_number).sum();
        assert_eq!(row.get::<i32, _>("report_count"), expected_reports);

        if report_number == 1 {
            sqlx::query(
                "UPDATE fleet_tasks
                    SET status = 'completed', completed_at = NOW(),
                        payload = payload || '{\"status\":\"completed\"}'::jsonb
                  WHERE id = $1",
            )
            .bind(task_id)
            .execute(&pool)
            .await
            .expect("complete first self-heal attempt");
        }
    }
}
