use anyhow::{Context, Result, bail};

use crate::RouteCommand;

pub async fn handle_route(command: RouteCommand) -> Result<()> {
    match command {
        RouteCommand::Debug {
            computer,
            fresh_secs,
            json,
        } => debug_route(computer, fresh_secs, json).await,
    }
}

async fn debug_route(computer: Option<String>, fresh_secs: i64, json: bool) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|error| anyhow::anyhow!("connect Postgres for route debug: {error}"))?;
    let computer = computer
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|name| !name.trim().is_empty())
        .context("pass --computer NAME (HOSTNAME is not set)")?;
    let computer_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM computers WHERE lower(name) = lower($1)",
    )
    .bind(&computer)
    .fetch_optional(&pool)
    .await?
    .unwrap_or_else(uuid::Uuid::nil);
    if computer_id.is_nil() {
        bail!("unknown fleet computer {computer:?}");
    }

    let decision =
        ff_db::pg_cloud_route_for_computer(&pool, computer_id, fresh_secs, "debug").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&decision)?);
        return Ok(());
    }

    println!("trace: {}", decision.trace_id);
    println!("chosen: {}", decision.chosen.as_deref().unwrap_or("none"));
    println!("estimated cost: ${:.6}", decision.estimated_cost_usd);
    println!("candidates:");
    for candidate in decision.candidates {
        if candidate.rejected {
            println!(
                "  {:<10} rejected [{}] {}",
                candidate.backend,
                candidate.rejection_code.as_deref().unwrap_or("unknown"),
                candidate.rejection_reason.as_deref().unwrap_or("")
            );
        } else {
            println!(
                "  {:<10} eligible score={:.4}",
                candidate.backend,
                candidate.score.unwrap_or_default()
            );
        }
    }
    Ok(())
}
