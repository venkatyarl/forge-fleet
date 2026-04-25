//! `ff tasks list [--computer <name>] [--status <s>] [--type <t>]`
//!
//! Fleet-wide task view. Queries `fleet_tasks` (V44) and renders a table.

use anyhow::Result;
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
    if let Some(sf) = status_filter {
        args.push(sf.to_string());
        sql.push_str(&format!(" AND t.status = ${}", args.len()));
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
