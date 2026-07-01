//! `ff usage` — show provider headroom sampled across fleet computers.

use serde::Serialize;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Serialize)]
struct UsageRow {
    computer: String,
    provider: String,
    remaining_pct: Option<f64>,
    window_kind: String,
    source: Option<String>,
    sampled_at: chrono::DateTime<chrono::Utc>,
}

/// Handle `ff usage`.
pub async fn handle_usage(json: bool) -> anyhow::Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;

    let rows: Vec<UsageRow> = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<f64>,
            String,
            Option<String>,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT c.name AS computer, u.provider, u.remaining_pct, u.window_kind, u.source, u.sampled_at \
         FROM fleet_provider_usage u \
         JOIN computers c ON c.id = u.computer_id \
         ORDER BY u.sampled_at DESC",
    )
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(
        |(computer, provider, remaining_pct, window_kind, source, sampled_at)| UsageRow {
            computer,
            provider,
            remaining_pct,
            window_kind,
            source,
            sampled_at,
        },
    )
    .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!(
        "{:<16} {:<10} {:>10} {:<10} {:<18} {}",
        "computer", "provider", "remaining", "window", "source", "sampled_at"
    );
    println!("{}", "─".repeat(92));

    for row in rows {
        let remaining = match row.remaining_pct {
            Some(pct) if pct < 15.0 => format!("{RED}{pct:>6.1}%{RESET}"),
            Some(pct) => format!("{GREEN}{pct:>6.1}%{RESET}"),
            None => format!("{DIM}{:>7}{RESET}", "-"),
        };
        let source = row.source.as_deref().unwrap_or("-");
        println!(
            "{:<16} {:<10} {:>19} {:<10} {:<18} {DIM}{}{RESET}",
            row.computer,
            row.provider,
            remaining,
            row.window_kind,
            source,
            row.sampled_at.format("%Y-%m-%d %H:%M:%S")
        );
    }

    Ok(())
}
