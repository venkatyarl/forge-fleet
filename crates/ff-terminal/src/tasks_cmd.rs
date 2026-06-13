//! `ff tasks list [--computer <name>] [--status <s>] [--type <t>]`
//!
//! Fleet-wide task view. Queries `fleet_tasks` (V44) and renders a table.

use anyhow::Result;
use ff_core::task_error::{TaskErrorClass, classify_task_error};
use serde_json::Value;
use sqlx::{PgPool, Row};

/// Documented `fleet_tasks.status` values (see schema.rs `fleet_tasks`).
/// Used to WARN on a likely-typo'd `--status` filter. Deliberately a soft
/// warning, not a hard error: a future status added to the schema would
/// otherwise be wrongly rejected, so drift here only costs a spurious warning.
const KNOWN_TASK_STATUSES: &[&str] = &[
    "pending",
    "claimed",
    "running",
    "completed",
    "failed",
    "handed_off",
    "cancelled",
    "paused",
];

/// Returns the comma-separated `--status` parts that aren't a documented task
/// status (likely typos). Pure — empty filter / all-known → empty vec. Match is
/// case-insensitive so `Running` doesn't warn.
fn unknown_status_parts(filter: &str) -> Vec<String> {
    filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !KNOWN_TASK_STATUSES.contains(&s.to_ascii_lowercase().as_str()))
        .map(|s| s.to_string())
        .collect()
}

/// Terminal statuses that represent a failure (worth classifying).
fn is_failed_status(status: &str) -> bool {
    matches!(status, "failed" | "cancelled" | "canceled")
}

/// Derive a [`TaskErrorClass`] on the fly from an already-stored task row.
///
/// Reads `stdout`/`stderr`/`exit` out of the result JSON (shape
/// `{exit, stdout, stderr}`, written by `ff_agent::task_runner`) plus the
/// free-form `error` column, and runs the pure classifier. Returns `None`
/// for non-failed tasks. Nothing is persisted — this is computed at display
/// time only.
fn classify_failed_task(
    status: &str,
    result: Option<&Value>,
    error: Option<&str>,
) -> Option<TaskErrorClass> {
    if !is_failed_status(status) {
        return None;
    }
    let stdout = result
        .and_then(|r| r.get("stdout"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let stderr = result
        .and_then(|r| r.get("stderr"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let exit = result.and_then(|r| r.get("exit")).and_then(Value::as_i64);
    // A cancelled task may carry no result/error text at all; the classifier
    // falls back to Unknown, but the status itself is the signal, so map
    // empty-but-cancelled to Cancelled explicitly.
    let class = classify_task_error(stderr, stdout, exit, error);
    if class == TaskErrorClass::Unknown && matches!(status, "cancelled" | "canceled") {
        return Some(TaskErrorClass::Cancelled);
    }
    Some(class)
}

pub async fn handle_tasks_list(
    pg: &PgPool,
    computer_filter: Option<&str>,
    status_filter: Option<&str>,
    type_filter: Option<&str>,
    show_id: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT t.id, t.task_type, t.summary, t.status, \
                c.name as claimer_name, t.progress_pct, t.progress_message, \
                t.result, t.error, t.created_at, t.started_at \
         FROM fleet_tasks t \
         LEFT JOIN computers c ON t.claimed_by_computer_id = c.id \
         WHERE 1=1",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(cf) = computer_filter {
        // Validate against the (drift-free) computers table so a typo errors
        // loudly instead of silently returning an empty list that reads like
        // "no such tasks".
        let known: i64 = sqlx::query_scalar("SELECT count(*) FROM computers WHERE name = $1")
            .bind(cf)
            .fetch_one(pg)
            .await?;
        if known == 0 {
            anyhow::bail!("unknown computer '{cf}' — run 'ff fleet health' to list computers");
        }
        args.push(cf.to_string());
        sql.push_str(&format!(" AND c.name = ${}", args.len()));
    }
    // Allow comma-separated list of statuses, e.g. "pending,running".
    if let Some(sf) = status_filter {
        // Warn (don't reject — see KNOWN_TASK_STATUSES) on a likely typo so an
        // empty result isn't mistaken for "no matching tasks".
        for bad in unknown_status_parts(sf) {
            eprintln!(
                "warning: unknown status '{bad}' — known: {}",
                KNOWN_TASK_STATUSES.join(", ")
            );
        }
        let parts: Vec<&str> = sf
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
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

    // ERR column carries the on-the-fly error class for failed rows only
    // (blank for everything else). 17 = width of the longest class string
    // ("permission_denied"). Nothing is persisted; it's derived per row.
    if show_id {
        println!(
            "{:<36} {:<10} {:<20} {:<12} {:<10} {:>5} {:<17} SUMMARY",
            "ID", "COMPUTER", "TYPE", "STATUS", "AGE", "PCT", "ERR"
        );
    } else {
        println!(
            "{:<10} {:<20} {:<12} {:<10} {:>5} {:<17} SUMMARY",
            "COMPUTER", "TYPE", "STATUS", "AGE", "PCT", "ERR"
        );
    }
    for r in rows {
        let id: uuid::Uuid = r.try_get("id")?;
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
        // Compact error class — only populated for failed/cancelled rows,
        // derived on the fly from the stored result/error (no storage change).
        let result: Option<Value> = r.try_get("result").ok();
        let error: Option<String> = r.try_get("error").ok();
        let err_class = classify_failed_task(&status, result.as_ref(), error.as_deref())
            .map(|c| c.as_str())
            .unwrap_or("");
        if show_id {
            println!(
                "{:<36} {:<10} {:<20} {:<12} {:<10} {:>5} {:<17} {}",
                id,
                computer.as_deref().unwrap_or("-"),
                ty_short,
                status_short,
                age_str,
                pct_str,
                err_class,
                summary_short
            );
        } else {
            println!(
                "{:<10} {:<20} {:<12} {:<10} {:>5} {:<17} {}",
                computer.as_deref().unwrap_or("-"),
                ty_short,
                status_short,
                age_str,
                pct_str,
                err_class,
                summary_short
            );
        }
    }
    Ok(())
}

/// Show full detail for one task.
pub async fn handle_tasks_get(pg: &PgPool, id: uuid::Uuid, json: bool) -> Result<()> {
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

    // Derive the structured error class once; reused by both output paths.
    let err_class = classify_failed_task(&status, result.as_ref(), error.as_deref());

    if json {
        // Preserve all existing fields verbatim; only ADD `error_class`
        // (and its hint) for failed/cancelled tasks. Computed, not stored.
        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), Value::String(id.to_string()));
        if let Some(p) = parent {
            obj.insert("parent_task_id".into(), Value::String(p.to_string()));
        }
        obj.insert("task_type".into(), Value::String(task_type.clone()));
        obj.insert("summary".into(), Value::String(summary.clone()));
        obj.insert("status".into(), Value::String(status.clone()));
        obj.insert("priority".into(), Value::from(priority));
        obj.insert("requires_capability".into(), caps.clone());
        obj.insert(
            "claimed_by".into(),
            claimer.clone().map(Value::String).unwrap_or(Value::Null),
        );
        obj.insert("payload".into(), payload.clone());
        obj.insert(
            "progress_pct".into(),
            pct.map(|p| Value::from(p as f64)).unwrap_or(Value::Null),
        );
        obj.insert("result".into(), result.clone().unwrap_or(Value::Null));
        obj.insert(
            "error".into(),
            error.clone().map(Value::String).unwrap_or(Value::Null),
        );
        if let Some(class) = err_class {
            obj.insert(
                "error_class".into(),
                Value::String(class.as_str().to_string()),
            );
            obj.insert(
                "error_class_hint".into(),
                Value::String(class.hint().to_string()),
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default()
        );
        return Ok(());
    }

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
    println!(
        "Created:         {}",
        created_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
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
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    );
    // Surface the structured error class before the raw blobs so it's the
    // first thing the operator sees. Computed on the fly above — not stored.
    if let Some(class) = err_class {
        println!();
        println!("ERROR CLASS:     {}  ({})", class.as_str(), class.hint());
    }
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

/// Render a composed task graph. In `dry_run` mode prints the full plan
/// (one block per task — summary, capabilities, priority, timeout, deps,
/// and the shell command) and a footer reminding nothing was enqueued. In
/// the real path the plan carries no tasks, so it just confirms the parent
/// id and how to watch progress.
pub fn print_compose_plan(plan: &ff_agent::task_runner::ComposePlan, dry_run: bool) {
    print!("{}", format_compose_plan(plan, dry_run));
}

/// Pure formatter behind [`print_compose_plan`] — returns the text so it can
/// be unit-tested without capturing stdout.
fn format_compose_plan(plan: &ff_agent::task_runner::ComposePlan, dry_run: bool) -> String {
    use crate::utils::{CYAN, GREEN, RESET, YELLOW};
    use std::fmt::Write as _;

    let mut out = String::new();

    if !dry_run {
        // Real path: ComposePlan::tasks is empty; parent is the enqueued id.
        match plan.parent {
            Some(parent) => writeln!(out, "composed parent task: {parent}").ok(),
            // Defensive — the real path always returns Some(parent).
            None => writeln!(out, "composed parent task (no id returned)").ok(),
        };
        writeln!(
            out,
            "watch progress with: ff tasks list --status pending,running"
        )
        .ok();
        return out;
    }

    writeln!(
        out,
        "{CYAN}DRY RUN{RESET} — nothing was enqueued. Would compose {GREEN}{}{RESET} task(s):",
        plan.tasks.len()
    )
    .ok();
    writeln!(out, "  parent (compound): {}", plan.parent_summary).ok();
    for (i, t) in plan.tasks.iter().enumerate() {
        writeln!(out).ok();
        writeln!(out, "  {GREEN}[{}]{RESET} {}", i + 1, t.summary).ok();
        let caps = if t.capabilities.is_empty() {
            "(any worker)".to_string()
        } else {
            t.capabilities.join(",")
        };
        write!(out, "      priority {} · capability {caps}", t.priority).ok();
        if let Some(secs) = t.timeout_secs {
            write!(out, " · timeout {secs}s").ok();
        }
        if let Some(dep) = &t.depends_on {
            write!(out, " · after [{dep}]").ok();
        }
        writeln!(out).ok();
        // Indent each command line so multi-line shell bodies stay readable.
        for line in t.command.lines() {
            writeln!(out, "      {line}").ok();
        }
    }
    writeln!(out).ok();
    writeln!(
        out,
        "{YELLOW}re-run without --dry-run to enqueue.{RESET} watch progress with: ff tasks list --status pending,running"
    )
    .ok();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_parts_flags_typos_only() {
        // Empty / whitespace → nothing to warn about.
        assert!(unknown_status_parts("").is_empty());
        assert!(unknown_status_parts("  ").is_empty());
        // All-known (with surrounding whitespace) → empty.
        assert!(unknown_status_parts("pending, running ,completed").is_empty());
        // Case-insensitive: a capitalized known status doesn't warn.
        assert!(unknown_status_parts("Running,FAILED").is_empty());
        // A typo is surfaced, valid siblings are not.
        assert_eq!(
            unknown_status_parts("running,runing,pending"),
            vec!["runing".to_string()]
        );
        // Every documented status is recognized (guards against drift between
        // KNOWN_TASK_STATUSES and the schema comment).
        for s in KNOWN_TASK_STATUSES {
            assert!(
                unknown_status_parts(s).is_empty(),
                "status {s} should be known"
            );
        }
    }

    #[test]
    fn dry_run_plan_lists_every_task_with_command_and_deps() {
        let plan = ff_agent::task_runner::ComposePlan {
            parent: None,
            parent_summary: "marcus: bring online".to_string(),
            tasks: vec![
                ff_agent::task_runner::PlannedTask {
                    summary: "marcus/1: ssh-probe".to_string(),
                    command: "set -e\necho probe".to_string(),
                    capabilities: vec!["leader".to_string()],
                    priority: 90,
                    timeout_secs: None,
                    depends_on: None,
                },
                ff_agent::task_runner::PlannedTask {
                    summary: "restart: marcus".to_string(),
                    command: "ssh marcus restart".to_string(),
                    capabilities: vec![],
                    priority: 30,
                    timeout_secs: Some(2700),
                    depends_on: Some("build: marcus".to_string()),
                },
            ],
        };
        let out = format_compose_plan(&plan, true);
        // Every task summary appears.
        assert!(out.contains("marcus/1: ssh-probe"));
        assert!(out.contains("restart: marcus"));
        // Multi-line command bodies are rendered, indented.
        assert!(out.contains("      echo probe"));
        // Capability rendering: explicit vs any-worker.
        assert!(out.contains("capability leader"));
        assert!(out.contains("capability (any worker)"));
        // Timeout + dependency annotations show up.
        assert!(out.contains("timeout 2700s"));
        assert!(out.contains("after [build: marcus]"));
        // Loud, unambiguous no-write banner.
        assert!(out.contains("DRY RUN"));
        assert!(out.contains("nothing was enqueued"));
    }

    #[test]
    fn real_plan_prints_parent_id_only() {
        let parent = uuid::Uuid::nil();
        let plan = ff_agent::task_runner::ComposePlan {
            parent: Some(parent),
            parent_summary: "ignored in real path".to_string(),
            tasks: vec![],
        };
        let out = format_compose_plan(&plan, false);
        assert!(out.contains(&format!("composed parent task: {parent}")));
        assert!(out.contains("ff tasks list --status pending,running"));
        // No dry-run banner leaks into the real path.
        assert!(!out.contains("DRY RUN"));
    }
}
