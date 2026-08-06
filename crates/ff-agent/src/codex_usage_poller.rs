//! Codex (OpenAI coding-plan) usage poller — leader tick.
//!
//! Modeled on [`crate::kimi_usage_poller`], but Codex has no reliably pollable
//! per-window usage endpoint the way Kimi does, so this poller has two paths and
//! prefers authoritative numbers whenever they are *safely* available:
//!
//!   1. **Authoritative** — only when an operator has wired BOTH the
//!      `codex.usages_url` and `codex.api_key` fleet_secrets. We `GET` the
//!      endpoint and parse its per-window `limit`/`used`/`remaining`/`reset`
//!      figures with the shared, provider-agnostic Kimi parser (the response
//!      shape is identical). Absent by default — an operator has to opt in — so
//!      we never invent a numbers-source that isn't really there.
//!   2. **Fallback (default)** — no usage endpoint is configured (or it failed),
//!      so we ESTIMATE the calendar-month spend from `ff_interactions` (the sum
//!      of `cost_usd` for `engine = 'codex'` turns since the start of the month)
//!      against an operator-configured monthly budget (`codex.monthly_limit_usd`
//!      secret / `CODEX_MONTHLY_LIMIT_USD` env) to derive `monthly_pct`, with
//!      `monthly_reset_at` set to the first of next month (UTC).
//!
//! Either path UPDATEs the `codex` row of `cloud_budget_buckets` (schema V189/
//! V191); only windows we actually computed are written, so a partial refresh
//! never wipes good data (see the `CASE WHEN present` guards in
//! [`apply_codex_budget_update`]).

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use sqlx::PgPool;

use crate::notifications::SHARED_HTTP;
// The authoritative Codex endpoint returns the same per-window shape as Kimi's,
// so we reuse Kimi's provider-agnostic parser and DB-ready types rather than
// copying ~150 lines of JSON-shape handling.
use crate::kimi_usage_poller::{
    KimiBudgetUpdate as BudgetUpdate, WindowStat, parse_kimi_usages as parse_usage_windows,
};

/// Provider key for the `cloud_budget_buckets` row this poller maintains.
const PROVIDER: &str = "codex";

/// Fleet-secret holding an (optional) authoritative usage endpoint URL.
const USAGES_URL_SECRET: &str = "codex.usages_url";

/// Fleet-secret holding the API key for the authoritative usage endpoint.
const API_KEY_SECRET: &str = "codex.api_key";

/// Fleet-secret holding the operator's monthly Codex budget, in USD, used to
/// turn accumulated `ff_interactions` spend into a `monthly_pct`.
const MONTHLY_LIMIT_SECRET: &str = "codex.monthly_limit_usd";

/// Per-request HTTP timeout for one authoritative usages fetch.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `source` stamped on the row when the authoritative endpoint was used.
const SOURCE_AUTHORITATIVE: &str = "codex usage poller";

/// `source` stamped on the row when the ff_interactions estimate was used.
const SOURCE_ESTIMATE: &str = "codex usage poller (ff_interactions estimate)";

/// Gate so the "no codex.monthly_limit_usd" message logs at most once per
/// process — an unconfigured budget is an operator choice, not a recurring fault.
static MISSING_LIMIT_LOGGED: AtomicBool = AtomicBool::new(false);

/// Compute a `0..=100` percent-used from month-to-date spend against a monthly
/// budget. `None` when the limit is missing/non-positive or the spend isn't a
/// finite number (can't form a ratio).
pub fn monthly_pct_from_spend(spent_usd: f64, monthly_limit_usd: f64) -> Option<i16> {
    if monthly_limit_usd.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        || !spent_usd.is_finite()
    {
        return None;
    }
    let pct = (spent_usd.max(0.0) / monthly_limit_usd * 100.0).round();
    Some(pct.clamp(0.0, 100.0) as i16)
}

/// First instant of the calendar month AFTER `now`, at 00:00:00 UTC — when the
/// monthly budget window rolls over. Falls back to `now` if the (always valid)
/// date construction ever fails, so callers never get a bogus reset time.
pub fn next_month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .unwrap_or(now)
}

/// Build a fallback [`BudgetUpdate`] carrying only the monthly window from an
/// estimated spend + limit. Returns an empty update (nothing written) when the
/// spend/limit can't form a percentage.
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

/// Sum this-calendar-month `cost_usd` for Codex turns in `ff_interactions`.
/// Cast to `double precision` so the `COALESCE(..., 0)` literal doesn't decay
/// the column type and sqlx can decode an `f64`.
pub async fn fetch_codex_month_spend_usd(pool: &PgPool) -> Result<f64> {
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

/// Apply a parsed/estimated update to the `codex` row of `cloud_budget_buckets`.
/// Only windows present in `update` are written; absent windows keep their prior
/// column values (the `CASE WHEN <present>` guards). Also stamps
/// `last_success_at`, `source`, `updated_at`.
pub async fn apply_codex_budget_update(
    pool: &PgPool,
    update: &BudgetUpdate,
    source: &str,
) -> Result<()> {
    let session_present = update.window_exhausted_until.is_some();
    let window_exhausted_until = update.window_exhausted_until.flatten();

    let weekly_present = update.weekly.is_some();
    let weekly_pct = update.weekly.as_ref().and_then(|w| w.pct);
    let weekly_reset_at = update.weekly.as_ref().and_then(|w| w.reset_at);

    let monthly_present = update.monthly.is_some();
    let monthly_pct = update.monthly.as_ref().and_then(|w| w.pct);
    let monthly_reset_at = update.monthly.as_ref().and_then(|w| w.reset_at);

    sqlx::query(
        "UPDATE cloud_budget_buckets \
            SET window_exhausted_until = CASE WHEN $1 THEN $2 ELSE window_exhausted_until END, \
                weekly_pct             = CASE WHEN $3 THEN $4 ELSE weekly_pct END, \
                weekly_reset_at        = CASE WHEN $3 THEN $5 ELSE weekly_reset_at END, \
                monthly_pct            = CASE WHEN $6 THEN $7 ELSE monthly_pct END, \
                monthly_reset_at       = CASE WHEN $6 THEN $8 ELSE monthly_reset_at END, \
                last_success_at        = NOW(), \
                source                 = $9, \
                updated_at             = NOW() \
          WHERE provider = $10",
    )
    .bind(session_present)
    .bind(window_exhausted_until)
    .bind(weekly_present)
    .bind(weekly_pct)
    .bind(weekly_reset_at)
    .bind(monthly_present)
    .bind(monthly_pct)
    .bind(monthly_reset_at)
    .bind(source)
    .bind(PROVIDER)
    .execute(pool)
    .await?;
    Ok(())
}

/// Best-effort: record that a poll failed on the `codex` row. Ignores its own
/// errors — this runs on the failure path and must not mask the real error.
async fn record_poll_error(pool: &PgPool) {
    let _ =
        sqlx::query("UPDATE cloud_budget_buckets SET last_error_at = NOW() WHERE provider = $1")
            .bind(PROVIDER)
            .execute(pool)
            .await;
}

/// Try the authoritative usage endpoint. Returns:
///   * `Ok(Some(update))` — endpoint configured and returned a usable body;
///   * `Ok(None)`         — no endpoint configured (fall back to the estimate);
///   * `Err(_)`           — endpoint configured but the fetch/parse failed
///     (caller logs and falls back rather than failing the whole tick).
async fn try_authoritative(pool: &PgPool) -> Result<Option<BudgetUpdate>> {
    let (Some(url), Some(api_key)) = (
        crate::fleet_info::fetch_secret(USAGES_URL_SECRET).await,
        crate::fleet_info::fetch_secret(API_KEY_SECRET).await,
    ) else {
        return Ok(None);
    };

    let resp = match SHARED_HTTP
        .get(&url)
        .bearer_auth(&api_key)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            record_poll_error(pool).await;
            return Err(anyhow::anyhow!("GET codex usages: {}", e.without_url()));
        }
    };

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("read codex usages body: {}", e.without_url()))?;
    if !status.is_success() {
        record_poll_error(pool).await;
        return Err(anyhow::anyhow!(
            "codex usages returned HTTP {}",
            status.as_u16()
        ));
    }

    match parse_usage_windows(&body) {
        Ok(u) => Ok(Some(u)),
        Err(e) => {
            record_poll_error(pool).await;
            Err(e)
        }
    }
}

/// One poll pass. Prefers the authoritative endpoint when configured and
/// returning data; otherwise estimates the monthly window from `ff_interactions`
/// against `codex.monthly_limit_usd`. Returns `Ok(false)` when nothing usable
/// could be written (no endpoint AND no configured limit, or the numbers didn't
/// yield a window); `Ok(true)` when the row was refreshed.
pub async fn poll_codex_usage_once(pool: &PgPool) -> Result<bool> {
    poll_codex_usage_once_at(pool, Utc::now()).await
}

/// [`poll_codex_usage_once`] with an injected `now` (for the monthly reset time)
/// so callers/tests can pin the clock.
async fn poll_codex_usage_once_at(pool: &PgPool, now: DateTime<Utc>) -> Result<bool> {
    // 1) Authoritative endpoint, if safely available.
    match try_authoritative(pool).await {
        Ok(Some(update)) => {
            if !update.is_empty() {
                apply_codex_budget_update(pool, &update, SOURCE_AUTHORITATIVE).await?;
                tracing::debug!(
                    ?update,
                    "codex usage poller: refreshed from authoritative endpoint"
                );
                return Ok(true);
            }
            tracing::warn!(
                "codex usage poller: authoritative endpoint returned no windows; estimating"
            );
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, "codex usage poller: authoritative endpoint failed; estimating");
        }
    }

    // 2) Fallback: estimate the monthly window from ff_interactions spend.
    let Some(limit_raw) = crate::fleet_info::fetch_secret(MONTHLY_LIMIT_SECRET).await else {
        if !MISSING_LIMIT_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::info!(
                secret = MONTHLY_LIMIT_SECRET,
                "codex usage poller: no monthly limit configured and no usage endpoint; skipping (logged once)"
            );
        }
        return Ok(false);
    };
    // A limit exists now, so re-arm the one-shot log for a future disappearance.
    MISSING_LIMIT_LOGGED.store(false, Ordering::Relaxed);

    let Ok(monthly_limit_usd) = limit_raw.trim().parse::<f64>() else {
        return Err(anyhow::anyhow!(
            "codex usage poller: {MONTHLY_LIMIT_SECRET} is not a number: {limit_raw:?}"
        ));
    };

    let spent_usd = fetch_codex_month_spend_usd(pool).await?;
    let update = estimate_update(spent_usd, monthly_limit_usd, now);
    if update.is_empty() {
        tracing::warn!(
            spent_usd,
            monthly_limit_usd,
            "codex usage poller: estimate yielded no monthly window"
        );
        return Ok(false);
    }

    apply_codex_budget_update(pool, &update, SOURCE_ESTIMATE).await?;
    tracing::debug!(
        spent_usd,
        monthly_limit_usd,
        ?update,
        "codex usage poller: refreshed from ff_interactions estimate"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── DB-free: monthly-percent math ────────────────────────────────────────

    #[test]
    fn monthly_pct_basic_ratio() {
        assert_eq!(monthly_pct_from_spend(25.0, 100.0), Some(25));
        // Rounds to nearest whole percent.
        assert_eq!(monthly_pct_from_spend(126.0, 400.0), Some(32)); // 31.5 → 32
    }

    #[test]
    fn monthly_pct_clamps_and_guards() {
        // Over budget clamps to 100.
        assert_eq!(monthly_pct_from_spend(500.0, 100.0), Some(100));
        // Zero spend is 0%.
        assert_eq!(monthly_pct_from_spend(0.0, 100.0), Some(0));
        // Negative spend floors at 0%.
        assert_eq!(monthly_pct_from_spend(-5.0, 100.0), Some(0));
        // No/invalid limit → cannot form a ratio.
        assert_eq!(monthly_pct_from_spend(10.0, 0.0), None);
        assert_eq!(monthly_pct_from_spend(10.0, -1.0), None);
        assert_eq!(monthly_pct_from_spend(10.0, f64::NAN), None);
        assert_eq!(monthly_pct_from_spend(10.0, f64::INFINITY), Some(0));
        // Non-finite spend → None (never panics).
        assert_eq!(monthly_pct_from_spend(f64::NAN, 100.0), None);
        assert_eq!(monthly_pct_from_spend(f64::INFINITY, 100.0), None);
    }

    // ── DB-free: reset-time rollover ─────────────────────────────────────────

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn next_month_start_mid_month() {
        assert_eq!(
            next_month_start(ts("2026-07-25T13:37:00Z")),
            ts("2026-08-01T00:00:00Z")
        );
    }

    #[test]
    fn next_month_start_rolls_year_at_december() {
        assert_eq!(
            next_month_start(ts("2026-12-31T23:59:59Z")),
            ts("2027-01-01T00:00:00Z")
        );
    }

    // ── DB-free: estimate builds only the monthly window ─────────────────────

    #[test]
    fn estimate_update_sets_monthly_only() {
        let now = ts("2026-07-25T00:00:00Z");
        let u = estimate_update(300.0, 1000.0, now);
        assert!(!u.is_empty());
        assert!(u.window_exhausted_until.is_none());
        assert!(u.weekly.is_none());
        let monthly = u.monthly.expect("monthly window present");
        assert_eq!(monthly.pct, Some(30));
        assert_eq!(monthly.reset_at, Some(ts("2026-08-01T00:00:00Z")));
    }

    #[test]
    fn estimate_update_without_limit_is_empty() {
        let now = ts("2026-07-25T00:00:00Z");
        // No usable limit → nothing to write, so absent windows stay untouched.
        assert!(estimate_update(300.0, 0.0, now).is_empty());
    }

    // ── DB-free: authoritative body reuses the shared window parser ──────────

    #[test]
    fn authoritative_body_parses_monthly_window() {
        // Same shape the Kimi endpoint returns; the shared parser handles it.
        let body = r#"{"usages":[{"window":"monthly","limit":5000,"used":950,
            "remaining":4050,"resetTime":"2026-08-03T00:00:00Z"}]}"#;
        let u = parse_usage_windows(body).expect("body parses");
        let monthly = u.monthly.expect("monthly window present");
        assert_eq!(monthly.pct, Some(19));
        assert_eq!(monthly.reset_at, Some(ts("2026-08-03T00:00:00Z")));
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
        let spent = fetch_codex_month_spend_usd(&pool)
            .await
            .expect("month spend query runs");
        assert!(spent >= 0.0, "month-to-date spend is non-negative");
    }
}
