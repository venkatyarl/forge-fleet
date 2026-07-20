//! Kimi (Moonshot) coding-plan usage poller — leader tick (~10 min).
//!
//! Runs on the fleet leader only (registered `LeaderOnly` in the daemon tick
//! registry). Each pass:
//!   1. Reads the `kimi.api_key` fleet_secret (env fallback via
//!      [`fetch_secret`]). If it is missing, we log **once** and skip — an
//!      operator who hasn't wired a Kimi key should not see a warning every
//!      tick forever.
//!   2. `GET https://api.kimi.com/coding/v1/usages` with
//!      `Authorization: Bearer <key>`.
//!   3. Parses the per-window `limit`/`used`/`remaining`/`resetTime` fields
//!      ([`parse_kimi_usages`]) into a [`KimiBudgetUpdate`].
//!   4. UPDATEs the `kimi` row of `cloud_budget_buckets` (schema V181, from
//!      quota-T1): the 5h window drives `window_exhausted_until` (set when
//!      `remaining == 0`, cleared otherwise); the 7-day window drives
//!      `weekly_pct` / `weekly_reset_at`; the monthly window drives
//!      `monthly_pct` / `monthly_reset_at`.
//!
//! Only windows actually present in the response are written — an absent
//! window leaves its columns untouched (see the `CASE WHEN present` guards in
//! [`apply_kimi_budget_update`]) so a partial response never wipes good data.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;

use crate::notifications::SHARED_HTTP;

/// Provider key for the `cloud_budget_buckets` row this poller maintains.
const PROVIDER: &str = "kimi";

/// Fleet-secret key holding the Kimi coding API key.
const API_KEY_SECRET: &str = "kimi.api_key";

/// Usage endpoint for the Kimi coding plan.
const USAGES_URL: &str = "https://api.kimi.com/coding/v1/usages";

/// Per-request HTTP timeout for one usages fetch.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `source` string stamped on the row so operators can see who last wrote it.
const SOURCE: &str = "kimi usage poller";

/// Gate so the "no kimi.api_key" message is logged at most once per process.
static MISSING_KEY_LOGGED: AtomicBool = AtomicBool::new(false);

/// One window's percentage-used + reset time, as distilled from the response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowStat {
    /// Percent of the window's budget consumed, `0..=100`. `None` when the
    /// response didn't carry enough numbers to compute it.
    pub pct: Option<i16>,
    /// When this window's counters reset. `None` if the response omitted it.
    pub reset_at: Option<DateTime<Utc>>,
}

/// Parsed, DB-ready view of a Kimi usages response.
///
/// Each field is `Some` **only** when its window appeared in the response, so
/// callers can leave absent windows' columns untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KimiBudgetUpdate {
    /// 5h/session window. Outer `Some` = window present. Inner value is what
    /// `window_exhausted_until` should become: `Some(reset)` when the window
    /// is exhausted (`remaining == 0`), `None` when it still has headroom
    /// (clearing any stale exhaustion).
    pub window_exhausted_until: Option<Option<DateTime<Utc>>>,
    /// 7-day rolling window → `weekly_pct` / `weekly_reset_at`.
    pub weekly: Option<WindowStat>,
    /// Calendar-month window → `monthly_pct` / `monthly_reset_at`.
    pub monthly: Option<WindowStat>,
}

impl KimiBudgetUpdate {
    /// True when the response yielded nothing usable (no recognised windows).
    pub fn is_empty(&self) -> bool {
        self.window_exhausted_until.is_none() && self.weekly.is_none() && self.monthly.is_none()
    }
}

/// Which budget window a usages entry describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowKind {
    Session,
    Weekly,
    Monthly,
}

/// Classify a window label (`window`/`name`/`type`/`period` field) into one of
/// the three buckets we track. Tolerant of the various spellings a provider
/// might use (`5h`, `five_hour`, `session`; `7day`, `weekly`; `month`).
fn classify_window(label: &str) -> Option<WindowKind> {
    let l = label.to_ascii_lowercase();
    let l = l.replace(['_', '-', ' '], "");
    if l.contains("month") {
        Some(WindowKind::Monthly)
    } else if l.contains("week") || l.contains("7day") || l.contains("7d") {
        Some(WindowKind::Weekly)
    } else if l.contains("5h")
        || l.contains("5hour")
        || l.contains("fivehour")
        || l.contains("session")
    {
        Some(WindowKind::Session)
    } else {
        None
    }
}

/// Read a numeric field that may arrive as a JSON number or a numeric string.
fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// First present numeric field among `keys`.
fn num_field(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| obj.get(*k).and_then(as_f64))
}

/// Parse a reset time that may be RFC3339, epoch-seconds, or epoch-millis.
fn parse_reset_time(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    match v {
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s.trim())
            .ok()
            .map(|dt| dt.with_timezone(&Utc)),
        serde_json::Value::Number(n) => {
            let raw = n.as_i64()?;
            // Heuristic: values past ~year 2001 in seconds are < 1e12; larger
            // means milliseconds.
            if raw.abs() >= 1_000_000_000_000 {
                Utc.timestamp_millis_opt(raw).single()
            } else {
                Utc.timestamp_opt(raw, 0).single()
            }
        }
        _ => None,
    }
}

/// Compute a `0..=100` percent-used from whatever the entry provides. Prefers
/// `used/limit`; falls back to `(limit-remaining)/limit`. `None` when the
/// limit is missing/zero (can't form a ratio).
fn window_pct(limit: Option<f64>, used: Option<f64>, remaining: Option<f64>) -> Option<i16> {
    let limit = limit?;
    if limit <= 0.0 {
        return None;
    }
    let used = used.or_else(|| remaining.map(|r| limit - r))?;
    let pct = (used / limit * 100.0).round();
    Some(pct.clamp(0.0, 100.0) as i16)
}

/// Locate the array of per-window usage entries in the response body. Accepts a
/// bare top-level array or one nested under a common key.
fn find_usage_array(root: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    if let Some(arr) = root.as_array() {
        return Some(arr);
    }
    for key in ["usages", "usage", "windows", "data", "buckets"] {
        if let Some(arr) = root.get(key).and_then(|v| v.as_array()) {
            return Some(arr);
        }
    }
    None
}

/// Parse a raw Kimi `/coding/v1/usages` JSON body into a [`KimiBudgetUpdate`].
///
/// Pure (no I/O) so it is unit-tested against a fixture. Unrecognised windows
/// are ignored; a body with no recognised windows yields an empty update.
pub fn parse_kimi_usages(body: &str) -> Result<KimiBudgetUpdate> {
    let root: serde_json::Value = serde_json::from_str(body)?;
    let entries = find_usage_array(&root)
        .ok_or_else(|| anyhow::anyhow!("kimi usages: no usage array in response"))?;

    let mut update = KimiBudgetUpdate::default();
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(label) = ["window", "name", "type", "period", "kind"]
            .iter()
            .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        else {
            continue;
        };
        let Some(kind) = classify_window(label) else {
            continue;
        };

        let limit = num_field(obj, &["limit", "quota", "total"]);
        let used = num_field(obj, &["used", "usage", "consumed"]);
        let remaining = num_field(obj, &["remaining", "left", "available"]);
        let reset_at = ["resetTime", "reset_time", "resetAt", "reset_at", "reset"]
            .iter()
            .find_map(|k| obj.get(*k).and_then(parse_reset_time));

        match kind {
            WindowKind::Session => {
                // Exhausted when nothing remains. Prefer the explicit
                // `remaining`; fall back to limit-used if that's all we have.
                let effective_remaining = remaining
                    .or_else(|| match (limit, used) {
                        (Some(l), Some(u)) => Some(l - u),
                        _ => None,
                    })
                    .unwrap_or(1.0);
                update.window_exhausted_until = Some(if effective_remaining <= 0.0 {
                    reset_at
                } else {
                    None
                });
            }
            WindowKind::Weekly => {
                update.weekly = Some(WindowStat {
                    pct: window_pct(limit, used, remaining),
                    reset_at,
                });
            }
            WindowKind::Monthly => {
                update.monthly = Some(WindowStat {
                    pct: window_pct(limit, used, remaining),
                    reset_at,
                });
            }
        }
    }
    Ok(update)
}

/// Apply a parsed update to the `kimi` row of `cloud_budget_buckets`. Only
/// windows present in `update` are written; absent windows keep their prior
/// column values (the `CASE WHEN <present>` guards). Also stamps
/// `last_success_at`, `source`, `updated_at`.
pub async fn apply_kimi_budget_update(pool: &PgPool, update: &KimiBudgetUpdate) -> Result<()> {
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
    .bind(SOURCE)
    .bind(PROVIDER)
    .execute(pool)
    .await?;
    Ok(())
}

/// Best-effort: record that a poll failed on the `kimi` row. Ignores its own
/// errors — this runs on the failure path and must not mask the real error.
async fn record_poll_error(pool: &PgPool) {
    let _ =
        sqlx::query("UPDATE cloud_budget_buckets SET last_error_at = NOW() WHERE provider = $1")
            .bind(PROVIDER)
            .execute(pool)
            .await;
}

/// One poll pass. Returns `Ok(false)` (skipped) when no `kimi.api_key` is
/// configured; `Ok(true)` when the row was refreshed.
pub async fn poll_kimi_usage_once(pool: &PgPool) -> Result<bool> {
    let Some(api_key) = crate::fleet_info::fetch_secret(API_KEY_SECRET).await else {
        // Log once, then stay quiet: an unconfigured key is an operator choice,
        // not a recurring fault.
        if !MISSING_KEY_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::info!(
                secret = API_KEY_SECRET,
                "kimi usage poller: no api key configured; skipping (logged once)"
            );
        }
        return Ok(false);
    };
    // A key exists now, so re-arm the one-shot log for a future disappearance.
    MISSING_KEY_LOGGED.store(false, Ordering::Relaxed);

    let resp = match SHARED_HTTP
        .get(USAGES_URL)
        .bearer_auth(&api_key)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            record_poll_error(pool).await;
            return Err(anyhow::anyhow!("GET kimi usages: {}", e.without_url()));
        }
    };

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("read kimi usages body: {}", e.without_url()))?;
    if !status.is_success() {
        record_poll_error(pool).await;
        return Err(anyhow::anyhow!(
            "kimi usages returned HTTP {}",
            status.as_u16()
        ));
    }

    let update = match parse_kimi_usages(&body) {
        Ok(u) => u,
        Err(e) => {
            record_poll_error(pool).await;
            return Err(e);
        }
    };
    if update.is_empty() {
        tracing::warn!("kimi usage poller: no recognised windows in response");
        return Ok(false);
    }

    apply_kimi_budget_update(pool, &update).await?;
    tracing::debug!(?update, "kimi usage poller: refreshed cloud_budget_buckets");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative `/coding/v1/usages` body: 5h window exhausted, plus a
    /// 7-day window at 64% and a monthly window at 19% (mirrors the T1 seed).
    const FIXTURE: &str = r#"
    {
      "usages": [
        {
          "window": "5h",
          "limit": 100,
          "used": 100,
          "remaining": 0,
          "resetTime": "2026-07-20T04:20:00Z"
        },
        {
          "window": "7day",
          "limit": 1000,
          "used": 640,
          "remaining": 360,
          "resetTime": "2026-07-21T16:23:00Z"
        },
        {
          "window": "monthly",
          "limit": 5000,
          "used": 950,
          "remaining": 4050,
          "resetTime": "2026-08-03T00:00:00Z"
        }
      ]
    }
    "#;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_all_three_windows_from_fixture() {
        let u = parse_kimi_usages(FIXTURE).expect("fixture parses");
        assert!(!u.is_empty());

        // 5h exhausted (remaining == 0) → window_exhausted_until = resetTime.
        assert_eq!(
            u.window_exhausted_until,
            Some(Some(ts("2026-07-20T04:20:00Z")))
        );

        let weekly = u.weekly.expect("weekly window present");
        assert_eq!(weekly.pct, Some(64));
        assert_eq!(weekly.reset_at, Some(ts("2026-07-21T16:23:00Z")));

        let monthly = u.monthly.expect("monthly window present");
        assert_eq!(monthly.pct, Some(19));
        assert_eq!(monthly.reset_at, Some(ts("2026-08-03T00:00:00Z")));
    }

    #[test]
    fn session_with_headroom_clears_exhaustion() {
        let body = r#"{"usages":[{"window":"5h","limit":100,"used":10,"remaining":90,
            "resetTime":"2026-07-20T04:20:00Z"}]}"#;
        let u = parse_kimi_usages(body).unwrap();
        // Present, but not exhausted → inner None (column should be cleared).
        assert_eq!(u.window_exhausted_until, Some(None));
        assert!(u.weekly.is_none());
        assert!(u.monthly.is_none());
    }

    #[test]
    fn absent_windows_stay_none() {
        let body = r#"{"usages":[{"window":"weekly","limit":1000,"used":500,
            "remaining":500,"resetTime":"2026-07-21T16:23:00Z"}]}"#;
        let u = parse_kimi_usages(body).unwrap();
        assert!(u.window_exhausted_until.is_none());
        assert_eq!(u.weekly.and_then(|w| w.pct), Some(50));
        assert!(u.monthly.is_none());
    }

    #[test]
    fn pct_derives_from_remaining_when_used_absent() {
        // No `used`; percentage must fall back to (limit - remaining)/limit.
        let body = r#"{"usages":[{"name":"7 day","limit":200,"remaining":50,
            "reset":"2026-07-21T16:23:00Z"}]}"#;
        let u = parse_kimi_usages(body).unwrap();
        assert_eq!(u.weekly.unwrap().pct, Some(75));
    }

    #[test]
    fn epoch_reset_time_is_parsed() {
        // 1_753_142_580 == 2025-07-22T00:03:00Z (seconds).
        let body = r#"{"usages":[{"window":"5h","remaining":0,"resetTime":1753142580}]}"#;
        let u = parse_kimi_usages(body).unwrap();
        assert_eq!(
            u.window_exhausted_until,
            Some(Some(Utc.timestamp_opt(1_753_142_580, 0).single().unwrap()))
        );
    }

    #[test]
    fn unrecognised_windows_yield_empty_update() {
        let body = r#"{"usages":[{"window":"daily","limit":10,"used":1,"remaining":9}]}"#;
        let u = parse_kimi_usages(body).unwrap();
        assert!(u.is_empty());
    }

    #[test]
    fn classify_window_handles_spellings() {
        assert_eq!(classify_window("5h"), Some(WindowKind::Session));
        assert_eq!(classify_window("five_hour"), Some(WindowKind::Session));
        assert_eq!(classify_window("session"), Some(WindowKind::Session));
        assert_eq!(classify_window("7day"), Some(WindowKind::Weekly));
        assert_eq!(classify_window("Weekly"), Some(WindowKind::Weekly));
        assert_eq!(classify_window("monthly"), Some(WindowKind::Monthly));
        assert_eq!(classify_window("daily"), None);
    }
}
