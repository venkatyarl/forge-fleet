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

/// Resolve this session's native token so multiple sessions of the SAME tool each
/// get their own seat. Priority (first non-empty wins):
///   1. explicit `--session <id>` (auto-attach hooks pass the CLI's native id)
///   2. `FORGEFLEET_WS_SESSION` env (operator/wrapper override)
///   3. a known CLI-native session env var (Claude/Codex/Kimi expose their own)
///   4. the SessionStart hook JSON on stdin — Claude Code pipes `{"session_id":…}`
///      to its hook command, so a bare `ff workstream attach` auto-detects it
/// Returns `None` when nothing is found → falls back to the single-seat-per-tool
/// id (backward compatible).
fn resolve_session_token(explicit: Option<&str>) -> Option<String> {
    let clean = |s: String| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    };
    if let Some(s) = explicit.and_then(|s| clean(s.to_string())) {
        return Some(s);
    }
    for var in [
        "FORGEFLEET_WS_SESSION",
        "CLAUDE_SESSION_ID",
        "CODEX_SESSION_ID",
        "KIMI_SESSION_ID",
        "CODEX_CONVERSATION_ID",
    ] {
        if let Ok(v) = std::env::var(var)
            && let Some(v) = clean(v)
        {
            return Some(v);
        }
    }
    // Hook context: Claude Code pipes a JSON payload to the hook command's stdin.
    // Only read when stdin is NOT a terminal (a real pipe), so an interactive
    // `ff workstream attach` never blocks waiting for input.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        use std::io::Read;
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_ok()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&buf)
            && let Some(sid) = v.get("session_id").and_then(|s| s.as_str())
            && let Some(sid) = clean(sid.to_string())
        {
            return Some(sid);
        }
    }
    None
}

pub async fn handle_workstream(cmd: crate::WorkstreamCommand, cwd: Option<PathBuf>) -> Result<()> {
    let pg = pool().await?;
    match cmd {
        crate::WorkstreamCommand::Attach {
            tool,
            goal,
            session,
        } => {
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
            let token = resolve_session_token(session.as_deref());
            let sid = workstreams::attach(
                &pg,
                &ws,
                &worker,
                &tool,
                &dir.display().to_string(),
                goal.as_deref(),
                token.as_deref(),
            )
            .await?;
            println!(
                "✅ attached to workstream '{}' ({})",
                ws.basename.as_deref().unwrap_or("<unnamed>"),
                ws.project_key
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
            session,
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
            let token = resolve_session_token(session.as_deref());
            let sid =
                workstreams::session_id_for_token(&worker, &ws.project_key, &tool, token.as_deref());
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
                updated.basename.as_deref().unwrap_or("<unnamed>"),
                updated.project_key
            );
            if let Some(s) = &updated.working_summary {
                println!("   summary: {s}");
            }
        }
        crate::WorkstreamCommand::Status { tool, session } => {
            let dir = effective_cwd(cwd)?;
            let ws = workstreams::workstream_for_dir(&pg, &dir)
                .await?
                .with_context(|| format!("no workstream matches {}", dir.display()))?;
            let clients = workstreams::attached_clients(&pg, ws.id).await?;
            let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
            // Only mark "this session" when the caller told us which CLI they
            // are — an unmarked listing beats a wrong marker.
            let me = tool.as_deref().map(|t| {
                let token = resolve_session_token(session.as_deref());
                workstreams::session_id_for_token(&worker, &ws.project_key, t, token.as_deref())
            });

            println!(
                "📽  Workstream: {} ({})",
                ws.basename.as_deref().unwrap_or("<unnamed>"),
                ws.project_key
            );
            println!(
                "   status: {}  ·  remote: {}",
                ws.status,
                ws.git_remote.as_deref().unwrap_or("<none>")
            );
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
                let mark = if me.as_deref() == Some(c.session_id.as_str()) {
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
        crate::WorkstreamCommand::Heartbeat { tool, session } => {
            // Best-effort liveness — never error a session's Stop hook. If the dir
            // isn't a known project or the session isn't attached, silently no-op.
            let dir = effective_cwd(cwd)?;
            if let Ok(Some(ws)) = workstreams::workstream_for_dir(&pg, &dir).await {
                let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
                let token = resolve_session_token(session.as_deref());
                let sid =
                    workstreams::session_id_for_token(&worker, &ws.project_key, &tool, token.as_deref());
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
            // Claude Code has deterministic SessionStart/Stop lifecycle hooks.
            "claude" => install_claude_hooks(&home, dry_run)?,
            // Codex hooks are TOOL-CALL/command lifecycle (fire around
            // tool use), NOT session-start — so there's no clean "on session
            // start" hook like Claude's. The cross-CLI mechanism that works today
            // is its AGENTS.md global-instructions file: a directive telling the
            // agent to attach on start + report as it works.
            // Instruction-based (relies on model compliance) rather than a
            // deterministic hook — honest tradeoff until an MCP-side auto-attach
            // (CLI-agnostic, deterministic) is built.
            "codex" => install_agents_md_directive(
                &home.join(".codex").join("AGENTS.md"),
                "codex",
                dry_run,
            )?,
            // Kimi Code CLI ≥0.29 has real SessionStart/Stop hooks (stdin JSON
            // carries session_id → per-session seats work). Keep the AGENTS.md
            // directive too as an instruction-level fallback.
            "kimi" => {
                install_kimi_hooks(&home, dry_run)?;
                install_agents_md_directive(&home.join(".kimi").join("AGENTS.md"), "kimi", dry_run)?;
            }
            other => println!("  ⚠ unknown tool '{other}' — skipping"),
        }
    }
    Ok(())
}

/// Append (idempotently) a ForgeFleet-workstream directive to a CLI's AGENTS.md
/// global-instructions file. Both codex and kimi read AGENTS.md; the directive
/// tells the agent to attach to its project workstream at session start and
/// report as it works. Delimited by markers so re-running replaces (not
/// duplicates) the block and never touches the operator's own instructions.
fn install_agents_md_directive(path: &std::path::Path, tool: &str, dry_run: bool) -> Result<()> {
    const BEGIN: &str = "<!-- ff-workstream:begin -->";
    const END: &str = "<!-- ff-workstream:end -->";
    let block = format!(
        "{BEGIN}\n\
         ## ForgeFleet workstream (session-of-record)\n\
         At the START of every session, run:\n\
         `ff workstream attach --tool {tool} --goal \"<what you're here to do>\"`\n\
         — this binds your session to the project's shared workstream (ff owns the\n\
         session across claude/codex/kimi). As you work, report progress with\n\
         `ff workstream report --tool {tool} --summary \"…\" --note \"…\"` so the\n\
         record reflects reality. Check `ff workstream status` to resume after a\n\
         crash. Do this WITHOUT being asked — it's standing policy.\n\
         {END}"
    );
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    // Strip any prior ff block, then append the fresh one.
    let cleaned = match (existing.find(BEGIN), existing.find(END)) {
        (Some(b), Some(e)) if e > b => {
            let mut s = existing.clone();
            s.replace_range(b..e + END.len(), "");
            s.trim_end().to_string()
        }
        _ => existing.trim_end().to_string(),
    };
    let joined = if cleaned.is_empty() {
        block
    } else {
        format!("{cleaned}\n\n{block}")
    };
    if dry_run {
        println!(
            "  [dry-run] would append ff-workstream directive to {}",
            path.display()
        );
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, format!("{joined}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    println!(
        "  ✓ {tool}: workstream directive → {} (instruction-based)",
        path.display()
    );
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
    let ff_entry = |cmd: &str| json!({ "hooks": [ { "type": "command", "command": cmd } ] });

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
    println!(
        "  ✓ claude: SessionStart auto-attach + Stop heartbeat → {}",
        path.display()
    );
    println!("    (new sessions in a project folder now auto-attach to its workstream)");
    Ok(())
}

/// Add SessionStart (`ff workstream attach`) + Stop (`ff workstream heartbeat`)
/// hooks to `~/.kimi-code/config.toml` as `[[hooks]]` entries. Kimi Code pipes
/// the hook payload (incl. `session_id`) to the command's stdin, and
/// `resolve_session_token` already reads that — so hooked kimi sessions get
/// per-session seats with no flags. Idempotent: our block is delimited by
/// marker comments and replaced (not duplicated) on re-run. A bare top-level
/// `hooks = []` key is removed first, since it would collide with `[[hooks]]`.
fn install_kimi_hooks(home: &std::path::Path, dry_run: bool) -> Result<()> {
    const BEGIN: &str = "# ff-workstream:begin";
    const END: &str = "# ff-workstream:end";
    let path = home.join(".kimi-code").join("config.toml");
    let block = format!(
        "{BEGIN} — managed by `ff workstream install-hooks`; re-run replaces this block\n\
         [[hooks]]\n\
         event = \"SessionStart\"\n\
         command = \"ff workstream attach --tool kimi >/dev/null 2>&1 || true\"\n\
         \n\
         [[hooks]]\n\
         event = \"Stop\"\n\
         command = \"ff workstream heartbeat --tool kimi >/dev/null 2>&1 || true\"\n\
         {END}"
    );
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    // Strip any prior ff block, then append the fresh one.
    let cleaned = match (existing.find(BEGIN), existing.find(END)) {
        (Some(b), Some(e)) if e > b => {
            let mut s = existing.clone();
            s.replace_range(b..e + END.len(), "");
            s.trim_end().to_string()
        }
        _ => existing.trim_end().to_string(),
    };
    // A bare `hooks = []` top-level key conflicts with the `[[hooks]]` array.
    let cleaned = cleaned
        .lines()
        .filter(|l| l.trim() != "hooks = []")
        .collect::<Vec<_>>()
        .join("\n");
    let joined = if cleaned.is_empty() {
        block
    } else {
        format!("{cleaned}\n\n{block}")
    };
    if dry_run {
        println!("  [dry-run] would append [[hooks]] block to {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, format!("{joined}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    println!(
        "  ✓ kimi: SessionStart auto-attach + Stop heartbeat → {}",
        path.display()
    );
    Ok(())
}
