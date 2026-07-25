//! Claude (Anthropic) cloud-usage poller — leader tick.
//!
//! Claude is operator-reserved for building the fleet itself, so ForgeFleet
//! never *routes* dispatch to it — but the operator still wants visibility into
//! how much Claude budget the build work is burning. Anthropic exposes no
//! per-window usage endpoint we can poll here, so (like the fallback path of
//! [`crate::codex_usage_poller`]) this poller ESTIMATES the calendar-month spend
//! from `ff_interactions` — the sum of `cost_usd` for `engine = 'claude'` turns
//! since the start of the month — against an operator-configured monthly budget
//! (`claude.monthly_limit_usd` secret / `CLAUDE_MONTHLY_LIMIT_USD` env) to derive
//! `monthly_pct`, with `monthly_reset_at` set to the first of next month (UTC).
//!
//! It UPDATEs only the monthly columns of the `claude` row of
//! `cloud_budget_buckets`, so a poll never clobbers other columns. The
//! percentage math and month-rollover helpers are the provider-agnostic ones
//! from [`crate::codex_usage_poller`] rather than copies.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::codex_usage_poller::{monthly_pct_from_spend, next_month_start};
use crate::kimi_usage_poller::{KimiBudgetUpdate as BudgetUpdate, WindowStat};

/// Provider key for the `cloud_budget_buckets` row this poller maintains.
const PROVIDER: &str = "claude";

/// Fleet-secret holding the operator's monthly Claude budget, in USD, used to
/// turn accumulated `ff_interactions` spend into a `monthly_pct`.
const MONTHLY_LIMIT_SECRET: &str = "claude.monthly_limit_usd";

/// `source` stamped on the row (Claude has no pollable usage endpoint here, so
/// the ff_interactions estimate is always how the number is derived).
const SOURCE_ESTIMATE: &str = "claude usage poller (ff_interactions estimate)";

/// Gate so the "no claude.monthly_limit_usd" message logs at most once per
/// process — an unconfigured budget is an operator choice, not a recurring fault.
static MISSING_LIMIT_LOGGED: AtomicBool = AtomicBool::new(false);

/// Sum this-calendar-month `cost_usd` for Claude turns in `ff_interactions`.
/// Cast to `double precision` so the `COALESCE(..., 0)` literal doesn't decay
/// the column type and sqlx can decode an `f64`.
pub async fn fetch_claude_month_spend_usd(pool: &PgPool) -> Result<f64> {
    let spent: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(cost_usd), 0)::double precision \
           FROM ff_interactions \
          WHERE lower(engine) = $1 \
            AND ts >= date_trunc('month', NOW())",
    )
    .bind(PROVIDER)
    .fetch_one(pool)
    .await?;
    Ok(spent)
}

/// Build a monthly-only [`BudgetUpdate`] from an estimated spend + limit.
/// Returns an empty update (nothing written) when the spend/limit can't form a
/// percentage.
fn estimate_update(spent_usd: f64, monthly_limit_usd: f64, now: DateTime<Utc>) -> BudgetUpdate {
    let mut update = BudgetUpdate::default();
    if let Some(pct) = monthly_pct_from_spend(spent_usd, monthly_limit_usd) {
        update.monthly = Some(WindowStat {
            pct: Some(pct),
            reset_at: Some(next_month_start(now)),
        });
    }
    update
}

/// Apply the monthly-window estimate to the `claude` row of
/// `cloud_budget_buckets`. Only the monthly columns are touched (and only when
/// present), so other columns keep their prior values. Also stamps
/// `last_success_at`, `source`, `updated_at`.
async fn apply_claude_budget_update(pool: &PgPool, update: &BudgetUpdate) -> Result<()> {
    let monthly_present = update.monthly.is_some();
    let monthly_pct = update.monthly.as_ref().and_then(|w| w.pct);
    let monthly_reset_at = update.monthly.as_ref().and_then(|w| w.reset_at);

    sqlx::query(
        "UPDATE cloud_budget_buckets \
            SET monthly_pct      = CASE WHEN $1 THEN $2 ELSE monthly_pct END, \
                monthly_reset_at = CASE WHEN $1 THEN $3 ELSE monthly_reset_at END, \
                last_success_at  = NOW(), \
                source           = $4, \
                updated_at       = NOW() \
          WHERE provider = $5",
    )
    .bind(monthly_present)
    .bind(monthly_pct)
    .bind(monthly_reset_at)
    .bind(SOURCE_ESTIMATE)
    .bind(PROVIDER)
    .execute(pool)
    .await?;
    Ok(())
}

/// Best-effort: record that a poll failed on the `claude` row. Ignores its own
/// errors — this runs on the failure path and must not mask the real error.
async fn record_poll_error(pool: &PgPool) {
    let _ =
        sqlx::query("UPDATE cloud_budget_buckets SET last_error_at = NOW() WHERE provider = $1")
            .bind(PROVIDER)
            .execute(pool)
            .await;
}

/// One poll pass. Estimates the monthly window from `ff_interactions` spend
/// against `claude.monthly_limit_usd`. Returns `Ok(false)` when nothing usable
/// could be written (no configured limit, or the numbers didn't yield a
/// window); `Ok(true)` when the row was refreshed.
pub async fn poll_claude_usage_once(pool: &PgPool) -> Result<bool> {
    poll_claude_usage_once_at(pool, Utc::now()).await
}

/// [`poll_claude_usage_once`] with an injected `now` (for the monthly reset
/// time) so callers/tests can pin the clock.
async fn poll_claude_usage_once_at(pool: &PgPool, now: DateTime<Utc>) -> Result<bool> {
    let Some(limit_raw) = crate::fleet_info::fetch_secret(MONTHLY_LIMIT_SECRET).await else {
        if !MISSING_LIMIT_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::info!(
                secret = MONTHLY_LIMIT_SECRET,
                "claude usage poller: no monthly limit configured; skipping (logged once)"
            );
        }
        return Ok(false);
    };
    // A limit exists now, so re-arm the one-shot log for a future disappearance.
    MISSING_LIMIT_LOGGED.store(false, Ordering::Relaxed);

    let Ok(monthly_limit_usd) = limit_raw.trim().parse::<f64>() else {
        return Err(anyhow::anyhow!(
            "claude usage poller: {} is not a number: {:?}",
            MONTHLY_LIMIT_SECRET,
            limit_raw
        ));
    };

    let spent_usd = match fetch_claude_month_spend_usd(pool).await {
        Ok(s) => s,
        Err(e) => {
            record_poll_error(pool).await;
            return Err(e);
        }
    };
    let update = estimate_update(spent_usd, monthly_limit_usd, now);
    if update.is_empty() {
        tracing::warn!(
            spent_usd,
            monthly_limit_usd,
            "claude usage poller: estimate yielded no monthly window"
        );
        return Ok(false);
    }

    apply_claude_budget_update(pool, &update).await?;
    tracing::debug!(
        spent_usd,
        monthly_limit_usd,
        ?update,
        "claude usage poller: refreshed from ff_interactions estimate"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // ── DB-free: estimate builds only the monthly window ─────────────────────

    #[test]
    fn estimate_update_sets_monthly_only() {
        let now = ts("2026-07-25T00:00:00Z");
        let u = estimate_update(250.0, 1000.0, now);
        assert!(!u.is_empty());
        assert!(u.window_exhausted_until.is_none());
        assert!(u.weekly.is_none());
        let monthly = u.monthly.expect("monthly window present");
        assert_eq!(monthly.pct, Some(25));
        assert_eq!(monthly.reset_at, Some(ts("2026-08-01T00:00:00Z")));
    }

    #[test]
    fn estimate_update_without_limit_is_empty() {
        let now = ts("2026-07-25T00:00:00Z");
        // No usable limit → nothing to write, so absent windows stay untouched.
        assert!(estimate_update(250.0, 0.0, now).is_empty());
    }

    // ── DB test: must SKIP cleanly when no Postgres env is configured ────────

    /// Guard mirrors the fleet rule: any test needing Postgres early-returns
    /// when neither `FORGEFLEET_POSTGRES_URL` nor `FORGEFLEET_DATABASE_URL` is
    /// set, so CI's DB-less `cargo test --lib` never panics.
    fn db_url() -> Option<String> {
        std::env::var("FORGEFLEET_POSTGRES_URL")
            .ok()
            .or_else(|| std::env::var("FORGEFLEET_DATABASE_URL").ok())
    }

    #[tokio::test]
    async fn month_spend_query_runs_against_live_db() {
        let Some(url) = db_url() else {
            eprintln!("no FORGEFLEET_POSTGRES_URL/DATABASE_URL; skipping DB test");
            return;
        };
        let pool = match PgPool::connect(&url).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping DB test: cannot connect: {e}");
                return;
            }
        };
        // The query must execute and return a non-negative sum (0 when empty).
        let spent = fetch_claude_month_spend_usd(&pool)
            .await
            .expect("month spend query runs");
        assert!(spent >= 0.0, "month-to-date spend is non-negative");
    }
}
