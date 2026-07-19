//! Integration tests for HA self-heal behavior.
//!
//! These tests exercise the self-heal re-arm path against a real Postgres
//! instance, modelling how recurring bug detection interacts with the
//! deduplicated `fleet_tasks` queue.

#[cfg(test)]
mod tests {
    use crate::ha::self_heal::rearm_self_heal_task;
    use chrono::{DateTime, Utc};
    use sqlx::Row;
    use std::env;

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_self_heal_int_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn create_temp_db() -> Option<(sqlx::PgPool, sqlx::PgPool, String)> {
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
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE fleet_tasks (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 task_type TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 payload JSONB NOT NULL DEFAULT '{}'::jsonb,
                 priority INT NOT NULL DEFAULT 50,
                 status TEXT NOT NULL DEFAULT 'pending',
                 created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                 completed_at TIMESTAMPTZ,
                 task_class TEXT,
                 dedup_signature TEXT
             );
             CREATE UNIQUE INDEX idx_fleet_tasks_dedup_signature
                 ON fleet_tasks (dedup_signature)
                 WHERE dedup_signature IS NOT NULL;",
        )
        .execute(&pool)
        .await
        .expect("create minimal fleet_tasks schema");
        Some((admin, pool, db_name))
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
        .expect("terminate temp db sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
        admin.close().await;
    }

    async fn insert_self_heal_task(
        pg: &sqlx::PgPool,
        bug_signature: &str,
        status: &str,
        completed_at: Option<DateTime<Utc>>,
    ) -> uuid::Uuid {
        let row = sqlx::query(
            "INSERT INTO fleet_tasks
                (id, task_type, summary, payload, priority, status, created_at, completed_at, task_class, dedup_signature)
             VALUES (
                gen_random_uuid(),
                'self_heal_writer',
                $1,
                jsonb_build_object('bug_signature', $1, 'status', $2),
                80,
                $2,
                NOW(),
                $3,
                'self_heal',
                $1
             )
             RETURNING id",
        )
        .bind(bug_signature)
        .bind(status)
        .bind(completed_at)
        .fetch_one(pg)
        .await
        .expect("insert self-heal task");
        row.get("id")
    }

    #[tokio::test]
    async fn self_heal_rearms_on_recurring_bug_detection() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!(
                "skipping self_heal_rearms_on_recurring_bug_detection: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        };

        // A previously processed bug signature that completed long enough ago
        // to have passed the re-arm cooldown.
        let bug_signature = "sig-recurring-detection";
        let completed = Utc::now() - chrono::Duration::hours(1);
        let id = insert_self_heal_task(&pool, bug_signature, "completed", Some(completed)).await;

        // First recurring detection after cooldown should re-arm the existing task.
        let rearmed = rearm_self_heal_task(&pool, bug_signature, "T2", 5, None)
            .await
            .expect("rearm on recurring detection");
        assert!(
            rearmed,
            "expected self-heal task to re-arm on recurring bug detection"
        );

        let row = sqlx::query(
            "SELECT status,
                    completed_at,
                    (payload->>'status')::text AS payload_status,
                    (payload->>'report_count')::int AS report_count,
                    (payload->>'attempts')::int AS attempts
               FROM fleet_tasks
              WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch rearmed task");

        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<String, _>("payload_status"), "detected");
        assert_eq!(row.get::<i32, _>("report_count"), 5);
        assert_eq!(row.get::<i32, _>("attempts"), 0);
        assert!(
            row.try_get::<Option<DateTime<Utc>>, _>("completed_at")
                .ok()
                .flatten()
                .is_none(),
            "completed_at should be cleared after re-arm"
        );

        // A second detection while the task is already pending must not re-arm
        // again, preventing thrashing.
        let rearmed_again = rearm_self_heal_task(&pool, bug_signature, "T2", 2, None)
            .await
            .expect("second rearm check");
        assert!(
            !rearmed_again,
            "expected no re-arm while the self-heal task is still pending"
        );

        drop_temp_db(admin, pool, &db_name).await;
    }
}
