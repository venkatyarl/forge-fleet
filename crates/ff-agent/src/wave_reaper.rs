//! Wave-reaper — rolls up parent fleet_tasks rows whose children have
//! all reached a terminal state. Without this, every fleet-upgrade-wave
//! leaves its parent row stuck in 'pending' forever even though all 28
//! children completed. Observed: 12 zombies accumulated over 3.8 days.
//!
//! Run from the daemon's wave_reaper_tick every 10 min, leader-only.

use sqlx::PgPool;
use tracing::info;

pub struct ReaperReport {
    pub reaped_completed: i64,
    pub reaped_failed: i64,
}

pub async fn reap_pending_parents(pool: &PgPool) -> Result<ReaperReport, sqlx::Error> {
    // Find every pending parent that (a) has at least one child and
    // (b) has zero non-terminal children. Roll-up rule:
    //   any child completed  -> parent = 'completed'
    //   else                 -> parent = 'failed'
    // Counts go into parent.result jsonb so operators can see the wave
    // outcome at a glance.
    let rows: Vec<(uuid::Uuid, i64, i64, i64)> = sqlx::query_as(
        "SELECT p.id,
                count(c.id) FILTER (WHERE c.status='completed'),
                count(c.id) FILTER (WHERE c.status='failed'),
                count(c.id) FILTER (WHERE c.status='cancelled')
           FROM fleet_tasks p
           JOIN fleet_tasks c ON c.parent_task_id = p.id
          WHERE p.status = 'pending'
          GROUP BY p.id
         HAVING count(c.id) FILTER (
                  WHERE c.status NOT IN ('completed','failed','cancelled')
                ) = 0",
    )
    .fetch_all(pool)
    .await?;

    let mut completed = 0i64;
    let mut failed = 0i64;
    for (parent_id, ok, fail, cancel) in rows {
        let new_status = if ok > 0 { "completed" } else { "failed" };
        sqlx::query(
            "UPDATE fleet_tasks
                SET status = $1,
                    completed_at = NOW(),
                    result = jsonb_build_object(
                        'reaped_at',  to_char(NOW(), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                        'children_completed', $2::bigint,
                        'children_failed',    $3::bigint,
                        'children_cancelled', $4::bigint
                    )
              WHERE id = $5",
        )
        .bind(new_status)
        .bind(ok)
        .bind(fail)
        .bind(cancel)
        .bind(parent_id)
        .execute(pool)
        .await?;
        if new_status == "completed" {
            completed += 1;
        } else {
            failed += 1;
        }
        info!(parent = %parent_id, status = new_status, ok, fail, cancel, "wave-reaper rolled up parent");
    }

    Ok(ReaperReport {
        reaped_completed: completed,
        reaped_failed: failed,
    })
}
