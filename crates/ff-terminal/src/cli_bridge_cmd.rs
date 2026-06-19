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
    require_change: bool,
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
    // Fingerprint the working tree so we can tell whether the vendor actually
    // changed anything (catches the silent "exit 0, wrote nothing" failure).
    let fingerprint_before = git_dirty_fingerprint(&work_dir);

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

    // Did the vendor actually change any files? `None` = not a git repo (can't
    // tell → assume it did, to avoid false alarms). `Some(false)` = exit 0 but
    // the working tree is byte-for-byte unchanged.
    let fingerprint_after = git_dirty_fingerprint(&work_dir);
    let made_changes: Option<bool> = match (&fingerprint_before, &fingerprint_after) {
        (Some(b), Some(a)) => Some(b != a),
        _ => None,
    };
    let silent_noop = result.exit_code == 0 && made_changes == Some(false);

    if output == "json" {
        // Compact, machine-readable envelope for JARVIS / scripted callers.
        let json = serde_json::json!({
            "vendor": result.backend,
            "binary_path": result.binary_path,
            "exit_code": result.exit_code,
            "output": result.stdout,
            "stderr": result.stderr,
            "duration_ms": result.duration_ms,
            // null when cwd isn't a git repo (undeterminable).
            "made_file_changes": made_changes,
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

    // Warn loudly on the silent no-op (exit 0 but wrote nothing) — the classic
    // "codex stdin consumed by a pipe" / no-op-prompt failure. Visible in both
    // modes so a human or a scripted caller notices.
    if silent_noop {
        eprintln!(
            "{YELLOW}⚠ {vendor} exited 0 but made NO file change in {}{RESET} — the prompt may \
             have been a no-op, or the vendor's stdin was consumed by a downstream pipe \
             (do not pipe `ff cli {vendor}` stdout into a reader). Use `--require-change` to \
             treat this as a failure.",
            work_dir.display()
        );
    }

    // Mirror the vendor CLI's exit status so scripts/JARVIS can branch on it;
    // with --require-change, a silent no-op also fails (exit 3).
    if result.exit_code != 0 {
        std::process::exit(result.exit_code as i32);
    }
    if require_change && silent_noop {
        std::process::exit(3);
    }
    Ok(())
}

/// `git status --porcelain` (+ HEAD sha) of `dir`, or `None` when `dir` isn't a
/// git work tree. Used to detect whether a vendor CLI changed any files.
fn git_dirty_fingerprint(dir: &std::path::Path) -> Option<String> {
    let porcelain = std::process::Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !porcelain.status.success() {
        return None; // not a git repo
    }
    let head = std::process::Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    Some(format!(
        "{head}\n{}",
        String::from_utf8_lossy(&porcelain.stdout)
    ))
}
