//! `ff ssh <worker> <command...>` — run a shell command on a fleet computer.
//!
//! The dogfood path for fleet debugging. Both autopilot loops shell out to
//! `ff` constantly; before this verb existed the only way to run a command on
//! a peer was raw `ssh user@ip` (which hides ff's resolver bugs — see the
//! "Always go through ff" rule) or `ff <worker> <cmd>` typed as a bare prompt,
//! which silently dispatched an LLM agent instead of erroring.
//!
//! Resolution is DB-first: `ff_agent::fleet_info::fetch_node_ip_user` reads the
//! worker's `ip` + `ssh_user` from Postgres (`computers` / `fleet_workers`),
//! never `~/.ssh/config`. The same plumbing the MCP `fleet_ssh` handler uses,
//! exposed on the CLI.

use anyhow::{Context, Result};
use tokio::process::Command;

const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Handle `ff ssh <worker> <command...>`.
pub async fn handle_ssh(
    worker: String,
    command: Vec<String>,
    sudo: bool,
    timeout: u64,
    json: bool,
) -> Result<()> {
    if command.is_empty() {
        anyhow::bail!("ff ssh requires a command to run, e.g. `ff ssh taylor uptime`");
    }

    // Resolve worker → (ip, ssh_user) from Postgres. DB is the source of truth;
    // we never read ~/.ssh/config.
    let (ip, ssh_user) = ff_agent::fleet_info::fetch_node_ip_user(&worker)
        .await
        .with_context(|| {
            format!(
                "could not resolve SSH target for '{worker}' — no computers/fleet_workers row \
                 (check `ff fleet nodes`)"
            )
        })?;

    // Join the trailing args into a single remote command line. The user is
    // responsible for quoting anything with shell metacharacters
    // (`ff ssh ace \"ps aux | grep mlx\"`).
    let remote_cmd = command.join(" ");
    let remote_cmd = if sudo {
        format!("sudo -n {remote_cmd}")
    } else {
        remote_cmd
    };

    let target = format!("{ssh_user}@{ip}");
    if !json {
        eprintln!("{CYAN}▶ ff ssh{RESET} {DIM}{target}{RESET}");
        eprintln!("{DIM}  $ {remote_cmd}{RESET}");
    }

    let connect_timeout = timeout.clamp(5, 30);
    let started = std::time::Instant::now();
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ConnectTimeout={connect_timeout}"),
            "-o",
            "StrictHostKeyChecking=accept-new",
            &target,
            &remote_cmd,
        ])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawn ssh: {e}"))?;

    let duration_ms = started.elapsed().as_millis();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let exit_code = out.status.code();

    if json {
        let v = serde_json::json!({
            "worker": worker,
            "host": ip,
            "user": ssh_user,
            "command": remote_cmd,
            "success": out.status.success(),
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "stdout": stdout,
            "stderr": stderr,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        if !stdout.is_empty() {
            print!("{stdout}");
            if !stdout.ends_with('\n') {
                println!();
            }
        }
        if !stderr.trim().is_empty() {
            eprint!("{stderr}");
        }
        if out.status.success() {
            eprintln!("{GREEN}✓ exit 0{RESET} {DIM}({duration_ms}ms){RESET}");
        } else {
            eprintln!(
                "{RED}✗ exit {}{RESET} {DIM}({duration_ms}ms){RESET}",
                exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            );
        }
    }

    // Propagate the remote exit status so scripts can branch on it.
    if !out.status.success() {
        std::process::exit(exit_code.unwrap_or(1));
    }
    Ok(())
}
