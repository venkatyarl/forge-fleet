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
use sqlx::PgPool;

use crate::pm_velocity::{PipelineStatusCounts, collect_pipeline_status_counts};

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
    let body = format_pipeline_digest(&current, previous);
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
}
