use anyhow::Result;
use clap::Subcommand;
use sqlx::PgPool;

use crate::{CYAN, GREEN, RESET};

#[derive(Debug, Clone, Subcommand)]
pub enum CapabilityCommand {
    /// Show local-vs-cloud split and estimated cloud dollars saved by local routing.
    Stats {
        /// Rolling window in hours.
        #[arg(long, default_value_t = 24)]
        hours: i64,
        /// Emit JSON for scripts/agents.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub async fn handle_capability(pool: &PgPool, command: CapabilityCommand) -> Result<()> {
    ff_db::run_postgres_migrations(pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match command {
        CapabilityCommand::Stats { hours, json } => stats(pool, hours, json).await,
    }
}

async fn stats(pool: &PgPool, hours: i64, json: bool) -> Result<()> {
    let hours = hours.max(1);
    let s = ff_db::pg_capability_usage_stats(pool, hours).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }

    println!("{GREEN}✓ Capability usage{RESET} ({hours}h)");
    println!("  total calls:              {}", s.total_calls);
    println!(
        "  local calls:              {} ({:.1}%)",
        s.local_calls, s.local_percent
    );
    println!(
        "  cloud calls:              {} ({:.1}%)",
        s.cloud_calls, s.cloud_percent
    );
    println!(
        "  avg latency:              {}",
        s.avg_latency_ms
            .map(|v| format!("{v:.0} ms"))
            .unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "  estimated cloud $ saved:  {CYAN}${:.4}{RESET}",
        s.estimated_cloud_saved_usd
    );
    Ok(())
}
