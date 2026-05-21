//! Per-host circuit breaker — quarantines a host after 3 failures of
//! the same category within 10 minutes. Reads/writes host_circuit_status
//! (V107).
//!
//! The dispatcher (#145) checks `is_quarantined` before assigning work;
//! the watchdog (#160) calls `record_failure` after every task failure.

use chrono::{Duration, Utc};
use sqlx::PgPool;

pub async fn record_failure(
    pool: &PgPool,
    worker_name: &str,
    category: &str,
) -> Result<bool, sqlx::Error> {
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
    if count.0 >= 3 {
        let opens_until = Utc::now() + Duration::minutes(15);
        sqlx::query(
            "INSERT INTO host_circuit_status (worker_name, failure_category, opens_until, reason)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (worker_name, failure_category) DO UPDATE
             SET opens_until = EXCLUDED.opens_until, reason = EXCLUDED.reason",
        )
        .bind(worker_name)
        .bind(category)
        .bind(opens_until)
        .bind("3+ failures in 10 min")
        .execute(pool)
        .await?;
        return Ok(true);
    }
    Ok(false)
}

pub async fn is_quarantined(pool: &PgPool, worker_name: &str) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT count(*) FROM host_circuit_status WHERE worker_name = $1 AND opens_until > NOW()",
    )
    .bind(worker_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|c| c.0 > 0).unwrap_or(false))
}
