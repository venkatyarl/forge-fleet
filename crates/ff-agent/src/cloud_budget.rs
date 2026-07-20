//! Best-effort persistence and consultation of cloud-provider quota windows.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSignal {
    pub exhausted_until: DateTime<Utc>,
    pub source: &'static str,
}

/// Interpret provider failures conservatively. Kimi's
/// `access_terminated_error` is a quota window when the accompanying message
/// mentions usage/limits; it is not an account-termination signal.
pub fn parse_quota_signal(status: i64, text: &str, now: DateTime<Utc>) -> Option<QuotaSignal> {
    let lower = text.to_ascii_lowercase();
    let quota_status = status == 403 || status == 429;
    let quota_words = lower.contains("usage limit")
        || lower.contains("rate limit")
        || lower.contains("quota")
        || lower.contains("billing cycle")
        || (lower.contains("access_terminated_error") && lower.contains("limit"));
    // A bare 403 can be authentication/authorization failure. Only persist a
    // quota window when the provider supplied quota/reset evidence.
    if !quota_words || (!quota_status && !lower.contains("access_terminated_error")) {
        return None;
    }

    let exhausted_until = parse_reset_hours(&lower)
        .map(|hours| now + Duration::hours(hours))
        // Unknown provider windows remain blocked briefly, then require a
        // cheap probe. This prevents a 403/429 storm without inventing a long
        // reset time.
        .unwrap_or_else(|| now + Duration::hours(1));
    Some(QuotaSignal {
        exhausted_until,
        source: "inference_error",
    })
}

fn parse_reset_hours(text: &str) -> Option<i64> {
    for marker in ["resets in ", "reset in ", "retry in "] {
        let rest = text.split_once(marker)?.1.trim_start();
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let value: i64 = digits.parse().ok()?;
        if rest[digits.len()..].trim_start().starts_with("hour") {
            return Some(value.max(1));
        }
    }
    None
}

pub async fn provider_is_exhausted(pg: &PgPool, provider: &str) -> bool {
    // Claude subscription exhaustion spills into enabled paid credits. It is
    // expensive and should be weighted last by routers, but never hard-blocked.
    if provider.eq_ignore_ascii_case("claude") {
        return false;
    }
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM cloud_budget_buckets \
         WHERE provider = $1 AND window_exhausted_until > NOW())",
    )
    .bind(provider)
    .fetch_one(pg)
    .await
    .unwrap_or(false)
}

pub async fn record_failure(pg: &PgPool, provider: &str, signal: &QuotaSignal) {
    let _ = sqlx::query(
        "INSERT INTO cloud_budget_buckets \
         (provider, window_kind, window_exhausted_until, last_error_at, source) \
         VALUES ($1, 'rolling', $2, NOW(), $3) \
         ON CONFLICT (provider, window_kind) DO UPDATE SET \
         window_exhausted_until = EXCLUDED.window_exhausted_until, \
         last_error_at = NOW(), source = EXCLUDED.source, updated_at = NOW()",
    )
    .bind(provider)
    .bind(signal.exhausted_until)
    .bind(signal.source)
    .execute(pg)
    .await;
}

/// A real successful inference is the cheap probe that clears a rolling block.
pub async fn record_success(pg: &PgPool, provider: &str) {
    let _ = sqlx::query(
        "UPDATE cloud_budget_buckets SET window_exhausted_until = NULL, \
         source = 'inference_success', updated_at = NOW() \
         WHERE provider = $1 AND window_kind = 'rolling'",
    )
    .bind(provider)
    .execute(pg)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kimi_usage_limit_as_window() {
        let now = Utc::now();
        let s = parse_quota_signal(
            403,
            "access_terminated_error: usage limit reached; resets in 5 hours",
            now,
        )
        .unwrap();
        assert_eq!(s.exhausted_until, now + Duration::hours(5));
    }

    #[test]
    fn ignores_unrelated_failures() {
        assert!(parse_quota_signal(500, "internal error", Utc::now()).is_none());
        assert!(parse_quota_signal(403, "forbidden", Utc::now()).is_none());
    }
}
