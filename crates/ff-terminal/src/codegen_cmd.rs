use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::warn;

use crate::{GREEN, RED, RESET, YELLOW};

pub async fn handle_codegen(
    task: String,
    repo: Option<String>,
    model: Option<String>,
    rounds: u32,
    no_backstop: bool,
) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    let repo_path = match repo {
        Some(repo) => PathBuf::from(repo),
        None => std::env::current_dir().context("resolve current dir")?,
    };

    let outcome = ff_agent::codegen_apply::codegen_apply(
        &pool,
        &repo_path,
        &task,
        model.as_deref(),
        rounds,
        None,
    )
    .await?;

    if outcome.applied {
        print_outcome(&outcome);
        return Ok(());
    }

    if no_backstop {
        print_outcome(&outcome);
        return Ok(());
    }

    let local_error = outcome
        .error
        .as_deref()
        .unwrap_or("no error detail returned by local coder");
    println!(
        "{YELLOW}local coder couldn't land it ({local_error}); escalating to codex backstop{RESET}"
    );

    match ff_agent::cli_executor::execute_cli_in_dir(
        "codex",
        &task,
        &[],
        Some(repo_path.as_path()),
        Some(Duration::from_secs(600)),
    )
    .await
    {
        Ok(result) if result.exit_code == 0 => {}
        Ok(result) => {
            println!(
                "{YELLOW}codex backstop also failed:{RESET} codex exited {}",
                result.exit_code
            );
            print_cli_output(&result.stdout, &result.stderr);
            return Ok(());
        }
        Err(e) => {
            warn!(error = %e, "codex backstop invocation failed");
            println!("{YELLOW}codex backstop also failed:{RESET} {e}");
            return Ok(());
        }
    }

    let status = match git_status_porcelain(&repo_path) {
        Ok(status) => status,
        Err(e) => {
            warn!(error = %e, repo = %repo_path.display(), "codex backstop status check failed");
            println!("{YELLOW}codex backstop also failed:{RESET} could not verify git status: {e}");
            return Ok(());
        }
    };

    if status.trim().is_empty() {
        println!("{YELLOW}codex backstop also failed:{RESET} no file changes landed");
        return Ok(());
    }

    match cargo_check(&repo_path) {
        Ok(output) if output.status.success() => {
            println!("{GREEN}✓ codex backstop landed the change{RESET}");
        }
        Ok(output) => {
            println!(
                "{YELLOW}codex backstop also failed:{RESET} cargo check did not pass\n{}",
                command_error("cargo check", &output)
            );
        }
        Err(e) => {
            warn!(error = %e, repo = %repo_path.display(), "codex backstop cargo check failed");
            println!("{YELLOW}codex backstop also failed:{RESET} could not run cargo check: {e}");
        }
    }

    Ok(())
}

fn print_outcome(outcome: &ff_agent::codegen_apply::CodegenOutcome) {
    let status = if outcome.applied {
        format!("{GREEN}true{RESET}")
    } else {
        format!("{RED}false{RESET}")
    };
    println!("applied: {status}");
    println!("rounds:  {}", outcome.rounds);
    if let Some(diff) = &outcome.final_diff {
        println!("diff:    {} bytes", diff.len());
    }
    if let Some(error) = &outcome.error {
        println!("{YELLOW}error:{RESET}\n{error}");
    }
}

fn git_status_porcelain(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("status")
        .arg("--porcelain")
        .output()
        .with_context(|| format!("run git status in {}", repo_path.display()))?;
    if !output.status.success() {
        anyhow::bail!("{}", command_error("git status --porcelain", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn cargo_check(repo_path: &Path) -> Result<Output> {
    Command::new("cargo")
        .arg("check")
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("run cargo check in {}", repo_path.display()))
}

fn command_error(label: &str, output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!(
        "{label} exited with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout.trim(),
        stderr.trim()
    )
}

fn print_cli_output(stdout: &str, stderr: &str) {
    if !stdout.trim().is_empty() {
        println!("stdout:\n{}", stdout.trim());
    }
    if !stderr.trim().is_empty() {
        println!("stderr:\n{}", stderr.trim());
    }
}
