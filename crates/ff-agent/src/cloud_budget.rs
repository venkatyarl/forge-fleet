//! Cached cloud-provider quota state used by dispatch and autonomous review.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{LazyLock, RwLock},
    time::{Duration, Instant},
};

const CACHE_TTL: Duration = Duration::from_secs(60);

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
                    // Cache failures too: T4 can deploy before T1 without
                    // turning every provider decision into a missing-table query.
                    cache.loaded_at = Some(Instant::now());
                    cache.providers.values().cloned().collect()
                },
            )
        }
    }
}

/// State observed immediately before a cloud call. A future exhaustion time
/// means skip; at/after the boundary a call may probe recovery.
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
