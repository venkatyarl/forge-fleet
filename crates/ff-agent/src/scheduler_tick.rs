//! Project Scheduler Tick (Phase 10)
//!
//! Leader-gated daily tick that evaluates `project_schedules` cron expressions
//! and enqueues `fleet_tasks` rows for any schedule whose `next_run_at` has
//! passed.  Uses `ff_cron::CronSchedule` for expression parsing and next-run
//! calculation.

use anyhow::Result;
use chrono::Utc;
use sqlx::{PgPool, Row};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Evaluate all enabled project schedules and enqueue due tasks.
///
/// Returns the number of tasks enqueued.
pub async fn evaluate_schedules(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let rows = sqlx::query(
        r#"
        SELECT id, project_id, name, cron_expression, task_template
        FROM project_schedules
        WHERE enabled = true
          AND next_run_at <= NOW()
        ORDER BY next_run_at
        "#,
    )
    .fetch_all(pg)
    .await?;

    let mut enqueued = 0usize;
    for row in &rows {
        let id: Uuid = row.get("id");
        let project_id: String = row.get("project_id");
        let name: String = row.get("name");
        let cron_expr: String = row.get("cron_expression");
        let task_template: sqlx::types::Json<serde_json::Value> = row.get("task_template");

        // Parse cron and compute next run after now.
        let schedule = match ff_cron::CronSchedule::parse(&cron_expr) {
            Ok(s) => s,
            Err(e) => {
                warn!(schedule_id = %id, error = %e, "invalid cron expression; disabling schedule");
                sqlx::query("UPDATE project_schedules SET enabled = false WHERE id = $1")
                    .bind(id)
                    .execute(pg)
                    .await?;
                continue;
            }
        };

        let next_run = schedule
            .next_after(Utc::now())
            .unwrap_or_else(|| Utc::now() + chrono::Duration::days(1));

        // Build fleet_tasks payload from template.
        let template = task_template.0;
        let summary = template
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or(&name)
            .to_string();
        let payload = serde_json::json!({
            "schedule_id": id,
            "project_id": project_id,
            "template": template,
        });

        // Enqueue shell task (schedules default to background / fleet_first).
        let task_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO fleet_tasks (
                task_type, summary, payload, priority,
                requires_capability, preferred_computer_id,
                created_by_computer_id, routing_mode
            )
            SELECT
                COALESCE($3, 'shell'), $1, $2, COALESCE($4, 50),
                COALESCE($5, '[]'::jsonb), NULL,
                c.id, 'fleet_first'
            FROM computers c WHERE c.name = $6
            RETURNING fleet_tasks.id
            "#,
        )
        .bind(&summary)
        .bind(&payload)
        .bind(template.get("task_type").and_then(|v| v.as_str()))
        .bind(
            template
                .get("priority")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32),
        )
        .bind(template.get("requires_capability").cloned())
        .bind(worker_name)
        .fetch_one(pg)
        .await?;

        // Update schedule metadata.
        sqlx::query(
            r#"
            UPDATE project_schedules
            SET next_run_at = $1,
                last_run_at = NOW(),
                run_count = run_count + 1
            WHERE id = $2
            "#,
        )
        .bind(next_run)
        .bind(id)
        .execute(pg)
        .await?;

        info!(
            schedule_id = %id,
            task_id = %task_id,
            project_id = %project_id,
            next_run = %next_run,
            "scheduled task enqueued"
        );
        enqueued += 1;
    }

    if enqueued > 0 {
        info!(enqueued, "scheduler tick complete");
    } else {
        debug!("scheduler tick: no due schedules");
    }

    Ok(enqueued)
}

/// Spawn a background loop that evaluates schedules every `interval_secs`.
/// The tick is leader-gated via Postgres `fleet_leader_state`.
pub fn spawn_scheduler_tick(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE leader_name = $1
                              AND last_heartbeat > NOW() - INTERVAL '60 seconds'
                        )
                        "#
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }

                    if let Err(e) = evaluate_schedules(&pg, &worker_name).await {
                        warn!(error = %e, "scheduler tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("scheduler tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn test_cron_parsing() {
        let s = ff_cron::CronSchedule::parse("0 9 * * 1-5").unwrap();
        let next = s.next_after(Utc::now());
        assert!(next.is_some());
    }

    #[test]
    fn test_cron_daily() {
        let s = ff_cron::CronSchedule::parse("0 0 * * *").unwrap();
        let now = Utc::now();
        let next = s.next_after(now).unwrap();
        assert!(next > now);
        assert_eq!(next.hour(), 0);
        assert_eq!(next.minute(), 0);
    }
}
