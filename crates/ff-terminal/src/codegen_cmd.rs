use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{GREEN, RED, RESET, YELLOW};

pub async fn handle_codegen(
    task: String,
    repo: Option<String>,
    model: Option<String>,
    rounds: u32,
) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    let repo_path = match repo {
        Some(repo) => PathBuf::from(repo),
        None => std::env::current_dir().context("resolve current dir")?,
    };

    let outcome =
        ff_agent::codegen_apply::codegen_apply(&pool, &repo_path, &task, model.as_deref(), rounds)
            .await?;

    let status = if outcome.applied {
        format!("{GREEN}true{RESET}")
    } else {
        format!("{RED}false{RESET}")
    };
    println!("applied: {status}");
    println!("rounds:  {}", outcome.rounds);
    if let Some(diff) = outcome.final_diff {
        println!("diff:    {} bytes", diff.len());
    }
    if let Some(error) = outcome.error {
        println!("{YELLOW}error:{RESET}\n{error}");
    }

    Ok(())
}
