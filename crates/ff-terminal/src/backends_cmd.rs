//! `ff backends` — list the LLM-CLI backends available on THIS node.
//!
//! Detects which of claude/codex/gemini/kimi/grok are installed and, with
//! `--probe-auth`, whether each is actually authenticated (a tiny non-interactive
//! request returns instead of wedging on a login prompt). This is capability
//! roadmap A1: the per-node availability signal the dispatch picker and the
//! forgefleetd detector tick build on.

use std::time::Duration;

use ff_agent::backend_detect::detect_backends;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Handle `ff backends`. `probe_auth` runs the (slower) authenticated health
/// check per installed backend; `timeout_secs` bounds each auth probe.
pub async fn handle_backends(
    probe_auth: bool,
    json: bool,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let statuses = detect_backends(probe_auth, Duration::from_secs(timeout_secs.max(1))).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
        return Ok(());
    }

    println!(
        "{:<10} {:<10} {:<12} {}",
        "backend", "installed", "authed", "detail"
    );
    println!("{}", "─".repeat(64));
    let mut dispatchable = 0usize;
    for s in &statuses {
        let installed = if s.installed {
            format!("{GREEN}yes{RESET}")
        } else {
            format!("{DIM}no{RESET} ")
        };
        let authed = match s.authenticated {
            Some(true) => format!("{GREEN}yes{RESET}"),
            Some(false) => format!("{RED}no{RESET} "),
            None => format!("{DIM}-{RESET}  "),
        };
        if s.dispatchable() {
            dispatchable += 1;
        }
        println!(
            "{:<10} {:<19} {:<21} {DIM}{}{RESET}",
            s.name, installed, authed, s.detail
        );
    }
    println!();
    if probe_auth {
        println!(
            "{GREEN}{dispatchable}{RESET}/{} dispatchable (installed + authenticated)",
            statuses.len()
        );
    } else {
        let n = statuses.iter().filter(|s| s.installed).count();
        println!(
            "{n}/{} installed {DIM}(run with --probe-auth to check authentication){RESET}",
            statuses.len()
        );
    }
    Ok(())
}
