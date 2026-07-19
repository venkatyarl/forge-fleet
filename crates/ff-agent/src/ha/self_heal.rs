//! Self-heal coordination helpers for the leader tick.
//!
//! These functions live outside `leader_tick.rs` so they can be unit-tested
//! without spinning the whole leader state machine.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

/// Default cooldown before a processed self-heal signature is eligible for
/// re-arming. Prevents thrashing on a bug that flaps every few minutes.
pub const DEFAULT_REARM_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Outcome of checking whether a bug signature should re-arm a self-heal task.
#[derive(Debug, Clone)]
pub struct RearmCheck {
    /// True when the existing task is in a terminal state and has cooled down.
    pub should_rearm: bool,
    /// ID of the existing self-heal task, if one exists.
    pub existing_task_id: Option<uuid::Uuid>,
    /// The current `fleet_tasks.status` of the existing task.
    pub terminal_status: Option<String>,
    /// When the existing task was completed (if recorded).
    pub completed_at: Option<DateTime<Utc>>,
}

/// Check whether a bug signature has already been processed to a terminal
/// state (`completed`, `failed`, or `cancelled`) and has passed its re-arm
/// cooldown.
///
/// When this returns `should_rearm = true`, callers should reset the existing
/// self-heal task back to `pending`/`detected` so that recurring failures are
/// not permanently suppressed by the unique `dedup_signature` constraint.
pub async fn signature_should_rearm(
    pg: &PgPool,
    bug_signature: &str,
    cooldown: Option<std::time::Duration>,
) -> Result<RearmCheck, sqlx::Error> {
    let cooldown = cooldown.unwrap_or(DEFAULT_REARM_COOLDOWN);
    let row = sqlx::query(
        "SELECT id,
                status,
                completed_at,
                created_at
           FROM fleet_tasks
          WHERE task_class = 'self_heal'
            AND dedup_signature = $1",
    )
    .bind(bug_signature)
    .fetch_optional(pg)
    .await?;

    let Some(row) = row else {
        return Ok(RearmCheck {
            should_rearm: false,
            existing_task_id: None,
            terminal_status: None,
            completed_at: None,
        });
    };

    let id: uuid::Uuid = row.try_get("id")?;
    let status: String = row.try_get("status")?;
    let completed_at: Option<DateTime<Utc>> = row.try_get("completed_at")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;

    let terminal = matches!(status.as_str(), "completed" | "failed" | "cancelled");
    let terminal_at = completed_at.unwrap_or(created_at);
    let cooldown_secs = i64::try_from(cooldown.as_secs()).unwrap_or(i64::MAX);
    let cutoff = Utc::now() - chrono::Duration::seconds(cooldown_secs);
    let cooled_down = terminal_at <= cutoff;

    Ok(RearmCheck {
        should_rearm: terminal && cooled_down,
        existing_task_id: Some(id),
        terminal_status: Some(status),
        completed_at,
    })
}

/// Re-arm an existing self-heal task if it has reached a terminal state and
/// has cooled down.
///
/// Returns `true` when the row was actually updated. This is the companion to
/// [`signature_should_rearm`] and is used by [`scan_interaction_errors`] so
/// that recurring interaction-log errors are not discarded by the
/// `ON CONFLICT ... DO NOTHING` insert path.
pub async fn rearm_self_heal_task(
    pg: &PgPool,
    bug_signature: &str,
    tier: &str,
    report_count: i32,
    cooldown: Option<std::time::Duration>,
) -> Result<bool, sqlx::Error> {
    let check = signature_should_rearm(pg, bug_signature, cooldown).await?;
    if !check.should_rearm {
        return Ok(false);
    }

    let priority = match tier {
        "T1" => 100,
        "T0" => 90,
        "T2" => 80,
        _ => 70,
    };

    let updated = sqlx::query(
        "UPDATE fleet_tasks
            SET status = 'pending',
                priority = $3,
                completed_at = NULL,
                payload = payload || jsonb_build_object(
                    'status', 'detected',
                    'attempts', 0,
                    'report_count', COALESCE((payload->>'report_count')::int, 0) + $2,
                    'tier', $4,
                    'rearmed_at', NOW()::text
                )
          WHERE task_class = 'self_heal'
            AND dedup_signature = $1
            AND status IN ('completed', 'failed', 'cancelled')",
    )
    .bind(bug_signature)
    .bind(report_count)
    .bind(priority)
    .bind(tier)
    .execute(pg)
    .await?;

    Ok(updated.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_db_urls() -> (String, String, String) {
        let base_url = env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .expect("FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL must be set for DB tests");
        let (prefix, _) = base_url
            .rsplit_once('/')
            .expect("database URL must end with /<db>");
        let db_name = format!("ff_self_heal_{}", uuid::Uuid::new_v4().simple());
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
    async fn missing_signature_never_rearms() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let check = signature_should_rearm(&pool, "sig-missing", None)
            .await
            .expect("check missing signature");
        assert!(!check.should_rearm);
        assert!(check.existing_task_id.is_none());

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn completed_signature_after_cooldown_should_rearm() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let completed = Utc::now() - chrono::Duration::hours(1);
        insert_self_heal_task(&pool, "sig-old-completed", "completed", Some(completed)).await;

        let check = signature_should_rearm(&pool, "sig-old-completed", None)
            .await
            .expect("check completed signature");
        assert!(check.should_rearm);
        assert_eq!(check.terminal_status.as_deref(), Some("completed"));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn completed_signature_inside_cooldown_should_not_rearm() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let completed = Utc::now() - chrono::Duration::minutes(5);
        insert_self_heal_task(&pool, "sig-recent-completed", "completed", Some(completed)).await;

        let check = signature_should_rearm(&pool, "sig-recent-completed", None)
            .await
            .expect("check completed signature");
        assert!(!check.should_rearm);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn non_terminal_signature_should_not_rearm() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping signature_should_rearm DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        insert_self_heal_task(&pool, "sig-pending", "pending", None).await;

        let check = signature_should_rearm(&pool, "sig-pending", None)
            .await
            .expect("check pending signature");
        assert!(!check.should_rearm);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn rearm_task_resets_terminal_row_to_pending() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping rearm_self_heal_task DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let completed = Utc::now() - chrono::Duration::hours(1);
        let id = insert_self_heal_task(&pool, "sig-rearm", "completed", Some(completed)).await;

        let rearmed = rearm_self_heal_task(&pool, "sig-rearm", "T2", 3, None)
            .await
            .expect("rearm task");
        assert!(rearmed);

        let row = sqlx::query(
            "SELECT status, (payload->>'report_count')::int AS report_count,
                    (payload->>'status')::text AS payload_status
               FROM fleet_tasks
              WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch rearmed task");
        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<String, _>("payload_status"), "detected");
        assert_eq!(row.get::<i32, _>("report_count"), 3);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn rearm_task_is_no_op_for_active_row() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping rearm_self_heal_task DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        insert_self_heal_task(&pool, "sig-no-rearm", "running", None).await;

        let rearmed = rearm_self_heal_task(&pool, "sig-no-rearm", "T2", 1, None)
            .await
            .expect("rearm task");
        assert!(!rearmed);

        drop_temp_db(admin, pool, &db_name).await;
    }
}
