//! Wave-reaper — rolls up parent fleet_tasks rows whose children have
//! all reached a terminal state. Without this, every fleet-upgrade-wave
//! leaves its parent row stuck in 'pending' forever even though all 28
//! children completed. Observed: 12 zombies accumulated over 3.8 days.
//!
//! Run from the daemon's wave_reaper_tick every 10 min, leader-only.

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info};

pub struct ReaperReport {
    pub reaped_completed: i64,
    pub reaped_failed: i64,
}

pub async fn reap_pending_parents(pool: &PgPool) -> Result<ReaperReport, sqlx::Error> {
    // 2026-05-24 (fix B cleanup): cancel any wave restart task left
    // pending behind a build that did NOT succeed. Fix B tightened the
    // claim gate to require the build dependency be 'completed', so a
    // restart whose build failed/cancelled would otherwise sit pending
    // forever — never claimable, never terminal — and keep its parent
    // wave from ever rolling up. Sweeping it to 'cancelled' makes the
    // parent's children all-terminal so the rollup below can fire, and
    // (with fix D) the parent reflects the failure.
    let orphaned = sqlx::query(
        r#"
        UPDATE fleet_tasks r
           SET status       = 'cancelled',
               completed_at = NOW(),
               error        = 'build dependency did not succeed — restart skipped (fix B)'
         WHERE r.status = 'pending'
           AND r.summary LIKE 'fleet-upgrade-wave/restart:%'
           AND r.depends_on_task_id IS NOT NULL
           AND EXISTS (
                 SELECT 1 FROM fleet_tasks dep
                  WHERE dep.id = r.depends_on_task_id
                    AND dep.status IN ('failed', 'cancelled')
           )
        "#,
    )
    .execute(pool)
    .await?;
    if orphaned.rows_affected() > 0 {
        info!(
            cancelled = orphaned.rows_affected(),
            "wave-reaper cancelled restart tasks orphaned by failed builds"
        );
    }

    // Find every pending parent that (a) has at least one child and
    // (b) has zero non-terminal children. Roll-up rule:
    //   any child failed     -> parent = 'failed'  (alert; fix D)
    //   else any completed   -> parent = 'completed'
    //   else (all cancelled) -> parent = 'failed'
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
        // fix D: a wave with ANY failed child rolls up to 'failed' — even
        // if some children completed. The old rule (`ok > 0 -> completed`)
        // marked partially-failed waves 'completed', so the pipeline saw a
        // green parent every hour while 11/14 children were dying and re-
        // thrashing silently. A wave whose children are all cancelled (no
        // failures, no completions) is also not a success.
        let new_status = if fail > 0 {
            "failed"
        } else if ok > 0 {
            "completed"
        } else {
            "failed"
        };
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
            info!(parent = %parent_id, status = new_status, ok, fail, cancel, "wave-reaper rolled up parent");
        } else {
            failed += 1;
            // fix D: surface failed waves loudly. A warn log plus a NATS
            // event means the upgrade pipeline alerts instead of failing
            // identically every hour with nobody noticing (observed 4+h
            // on 2026-05-24).
            tracing::warn!(parent = %parent_id, status = new_status, ok, fail, cancel, "wave-reaper rolled up FAILED wave");
            let payload = serde_json::json!({
                "parent_task_id": parent_id,
                "status": new_status,
                "children_completed": ok,
                "children_failed": fail,
                "children_cancelled": cancel,
                "ts": chrono::Utc::now().to_rfc3339(),
            });
            crate::nats_client::publish_json("fleet.events.wave.failed".to_string(), &payload)
                .await;
        }
    }

    Ok(ReaperReport {
        reaped_completed: completed,
        reaped_failed: failed,
    })
}

/// Spawn the wave-reaper as a leader-gated background tick. Mirrors
/// the shape of `batch_manager::spawn_completion_watcher`. Self-
/// contained so `src/main.rs` only needs one call.
pub fn spawn_reaper(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(interval_secs.max(60));
        let mut ticker = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
            // Leader-only: skip when this host isn't the elected leader
            // so we don't have 15 daemons racing on the same parents.
            if !crate::leader_cache::is_current_leader() {
                debug!("wave-reaper: skipping (not leader)");
                continue;
            }
            match reap_pending_parents(&pg).await {
                Ok(r) if r.reaped_completed + r.reaped_failed > 0 => info!(
                    completed = r.reaped_completed,
                    failed = r.reaped_failed,
                    "wave-reaper rolled up parents"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "wave-reaper failed"),
            }
        }
    })
}
