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
///
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

/// Resolve which CLI is attaching/reporting. Priority:
///   1. explicit `--tool <cli>`
///   2. `FORGEFLEET_WS_TOOL` env (operator/wrapper override)
///   3. the nearest CLI in this process's ancestor chain (claude/codex/kimi
///      spawn `ff` as a child, so the parent walk finds the real caller even
///      through intermediate shells)
///   4. CLI-specific env markers (inherited by spawned children)
///
/// Falls back to "unknown" rather than guessing "claude" — a seat under the
/// WRONG tool (the old default) silently splits one session's record in two.
fn resolve_tool(explicit: Option<&str>) -> String {
    let clean = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    if let Some(t) = explicit.and_then(clean) {
        return t;
    }
    if let Ok(v) = std::env::var("FORGEFLEET_WS_TOOL")
        && let Some(t) = clean(&v)
    {
        return t;
    }
    detect_tool_from_ancestors()
        .or_else(detect_tool_from_env)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Like `resolve_tool` but returns `None` when detection fails — for read-only
/// paths (status "← this session" marker) where no marker beats a wrong one.
fn detect_tool() -> Option<String> {
    let clean = |s: String| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    };
    if let Ok(v) = std::env::var("FORGEFLEET_WS_TOOL")
        && let Some(t) = clean(v)
    {
        return Some(t);
    }
    detect_tool_from_ancestors().or_else(detect_tool_from_env)
}

/// Map a process command name (basename) to its CLI tool, when it's one of ours.
fn tool_from_comm(comm: &str) -> Option<&'static str> {
    match comm.rsplit('/').next().unwrap_or(comm) {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "kimi" => Some("kimi"),
        _ => None,
    }
}

/// CLI-specific env markers. Claude Code exports CLAUDECODE/CLAUDE_CODE_*;
/// Codex and Kimi expose session-id vars. `get` is injectable for testing.
fn detect_tool_from_env() -> Option<String> {
    tool_from_env_markers(&|k| std::env::var(k).ok())
}

fn tool_from_env_markers(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let set = |vars: &[&str]| {
        vars.iter()
            .any(|v| get(v).is_some_and(|s| !s.trim().is_empty()))
    };
    if set(&["KIMI_SESSION_ID"]) {
        Some("kimi".to_string())
    } else if set(&["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_SESSION_ID"]) {
        Some("claude".to_string())
    } else if set(&["CODEX_SESSION_ID", "CODEX_CONVERSATION_ID", "CODEX_SANDBOX"]) {
        Some("codex".to_string())
    } else {
        None
    }
}

/// Walk up to 8 ancestors looking for a claude/codex/kimi process. Nearest hit
/// wins, which also resolves the nested case (one CLI launched from another's
/// shell) correctly — env markers can't express "closest caller".
#[cfg(target_os = "linux")]
fn detect_tool_from_ancestors() -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..8 {
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            && let Some(t) = tool_from_comm(comm.trim())
        {
            return Some(t.to_string());
        }
        let ppid = linux_ppid(pid)?;
        if ppid <= 1 {
            return None;
        }
        pid = ppid;
    }
    None
}

/// Parent pid from /proc/<pid>/stat. After the closing paren of comm (which may
/// itself contain spaces/parens) the fields are: state, ppid, …
#[cfg(target_os = "linux")]
fn linux_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat[stat.rfind(')')? + 2..].split_whitespace();
    fields.next()?; // state
    fields.next()?.parse().ok()
}

/// `ps`-based fallback for non-Linux (macOS): walk ppids via `ps -o ppid=,comm=`.
#[cfg(not(target_os = "linux"))]
fn detect_tool_from_ancestors() -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..8 {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let mut fields = line.trim().split_whitespace();
        let ppid: u32 = fields.next()?.parse().ok()?;
        if let Some(comm) = fields.next()
            && let Some(t) = tool_from_comm(comm)
        {
            return Some(t.to_string());
        }
        if ppid <= 1 {
            return None;
        }
        pid = ppid;
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
            let tool = resolve_tool(tool.as_deref());
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
        crate::WorkstreamCommand::Detach { tool, session } => {
            let tool = resolve_tool(tool.as_deref());
            let dir = effective_cwd(cwd)?;
            let ws = workstreams::workstream_for_dir(&pg, &dir)
                .await?
                .with_context(|| format!("no workstream matches {}", dir.display()))?;
            let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
            let token = resolve_session_token(session.as_deref());
            let sid = workstreams::session_id_for_token(
                &worker,
                &ws.project_key,
                &tool,
                token.as_deref(),
            );
            workstreams::detach(&pg, &sid).await?;
            println!("detached from workstream {}", ws.project_key);
        }
        crate::WorkstreamCommand::Report {
            tool,
            summary,
            focus,
            note,
            session,
        } => {
            let tool = resolve_tool(tool.as_deref());
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
            let sid = workstreams::session_id_for_token(
                &worker,
                &ws.project_key,
                &tool,
                token.as_deref(),
            );
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
            // Mark "this session" from the explicit flag or auto-detection; if
            // neither identifies a CLI, show no marker — an unmarked listing
            // beats a wrong one.
            let me = tool
                .as_deref()
                .map(str::to_string)
                .or_else(detect_tool)
                .map(|t| {
                    let token = resolve_session_token(session.as_deref());
                    workstreams::session_id_for_token(
                        &worker,
                        &ws.project_key,
                        &t,
                        token.as_deref(),
                    )
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
        crate::WorkstreamCommand::List { json } => {
            let rows = sqlx::query_as::<_, (String, String, String)>(
                "SELECT project_key, basename, coalesce(git_remote,'') \
                   FROM ff_workstreams WHERE status='active' ORDER BY project_key",
            )
            .fetch_all(&pg)
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("Active workstreams ({}):", rows.len());
                for (key, base, remote) in rows {
                    println!("  • {base}  ({key})  {remote}");
                }
            }
        }
        crate::WorkstreamCommand::Heartbeat { tool, session } => {
            // Best-effort liveness — never error a session's Stop hook. If the dir
            // isn't a known project or the session isn't attached, silently no-op.
            let tool = resolve_tool(tool.as_deref());
            let dir = effective_cwd(cwd)?;
            if let Ok(Some(ws)) = workstreams::workstream_for_dir(&pg, &dir).await {
                let worker = ff_agent::fleet_info::resolve_this_worker_name().await;
                let token = resolve_session_token(session.as_deref());
                let sid = workstreams::session_id_for_token(
                    &worker,
                    &ws.project_key,
                    &tool,
                    token.as_deref(),
                );
                let _ = workstreams::heartbeat(&pg, &sid).await;
            }
        }
        crate::WorkstreamCommand::InstallHooks { r#for, dry_run } => {
            install_workstream_hooks(&r#for, dry_run)?;
        }
        crate::WorkstreamCommand::Prune {
            older_than_hours,
            dry_run,
        } => {
            let dir = effective_cwd(cwd)?;
            let ws = workstreams::workstream_for_dir(&pg, &dir)
                .await?
                .with_context(|| format!("no workstream matches {}", dir.display()))?;
            let cutoff = chrono::Utc::now() - chrono::Duration::hours(i64::from(older_than_hours));
            let pruned = workstreams::prune_stale_clients(&pg, ws.id, cutoff, dry_run).await?;
            let verb = if dry_run { "would detach" } else { "detached" };
            println!(
                "🧹 {} {} stale seat(s) in '{}' (idle > {}h):",
                verb,
                pruned.len(),
                ws.project_key,
                older_than_hours
            );
            for p in &pruned {
                println!(
                    "     • {} [{}] {} · last active {}",
                    p.worker_name,
                    p.tool,
                    p.session_id,
                    p.last_active_at.format("%Y-%m-%d %H:%M UTC")
                );
            }
            if pruned.is_empty() {
                println!("     <none — every seat is fresh>");
            }
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
                install_agents_md_directive(
                    &home.join(".kimi").join("AGENTS.md"),
                    "kimi",
                    dry_run,
                )?;
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
        println!(
            "  [dry-run] would append [[hooks]] block to {}",
            path.display()
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn comm_names_map_to_their_cli() {
        assert_eq!(tool_from_comm("claude"), Some("claude"));
        assert_eq!(tool_from_comm("codex"), Some("codex"));
        assert_eq!(tool_from_comm("kimi"), Some("kimi"));
        // ps comm can be a full path — basename still matches.
        assert_eq!(tool_from_comm("/usr/local/bin/kimi"), Some("kimi"));
        // Shells, node, and unknown processes are not CLIs we track.
        assert_eq!(tool_from_comm("bash"), None);
        assert_eq!(tool_from_comm("node"), None);
        assert_eq!(tool_from_comm("gnome-terminal-"), None);
    }

    #[test]
    fn env_markers_identify_each_cli() {
        assert_eq!(
            tool_from_env_markers(&env_with(&[("KIMI_SESSION_ID", "abc")])),
            Some("kimi".to_string())
        );
        assert_eq!(
            tool_from_env_markers(&env_with(&[("CLAUDECODE", "1")])),
            Some("claude".to_string())
        );
        assert_eq!(
            tool_from_env_markers(&env_with(&[("CLAUDE_SESSION_ID", "s1")])),
            Some("claude".to_string())
        );
        assert_eq!(
            tool_from_env_markers(&env_with(&[("CODEX_CONVERSATION_ID", "c1")])),
            Some("codex".to_string())
        );
    }

    #[test]
    fn codex_session_id_identifies_codex() {
        assert_eq!(
            tool_from_env_markers(&env_with(&[("CODEX_SESSION_ID", "c1")])),
            Some("codex".to_string())
        );
    }

    #[test]
    fn empty_env_marker_does_not_count() {
        assert_eq!(
            tool_from_env_markers(&env_with(&[("CLAUDECODE", "")])),
            None
        );
        assert_eq!(tool_from_env_markers(&env_with(&[])), None);
    }

    #[test]
    fn kimi_marker_env_detection() {
        assert_eq!(
            tool_from_env_markers(&env_with(&[("KIMI_SESSION_ID", "k1")])),
            Some("kimi".to_string())
        );
    }

    #[test]
    fn kimi_marker_beats_inherited_claude_marker() {
        // A kimi session launched from inside a claude shell inherits
        // CLAUDECODE=1; kimi's own marker must win for the env fallback.
        let env = env_with(&[("KIMI_SESSION_ID", "k1"), ("CLAUDECODE", "1")]);
        assert_eq!(tool_from_env_markers(&env), Some("kimi".to_string()));
    }

    #[test]
    fn explicit_tool_always_wins() {
        assert_eq!(resolve_tool(Some("codex")), "codex");
        assert_eq!(resolve_tool(Some("  kimi  ")), "kimi");
    }

    #[test]
    fn explicit_empty_tool_falls_through_to_detection() {
        // An empty --tool is treated as omitted; detection runs (ancestor walk
        // finds the test runner's tree or nothing), never a panic.
        let detected = resolve_tool(Some("   "));
        assert!(!detected.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_ppid_matches_ps() {
        let pid = std::process::id();
        let expected = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            });
        if let Some(expected) = expected {
            assert_eq!(linux_ppid(pid), Some(expected));
        }
    }
}
