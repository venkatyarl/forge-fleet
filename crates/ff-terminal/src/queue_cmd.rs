//! `ff queue` — at-a-glance visibility into the fleet work queue.

use anyhow::{Context, Result, anyhow};
use sqlx::Row;

use crate::{CYAN, RESET, truncate_for_col};

const FLEET_TASK_STATUSES: &[&str] = &["pending", "running", "completed", "failed"];
const WORK_ITEM_STATUSES: &[&str] = &["idea", "ready", "building", "in_review", "done", "failed"];

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
          WHERE status = ANY($1)
          GROUP BY COALESCE(project_id, '-'), COALESCE(status, '-')
          ORDER BY COALESCE(project_id, '-'), COALESCE(status, '-')",
    )
    .bind(WORK_ITEM_STATUSES)
    .fetch_all(&pool)
    .await
    .context("query work_items queue counts")?;

    let mut work_item_counts =
        std::collections::BTreeMap::<String, std::collections::HashMap<String, i64>>::new();
    for row in work_item_rows {
        let project_id: String = row.try_get("project_id")?;
        let status: String = row.try_get("status")?;
        let count: i64 = row.try_get("count")?;
        work_item_counts
            .entry(project_id)
            .or_default()
            .insert(status, count);
    }

    let active_leases: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM work_item_leases WHERE released_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .context("query active work_item leases")?;

    let drainable_work_items: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM work_items WHERE status IN ('ready', 'building')",
    )
    .fetch_one(&pool)
    .await
    .context("query ready/building work_item count")?;

    let free_sub_agent_slots: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM sub_agents WHERE status = 'idle'")
            .fetch_one(&pool)
            .await
            .context("query free sub_agent slots")?;

    println!("{CYAN}fleet_tasks by status{RESET}");
    println!("{:<12} {:>8}", "STATUS", "COUNT");
    for status in FLEET_TASK_STATUSES {
        let count = fleet_counts.get(*status).copied().unwrap_or(0);
        println!("{:<12} {:>8}", status, count);
    }

    println!("\n{CYAN}work_items by project{RESET}");
    println!(
        "{:<24} {:>8} {:>8} {:>8} {:>10} {:>8} {:>8}",
        "PROJECT", "IDEA", "READY", "BUILDING", "IN_REVIEW", "DONE", "FAILED"
    );
    for (project_id, counts) in work_item_counts {
        println!(
            "{:<24} {:>8} {:>8} {:>8} {:>10} {:>8} {:>8}",
            truncate_for_col(&project_id, 24),
            counts.get("idea").copied().unwrap_or(0),
            counts.get("ready").copied().unwrap_or(0),
            counts.get("building").copied().unwrap_or(0),
            counts.get("in_review").copied().unwrap_or(0),
            counts.get("done").copied().unwrap_or(0),
            counts.get("failed").copied().unwrap_or(0)
        );
    }

    println!("\n{CYAN}active work_item leases{RESET}: {active_leases}");
    if free_sub_agent_slots > 0 {
        let waves_to_drain =
            (drainable_work_items + free_sub_agent_slots - 1) / free_sub_agent_slots;
        println!(
            "{CYAN}queue ETA{RESET}: est. {waves_to_drain} waves to drain ({drainable_work_items} ready/building, {free_sub_agent_slots} free slots)"
        );
    } else {
        println!(
            "{CYAN}queue ETA{RESET}: est. unavailable waves to drain ({drainable_work_items} ready/building, 0 free slots)"
        );
    }
    Ok(())
}
