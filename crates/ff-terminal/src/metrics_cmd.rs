use crate::parse_duration_secs;
use anyhow::Result;

pub async fn handle_metrics(cmd: crate::MetricsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::MetricsCommand::History { computer, since } => {
            let secs = parse_duration_secs(&since).unwrap_or(3600);
            let rows =
                ff_agent::metrics_downsampler::history_for_computer(&pool, &computer, secs as i64)
                    .await?;

            if rows.is_empty() {
                println!(
                    "(no metrics rows for {computer} in the last {since} — downsampler writes at minute boundaries on the leader)"
                );
                return Ok(());
            }
            println!(
                "{:<20} {:>6} {:>6} {:>7} {:>8} {:>6} {:>4} {:>4} {:>6}",
                "TIME", "CPU%", "RAM%", "RAM-GB", "DISK-GB", "GPU%", "Q", "ACT", "TOK/S"
            );
            for r in rows {
                println!(
                    "{:<20} {:>6.1} {:>6.1} {:>7.1} {:>8.1} {:>6.1} {:>4} {:>4} {:>6.1}",
                    r.recorded_at.format("%Y-%m-%d %H:%M:%S"),
                    r.cpu_pct.unwrap_or(0.0),
                    r.ram_pct.unwrap_or(0.0),
                    r.ram_used_gb.unwrap_or(0.0),
                    r.disk_free_gb.unwrap_or(0.0),
                    r.gpu_pct.unwrap_or(0.0),
                    r.llm_queue_depth.unwrap_or(0),
                    r.llm_active_requests.unwrap_or(0),
                    r.llm_tokens_per_sec.unwrap_or(0.0),
                );
            }
        }
    }
    Ok(())
}
