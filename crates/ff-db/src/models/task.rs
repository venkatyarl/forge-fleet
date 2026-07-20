//! Persistence helpers for task history.

use sqlx::PgPool;

use crate::error::Result;

/// Archive self-heal bug signatures before pruning terminal task rows.
///
/// Both operations share a transaction so retention can never discard the
/// only persistent record used to recognize and re-arm a recurring bug.
pub async fn prune_terminal_history(pool: &PgPool, retention_days: i32) -> Result<(u64, u64)> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO self_heal_bug_history
             (bug_signature, last_task_id, last_status, completed_at, last_seen_at)
         SELECT dedup_signature, id, status, completed_at, NOW()
           FROM fleet_tasks
          WHERE task_class = 'self_heal'
            AND dedup_signature IS NOT NULL
            AND status IN ('completed','failed','cancelled')
            AND created_at < NOW() - make_interval(days => $1)
         ON CONFLICT (bug_signature) DO UPDATE
             SET last_task_id = EXCLUDED.last_task_id,
                 last_status = EXCLUDED.last_status,
                 completed_at = EXCLUDED.completed_at,
                 last_seen_at = EXCLUDED.last_seen_at",
    )
    .bind(retention_days)
    .execute(&mut *tx)
    .await?;

    let fleet = sqlx::query(
        "DELETE FROM fleet_tasks
          WHERE status IN ('completed','failed','cancelled')
            AND created_at < NOW() - make_interval(days => $1)",
    )
    .bind(retention_days)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let deferred = sqlx::query(
        "DELETE FROM deferred_tasks
          WHERE status IN ('completed','failed','cancelled')
            AND created_at < NOW() - make_interval(days => $1)",
    )
    .bind(retention_days)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    tx.commit().await?;
    Ok((fleet, deferred))
}
