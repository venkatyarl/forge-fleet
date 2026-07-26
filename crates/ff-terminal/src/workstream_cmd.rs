//! `ff workstream` — attach a CLI session to its project's session-of-record
//! and report working state into it. Makes "ff owns the session" real: this
//! Claude session on forge-fleet (or Codex/Kimi elsewhere) resolves its project
//! from cwd, attaches to the single `ff_workstreams` row, and streams what it's
//! doing into the shared `working_summary` / `focus` / `open_threads` so the
//! record is visible to every other CLI + the TUI/web.

use anyhow::{Context, Result};
use std::path::PathBuf;

use ff_agent::workstreams;

/// Resolve the effective working directory: explicit global `--cwd`, else the
/// process cwd.
fn effective_cwd(cwd: Option<PathBuf>) -> Result<PathBuf> {
    match cwd {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("resolve current directory"),
    }
}

async fn pool() -> Result<sqlx::PgPool> {
    ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect to fleet Postgres: {e}"))
}

pub async fn handle_workstream(cmd: crate::WorkstreamCommand, cwd: Option<PathBuf>) -> Result<()> {
    let pg = pool().await?;
    match cmd {
        crate::WorkstreamCommand::Attach { tool, goal } => {
            let dir = effective_cwd(cwd)?;
            let ws = workstreams::workstream_for_dir(&pg, &dir)
                .await?
                .with_context(|| {
                    format!(
                        "no workstream matches {} — is this a known project? (`ff workstream list`)",
                        dir.display()
                    )
                })?;
            let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
            let sid = workstreams::attach(
                &pg,
                &ws,
                &worker,
                &tool,
                &dir.display().to_string(),
                goal.as_deref(),
            )
            .await?;
            println!(
                "✅ attached to workstream '{}' ({})",
                ws.basename, ws.project_key
            );
            println!("   session: {sid}");
            println!("   node: {worker} · tool: {tool}");
            if let Some(g) = goal {
                println!("   goal: {g}");
            }
            println!("\nreport progress with:  ff workstream report --summary \"…\" --note \"…\"");
        }
        crate::WorkstreamCommand::Report {
            tool,
            summary,
            focus,
            note,
        } => {
            if summary.is_none() && focus.is_none() && note.is_none() {
                anyhow::bail!(
                    "nothing to report — pass at least one of --summary / --focus / --note"
                );
            }
            let dir = effective_cwd(cwd)?;
            let ws = workstreams::workstream_for_dir(&pg, &dir)
                .await?
                .with_context(|| format!("no workstream matches {}", dir.display()))?;
            let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
            let sid = workstreams::session_id_for(&worker, &ws.project_key, &tool);
            let updated = workstreams::report(
                &pg,
                &sid,
                summary.as_deref(),
                focus.as_deref(),
                note.as_deref(),
            )
            .await?;
            println!(
                "✅ reported into '{}' ({})",
                updated.basename, updated.project_key
            );
            if let Some(s) = &updated.working_summary {
                println!("   summary: {s}");
            }
        }
        crate::WorkstreamCommand::Status { tool } => {
            let dir = effective_cwd(cwd)?;
            let ws = workstreams::workstream_for_dir(&pg, &dir)
                .await?
                .with_context(|| format!("no workstream matches {}", dir.display()))?;
            let clients = workstreams::attached_clients(&pg, ws.id).await?;
            let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
            let me = workstreams::session_id_for(&worker, &ws.project_key, &tool);

            println!("📽  Workstream: {} ({})", ws.basename, ws.project_key);
            println!("   status: {}  ·  remote: {}", ws.status, ws.git_remote);
            println!(
                "   working_summary: {}",
                ws.working_summary.as_deref().unwrap_or("<none>")
            );
            println!("\n   attached clients ({}):", clients.len());
            if clients.is_empty() {
                println!("     <none — no CLI has attached yet>");
            }
            for c in &clients {
                let last = c
                    .last_report_at
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "never".to_string());
                let mark = if c.session_id == me {
                    " ← this session"
                } else {
                    ""
                };
                println!(
                    "     • {} [{}] {} · last report {}{}",
                    c.worker_name, c.tool, c.status, last, mark
                );
                if let Some(g) = &c.goal {
                    println!("       goal: {g}");
                }
            }
        }
        crate::WorkstreamCommand::List => {
            let rows = sqlx::query_as::<_, (String, String, String)>(
                "SELECT project_key, basename, coalesce(git_remote,'') \
                   FROM ff_workstreams WHERE status='active' ORDER BY project_key",
            )
            .fetch_all(&pg)
            .await?;
            println!("Active workstreams ({}):", rows.len());
            for (key, base, remote) in rows {
                println!("  • {base}  ({key})  {remote}");
            }
        }
    }
    Ok(())
}
