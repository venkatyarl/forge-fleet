use anyhow::Result;
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
    source: String,
    sampled_at: chrono::DateTime<chrono::Utc>,
}

/// Handle `ff usage`, showing provider headroom sampled across the fleet.
pub async fn handle_usage(json: bool) -> Result<()> {
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
            String,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT c.name AS computer,
                u.provider,
                u.remaining_pct,
                u.window_kind,
                u.source,
                u.sampled_at
           FROM fleet_provider_usage u
           JOIN computers c ON c.id = u.computer_id
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

    if rows.is_empty() {
        println!("{DIM}(no provider usage samples){RESET}");
        return Ok(());
    }

    println!(
        "{:<16} {:<12} {:>10} {:<10} {:<14} sampled_at",
        "computer", "provider", "remaining", "window", "source"
    );
    println!("{}", "─".repeat(88));
    for r in rows {
        let remaining = match r.remaining_pct {
            Some(pct) if pct < 15.0 => format!("{RED}{pct:>6.1}%{RESET}"),
            Some(pct) => format!("{GREEN}{pct:>6.1}%{RESET}"),
            None => format!("{DIM}     -{RESET}"),
        };
        println!(
            "{:<16} {:<12} {:>19} {:<10} {:<14} {DIM}{}{RESET}",
            r.computer,
            r.provider,
            remaining,
            r.window_kind,
            r.source,
            r.sampled_at.format("%Y-%m-%d %H:%M:%S")
        );
    }

    Ok(())
}
