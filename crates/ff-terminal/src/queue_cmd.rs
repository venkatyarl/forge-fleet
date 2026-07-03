//! `ff queue` — at-a-glance visibility into the fleet work queue.

use anyhow::{Context, Result, anyhow};
use sqlx::Row;

use crate::{CYAN, RESET, truncate_for_col};

const FLEET_TASK_STATUSES: &[&str] = &["pending", "running", "completed", "failed"];

pub async fn handle_queue() -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow!("connect Postgres: {e}"))?;

    let fleet_rows = sqlx::query(
        "SELECT status, count(*)::bigint AS count
           FROM fleet_tasks
          WHERE status = ANY($1)
          GROUP BY status",
    )
    .bind(FLEET_TASK_STATUSES)
    .fetch_all(&pool)
    .await
    .context("query fleet_tasks queue counts")?;

    let mut fleet_counts = std::collections::HashMap::new();
    for row in fleet_rows {
        let status: String = row.try_get("status")?;
        let count: i64 = row.try_get("count")?;
        fleet_counts.insert(status, count);
    }

    let work_item_rows = sqlx::query(
        "SELECT COALESCE(project_id, '-') AS project_id,
                COALESCE(status, '-') AS status,
                count(*)::bigint AS count
           FROM work_items
          GROUP BY COALESCE(project_id, '-'), COALESCE(status, '-')
          ORDER BY COALESCE(project_id, '-'), COALESCE(status, '-')",
    )
    .fetch_all(&pool)
    .await
    .context("query work_items queue counts")?;

    let active_leases: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM work_item_leases WHERE released_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .context("query active work_item leases")?;

    println!("{CYAN}fleet_tasks by status{RESET}");
    println!("{:<12} {:>8}", "STATUS", "COUNT");
    for status in FLEET_TASK_STATUSES {
        let count = fleet_counts.get(*status).copied().unwrap_or(0);
        println!("{:<12} {:>8}", status, count);
    }

    println!("\n{CYAN}work_items by project/status{RESET}");
    println!("{:<24} {:<16} {:>8}", "PROJECT", "STATUS", "COUNT");
    for row in work_item_rows {
        let project_id: String = row.try_get("project_id")?;
        let status: String = row.try_get("status")?;
        let count: i64 = row.try_get("count")?;
        println!(
            "{:<24} {:<16} {:>8}",
            truncate_for_col(&project_id, 24),
            truncate_for_col(&status, 16),
            count
        );
    }

    println!("\n{CYAN}active work_item leases{RESET}: {active_leases}");
    Ok(())
}
