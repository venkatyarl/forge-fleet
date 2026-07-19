//! Work queue persistence.
//!
//! Postgres-backed durable work queue with priority and status. Items are
//! inserted, claimed by workers, retried up to `max_attempts`, and completed
//! with an optional result or failure reason.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::Result;

/// Typed representation of a `work_queue` row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WorkQueueItem {
    pub id: Uuid,
    pub queue_name: String,
    pub payload: JsonValue,
    pub priority: i32,
    pub status: String,
    pub worker_id: Option<String>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub scheduled_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub result: Option<JsonValue>,
}

/// Input for creating a new work queue item.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkQueueItemInput {
    pub queue_name: Option<String>,
    pub payload: Option<JsonValue>,
    pub priority: Option<i32>,
    pub max_attempts: Option<i32>,
    pub scheduled_at: Option<DateTime<Utc>>,
}

/// Insert a new item into the work queue.
pub async fn insert_work_queue_item(pool: &PgPool, input: &WorkQueueItemInput) -> Result<Uuid> {
    let row = sqlx::query(
        r#"
        INSERT INTO work_queue (
            queue_name,
            payload,
            priority,
            max_attempts,
            scheduled_at
        ) VALUES (
            COALESCE($1, 'default'),
            COALESCE($2, '{}'::jsonb),
            COALESCE($3, 0),
            COALESCE($4, 3),
            COALESCE($5, NOW())
        )
        RETURNING id
        "#,
    )
    .bind(input.queue_name.as_deref())
    .bind(input.payload.as_ref())
    .bind(input.priority)
    .bind(input.max_attempts)
    .bind(input.scheduled_at)
    .fetch_one(pool)
    .await?;

    Ok(row.get("id"))
}

/// Fetch a single work queue item by id.
pub async fn get_work_queue_item(pool: &PgPool, id: Uuid) -> Result<Option<WorkQueueItem>> {
    let row = sqlx::query_as::<_, WorkQueueItem>(
        r#"
        SELECT *
        FROM work_queue
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// List work queue items, optionally filtered by queue name and status.
///
/// Ordered by priority descending then oldest scheduled first.
pub async fn list_work_queue_items(
    pool: &PgPool,
    queue_name: Option<&str>,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<WorkQueueItem>> {
    let rows = sqlx::query_as::<_, WorkQueueItem>(
        r#"
        SELECT *
        FROM work_queue
        WHERE ($1::TEXT IS NULL OR queue_name = $1)
          AND ($2::TEXT IS NULL OR status = $2)
        ORDER BY priority DESC, scheduled_at ASC, id ASC
        LIMIT $3
        "#,
    )
    .bind(queue_name)
    .bind(status)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Claim the next pending item from a queue for a worker.
///
/// Atomically selects the highest-priority, oldest-scheduled pending item and
/// transitions it to `claimed`, stamping `worker_id` and `started_at`.
pub async fn claim_next_work_queue_item(
    pool: &PgPool,
    queue_name: &str,
    worker_id: &str,
) -> Result<Option<WorkQueueItem>> {
    let mut tx = pool.begin().await?;

    let id: Option<Uuid> = sqlx::query_scalar(
        r#"
        SELECT id
        FROM work_queue
        WHERE queue_name = $1
          AND status = 'pending'
          AND scheduled_at <= NOW()
        ORDER BY priority DESC, scheduled_at ASC, id ASC
        FOR UPDATE SKIP LOCKED
        LIMIT 1
        "#,
    )
    .bind(queue_name)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(id) = id else {
        tx.commit().await?;
        return Ok(None);
    };

    let item = sqlx::query_as::<_, WorkQueueItem>(
        r#"
        UPDATE work_queue
        SET status = 'claimed',
            worker_id = $2,
            started_at = NOW(),
            updated_at = NOW(),
            attempts = attempts + 1
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(worker_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some(item))
}

/// Update a work queue item's status and optional terminal fields.
///
/// `last_error` and `result` are only written when the status is `failed` or
/// `completed`. `completed_at` is set automatically for terminal statuses.
pub async fn update_work_queue_item_status(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    last_error: Option<&str>,
    result: Option<&JsonValue>,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE work_queue
        SET status = $2,
            last_error = CASE
                WHEN $2 = 'failed' THEN $3
                ELSE last_error
            END,
            result = CASE
                WHEN $2 = 'completed' THEN $4
                ELSE result
            END,
            completed_at = CASE
                WHEN $2 IN ('completed', 'failed', 'cancelled') THEN NOW()
                ELSE completed_at
            END,
            updated_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(last_error)
    .bind(result)
    .execute(pool)
    .await?;

    Ok(())
}

/// Delete a work queue item by id.
pub async fn delete_work_queue_item(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM work_queue WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    use sqlx::postgres::PgPoolOptions;

    fn db_url() -> Option<String> {
        env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()
    }

    async fn connect() -> Option<PgPool> {
        let url = db_url()?;
        Some(
            PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .expect("connect to test database"),
        )
    }

    /// Ensure the `work_queue` table exists for these tests. If the test
    /// database is a fresh ephemeral DB, run the V170 migration directly.
    async fn ensure_table(pool: &PgPool) {
        sqlx::raw_sql(crate::schema::SCHEMA_V170_WORK_QUEUE)
            .execute(pool)
            .await
            .expect("apply work_queue schema");
    }

    #[tokio::test]
    async fn work_queue_crud_lifecycle() {
        let Some(pool) = connect().await else {
            return;
        };
        ensure_table(&pool).await;

        // Create
        let input = WorkQueueItemInput {
            queue_name: Some("test-queue".to_string()),
            payload: Some(serde_json::json!({"task": "demo"})),
            priority: Some(10),
            max_attempts: Some(5),
            scheduled_at: None,
        };
        let id = insert_work_queue_item(&pool, &input).await.unwrap();

        // Read
        let item = get_work_queue_item(&pool, id).await.unwrap().unwrap();
        assert_eq!(item.queue_name, "test-queue");
        assert_eq!(item.priority, 10);
        assert_eq!(item.status, "pending");
        assert_eq!(item.attempts, 0);
        assert_eq!(item.max_attempts, 5);
        assert_eq!(item.payload, serde_json::json!({"task": "demo"}));

        // List
        let items = list_work_queue_items(&pool, Some("test-queue"), None, 10)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);

        // Claim
        let claimed = claim_next_work_queue_item(&pool, "test-queue", "worker-1")
            .await
            .unwrap();
        assert!(claimed.is_some());
        let claimed = claimed.unwrap();
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.status, "claimed");
        assert_eq!(claimed.worker_id.as_deref(), Some("worker-1"));
        assert_eq!(claimed.attempts, 1);
        assert!(claimed.started_at.is_some());

        // No more pending items
        let next = claim_next_work_queue_item(&pool, "test-queue", "worker-2")
            .await
            .unwrap();
        assert!(next.is_none());

        // Complete
        update_work_queue_item_status(
            &pool,
            id,
            "completed",
            None,
            Some(&serde_json::json!({"outcome": "ok"})),
        )
        .await
        .unwrap();
        let completed = get_work_queue_item(&pool, id).await.unwrap().unwrap();
        assert_eq!(completed.status, "completed");
        assert!(completed.completed_at.is_some());
        assert_eq!(completed.result, Some(serde_json::json!({"outcome": "ok"})));

        // Delete
        delete_work_queue_item(&pool, id).await.unwrap();
        let gone = get_work_queue_item(&pool, id).await.unwrap();
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn claim_respects_priority_and_scheduled_at() {
        let Some(pool) = connect().await else {
            return;
        };
        ensure_table(&pool).await;

        let low = insert_work_queue_item(
            &pool,
            &WorkQueueItemInput {
                queue_name: Some("prio-queue".to_string()),
                priority: Some(1),
                scheduled_at: Some(Utc::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let high = insert_work_queue_item(
            &pool,
            &WorkQueueItemInput {
                queue_name: Some("prio-queue".to_string()),
                priority: Some(100),
                scheduled_at: Some(Utc::now() + chrono::Duration::seconds(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let claimed = claim_next_work_queue_item(&pool, "prio-queue", "worker")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, high);

        let next = claim_next_work_queue_item(&pool, "prio-queue", "worker")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.id, low);

        // Cleanup
        delete_work_queue_item(&pool, high).await.unwrap();
        delete_work_queue_item(&pool, low).await.unwrap();
    }

    #[tokio::test]
    async fn update_failure_records_error() {
        let Some(pool) = connect().await else {
            return;
        };
        ensure_table(&pool).await;

        let id = insert_work_queue_item(
            &pool,
            &WorkQueueItemInput {
                queue_name: Some("fail-queue".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        update_work_queue_item_status(&pool, id, "failed", Some("boom"), None)
            .await
            .unwrap();

        let item = get_work_queue_item(&pool, id).await.unwrap().unwrap();
        assert_eq!(item.status, "failed");
        assert_eq!(item.last_error.as_deref(), Some("boom"));
        assert!(item.completed_at.is_some());

        delete_work_queue_item(&pool, id).await.unwrap();
    }
}
