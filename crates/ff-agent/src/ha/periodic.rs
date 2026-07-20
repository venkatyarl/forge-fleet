//! Periodic (clock-scheduled) daemon tasks — currently the **nightly Telegram
//! digest**.
//!
//! Unlike the fixed-interval ticks in [`crate::daemon`], the digest is pinned
//! to a wall-clock time: once per day at [`DIGEST_HOUR_LOCAL`]:00 local time
//! the leader summarizes the fleet's velocity views (V179:
//! `v_throughput_hourly`, `v_lead_time_daily`, `v_computer_builds_daily`,
//! `v_first_pass_rate_daily`) and sends the summary to the operator via
//! [`crate::telegram::send_telegram_recorded`].
//!
//! Design notes:
//!   - **Driven by the daemon tick registry** (leader-gated, 60s cadence).
//!     Each tick is a cheap pure-time check before the send hour; after it,
//!     one `EXISTS` probe against `telegram_messages` until the digest lands.
//!   - **At-most-once per day, fleet-wide.** The digest is recorded with a
//!     deterministic per-date session id (`nightly-digest-YYYY-MM-DD`), so the
//!     `telegram_messages` row written by the send doubles as the dedup marker
//!     across daemon restarts and leader failover. It also means an operator
//!     REPLY to the digest routes back to a stable session id.
//!   - **Catch-up, not skip.** If the daemon was down at 08:00, the first
//!     leader tick after it comes back (same local day) still sends.
//!   - **Best-effort sections.** The velocity views ship in migration V179;
//!     on a fleet that hasn't applied it yet each section degrades to "n/a"
//!     instead of failing the tick.

use anyhow::Result;
use chrono::Timelike;
use sqlx::PgPool;

/// Local hour (0-23) after which the daily digest becomes due.
pub const DIGEST_HOUR_LOCAL: u32 = 8;

const DIGEST_SESSION_PREFIX: &str = "nightly-digest";

/// Velocity numbers backing one digest. Every section is optional so a fleet
/// without the V179 views (or with no events yet) still produces a digest.
#[derive(Debug, Clone, Default)]
pub struct DigestData {
    /// (completed, failed) work items over the last 24h.
    pub throughput_24h: Option<(i64, i64)>,
    /// Completion-weighted average lead time in seconds since yesterday.
    pub avg_lead_time_secs: Option<f64>,
    /// (completed, first-pass) work items since yesterday.
    pub first_pass: Option<(i64, i64)>,
    /// Per-computer (name, started, succeeded, failed) builds since yesterday.
    pub computer_builds: Vec<(String, i64, i64, i64)>,
    /// (provider, exhausted-until, weekly percent used, weekly reset).
    pub cloud_budgets: Vec<(
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<i16>,
        Option<chrono::DateTime<chrono::Utc>>,
    )>,
}

/// Deterministic session id for one calendar day's digest. Doubles as the
/// fleet-wide "already sent today" marker in `telegram_messages`.
pub fn digest_session_id(date: chrono::NaiveDate) -> String {
    format!("{DIGEST_SESSION_PREFIX}-{}", date.format("%Y-%m-%d"))
}

/// Is the digest due at this local time? Due from [`DIGEST_HOUR_LOCAL`]:00
/// until midnight, so a daemon that was down at 08:00 sharp catches up on its
/// next tick instead of skipping a day.
pub fn digest_due(now_local: chrono::NaiveTime) -> bool {
    now_local.hour() >= DIGEST_HOUR_LOCAL
}

fn fmt_duration_secs(secs: f64) -> String {
    let secs = secs.max(0.0) as i64;
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Render the digest body. Pure so it unit-tests without a database.
pub fn format_digest(data: &DigestData) -> String {
    const NA: &str = "n/a (velocity views not populated)";
    let mut lines = Vec::new();

    lines.push(match data.throughput_24h {
        Some((completed, failed)) => {
            format!("Throughput (24h): {completed} completed, {failed} failed")
        }
        None => format!("Throughput (24h): {NA}"),
    });
    lines.push(match data.avg_lead_time_secs {
        Some(secs) => format!("Avg lead time: {}", fmt_duration_secs(secs)),
        None => format!("Avg lead time: {NA}"),
    });
    lines.push(match data.first_pass {
        Some((completed, first_pass)) if completed > 0 => {
            let pct = 100.0 * first_pass as f64 / completed as f64;
            format!("First-pass rate: {pct:.0}% ({first_pass}/{completed})")
        }
        Some(_) => "First-pass rate: no completions".to_string(),
        None => format!("First-pass rate: {NA}"),
    });
    if data.computer_builds.is_empty() {
        lines.push("Builds by computer: none recorded".to_string());
    } else {
        lines.push("Builds by computer:".to_string());
        for (name, started, succeeded, failed) in &data.computer_builds {
            lines.push(format!(
                "  • {name}: {succeeded} ok / {failed} failed ({started} started)"
            ));
        }
    }
    if data.cloud_budgets.is_empty() {
        lines.push("Cloud capacity: unknown".to_string());
    } else {
        lines.push("Cloud capacity:".to_string());
        for (provider, exhausted, weekly, reset) in &data.cloud_budgets {
            let state = exhausted
                .filter(|until| *until > chrono::Utc::now())
                .map(|until| format!("exhausted until {}", until.to_rfc3339()))
                .unwrap_or_else(|| "available".to_string());
            let weekly = weekly
                .map(|pct| format!(", weekly {pct}% used"))
                .unwrap_or_default();
            let reset = reset
                .map(|at| format!(", resets {}", at.to_rfc3339()))
                .unwrap_or_default();
            lines.push(format!("  • {provider}: {state}{weekly}{reset}"));
        }
    }
    lines.join("\n")
}

/// Gather each digest section best-effort. A failed query (most likely the
/// V179 views not existing yet) leaves that section `None` rather than
/// failing the whole digest.
async fn collect_digest_data(pg: &PgPool) -> DigestData {
    let mut data = DigestData::default();

    match sqlx::query_as::<_, (i64, i64)>(
        "SELECT COALESCE(SUM(completed_count), 0)::BIGINT, \
                COALESCE(SUM(failed_count), 0)::BIGINT \
           FROM v_throughput_hourly \
          WHERE hour_bucket >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_one(pg)
    .await
    {
        Ok(row) => data.throughput_24h = Some(row),
        Err(e) => tracing::debug!(error = %e, "nightly digest: v_throughput_hourly unavailable"),
    }

    match sqlx::query_scalar::<_, Option<f64>>(
        "SELECT (SUM(avg_lead_time_seconds * completed_count) \
                 / NULLIF(SUM(completed_count), 0))::DOUBLE PRECISION \
           FROM v_lead_time_daily \
          WHERE day_bucket >= date_trunc('day', NOW() - INTERVAL '1 day')",
    )
    .fetch_one(pg)
    .await
    {
        Ok(avg) => data.avg_lead_time_secs = avg,
        Err(e) => tracing::debug!(error = %e, "nightly digest: v_lead_time_daily unavailable"),
    }

    match sqlx::query_as::<_, (i64, i64)>(
        "SELECT COALESCE(SUM(completed_count), 0)::BIGINT, \
                COALESCE(SUM(first_pass_count), 0)::BIGINT \
           FROM v_first_pass_rate_daily \
          WHERE day_bucket >= date_trunc('day', NOW() - INTERVAL '1 day')",
    )
    .fetch_one(pg)
    .await
    {
        Ok(row) => data.first_pass = Some(row),
        Err(e) => {
            tracing::debug!(error = %e, "nightly digest: v_first_pass_rate_daily unavailable")
        }
    }

    match sqlx::query_as::<_, (String, i64, i64, i64)>(
        "SELECT computer_name, \
                COALESCE(SUM(builds_started), 0)::BIGINT, \
                COALESCE(SUM(builds_succeeded), 0)::BIGINT, \
                COALESCE(SUM(builds_failed), 0)::BIGINT \
           FROM v_computer_builds_daily \
          WHERE day_bucket >= date_trunc('day', NOW() - INTERVAL '1 day') \
          GROUP BY computer_name \
          ORDER BY 2 DESC \
          LIMIT 8",
    )
    .fetch_all(pg)
    .await
    {
        Ok(rows) => data.computer_builds = rows,
        Err(e) => {
            tracing::debug!(error = %e, "nightly digest: v_computer_builds_daily unavailable")
        }
    }

    match sqlx::query_as::<
        _,
        (
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<i16>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        "SELECT provider, MAX(window_exhausted_until), MAX(weekly_pct), MAX(weekly_reset_at) \
         FROM cloud_budget_buckets GROUP BY provider ORDER BY provider",
    )
    .fetch_all(pg)
    .await
    {
        Ok(rows) => data.cloud_budgets = rows,
        Err(e) => tracing::debug!(error = %e, "nightly digest: cloud budgets unavailable"),
    }

    data
}

/// One scheduler pass of the nightly digest. Registered in the daemon tick
/// registry (leader-only), so by the time this runs the caller has already
/// established that this node is the live leader.
///
/// No-ops until [`DIGEST_HOUR_LOCAL`]:00 local time and after today's digest
/// has been sent (dedup via the `telegram_messages` row the send records).
pub async fn run_nightly_digest_tick(pg: &PgPool, worker_name: &str) -> Result<()> {
    let now = chrono::Local::now();
    if !digest_due(now.time()) {
        return Ok(());
    }

    let session_id = digest_session_id(now.date_naive());
    let already_sent: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM telegram_messages WHERE session_id = $1)")
            .bind(&session_id)
            .fetch_one(pg)
            .await?;
    if already_sent {
        return Ok(());
    }

    let data = collect_digest_data(pg).await;
    let title = format!(
        "ForgeFleet nightly digest — {}",
        now.date_naive().format("%Y-%m-%d")
    );
    let body = format_digest(&data);

    match crate::telegram::send_telegram_recorded(pg, &title, &body, &session_id).await? {
        Some(message_id) => {
            tracing::info!(
                leader = worker_name,
                session_id = %session_id,
                tg_message_id = message_id,
                "nightly digest sent"
            );
        }
        None => {
            // Telegram not configured — nothing recorded, so we'll re-check on
            // later ticks today. Cheap (one EXISTS + two secret lookups).
            tracing::debug!("nightly digest due but telegram not configured; skipping");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn digest_due_only_from_send_hour_onward() {
        assert!(!digest_due(t(0, 0)));
        assert!(!digest_due(t(7, 59)));
        assert!(digest_due(t(8, 0)));
        assert!(digest_due(t(12, 30)));
        assert!(digest_due(t(23, 59)));
    }

    #[test]
    fn session_id_is_stable_per_date() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 7, 19).unwrap();
        assert_eq!(digest_session_id(date), "nightly-digest-2026-07-19");
        assert_eq!(digest_session_id(date), digest_session_id(date));
    }

    #[test]
    fn format_digest_renders_all_sections() {
        let data = DigestData {
            throughput_24h: Some((12, 3)),
            avg_lead_time_secs: Some(4_320.0),
            first_pass: Some((12, 10)),
            computer_builds: vec![("alpha".into(), 6, 5, 1)],
            cloud_budgets: vec![("kimi".into(), None, Some(64), None)],
        };
        let body = format_digest(&data);
        assert!(body.contains("Throughput (24h): 12 completed, 3 failed"));
        assert!(body.contains("Avg lead time: 1h 12m"));
        assert!(body.contains("First-pass rate: 83% (10/12)"));
        assert!(body.contains("• alpha: 5 ok / 1 failed (6 started)"));
    }

    #[test]
    fn format_digest_degrades_when_views_missing() {
        let body = format_digest(&DigestData::default());
        assert!(body.contains("Throughput (24h): n/a"));
        assert!(body.contains("Avg lead time: n/a"));
        assert!(body.contains("First-pass rate: n/a"));
        assert!(body.contains("Builds by computer: none recorded"));
    }

    #[test]
    fn format_digest_handles_zero_completions() {
        let data = DigestData {
            first_pass: Some((0, 0)),
            ..Default::default()
        };
        assert!(format_digest(&data).contains("First-pass rate: no completions"));
    }

    #[test]
    fn duration_formatting_picks_sane_units() {
        assert_eq!(fmt_duration_secs(35.0), "35s");
        assert_eq!(fmt_duration_secs(2_520.0), "42m");
        assert_eq!(fmt_duration_secs(4_320.0), "1h 12m");
        assert_eq!(fmt_duration_secs(-5.0), "0s");
    }
}
