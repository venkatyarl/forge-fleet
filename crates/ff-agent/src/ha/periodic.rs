//! Periodic (clock-scheduled) daemon tasks — currently the **nightly Telegram
//! digest**.
//!
//! Unlike the fixed-interval ticks in [`crate::daemon`], the digest is pinned
//! to a wall-clock time: once per day at [`DIGEST_HOUR_LOCAL`]:00 local time
//! the leader summarizes the fleet's velocity views (V204:
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
    /// Yesterday's merges and the preceding seven-day daily average.
    pub throughput: Option<(f64, f64)>,
    /// Yesterday's p50 lead time and the preceding seven-day average p50.
    pub lead_time_p50: Option<(f64, f64)>,
    /// Yesterday's first-pass rate and the preceding seven-day average rate.
    pub first_pass_rate: Option<(f64, f64)>,
    /// Per-computer (name, builds, average minutes) for yesterday.
    pub computer_builds: Vec<(String, i64, f64)>,
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

    lines.push(match data.throughput {
        Some((yesterday, average)) => {
            format!("Throughput: {yesterday:.0} merges yesterday vs {average:.1}/day (7d)")
        }
        None => format!("Throughput: {NA}"),
    });
    lines.push(match data.lead_time_p50 {
        Some((yesterday, average)) => format!(
            "Lead time p50: {} yesterday vs {} (7d)",
            fmt_duration_secs(yesterday),
            fmt_duration_secs(average)
        ),
        None => format!("Lead time p50: {NA}"),
    });
    lines.push(match data.first_pass_rate {
        Some((yesterday, average)) => format!(
            "First-pass rate: {:.0}% yesterday vs {:.0}% (7d)",
            yesterday * 100.0,
            average * 100.0
        ),
        None => format!("First-pass rate: {NA}"),
    });
    if data.computer_builds.is_empty() {
        lines.push("Builds by computer: none recorded".to_string());
    } else {
        let mut ranked = data.computer_builds.clone();
        ranked.sort_by(|a, b| a.2.total_cmp(&b.2));
        lines.push("Top computers (fastest avg build):".to_string());
        for (name, builds, minutes) in ranked.iter().take(3) {
            lines.push(format!("  • {name}: {minutes:.1}m ({builds} builds)"));
        }
        lines.push("Bottom computers (slowest avg build):".to_string());
        for (name, builds, minutes) in ranked.iter().rev().take(3) {
            lines.push(format!("  • {name}: {minutes:.1}m ({builds} builds)"));
        }
    }
    lines.join("\n")
}

/// Gather each digest section best-effort. A failed query (most likely the
/// V179 views not existing yet) leaves that section `None` rather than
/// failing the whole digest.
async fn collect_digest_data(pg: &PgPool) -> DigestData {
    let mut data = DigestData::default();

    match sqlx::query_as::<_, (f64, f64)>(
        "SELECT COALESCE(SUM(merge_count) FILTER (WHERE hour_bucket::date = CURRENT_DATE - 1), 0)::DOUBLE PRECISION, \
                (COALESCE(SUM(merge_count) FILTER (WHERE hour_bucket::date BETWEEN CURRENT_DATE - 7 AND CURRENT_DATE - 1), 0) / 7.0)::DOUBLE PRECISION \
           FROM v_throughput_hourly",
    )
    .fetch_one(pg)
    .await
    {
        Ok(row) => data.throughput = Some(row),
        Err(e) => tracing::debug!(error = %e, "nightly digest: v_throughput_hourly unavailable"),
    }

    match sqlx::query_as::<_, (Option<f64>, Option<f64>)>(
        "SELECT MAX(p50_lead_time_seconds) FILTER (WHERE day_bucket::date = CURRENT_DATE - 1), \
                AVG(p50_lead_time_seconds) FILTER (WHERE day_bucket::date BETWEEN CURRENT_DATE - 7 AND CURRENT_DATE - 1) \
           FROM v_lead_time_daily",
    )
    .fetch_one(pg)
    .await
    {
        Ok((Some(yesterday), Some(average))) => data.lead_time_p50 = Some((yesterday, average)),
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "nightly digest: v_lead_time_daily unavailable"),
    }

    match sqlx::query_as::<_, (Option<f64>, Option<f64>)>(
        "SELECT MAX(first_pass_rate) FILTER (WHERE day_bucket::date = CURRENT_DATE - 1), \
                AVG(first_pass_rate) FILTER (WHERE day_bucket::date BETWEEN CURRENT_DATE - 7 AND CURRENT_DATE - 1) \
           FROM v_first_pass_rate_daily",
    )
    .fetch_one(pg)
    .await
    {
        Ok((Some(yesterday), Some(average))) => data.first_pass_rate = Some((yesterday, average)),
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(error = %e, "nightly digest: v_first_pass_rate_daily unavailable")
        }
    }

    match sqlx::query_as::<_, (String, i64, f64)>(
        "SELECT computer_name, \
                SUM(build_count)::BIGINT, \
                (SUM(avg_build_minutes * build_count) / NULLIF(SUM(build_count), 0))::DOUBLE PRECISION \
           FROM v_computer_builds_daily \
          WHERE day_bucket::date = CURRENT_DATE - 1 \
          GROUP BY computer_name \
          ORDER BY 3",
    )
    .fetch_all(pg)
    .await
    {
        Ok(rows) => data.computer_builds = rows,
        Err(e) => {
            tracing::debug!(error = %e, "nightly digest: v_computer_builds_daily unavailable")
        }
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
            throughput: Some((12.0, 9.5)),
            lead_time_p50: Some((4_320.0, 3_600.0)),
            first_pass_rate: Some((10.0 / 12.0, 0.75)),
            computer_builds: vec![("alpha".into(), 6, 12.0), ("beta".into(), 4, 30.0)],
        };
        let body = format_digest(&data);
        assert!(body.contains("Throughput: 12 merges yesterday vs 9.5/day (7d)"));
        assert!(body.contains("Lead time p50: 1h 12m yesterday vs 1h 0m (7d)"));
        assert!(body.contains("First-pass rate: 83% yesterday vs 75% (7d)"));
        assert!(body.contains("• alpha: 12.0m (6 builds)"));
        assert!(body.contains("• beta: 30.0m (4 builds)"));
    }

    #[test]
    fn format_digest_degrades_when_views_missing() {
        let body = format_digest(&DigestData::default());
        assert!(body.contains("Throughput: n/a"));
        assert!(body.contains("Lead time p50: n/a"));
        assert!(body.contains("First-pass rate: n/a"));
        assert!(body.contains("Builds by computer: none recorded"));
    }

    #[test]
    fn format_digest_handles_zero_first_pass_rate() {
        let data = DigestData {
            first_pass_rate: Some((0.0, 0.25)),
            ..Default::default()
        };
        assert!(format_digest(&data).contains("First-pass rate: 0% yesterday vs 25% (7d)"));
    }

    #[test]
    fn duration_formatting_picks_sane_units() {
        assert_eq!(fmt_duration_secs(35.0), "35s");
        assert_eq!(fmt_duration_secs(2_520.0), "42m");
        assert_eq!(fmt_duration_secs(4_320.0), "1h 12m");
        assert_eq!(fmt_duration_secs(-5.0), "0s");
    }
}
