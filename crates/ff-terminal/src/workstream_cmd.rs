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
        crate::WorkstreamCommand::Heartbeat { tool } => {
            // Best-effort liveness — never error a session's Stop hook. If the dir
            // isn't a known project or the session isn't attached, silently no-op.
            let dir = effective_cwd(cwd)?;
            if let Ok(Some(ws)) = workstreams::workstream_for_dir(&pg, &dir).await {
                let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
                let sid = workstreams::session_id_for(&worker, &ws.project_key, &tool);
                let _ = workstreams::heartbeat(&pg, &sid).await;
            }
        }
        crate::WorkstreamCommand::InstallHooks { r#for, dry_run } => {
            install_workstream_hooks(&r#for, dry_run)?;
        }
    }
    Ok(())
}

/// Write SessionStart auto-attach + Stop heartbeat hooks into each CLI's config
/// so a session in a project folder binds to its workstream with no manual step.
fn install_workstream_hooks(which: &str, dry_run: bool) -> Result<()> {
    let home = dirs::home_dir().context("resolve home directory")?;
    let targets: Vec<&str> = match which {
        "all" => vec!["claude", "codex", "kimi"],
        one => vec![one],
    };
    for tool in targets {
        match tool {
            "claude" => install_claude_hooks(&home, dry_run)?,
            // codex/kimi hook formats differ; wire claude first (this session),
            // extend to the others once their hook schema is confirmed.
            "codex" | "kimi" => {
                println!("  ⏭  {tool}: hook install not yet implemented (claude first)");
            }
            other => println!("  ⚠ unknown tool '{other}' — skipping"),
        }
    }
    Ok(())
}

/// Add SessionStart (`ff workstream attach`) + Stop (`ff workstream heartbeat`)
/// hooks to `~/.claude/settings.json`, preserving every other key. Idempotent —
/// re-running replaces the ff hook entries without duplicating them.
fn install_claude_hooks(home: &std::path::Path, dry_run: bool) -> Result<()> {
    use serde_json::{Value, json};
    let path = home.join(".claude").join("settings.json");
    let mut doc: Value = if path.exists() {
        let s = std::fs::read_to_string(&path)?;
        if s.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        json!({})
    };

    // Marker so we can find + replace only OUR hook entries on re-install.
    let attach_cmd = "ff workstream attach --tool claude >/dev/null 2>&1 || true";
    let beat_cmd = "ff workstream heartbeat --tool claude >/dev/null 2>&1 || true";
    let ff_entry = |cmd: &str| {
        json!({ "hooks": [ { "type": "command", "command": cmd } ] })
    };

    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks is not an object"))?;

    // For each event, drop any prior ff entry (identified by our command string)
    // then append the fresh one — keeps the operator's own hooks untouched.
    for (event, cmd) in [("SessionStart", attach_cmd), ("Stop", beat_cmd)] {
        let arr = hooks.entry(event).or_insert_with(|| json!([]));
        if let Some(list) = arr.as_array_mut() {
            list.retain(|group| {
                !group
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|h| {
                        h.iter().any(|e| {
                            e.get("command").and_then(|c| c.as_str()).is_some_and(|c| {
                                c.contains("ff workstream attach")
                                    || c.contains("ff workstream heartbeat")
                            })
                        })
                    })
                    .unwrap_or(false)
            });
            list.push(ff_entry(cmd));
        }
    }

    let pretty = serde_json::to_string_pretty(&doc)?;
    if dry_run {
        println!("  [dry-run] would write {}:\n{pretty}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, pretty).with_context(|| format!("write {}", path.display()))?;
    println!("  ✓ claude: SessionStart auto-attach + Stop heartbeat → {}", path.display());
    println!("    (new sessions in a project folder now auto-attach to its workstream)");
    Ok(())
}
