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
use sqlx::{PgPool, Row};

use crate::pm_velocity::{PipelineStatusCounts, collect_pipeline_status_counts};

/// Render the digest body: current counts plus the delta since `previous`.
/// Pure so it unit-tests without a database. "Merged/done" mirrors
/// [`PipelineStatusCounts::completed`], which counts both statuses.
pub fn format_pipeline_digest(
    current: &PipelineStatusCounts,
    previous: &PipelineStatusCounts,
    failures: &[FailedWorkItem],
    now: DateTime<Utc>,
) -> String {
    fn line(label: &str, current: i64, previous: i64) -> String {
        let delta = current - previous;
        let sign = if delta >= 0 { "+" } else { "" };
        format!("{label}: {current} (Δ {sign}{delta})")
    }

    fn age_tag(failed_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
        let minutes = (now - failed_at).num_minutes().max(0);
        if minutes < 60 {
            format!("{minutes}m")
        } else if minutes < 1440 {
            format!("{}h", minutes / 60)
        } else {
            format!("{}d", minutes / 1440)
        }
    }
    fn lines(items: Vec<&FailedWorkItem>, now: DateTime<Utc>) -> String {
        if items.is_empty() {
            return "  none".into();
        }
        items
            .into_iter()
            .map(|item| {
                format!(
                    "  • {} — failed {} ago",
                    item.title,
                    age_tag(item.failed_at, now)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    let cutoff = now - chrono::Duration::minutes(10);
    let new = failures.iter().filter(|f| f.failed_at >= cutoff).collect();
    let standing = failures.iter().filter(|f| f.failed_at < cutoff).collect();
    format!(
        "🚦 Pipeline digest\n{}\n{}\n{}\n\nNEW failures (last 10m)\n{}\n\nStanding failures\n{}",
        line("Merged/done", current.completed, previous.completed),
        line("Building", current.building, previous.building),
        line("Failed", current.failed, previous.failed),
        lines(new, now),
        lines(standing, now),
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
        "SELECT title, completed_at FROM work_items
          WHERE status = 'failed' AND completed_at IS NOT NULL
          ORDER BY completed_at DESC, id ASC",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| FailedWorkItem {
        title: row.get("title"),
        failed_at: row.get("completed_at"),
    })
    .collect::<Vec<_>>();
    let body = format_pipeline_digest(&current, previous, &failures, now);
    let message_id =
        crate::telegram::send_telegram_recorded(pool, "Pipeline digest", &body, session_id).await?;
    Ok((current, message_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_digest_splits_and_age_tags_failures() {
        let current = PipelineStatusCounts {
            completed: 12,
            building: 3,
            failed: 2,
        };
        let previous = PipelineStatusCounts {
            completed: 9,
            building: 5,
            failed: 1,
        };
        let now = Utc::now();
        let failures = vec![
            FailedWorkItem {
                title: "fresh failure".into(),
                failed_at: now - chrono::Duration::minutes(4),
            },
            FailedWorkItem {
                title: "old failure".into(),
                failed_at: now - chrono::Duration::minutes(40),
            },
        ];
        let body = format_pipeline_digest(&current, &previous, &failures, now);
        assert!(body.contains("Merged/done: 12 (Δ +3)"));
        assert!(body.contains("Building: 3 (Δ -2)"));
        assert!(body.contains("Failed: 2 (Δ +1)"));
        assert!(body.contains("fresh failure — failed 4m ago"));
        assert!(body.contains("old failure — failed 40m ago"));
    }

    #[test]
    fn format_digest_handles_zero_previous() {
        let body = format_pipeline_digest(
            &PipelineStatusCounts::default(),
            &Default::default(),
            &[],
            Utc::now(),
        );
        assert!(body.contains("Merged/done: 0 (Δ +0)"));
        assert!(body.contains("Building: 0 (Δ +0)"));
        assert!(body.contains("Failed: 0 (Δ +0)"));
        assert_eq!(body.matches("  none").count(), 2);
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedWorkItem {
    pub title: String,
    pub failed_at: DateTime<Utc>,
}
