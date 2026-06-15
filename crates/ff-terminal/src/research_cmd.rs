use crate::{CYAN, GREEN, RESET, truncate_str};
use anyhow::Result;
use std::path::PathBuf;

pub async fn handle_research(
    prompt: &str,
    parallel: u32,
    depth: u32,
    output: Option<PathBuf>,
    gateway: Option<String>,
    planner_model: Option<String>,
    subagent_model: Option<String>,
    web_grounding: bool,
    detached: bool,
    verbose: bool,
) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let config = ff_agent::research::ResearchConfig {
        query: prompt.to_string(),
        parallel,
        depth,
        output_path: output.clone(),
        gateway_url: gateway.unwrap_or_default(),
        planner_model: planner_model.unwrap_or_default(),
        subagent_model: subagent_model.unwrap_or_default(),
        web_grounding,
        detached,
        ..Default::default()
    };

    // Detached: create the `queued` session and return immediately. The leader's
    // forgefleetd ResearchRunnerTick claims it and drives the full run inside the
    // daemon, so it survives this CLI being killed. Poll with `ff research --show`.
    if detached {
        if output.is_some() {
            eprintln!(
                "\x1b[2m  note: --output is ignored in --detach mode; the report lands in the \
                 DB — fetch it with `ff research --show <id> --output <path>`{RESET}"
            );
        }
        let session = ff_agent::research::ResearchSession::new(pool, config)
            .await
            .map_err(|e| anyhow::anyhow!("create research_session: {e}"))?;
        let id = session.id();
        eprintln!(
            "{GREEN}✓ research queued (detached){RESET}  \x1b[2mthe fleet leader will run it; \
             this CLI can exit{RESET}"
        );
        eprintln!("\x1b[2m  Session: {id}{RESET}");
        eprintln!("\x1b[2m  Poll:    ff research --show {id}{RESET}");
        // stdout = just the id, so scripts can capture it.
        println!("{id}");
        return Ok(());
    }

    eprintln!(
        "{CYAN}▶ ff research{RESET}  \x1b[2mparallel={parallel} depth={depth} \
         web={} planner={} subagent={}{RESET}",
        if web_grounding { "on" } else { "off" },
        config.planner_model,
        config.subagent_model
    );
    eprintln!("\x1b[2m  Query: {}{RESET}\n", prompt);

    let session = ff_agent::research::ResearchSession::new(pool, config)
        .await
        .map_err(|e| anyhow::anyhow!("create research_session: {e}"))?;
    eprintln!("\x1b[2m  Session: {}{RESET}", session.id());

    let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel(256);
    let verbose_flag = verbose;
    let progress_task = tokio::spawn(async move {
        while let Some(ev) = prog_rx.recv().await {
            use ff_agent::research::ResearchProgress;
            match ev {
                ResearchProgress::Planning { query } => {
                    eprintln!(
                        "{CYAN}[planner]{RESET} decomposing: {}",
                        truncate_str(&query, 80)
                    );
                }
                ResearchProgress::Dispatching { sub_count } => {
                    eprintln!("{CYAN}[dispatch]{RESET} {sub_count} sub-agents running in parallel");
                }
                ResearchProgress::Synthesizing => {
                    eprintln!("{CYAN}[synthesizer]{RESET} merging sub-agent outputs");
                }
                ResearchProgress::Event(ev) if verbose_flag => {
                    eprintln!("\x1b[2m  · {ev:?}\x1b[0m");
                }
                ResearchProgress::Event(_) => {}
            }
        }
    });

    let report = session
        .run(Some(prog_tx))
        .await
        .map_err(|e| anyhow::anyhow!("research run: {e}"))?;
    let _ = progress_task.await;

    eprintln!();
    eprintln!(
        "{GREEN}✓ research complete{RESET}  \x1b[2m{}/{} sub-agents succeeded · {}ms · \
         session {}{RESET}",
        report.subtasks_succeeded, report.subtask_count, report.duration_ms, report.session_id,
    );
    eprintln!();
    println!("{}", report.markdown);
    Ok(())
}

/// `ff research --show <session-id>` — read-only status + report for a session.
/// Unlike `--recover`, this never re-dispatches or re-synthesizes; it just
/// prints what's persisted. Primary way to poll a detached (`--detach`) run.
pub async fn handle_research_show(session_id: &str, output: Option<PathBuf>) -> Result<()> {
    let id = uuid::Uuid::parse_str(session_id.trim())
        .map_err(|_| anyhow::anyhow!("invalid session id {session_id:?} — expected a UUID"))?;

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;

    let st = ff_agent::research::fetch_status(&pool, id)
        .await
        .map_err(|e| anyhow::anyhow!("fetch research status: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no research session with id {id}"))?;

    let dot = match st.status.as_str() {
        "done" => GREEN,
        "failed" => "\x1b[31m",
        _ => CYAN,
    };
    eprintln!(
        "{dot}● {}{RESET}  \x1b[2m{}/{} sub-agents done · session {}{RESET}",
        st.status, st.subtask_done, st.subtask_total, st.id
    );
    eprintln!("\x1b[2m  Query: {}{RESET}", truncate_str(&st.query, 100));
    if let Some(err) = &st.error {
        eprintln!("\x1b[2m  Error: {}{RESET}", truncate_str(err, 200));
    }
    eprintln!();

    match st.report {
        Some(md) if !md.trim().is_empty() => {
            if let Some(path) = &output {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(path, &md)
                    .map_err(|e| anyhow::anyhow!("write report to {}: {e}", path.display()))?;
                eprintln!("{GREEN}✓ report written to {}{RESET}", path.display());
            } else {
                println!("{md}");
            }
        }
        _ => {
            eprintln!(
                "\x1b[2m  (no report yet — still {}. If this is a detached run, the leader is \
                 working on it; re-run --show in a bit.){RESET}",
                st.status
            );
        }
    }
    Ok(())
}

/// `ff research --recover <session-id>` — re-synthesize a killed run's report
/// from its already-persisted sub-agent outputs. No sub-agents are
/// re-dispatched; this only runs the synthesizer turn and flips the orphaned
/// session back to `done`. Recovers the work a crashed/killed `ff research`
/// CLI would otherwise lose (the reaper marks such sessions `failed`).
pub async fn handle_research_recover(session_id: &str, output: Option<PathBuf>) -> Result<()> {
    let id = uuid::Uuid::parse_str(session_id.trim())
        .map_err(|_| anyhow::anyhow!("invalid session id {session_id:?} — expected a UUID"))?;

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    eprintln!("{CYAN}▶ ff research --recover{RESET}  \x1b[2msession {id}{RESET}");
    eprintln!(
        "\x1b[2m  Re-synthesizing from persisted sub-agent outputs (no re-dispatch){RESET}\n"
    );

    let report = ff_agent::research::ResearchSession::recover(pool, id)
        .await
        .map_err(|e| anyhow::anyhow!("recover research session: {e}"))?;

    // Honor --output like the live run does.
    if let Some(path) = &output {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, &report.markdown)
            .map_err(|e| anyhow::anyhow!("write report to {}: {e}", path.display()))?;
    }

    eprintln!(
        "{GREEN}✓ research recovered{RESET}  \x1b[2m{}/{} sub-agents had usable output · \
         session {}{RESET}",
        report.subtasks_succeeded, report.subtask_count, report.session_id,
    );
    eprintln!();
    println!("{}", report.markdown);
    Ok(())
}
