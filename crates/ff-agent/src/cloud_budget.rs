//! Cached cloud-provider quota state and best-effort quota-window inference.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{LazyLock, RwLock},
    time::{Duration, Instant},
};

const CACHE_TTL: Duration = Duration::from_secs(60);
const DEFAULT_FAILURE_WINDOW: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBudget {
    pub provider: String,
    pub window_exhausted_until: Option<DateTime<Utc>>,
    pub weekly_pct: Option<i16>,
}

#[derive(Default)]
struct BudgetCache {
    loaded_at: Option<Instant>,
    providers: HashMap<String, ProviderBudget>,
}

static CACHE: LazyLock<RwLock<BudgetCache>> = LazyLock::new(|| RwLock::new(BudgetCache::default()));

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

async fn snapshot(pg: &PgPool) -> Vec<ProviderBudget> {
    if let Ok(cache) = CACHE.read()
        && cache
            .loaded_at
            .is_some_and(|loaded| loaded.elapsed() < CACHE_TTL)
    {
        return cache.providers.values().cloned().collect();
    }

    let rows = sqlx::query_as::<_, (String, Option<DateTime<Utc>>, Option<i16>)>(
        "SELECT provider, window_exhausted_until, weekly_pct \
           FROM cloud_budget_buckets ORDER BY provider",
    )
    .fetch_all(pg)
    .await;

    match rows {
        Ok(rows) => {
            let providers = rows
                .into_iter()
                .map(|(provider, window_exhausted_until, weekly_pct)| {
                    let key = provider.to_ascii_lowercase();
                    (
                        key,
                        ProviderBudget {
                            provider,
                            window_exhausted_until,
                            weekly_pct,
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            let result = providers.values().cloned().collect();
            if let Ok(mut cache) = CACHE.write() {
                cache.loaded_at = Some(Instant::now());
                cache.providers = providers;
            }
            result
        }
        Err(error) => {
            tracing::debug!(%error, "cloud budget cache refresh unavailable; retaining prior state");
            CACHE.write().map_or_else(
                |_| Vec::new(),
                |mut cache| {
                    cache.loaded_at = Some(Instant::now());
                    cache.providers.values().cloned().collect()
                },
            )
        }
    }
}

pub async fn provider_budget(pg: &PgPool, provider: &str) -> Option<ProviderBudget> {
    snapshot(pg)
        .await
        .into_iter()
        .find(|row| row.provider.eq_ignore_ascii_case(provider))
}

pub fn is_exhausted(row: Option<&ProviderBudget>, now: DateTime<Utc>) -> bool {
    row.and_then(|row| row.window_exhausted_until)
        .is_some_and(|until| until > now)
}

pub async fn all_provider_budgets(pg: &PgPool) -> Vec<ProviderBudget> {
    let mut rows = snapshot(pg).await;
    rows.sort_by(|a, b| a.provider.cmp(&b.provider));
    rows
}

/// Provider failures without response text use a safe short default window.
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
         WHERE lower(provider) = lower($1)",
    )
    .bind(provider)
    .bind(seconds as f64)
    .execute(pg)
    .await?;
    if let Ok(mut cache) = CACHE.write() {
        cache.loaded_at = None;
    }
    Ok(())
}

/// Clear recovery only if no newer quota window replaced the state this call
/// observed. This prevents stale in-flight successes from causing flapping.
pub async fn record_success(pg: &PgPool, provider: &str, observed_until: Option<DateTime<Utc>>) {
    let updated = sqlx::query(
        "UPDATE cloud_budget_buckets \
            SET window_exhausted_until = NULL, last_success_at = NOW(), updated_at = NOW() \
          WHERE lower(provider) = lower($1) \
            AND window_exhausted_until IS NOT DISTINCT FROM $2",
    )
    .bind(provider)
    .bind(observed_until)
    .execute(pg)
    .await;

    match updated {
        Ok(done) if done.rows_affected() > 0 => {
            if let Ok(mut cache) = CACHE.write()
                && let Some(row) = cache.providers.get_mut(&provider.to_ascii_lowercase())
            {
                row.window_exhausted_until = None;
            }
        }
        Ok(_) => tracing::debug!(
            provider,
            "cloud budget success did not clear newer exhaustion"
        ),
        Err(error) => tracing::debug!(provider, %error, "cloud budget success record unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

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

    #[test]
    fn only_future_window_is_exhausted() {
        let now = Utc::now();
        let mut row = ProviderBudget {
            provider: "kimi".into(),
            window_exhausted_until: Some(now + TimeDelta::seconds(1)),
            weekly_pct: Some(64),
        };
        assert!(is_exhausted(Some(&row), now));
        row.window_exhausted_until = Some(now);
        assert!(!is_exhausted(Some(&row), now));
        row.window_exhausted_until = Some(now - TimeDelta::seconds(1));
        assert!(!is_exhausted(Some(&row), now));
        assert!(!is_exhausted(None, now));
    }
}
