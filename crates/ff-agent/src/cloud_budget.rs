//! Best-effort inference of cloud CLI quota-window exhaustion.

use sqlx::PgPool;
use std::time::Duration;
const DEFAULT_FAILURE_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Infer a quota reset window from vendor CLI output. `None` means the failure
/// did not look quota-related and must not suppress an otherwise healthy lane.
pub fn failure_window(text: &str) -> Option<Duration> {
    let lower = text.to_ascii_lowercase();
    let usage_limit = lower.contains("usage limit")
        || lower.contains("usage-limit")
        || lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("billing cycle");
    let status_limit = lower.contains("403") || lower.contains("429");
    let terminated_limit = lower.contains("access_terminated_error") && usage_limit;
    if !(usage_limit || status_limit || terminated_limit) {
        return None;
    }

    reset_hours(&lower).or(Some(DEFAULT_FAILURE_WINDOW))
}

fn reset_hours(text: &str) -> Option<Duration> {
    for marker in ["resets in ", "reset in "] {
        let Some(rest) = text.split(marker).nth(1) else {
            continue;
        };
        let number: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let Ok(hours) = number.parse::<f64>() else {
            continue;
        };
        if hours.is_finite() && hours > 0.0 {
            return Some(Duration::from_secs_f64(hours * 3600.0));
        }
    }
    None
}

/// Provider failures without response text (timeout/empty stdout) use the safe
/// short default window so the next lane is tried without disabling it all day.
pub async fn record_backend_failure(pg: &PgPool, provider: &str) -> Result<(), sqlx::Error> {
    record_failure(pg, provider, DEFAULT_FAILURE_WINDOW).await
}

pub async fn record_failure(
    pg: &PgPool,
    provider: &str,
    duration: Duration,
) -> Result<(), sqlx::Error> {
    let seconds = duration.as_secs().min(i64::MAX as u64) as i64;
    sqlx::query(
        "UPDATE cloud_budget_buckets \
         SET window_exhausted_until = GREATEST(COALESCE(window_exhausted_until, NOW()), \
                                               NOW() + make_interval(secs => $2::double precision)) \
         WHERE provider = $1",
    )
    .bind(provider)
    .bind(seconds as f64)
    .execute(pg)
    .await?;
    Ok(())
}

/// A successful CLI call proves the provider is usable again.
pub async fn record_success(pg: &PgPool, provider: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE cloud_budget_buckets \
         SET window_exhausted_until = NULL, last_success_at = NOW() \
         WHERE provider = $1",
    )
    .bind(provider)
    .execute(pg)
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
    fn recognizes_billing_and_kimi_usage_limits() {
        assert_eq!(
            failure_window("limit renews next billing cycle"),
            Some(DEFAULT_FAILURE_WINDOW)
        );
        assert_eq!(
            failure_window("access_terminated_error: usage-limit exceeded"),
            Some(DEFAULT_FAILURE_WINDOW)
        );
    }

    #[test]
    fn ignores_unrelated_backend_failures() {
        assert_eq!(failure_window("process exited 1: syntax error"), None);
        assert_eq!(
            failure_window("HTTP 403 forbidden"),
            Some(DEFAULT_FAILURE_WINDOW)
        );
        assert_eq!(
            failure_window("HTTP 429 too many requests"),
            Some(DEFAULT_FAILURE_WINDOW)
        );
        assert_eq!(failure_window("ordinary compiler error"), None);
    }
}
