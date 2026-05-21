//! Retry policy — decides backoff seconds based on attempt count.
//! Reads failure_taxonomy.transient + retryable (V107) to know whether
//! the category is worth retrying at all.
//!
//! Returns:
//!   Ok(None)              — give up, this attempt is terminal
//!   Ok(Some(delay_secs))  — sleep `delay_secs` then retry

use sqlx::PgPool;

pub async fn should_retry(
    pool: &PgPool,
    category: &str,
    attempt: i32,
) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(bool, bool)> =
        sqlx::query_as("SELECT transient, retryable FROM failure_taxonomy WHERE category = $1")
            .bind(category)
            .fetch_optional(pool)
            .await?;
    let Some((transient, retryable)) = row else {
        return Ok(None);
    };
    if !retryable || !transient {
        return Ok(None);
    }
    if attempt >= 3 {
        return Ok(None);
    }
    let delay = match attempt {
        0 => 5,
        1 => 30,
        2 => 120,
        _ => 0,
    };
    Ok(Some(delay))
}
