//! Operator notification policy — Telegram only when a human action
//! could matter. Dedups same (category, host) within a 5-min window so
//! routine retries don't flood the operator's phone.
//!
//! Backed by `failure_taxonomy.notify_threshold` (V107) — each category
//! sets how many occurrences in 10 min trigger a notify.

use sqlx::PgPool;

pub async fn should_notify(
    pool: &PgPool,
    worker_name: &str,
    category: &str,
) -> Result<bool, sqlx::Error> {
    let threshold: Option<(i32,)> =
        sqlx::query_as("SELECT notify_threshold FROM failure_taxonomy WHERE category = $1")
            .bind(category)
            .fetch_optional(pool)
            .await?;
    let Some((threshold,)) = threshold else {
        return Ok(false);
    };
    if threshold == 0 {
        return Ok(false);
    }

    let count: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM task_failures tf
           JOIN fleet_tasks t ON t.id = tf.task_id
           JOIN computers  c ON c.id = t.claimed_by_computer_id
          WHERE c.name = $1
            AND tf.category = $2
            AND tf.occurred_at > NOW() - INTERVAL '10 minutes'",
    )
    .bind(worker_name)
    .bind(category)
    .fetch_one(pool)
    .await?;
    if (count.0 as i32) < threshold {
        return Ok(false);
    }

    // Dedup: skip if we've already notified for (worker, category) in
    // the last 5 minutes. We piggyback on task_failures.action_taken to
    // record the prior notify; absence of a recent 'notify_operator'
    // row means we're due to send.
    let recent_notify: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM task_failures tf
           JOIN fleet_tasks t ON t.id = tf.task_id
           JOIN computers  c ON c.id = t.claimed_by_computer_id
          WHERE c.name = $1
            AND tf.category = $2
            AND tf.action_taken = 'notify_operator'
            AND tf.occurred_at > NOW() - INTERVAL '5 minutes'",
    )
    .bind(worker_name)
    .bind(category)
    .fetch_one(pool)
    .await?;
    Ok(recent_notify.0 == 0)
}

/// Record that an operator notification was dispatched. Inserts a row
/// into `task_failures` with action_taken='notify_operator' so the
/// dedup check above sees it on the next probe.
pub async fn record_notification(
    pool: &PgPool,
    task_id: Option<uuid::Uuid>,
    category: &str,
    details: serde_json::Value,
) -> Result<(), sqlx::Error> {
    // task_failures.task_id is NOT NULL; fall back to a synthetic UUID
    // when the notification isn't tied to a specific task (e.g. host
    // health alerts). Caller should pass Some when possible.
    let tid = task_id.unwrap_or_else(uuid::Uuid::nil);
    sqlx::query(
        "INSERT INTO task_failures (task_id, category, attempt, action_taken, details)
         VALUES ($1, $2, 0, 'notify_operator', $3)",
    )
    .bind(tid)
    .bind(category)
    .bind(details)
    .execute(pool)
    .await?;
    Ok(())
}
