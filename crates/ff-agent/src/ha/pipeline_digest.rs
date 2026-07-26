//! Pipeline status digest — merged/building/failed deltas for Telegram.
//!
//! The counts come from the existing pipeline status API
//! [`crate::pm_velocity::collect_pipeline_status_counts`] — the same module
//! (and status sets) behind `ff pm velocity` — so this digest can never
//! disagree with the velocity rollup about what "completed" or "building"
//! means. This module only renders the change since the caller's previous
//! snapshot and sends it through [`crate::telegram::send_telegram_recorded`],
//! so an operator reply routes back to a session, same as
//! [`crate::ha::periodic`] and [`crate::ha::status_updater`].

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::Row;

use crate::pm_velocity::{PipelineStatusCounts, collect_pipeline_status_counts};

#[derive(Debug, Clone, PartialEq, Eq)]
struct FailedItem {
    title: String,
    completed_at: DateTime<Utc>,
}

fn failure_age(now: DateTime<Utc>, failed_at: DateTime<Utc>) -> String {
    let minutes = now.signed_duration_since(failed_at).num_minutes().max(0);
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 24 * 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (24 * 60))
    }
}

fn format_failures(failures: &[FailedItem], now: DateTime<Utc>) -> String {
    let cutoff = now - chrono::Duration::minutes(10);
    let mut new = Vec::new();
    let mut standing = Vec::new();
    for failure in failures {
        let line = format!(
            "• {} — failed {} ago",
            failure.title,
            failure_age(now, failure.completed_at)
        );
        if failure.completed_at >= cutoff {
            new.push(line);
        } else {
            standing.push(line);
        }
    }

    let section = |label: &str, rows: Vec<String>| {
        if rows.is_empty() {
            format!("{label}: none")
        } else {
            format!("{label}:\n{}", rows.join("\n"))
        }
    };
    format!(
        "{}\n{}",
        section("NEW failures (last 10m)", new),
        section("Standing failures", standing)
    )
}

/// Render the digest body: current counts plus the delta since `previous`.
/// Pure so it unit-tests without a database. "Merged/done" mirrors
/// [`PipelineStatusCounts::completed`], which counts both statuses.
pub fn format_pipeline_digest(
    current: &PipelineStatusCounts,
    previous: &PipelineStatusCounts,
) -> String {
    fn line(label: &str, current: i64, previous: i64) -> String {
        let delta = current - previous;
        let sign = if delta >= 0 { "+" } else { "" };
        format!("{label}: {current} (Δ {sign}{delta})")
    }

    format!(
        "🚦 Pipeline digest\n{}\n{}\n{}",
        line("Merged/done", current.completed, previous.completed),
        line("Building", current.building, previous.building),
        line("Failed", current.failed, previous.failed),
    )
}

/// Collect the current counts, render the digest against `previous`, and
/// send it to Telegram. Returns the freshly-collected counts — the caller's
/// next call should pass these back as `previous` to keep the deltas
/// contiguous — alongside the Telegram message id (`None` when Telegram
/// isn't configured).
pub async fn send_pipeline_digest(
    pool: &PgPool,
    previous: &PipelineStatusCounts,
    session_id: &str,
) -> Result<(PipelineStatusCounts, Option<i64>)> {
    let current = collect_pipeline_status_counts(pool).await?;
    let now = Utc::now();
    let failures = sqlx::query(
        "SELECT title, completed_at
           FROM work_items
          WHERE status = 'failed' AND completed_at IS NOT NULL
          ORDER BY completed_at DESC
          LIMIT 25",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| FailedItem {
        title: row.get("title"),
        completed_at: row.get("completed_at"),
    })
    .collect::<Vec<_>>();
    let body = format!(
        "{}\n\n{}",
        format_pipeline_digest(&current, previous),
        format_failures(&failures, now)
    );
    let message_id =
        crate::telegram::send_telegram_recorded(pool, "Pipeline digest", &body, session_id).await?;
    Ok((current, message_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_digest_shows_positive_and_negative_deltas() {
        let current = PipelineStatusCounts {
            completed: 12,
            building: 3,
            failed: 1,
        };
        let previous = PipelineStatusCounts {
            completed: 9,
            building: 5,
            failed: 1,
        };
        let body = format_pipeline_digest(&current, &previous);
        assert!(body.contains("Merged/done: 12 (Δ +3)"));
        assert!(body.contains("Building: 3 (Δ -2)"));
        assert!(body.contains("Failed: 1 (Δ +0)"));
    }

    #[test]
    fn format_digest_handles_zero_previous() {
        let body = format_pipeline_digest(&PipelineStatusCounts::default(), &Default::default());
        assert!(body.contains("Merged/done: 0 (Δ +0)"));
        assert!(body.contains("Building: 0 (Δ +0)"));
        assert!(body.contains("Failed: 0 (Δ +0)"));
    }

    #[test]
    fn failures_are_age_tagged_and_split_new_from_standing() {
        let now = DateTime::parse_from_rfc3339("2026-07-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let failures = vec![
            FailedItem {
                title: "new failure".into(),
                completed_at: now - chrono::Duration::minutes(4),
            },
            FailedItem {
                title: "old failure".into(),
                completed_at: now - chrono::Duration::minutes(40),
            },
        ];
        let body = format_failures(&failures, now);
        assert!(body.contains("NEW failures (last 10m):\n• new failure — failed 4m ago"));
        assert!(body.contains("Standing failures:\n• old failure — failed 40m ago"));
    }
}
