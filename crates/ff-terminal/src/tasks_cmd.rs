//! `ff tasks list [--computer <name>] [--status <s>] [--type <t>]`
//!
//! Fleet-wide task view. Queries `fleet_tasks` (V44) and renders a table.

use anyhow::Result;
use serde_json::Value;
use sqlx::{PgPool, Row};

pub async fn handle_tasks_list(
    pg: &PgPool,
    computer_filter: Option<&str>,
    status_filter: Option<&str>,
    type_filter: Option<&str>,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT t.id, t.task_type, t.summary, t.status, \
                c.name as claimer_name, t.progress_pct, t.progress_message, \
                t.created_at, t.started_at \
         FROM fleet_tasks t \
         LEFT JOIN computers c ON t.claimed_by_computer_id = c.id \
         WHERE 1=1",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(cf) = computer_filter {
        args.push(cf.to_string());
        sql.push_str(&format!(" AND c.name = ${}", args.len()));
    }
    // Allow comma-separated list of statuses, e.g. "pending,running".
    if let Some(sf) = status_filter {
        let parts: Vec<&str> = sf.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        if !parts.is_empty() {
            let placeholders: Vec<String> = parts
                .iter()
                .map(|p| {
                    args.push(p.to_string());
                    format!("${}", args.len())
                })
                .collect();
            sql.push_str(&format!(" AND t.status IN ({})", placeholders.join(", ")));
        }
    }
    if let Some(tf) = type_filter {
        args.push(tf.to_string());
        sql.push_str(&format!(" AND t.task_type = ${}", args.len()));
    }
    sql.push_str(" ORDER BY t.created_at DESC LIMIT 100");

    let mut q = sqlx::query(&sql);
    for a in &args {
        q = q.bind(a);
    }
    let rows = q.fetch_all(pg).await?;

    println!(
        "{:<10} {:<20} {:<12} {:<10} {:>5} {}",
        "COMPUTER", "TYPE", "STATUS", "AGE", "PCT", "SUMMARY"
    );
    for r in rows {
        let computer: Option<String> = r.try_get("claimer_name").ok();
        let ty: String = r.try_get("task_type")?;
        let status: String = r.try_get("status")?;
        let pct: Option<f32> = r.try_get("progress_pct").ok();
        let summary: String = r.try_get("summary")?;
        let created_at: chrono::DateTime<chrono::Utc> = r.try_get("created_at")?;
        let age_secs = (chrono::Utc::now() - created_at).num_seconds().max(0) as u64;
        let age_str = if age_secs < 60 {
            format!("{}s", age_secs)
        } else if age_secs < 3600 {
            format!("{}m", age_secs / 60)
        } else if age_secs < 86400 {
            format!("{}h", age_secs / 3600)
        } else {
            format!("{}d", age_secs / 86400)
        };
        let pct_str = pct
            .map(|p| format!("{:.0}", p))
            .unwrap_or_else(|| "-".into());
        let ty_short: String = ty.chars().take(20).collect();
        let status_short: String = status.chars().take(12).collect();
        let summary_short: String = summary.chars().take(60).collect();
        println!(
            "{:<10} {:<20} {:<12} {:<10} {:>5} {}",
            computer.as_deref().unwrap_or("-"),
            ty_short,
            status_short,
            age_str,
            pct_str,
            summary_short
        );
    }
    Ok(())
}

/// Show full detail for one task.
pub async fn handle_tasks_get(pg: &PgPool, id: uuid::Uuid) -> Result<()> {
    let row = sqlx::query(
        "SELECT t.id, t.parent_task_id, t.task_type, t.summary, t.payload,
                t.priority, t.requires_capability, t.status, c.name as claimer_name,
                t.progress_pct, t.progress_message, t.result, t.error,
                t.handoff_count, t.handoff_reason,
                t.created_at, t.claimed_at, t.started_at, t.completed_at,
                t.last_heartbeat_at
           FROM fleet_tasks t
           LEFT JOIN computers c ON t.claimed_by_computer_id = c.id
          WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(pg)
    .await?;

    let Some(r) = row else {
        anyhow::bail!("task {id} not found");
    };

    let task_type: String = r.try_get("task_type")?;
    let summary: String = r.try_get("summary")?;
    let status: String = r.try_get("status")?;
    let priority: i32 = r.try_get("priority")?;
    let parent: Option<uuid::Uuid> = r.try_get("parent_task_id").ok();
    let claimer: Option<String> = r.try_get("claimer_name").ok();
    let payload: Value = r.try_get("payload")?;
    let caps: Value = r.try_get("requires_capability")?;
    let pct: Option<f32> = r.try_get("progress_pct").ok();
    let prog_msg: Option<String> = r.try_get("progress_message").ok();
    let result: Option<Value> = r.try_get("result").ok();
    let error: Option<String> = r.try_get("error").ok();
    let handoff_count: i32 = r.try_get("handoff_count")?;
    let handoff_reason: Option<String> = r.try_get("handoff_reason").ok();
    let created_at: chrono::DateTime<chrono::Utc> = r.try_get("created_at")?;
    let started_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("started_at").ok();
    let completed_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("completed_at").ok();
    let heartbeat_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("last_heartbeat_at").ok();

    println!("ID:              {id}");
    if let Some(p) = parent {
        println!("Parent:          {p}");
    }
    println!("Type:            {task_type}");
    println!("Summary:         {summary}");
    println!("Status:          {status}");
    println!("Priority:        {priority}");
    println!("Capabilities:    {}", caps);
    println!("Claimed by:      {}", claimer.as_deref().unwrap_or("-"));
    if handoff_count > 0 {
        println!(
            "Handoffs:        {handoff_count}{}",
            handoff_reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        );
    }
    if let Some(p) = pct {
        println!("Progress:        {p:.0}%");
    }
    if let Some(m) = prog_msg {
        println!("Progress msg:    {m}");
    }
    println!("Created:         {}", created_at.format("%Y-%m-%d %H:%M:%S UTC"));
    if let Some(s) = started_at {
        println!("Started:         {}", s.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    if let Some(h) = heartbeat_at {
        let age = (chrono::Utc::now() - h).num_seconds();
        println!("Last heartbeat:  {age}s ago");
    }
    if let Some(c) = completed_at {
        println!("Completed:       {}", c.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    println!();
    println!("Payload:");
    println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    if let Some(r) = result {
        println!();
        println!("Result:");
        println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    }
    if let Some(e) = error {
        println!();
        println!("Error: {e}");
    }
    Ok(())
}
