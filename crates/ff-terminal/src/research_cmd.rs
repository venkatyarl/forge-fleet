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
        output_path: output,
        gateway_url: gateway.unwrap_or_default(),
        planner_model: planner_model.unwrap_or_default(),
        subagent_model: subagent_model.unwrap_or_default(),
        ..Default::default()
    };

    eprintln!(
        "{CYAN}▶ ff research{RESET}  \x1b[2mparallel={parallel} depth={depth} \
         planner={} subagent={}{RESET}",
        config.planner_model, config.subagent_model
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
