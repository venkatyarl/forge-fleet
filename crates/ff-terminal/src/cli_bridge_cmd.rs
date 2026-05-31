//! `ff cli <vendor> "<prompt>"` — invoke a cloud coding CLI (Claude Code,
//! Codex, Kimi, …) as a headless subprocess so JARVIS / ff can wield the
//! frontier vendor agents alongside the local fleet.
//!
//! This is the operator-facing front door for the Layer-2 vendor-CLI bridge
//! (`ff_agent::cli_executor`). Unlike `ff run --backend <vendor>` (which wraps
//! the call in ff's supervisor + failure-retry loop), `ff cli` is a thin,
//! one-shot pass-through: resolve vendor → binary + headless flags, spawn,
//! capture, print. The vendor CLI handles its own auth (the user's logged-in
//! Claude Code / Codex / Kimi session) — ff never touches secrets here.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use ff_agent::cli_executor::{BACKENDS, execute_cli_in_dir, which_on_path};

use crate::{CYAN, GREEN, RED, RESET, YELLOW};

/// Handle `ff cli <vendor> <prompt> [--cwd] [--output] [--timeout]`.
pub async fn handle_cli(
    vendor: String,
    prompt: String,
    cwd: Option<PathBuf>,
    output: String,
    timeout_secs: Option<u64>,
) -> Result<()> {
    let output = output.to_lowercase();
    if output != "text" && output != "json" {
        anyhow::bail!("--output must be `text` or `json` (got `{output}`)");
    }

    // Resolve the working directory: explicit --cwd, else the process cwd.
    let work_dir: PathBuf = match cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    if !work_dir.is_dir() {
        anyhow::bail!("--cwd `{}` is not a directory", work_dir.display());
    }

    // Validate the vendor up front so we can print a helpful, vendor-aware
    // error listing which CLIs ARE installed on this machine.
    let known = BACKENDS
        .iter()
        .any(|b| b.name.eq_ignore_ascii_case(&vendor));
    if !known {
        let names = BACKENDS
            .iter()
            .map(|b| b.name)
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("unknown vendor '{vendor}'; expected one of: {names}");
    }
    if let Some(cfg) = BACKENDS
        .iter()
        .find(|b| b.name.eq_ignore_ascii_case(&vendor))
    {
        if which_on_path(cfg.binary).is_none() {
            let installed: Vec<&str> = BACKENDS
                .iter()
                .filter(|b| which_on_path(b.binary).is_some())
                .map(|b| b.name)
                .collect();
            let installed_msg = if installed.is_empty() {
                "none of the supported vendor CLIs are installed on this machine".to_string()
            } else {
                format!("installed here: {}", installed.join(", "))
            };
            anyhow::bail!(
                "vendor '{}' CLI (`{}`) is not installed on this machine — {}.\n\
                 Install the vendor's CLI and log in, then retry.",
                cfg.name,
                cfg.binary,
                installed_msg
            );
        }
    }

    let timeout = timeout_secs.map(Duration::from_secs);

    if output == "text" {
        eprintln!(
            "{CYAN}→{RESET} dispatching to {GREEN}{vendor}{RESET} CLI in {}{}",
            work_dir.display(),
            timeout_secs
                .map(|s| format!(" (timeout {s}s)"))
                .unwrap_or_default()
        );
    }

    let result =
        execute_cli_in_dir(&vendor, &prompt, &[], Some(work_dir.as_path()), timeout).await?;

    if output == "json" {
        // Compact, machine-readable envelope for JARVIS / scripted callers.
        let json = serde_json::json!({
            "vendor": result.backend,
            "binary_path": result.binary_path,
            "exit_code": result.exit_code,
            "output": result.stdout,
            "stderr": result.stderr,
            "duration_ms": result.duration_ms,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        // Human path: stdout is the answer; surface stderr + non-zero exit.
        print!("{}", result.stdout);
        if !result.stdout.ends_with('\n') && !result.stdout.is_empty() {
            println!();
        }
        if !result.stderr.trim().is_empty() {
            eprintln!("{YELLOW}[{vendor} stderr]{RESET} {}", result.stderr.trim());
        }
        if result.exit_code != 0 {
            eprintln!(
                "{RED}✗{RESET} {vendor} CLI exited with code {} ({}ms)",
                result.exit_code, result.duration_ms
            );
        }
    }

    // Mirror the vendor CLI's exit status so scripts/JARVIS can branch on it.
    if result.exit_code != 0 {
        std::process::exit(result.exit_code as i32);
    }
    Ok(())
}
