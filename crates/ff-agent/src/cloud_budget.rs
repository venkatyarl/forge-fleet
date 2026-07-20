//! Outcome-derived availability for cloud CLI provider buckets (schema quota-T1).

use std::time::Duration;

use sqlx::PgPool;

const DEFAULT_FAILURE_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Infer a provider-unavailable window from vendor CLI output. Explicit reset
/// hours win; otherwise quota/permission/rate-limit markers use a short default
/// so another lane gets the work without permanently disabling the provider.
pub fn failure_window(output: &str) -> Option<Duration> {
    let lower = output.to_ascii_lowercase();
    let limited = [
        "403",
        "429",
        "billing cycle",
        "usage limit",
        "access_terminated_error",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if !limited {
        return None;
    }

    if let Some(rest) = lower.split("resets in").nth(1) {
        if let Some(hours) = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|hours| hours.is_finite() && *hours > 0.0)
        {
            return Some(Duration::from_secs_f64(hours * 60.0 * 60.0));
        }
    }
    Some(DEFAULT_FAILURE_WINDOW)
}

pub async fn record_failure(
    pool: &PgPool,
    provider: &str,
    window: Duration,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE cloud_budget_buckets
            SET window_exhausted_until = GREATEST(
                    COALESCE(window_exhausted_until, NOW()),
                    NOW() + make_interval(secs => $2::double precision)
                )
          WHERE provider = $1",
    )
    .bind(provider)
    .bind(window.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_success(pool: &PgPool, provider: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE cloud_budget_buckets
            SET window_exhausted_until = NULL, last_success_at = NOW()
          WHERE provider = $1",
    )
    .bind(provider)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_reset_hours() {
        assert_eq!(
            failure_window("429 usage limit; resets in 2.5 hours"),
            Some(Duration::from_secs(9_000))
        );
    }

    #[test]
    fn recognizes_vendor_limit_markers_with_default() {
        for text in [
            "HTTP 403 forbidden",
            "HTTP 429 too many requests",
            "available again next billing cycle",
            "access_terminated_error: usage limit reached",
        ] {
            assert_eq!(failure_window(text), Some(DEFAULT_FAILURE_WINDOW), "{text}");
        }
        assert_eq!(failure_window("ordinary compiler error"), None);
    }
}
