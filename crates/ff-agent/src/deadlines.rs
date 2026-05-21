//! Per-task deadline reader — pulls `expected_duration_secs`,
//! `max_idle_secs`, and `wall_clock_max_secs` from a task payload,
//! falling back to per-workload defaults from `workload_taxonomy` (V107)
//! when the payload doesn't set them.
//!
//! Used by the watchdog (#160) and the dispatcher (#145) so every task
//! has well-defined "how long is this expected to take" + "when do we
//! declare it stuck" + "when do we hard-cancel" bounds.

use serde_json::Value;
use sqlx::PgPool;

pub struct TaskDeadlines {
    pub expected_duration_secs: u64,
    pub max_idle_secs: u64,
    pub wall_clock_max_secs: u64,
}

pub async fn from_payload(
    payload: &Value,
    workload: &str,
    pool: &PgPool,
) -> Result<TaskDeadlines, sqlx::Error> {
    let expected = payload
        .get("expected_duration_secs")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let defaults: Option<(i32, i32)> = sqlx::query_as(
        "SELECT default_max_idle_secs, default_wall_clock_max_secs
           FROM workload_taxonomy
          WHERE workload = $1",
    )
    .bind(workload)
    .fetch_optional(pool)
    .await?;
    let (default_idle, default_wall) = defaults.unwrap_or((300, 3600));

    let max_idle = payload
        .get("max_idle_secs")
        .and_then(Value::as_u64)
        .unwrap_or(default_idle as u64);
    let wall_clock_max = payload
        .get("wall_clock_max_secs")
        .and_then(Value::as_u64)
        .unwrap_or(default_wall as u64);

    Ok(TaskDeadlines {
        expected_duration_secs: expected,
        max_idle_secs: max_idle,
        wall_clock_max_secs: wall_clock_max,
    })
}
