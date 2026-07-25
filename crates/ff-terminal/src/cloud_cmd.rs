//! `ff cloud` subcommand implementations.
//!
//! `ff cloud usage` surfaces the per-provider cloud budget the usage pollers
//! (kimi/codex/claude) keep fresh in `cloud_budget_buckets`, so an operator can
//! see at a glance how much weekly/monthly headroom each backend has — the same
//! numbers the usage-weighted dispatch preference sorts on.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::{CYAN, GREEN, RED, RESET, YELLOW};

/// One `cloud_budget_buckets` row, distilled to the fields `ff cloud usage`
/// reports. Numerics are cast to `float8` in SQL so sqlx decodes plain `f64`.
struct BudgetRow {
    provider: String,
    weekly_pct: Option<i16>,
    monthly_pct: Option<i16>,
    spent_today: Option<f64>,
    spent_month_usd: Option<f64>,
    monthly_limit_usd: Option<f64>,
    window_exhausted_until: Option<DateTime<Utc>>,
    source: Option<String>,
    last_success_at: Option<DateTime<Utc>>,
    last_error_at: Option<DateTime<Utc>>,
}

/// Weekly headroom (`100 - weekly_pct`), i.e. the number the usage-weighted
/// cloud preference sorts backends by. `None` when no poller has written a
/// weekly percentage yet.
fn weekly_remaining_pct(weekly_pct: Option<i16>) -> Option<i16> {
    weekly_pct.map(|p| (100 - p).clamp(0, 100))
}

async fn fetch_budget_rows(pool: &sqlx::PgPool) -> Result<Vec<BudgetRow>> {
    let rows = sqlx::query(
        "SELECT provider, weekly_pct, monthly_pct, \
                spent_today::float8        AS spent_today, \
                spent_month_usd::float8    AS spent_month_usd, \
                monthly_limit_usd::float8  AS monthly_limit_usd, \
                window_exhausted_until, source, last_success_at, last_error_at \
           FROM cloud_budget_buckets \
          ORDER BY provider",
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(BudgetRow {
                provider: r.try_get("provider")?,
                weekly_pct: r.try_get("weekly_pct")?,
                monthly_pct: r.try_get("monthly_pct")?,
                spent_today: r.try_get("spent_today")?,
                spent_month_usd: r.try_get("spent_month_usd")?,
                monthly_limit_usd: r.try_get("monthly_limit_usd")?,
                window_exhausted_until: r.try_get("window_exhausted_until")?,
                source: r.try_get("source")?,
                last_success_at: r.try_get("last_success_at")?,
                last_error_at: r.try_get("last_error_at")?,
            })
        })
        .collect()
}

fn pct_cell(pct: Option<i16>) -> String {
    pct.map(|p| format!("{p}%")).unwrap_or_else(|| "—".into())
}

fn usd_cell(v: Option<f64>) -> String {
    v.map(|n| format!("${n:.2}")).unwrap_or_else(|| "—".into())
}

pub async fn handle_cloud_usage(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let rows = fetch_budget_rows(pool).await?;

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|b| {
                serde_json::json!({
                    "provider": b.provider,
                    "weekly_pct": b.weekly_pct,
                    "weekly_remaining_pct": weekly_remaining_pct(b.weekly_pct),
                    "monthly_pct": b.monthly_pct,
                    "spent_today_usd": b.spent_today,
                    "spent_month_usd": b.spent_month_usd,
                    "monthly_limit_usd": b.monthly_limit_usd,
                    "window_exhausted_until": b.window_exhausted_until.map(|t| t.to_rfc3339()),
                    "source": b.source,
                    "last_success_at": b.last_success_at.map(|t| t.to_rfc3339()),
                    "last_error_at": b.last_error_at.map(|t| t.to_rfc3339()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "{YELLOW}No cloud_budget_buckets rows. Usage pollers populate them once a monthly \
             limit / usage key is configured.{RESET}"
        );
        return Ok(());
    }

    println!(
        "{CYAN}{:<9} {:<8} {:<10} {:<8} {:<11} {:<12} {:<10} SOURCE{RESET}",
        "PROVIDER", "WEEKLY", "REMAINING", "MONTHLY", "SPENT_TODAY", "SPENT_MONTH", "LIMIT",
    );
    for b in &rows {
        // Colour the weekly-remaining headroom: green healthy, yellow tight,
        // red when the routing preference would treat this backend as spent.
        let remaining = weekly_remaining_pct(b.weekly_pct);
        let remaining_cell = match remaining {
            Some(r) if r <= 10 => format!("{RED}{r}%{RESET}"),
            Some(r) if r <= 25 => format!("{YELLOW}{r}%{RESET}"),
            Some(r) => format!("{GREEN}{r}%{RESET}"),
            None => "—".into(),
        };
        // The visible ANSI codes throw off `{:<10}` width, so pad the plain
        // form and colour separately.
        let remaining_plain = remaining
            .map(|r| format!("{r}%"))
            .unwrap_or_else(|| "—".into());
        let pad = 10usize.saturating_sub(remaining_plain.chars().count());

        let exhausted = b
            .window_exhausted_until
            .filter(|t| *t > Utc::now())
            .map(|t| format!(" {RED}[5h until {}]{RESET}", t.format("%m-%d %H:%M")))
            .unwrap_or_default();

        println!(
            "{:<9} {:<8} {}{:pad$} {:<8} {:<11} {:<12} {:<10} {}{}",
            b.provider,
            pct_cell(b.weekly_pct),
            remaining_cell,
            "",
            pct_cell(b.monthly_pct),
            usd_cell(b.spent_today),
            usd_cell(b.spent_month_usd),
            usd_cell(b.monthly_limit_usd),
            b.source.as_deref().unwrap_or("—"),
            exhausted,
            pad = pad,
        );
        if let Some(err_at) = b.last_error_at {
            let fresher_success = b.last_success_at.map(|s| s >= err_at).unwrap_or(false);
            if !fresher_success {
                println!(
                    "  {RED}⚠ last poll errored at {}{RESET}",
                    err_at.format("%Y-%m-%d %H:%M UTC")
                );
            }
        }
    }
    println!(
        "\n{} provider(s). REMAINING = weekly headroom the usage-weighted dispatch sorts on.",
        rows.len()
    );
    Ok(())
}

pub async fn handle_cloud(cmd: crate::CloudCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    match cmd {
        crate::CloudCommand::Usage { json } => handle_cloud_usage(&pool, json).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_remaining_is_complement_and_clamped() {
        assert_eq!(weekly_remaining_pct(Some(60)), Some(40));
        assert_eq!(weekly_remaining_pct(Some(0)), Some(100));
        assert_eq!(weekly_remaining_pct(Some(100)), Some(0));
        // Defensive: a stored >100 pct never yields a negative remaining.
        assert_eq!(weekly_remaining_pct(Some(120)), Some(0));
        assert_eq!(weekly_remaining_pct(None), None);
    }

    #[test]
    fn cells_render_none_as_dash() {
        assert_eq!(pct_cell(Some(19)), "19%");
        assert_eq!(pct_cell(None), "—");
        assert_eq!(usd_cell(Some(12.3)), "$12.30");
        assert_eq!(usd_cell(None), "—");
    }
}
