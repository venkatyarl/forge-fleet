//! PM velocity KPI digest — the single source of the throughput/velocity
//! rollup printed by `ff pm velocity` (and reused verbatim by the Telegram
//! daemon digest, so terminal and Telegram always show the identical text).
//!
//! No dedicated velocity SQL views exist yet (neither in ff-db migrations nor
//! the live DB — confirmed via `ff db query` against information_schema.views),
//! so the KPIs are computed straight from `work_items`, which IS part of the
//! ff-db baseline schema. If/when velocity views land, swap the queries in
//! [`collect_velocity_stats`] — the digest text must keep coming from
//! [`render_velocity_digest`] so every surface stays in sync.

use anyhow::Result;
use chrono::NaiveDate;
use sqlx::PgPool;

/// Statuses that count as "completed" for throughput purposes.
const COMPLETED_STATUSES: &str = "('done','merged')";
/// Statuses that count as work-in-progress, in pipeline order.
const WIP_STATUSES: [&str; 4] = ["ready", "claimed", "building", "in_review"];

/// Raw KPI numbers behind the digest, split from rendering so the formatter
/// is unit-testable without a database.
#[derive(Debug, Clone, Default)]
pub struct VelocityStats {
    /// Items completed in the last 7 days.
    pub completed_7d: i64,
    /// Items completed in the 7 days before that (trend baseline).
    pub completed_prev_7d: i64,
    /// Mean started→completed time over the last 7 days, in hours.
    /// `None` when no completed item in the window recorded `started_at`.
    pub avg_cycle_hours_7d: Option<f64>,
    /// Completions per ISO week (week start date, count), oldest first,
    /// covering the last 4 weeks.
    pub weekly: Vec<(NaiveDate, i64)>,
    /// Current WIP counts per status, in pipeline order; zero counts omitted.
    pub wip: Vec<(String, i64)>,
    /// Projects with the most completions in the last 7 days (top 5).
    pub top_projects_7d: Vec<(String, i64)>,
}

/// Query `work_items` for the velocity KPIs.
pub async fn collect_velocity_stats(pool: &PgPool) -> Result<VelocityStats> {
    let (completed_7d, completed_prev_7d, avg_cycle_hours_7d): (i64, i64, Option<f64>) =
        sqlx::query_as(&format!(
            "SELECT COUNT(*) FILTER (WHERE completed_at >= NOW() - INTERVAL '7 days'), \
                    COUNT(*) FILTER (WHERE completed_at >= NOW() - INTERVAL '14 days' \
                                       AND completed_at <  NOW() - INTERVAL '7 days'), \
                    (AVG(EXTRACT(EPOCH FROM completed_at - started_at)) \
                         FILTER (WHERE completed_at >= NOW() - INTERVAL '7 days' \
                                   AND started_at IS NOT NULL) / 3600.0)::float8 \
             FROM work_items \
             WHERE status IN {COMPLETED_STATUSES} AND completed_at IS NOT NULL"
        ))
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("velocity throughput query: {e}"))?;

    let weekly: Vec<(NaiveDate, i64)> = sqlx::query_as(&format!(
        "SELECT date_trunc('week', completed_at)::date, COUNT(*) \
         FROM work_items \
         WHERE status IN {COMPLETED_STATUSES} \
           AND completed_at >= date_trunc('week', NOW()) - INTERVAL '21 days' \
         GROUP BY 1 ORDER BY 1"
    ))
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("velocity weekly-trend query: {e}"))?;

    let wip_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, COUNT(*) FROM work_items \
         WHERE status = ANY($1) GROUP BY status",
    )
    .bind(
        WIP_STATUSES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("velocity WIP query: {e}"))?;
    // Re-order the GROUP BY result into pipeline order, dropping zero counts.
    let wip = WIP_STATUSES
        .iter()
        .filter_map(|s| {
            wip_rows
                .iter()
                .find(|(status, _)| status == s)
                .map(|(status, n)| (status.clone(), *n))
        })
        .collect();

    let top_projects_7d: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT project_id, COUNT(*) FROM work_items \
         WHERE status IN {COMPLETED_STATUSES} \
           AND completed_at >= NOW() - INTERVAL '7 days' \
         GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 5"
    ))
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("velocity top-projects query: {e}"))?;

    Ok(VelocityStats {
        completed_7d,
        completed_prev_7d,
        avg_cycle_hours_7d,
        weekly,
        wip,
        top_projects_7d,
    })
}

/// Format the KPI digest. Plain text (no ANSI) so the same string is valid
/// for the terminal, Telegram, and logs.
pub fn render_velocity_digest(stats: &VelocityStats) -> String {
    let mut out = String::from("📊 ForgeFleet velocity digest\n");

    let delta = stats.completed_7d - stats.completed_prev_7d;
    out.push_str(&format!(
        "Completed (7d):      {}  (prev 7d: {}, Δ {}{})\n",
        stats.completed_7d,
        stats.completed_prev_7d,
        if delta >= 0 { "+" } else { "" },
        delta
    ));

    match stats.avg_cycle_hours_7d {
        Some(h) => out.push_str(&format!("Avg cycle time (7d): {h:.1}h started→completed\n")),
        None => out.push_str("Avg cycle time (7d): n/a (no timed completions)\n"),
    }

    if stats.wip.is_empty() {
        out.push_str("WIP:                 none\n");
    } else {
        let wip = stats
            .wip
            .iter()
            .map(|(status, n)| format!("{status} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        out.push_str(&format!("WIP:                 {wip}\n"));
    }

    if !stats.weekly.is_empty() {
        out.push_str("Weekly completions:\n");
        for (week, n) in &stats.weekly {
            out.push_str(&format!("  {week}  {n}\n"));
        }
    }

    if !stats.top_projects_7d.is_empty() {
        let tops = stats
            .top_projects_7d
            .iter()
            .map(|(project, n)| format!("{project} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        out.push_str(&format!("Top projects (7d):   {tops}\n"));
    }

    out
}

/// Collect + render in one call — the entry point both CLIs (and the Telegram
/// daemon) use.
pub async fn velocity_digest(pool: &PgPool) -> Result<String> {
    Ok(render_velocity_digest(&collect_velocity_stats(pool).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stats() -> VelocityStats {
        VelocityStats {
            completed_7d: 12,
            completed_prev_7d: 9,
            avg_cycle_hours_7d: Some(4.25),
            weekly: vec![
                (NaiveDate::from_ymd_opt(2026, 6, 29).unwrap(), 7),
                (NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(), 9),
            ],
            wip: vec![("building".to_string(), 3), ("in_review".to_string(), 5)],
            top_projects_7d: vec![("forge-fleet".to_string(), 10)],
        }
    }

    #[test]
    fn digest_renders_all_sections() {
        let text = render_velocity_digest(&sample_stats());
        assert!(text.starts_with("📊 ForgeFleet velocity digest"));
        assert!(text.contains("Completed (7d):      12  (prev 7d: 9, Δ +3)"));
        assert!(text.contains("Avg cycle time (7d): 4.2h"));
        assert!(text.contains("building 3 · in_review 5"));
        assert!(text.contains("2026-06-29  7"));
        assert!(text.contains("Top projects (7d):   forge-fleet 10"));
    }

    #[test]
    fn digest_handles_empty_stats() {
        let text = render_velocity_digest(&VelocityStats::default());
        assert!(text.contains("Completed (7d):      0  (prev 7d: 0, Δ +0)"));
        assert!(text.contains("Avg cycle time (7d): n/a"));
        assert!(text.contains("WIP:                 none"));
        assert!(!text.contains("Weekly completions"));
        assert!(!text.contains("Top projects"));
    }
}
