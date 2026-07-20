//! Deploy target resolution with retry for incomplete lookups.
//!
//! Target rows can be transiently incomplete: a host that just registered may
//! not have reported its `primary_ip` or RAM yet, so a single lookup can
//! return empty values even though the host is healthy. `resolve_with_retry`
//! re-runs the lookup with exponential backoff until the target is complete
//! or the attempt budget is exhausted.

use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

/// A deploy target as returned by a lookup (e.g. the `computers` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTarget {
    /// Host name.
    pub name: String,
    /// Primary IPv4 address; may be empty when the host has not reported yet.
    pub primary_ip: String,
    /// Total RAM in GB; 0 when the host has not reported yet.
    pub ram_gb: i32,
}

impl ResolvedTarget {
    /// `true` when both `primary_ip` and `ram_gb` carry usable values.
    pub fn is_complete(&self) -> bool {
        !self.primary_ip.trim().is_empty() && self.ram_gb > 0
    }
}

/// A type that carries the fields needed for deploy-target completeness.
///
/// Callers can implement this trait on their own row/candidate types so the
/// retry helpers can check completeness without needing to know the full
/// target shape.
pub trait TargetLike {
    /// Host name, used only for diagnostics.
    fn target_name(&self) -> &str;
    /// Primary IPv4 address; empty when not yet reported.
    fn target_primary_ip(&self) -> &str;
    /// Total RAM in GB; 0 when not yet reported.
    fn target_ram_gb(&self) -> i32;

    /// `true` when both `target_primary_ip` and `target_ram_gb` are populated.
    fn is_complete(&self) -> bool {
        !self.target_primary_ip().trim().is_empty() && self.target_ram_gb() > 0
    }
}

impl TargetLike for ResolvedTarget {
    fn target_name(&self) -> &str {
        &self.name
    }
    fn target_primary_ip(&self) -> &str {
        &self.primary_ip
    }
    fn target_ram_gb(&self) -> i32 {
        self.ram_gb
    }
}

/// Retry policy for target resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionRetryPolicy {
    /// Maximum number of lookup attempts (including the first).
    pub max_attempts: u32,
    /// Delay before the second attempt; later delays multiply from here.
    pub initial_delay: Duration,
    /// Multiplier applied to the delay after each failed attempt.
    pub backoff_multiplier: u32,
}

impl Default for ResolutionRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_secs(1),
            backoff_multiplier: 2,
        }
    }
}

impl ResolutionRetryPolicy {
    /// Backoff delay after failed attempt `attempt` (1-based): 1s, 2s, 4s, 8s…
    pub fn delay_after_attempt(&self, attempt: u32) -> Duration {
        let factor = self
            .backoff_multiplier
            .saturating_pow(attempt.saturating_sub(1));
        self.initial_delay.saturating_mul(factor)
    }
}

/// Target resolution failure.
#[derive(Debug, Error)]
pub enum ResolutionError {
    /// Every attempt errored; carries the last lookup error.
    #[error("target lookup failed after {attempts} attempt(s): {source}")]
    LookupFailed {
        /// Attempts performed.
        attempts: u32,
        /// Last lookup error.
        source: anyhow::Error,
    },
    /// The retry budget was exhausted while the target remained incomplete.
    /// The circuit is open and the deployment must fail rather than retrying
    /// indefinitely.
    #[error(
        "target resolution circuit opened for '{name}' after {attempts} attempt(s) \
         (primary_ip='{primary_ip}', ram_gb={ram_gb}); missing: {missing}"
    )]
    CircuitOpen {
        /// Attempts performed.
        attempts: u32,
        /// Host name from the last lookup.
        name: String,
        /// Last observed primary IP.
        primary_ip: String,
        /// Last observed RAM in GB.
        ram_gb: i32,
        /// Comma-separated list of missing fields (e.g. "primary_ip" or "ram").
        missing: String,
    },
}

impl ResolutionError {
    /// Returns whether the caller may safely retry this error.
    ///
    /// Errors returned by `resolve_with_retry` are terminal because the
    /// configured attempt budget has already been exhausted.
    pub fn is_retryable(&self) -> bool {
        false
    }
}

/// Run `lookup` until it returns a complete target, retrying incomplete
/// results and lookup errors per `policy`. Sleeps between attempts.
pub fn resolve_with_retry<F>(
    policy: &ResolutionRetryPolicy,
    mut lookup: F,
) -> Result<ResolvedTarget, ResolutionError>
where
    F: FnMut() -> anyhow::Result<ResolvedTarget>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut last: Option<Result<ResolvedTarget, anyhow::Error>> = None;

    for attempt in 1..=max_attempts {
        match lookup() {
            Ok(target) if target.is_complete() => return Ok(target),
            Ok(target) => {
                warn!(
                    attempt,
                    max_attempts,
                    name = %target.name,
                    primary_ip = %target.primary_ip,
                    ram_gb = target.ram_gb,
                    "deploy target incomplete; retrying lookup"
                );
                last = Some(Ok(target));
            }
            Err(e) => {
                warn!(attempt, max_attempts, error = %e, "deploy target lookup failed; retrying");
                last = Some(Err(e));
            }
        }
        if attempt < max_attempts {
            std::thread::sleep(policy.delay_after_attempt(attempt));
        }
    }

    match last.expect("at least one attempt runs") {
        Ok(target) => {
            let mut missing = Vec::new();
            if target.primary_ip.trim().is_empty() {
                missing.push("primary_ip");
            }
            if target.ram_gb <= 0 {
                missing.push("ram");
            }
            Err(ResolutionError::CircuitOpen {
                attempts: max_attempts,
                name: target.name,
                primary_ip: target.primary_ip,
                ram_gb: target.ram_gb,
                missing: missing.join(", "),
            })
        }
        Err(source) => Err(ResolutionError::LookupFailed {
            attempts: max_attempts,
            source,
        }),
    }
}

/// Build a retryable error describing which required fields are missing.
fn retryable_error<T: TargetLike>(target: &T, attempts: u32) -> ResolutionError {
    let mut missing = Vec::new();
    if target.target_primary_ip().trim().is_empty() {
        missing.push("primary_ip");
    }
    if target.target_ram_gb() <= 0 {
        missing.push("ram");
    }
    ResolutionError::Retryable {
        attempts,
        name: target.target_name().to_string(),
        primary_ip: target.target_primary_ip().to_string(),
        ram_gb: target.target_ram_gb(),
        missing: missing.join(", "),
    }
}

/// Async variant of [`resolve_with_retry`].
///
/// The lookup closure returns a [`Future`] so it can await database or API
/// calls. Delays use [`tokio::time::sleep`] instead of blocking the thread.
pub async fn resolve_with_retry_async<T, F, Fut>(
    policy: &ResolutionRetryPolicy,
    mut lookup: F,
) -> Result<T, ResolutionError>
where
    T: TargetLike,
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut last: Option<anyhow::Result<T>> = None;

    for attempt in 1..=max_attempts {
        match lookup().await {
            Ok(target) if target.is_complete() => return Ok(target),
            Ok(target) => {
                warn!(
                    attempt,
                    max_attempts,
                    name = %target.target_name(),
                    primary_ip = %target.target_primary_ip(),
                    ram_gb = target.target_ram_gb(),
                    "deploy target incomplete; retrying lookup"
                );
                last = Some(Ok(target));
            }
            Err(e) => {
                warn!(attempt, max_attempts, error = %e, "deploy target lookup failed; retrying");
                last = Some(Err(e));
            }
        }
        if attempt < max_attempts {
            tokio::time::sleep(policy.delay_after_attempt(attempt)).await;
        }
    }

    match last.expect("at least one attempt runs") {
        Ok(target) => Err(retryable_error(&target, max_attempts)),
        Err(source) => Err(ResolutionError::LookupFailed {
            attempts: max_attempts,
            source,
        }),
    }
}

/// Async retry for a lookup that returns multiple targets.
///
/// The lookup is retried until every returned target is complete (non-empty
/// `primary_ip` and positive `ram_gb`) or the attempt budget is exhausted.
pub async fn resolve_all_with_retry_async<T, F, Fut>(
    policy: &ResolutionRetryPolicy,
    mut lookup: F,
) -> Result<Vec<T>, ResolutionError>
where
    T: TargetLike,
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<T>>>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut last: Option<anyhow::Result<Vec<T>>> = None;

    for attempt in 1..=max_attempts {
        match lookup().await {
            Ok(targets) if targets.iter().all(|t| t.is_complete()) => return Ok(targets),
            Ok(targets) => {
                let incomplete: Vec<&str> = targets
                    .iter()
                    .filter(|t| !t.is_complete())
                    .map(|t| t.target_name())
                    .collect();
                warn!(
                    attempt,
                    max_attempts,
                    incomplete = ?incomplete,
                    "deploy targets incomplete; retrying lookup"
                );
                last = Some(Ok(targets));
            }
            Err(e) => {
                warn!(attempt, max_attempts, error = %e, "deploy target lookup failed; retrying");
                last = Some(Err(e));
            }
        }
        if attempt < max_attempts {
            tokio::time::sleep(policy.delay_after_attempt(attempt)).await;
        }
    }

    match last.expect("at least one attempt runs") {
        Ok(targets) => {
            let target = targets
                .into_iter()
                .find(|t| !t.is_complete())
                .expect("at least one target was incomplete");
            Err(retryable_error(&target, max_attempts))
        }
        Err(source) => Err(ResolutionError::LookupFailed {
            attempts: max_attempts,
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn instant_policy() -> ResolutionRetryPolicy {
        ResolutionRetryPolicy {
            initial_delay: Duration::ZERO,
            ..ResolutionRetryPolicy::default()
        }
    }

    fn target(primary_ip: &str, ram_gb: i32) -> ResolvedTarget {
        ResolvedTarget {
            name: "taylor".into(),
            primary_ip: primary_ip.into(),
            ram_gb,
        }
    }

    #[test]
    fn default_policy_is_five_attempts_from_one_second() {
        let policy = ResolutionRetryPolicy::default();
        assert_eq!(policy.max_attempts, 5);
        assert_eq!(policy.delay_after_attempt(1), Duration::from_secs(1));
        assert_eq!(policy.delay_after_attempt(2), Duration::from_secs(2));
        assert_eq!(policy.delay_after_attempt(3), Duration::from_secs(4));
        assert_eq!(policy.delay_after_attempt(4), Duration::from_secs(8));
    }

    #[test]
    fn complete_target_resolves_on_first_attempt() {
        let mut calls = 0;
        let resolved = resolve_with_retry(&instant_policy(), || {
            calls += 1;
            Ok(target("192.168.1.20", 128))
        })
        .expect("complete target resolves");
        assert_eq!(calls, 1);
        assert_eq!(resolved.primary_ip, "192.168.1.20");
    }

    #[test]
    fn empty_ip_retries_until_populated() {
        let mut calls = 0;
        let resolved = resolve_with_retry(&instant_policy(), || {
            calls += 1;
            if calls < 3 {
                Ok(target("", 128))
            } else {
                Ok(target("192.168.1.20", 128))
            }
        })
        .expect("resolves once ip appears");
        assert_eq!(calls, 3);
        assert!(resolved.is_complete());
    }

    #[test]
    fn zero_ram_retries_until_populated() {
        let mut calls = 0;
        let resolved = resolve_with_retry(&instant_policy(), || {
            calls += 1;
            if calls < 2 {
                Ok(target("192.168.1.20", 0))
            } else {
                Ok(target("192.168.1.20", 64))
            }
        })
        .expect("resolves once ram appears");
        assert_eq!(calls, 2);
        assert_eq!(resolved.ram_gb, 64);
    }

    #[test]
    fn incomplete_target_exhausts_five_attempts() {
        let mut calls = 0;
        let err = resolve_with_retry(&instant_policy(), || {
            calls += 1;
            Ok(target("", 0))
        })
        .expect_err("never completes");
        assert_eq!(calls, 5);
        assert!(matches!(
            err,
            ResolutionError::CircuitOpen { attempts: 5, .. }
        ));
        assert!(!err.is_retryable());
    }

    #[test]
    fn circuit_open_error_lists_empty_primary_ip() {
        let err = resolve_with_retry(&instant_policy(), || Ok(target("", 64)))
            .expect_err("empty ip never completes");
        match err {
            ResolutionError::CircuitOpen { missing, .. } => {
                assert_eq!(missing, "primary_ip");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn circuit_open_error_lists_zero_ram() {
        let err = resolve_with_retry(&instant_policy(), || Ok(target("192.168.1.20", 0)))
            .expect_err("zero ram never completes");
        match err {
            ResolutionError::CircuitOpen { missing, .. } => {
                assert_eq!(missing, "ram");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn circuit_open_error_lists_both_missing_fields() {
        let err = resolve_with_retry(&instant_policy(), || Ok(target("", 0)))
            .expect_err("both missing never completes");
        match err {
            ResolutionError::CircuitOpen { missing, .. } => {
                assert_eq!(missing, "primary_ip, ram");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn lookup_failed_error_is_not_retryable() {
        let err = resolve_with_retry(&instant_policy(), || anyhow::bail!("db unreachable"))
            .expect_err("all attempts error");
        assert!(!err.is_retryable());
    }

    #[test]
    fn lookup_error_retries_then_succeeds() {
        let mut calls = 0;
        let resolved = resolve_with_retry(&instant_policy(), || {
            calls += 1;
            if calls == 1 {
                anyhow::bail!("db unreachable")
            }
            Ok(target("192.168.1.20", 128))
        })
        .expect("resolves after transient error");
        assert_eq!(calls, 2);
        assert!(resolved.is_complete());
    }

    #[test]
    fn persistent_lookup_error_reports_last_error() {
        let err = resolve_with_retry(&instant_policy(), || anyhow::bail!("db unreachable"))
            .expect_err("all attempts error");
        match err {
            ResolutionError::LookupFailed { attempts, source } => {
                assert_eq!(attempts, 5);
                assert_eq!(source.to_string(), "db unreachable");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn whitespace_ip_counts_as_empty() {
        assert!(!target("   ", 64).is_complete());
    }

    #[tokio::test]
    async fn async_complete_target_resolves_on_first_attempt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolved = resolve_with_retry_async(&instant_policy(), {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { Ok(target("192.168.1.20", 128)) }
            }
        })
        .await
        .expect("complete target resolves");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved.primary_ip, "192.168.1.20");
    }

    #[tokio::test]
    async fn async_empty_ip_retries_until_populated() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolved = resolve_with_retry_async(&instant_policy(), {
            let calls = calls.clone();
            move || {
                let c = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if c < 3 {
                        Ok(target("", 128))
                    } else {
                        Ok(target("192.168.1.20", 128))
                    }
                }
            }
        })
        .await
        .expect("resolves once ip appears");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(resolved.is_complete());
    }

    #[tokio::test]
    async fn async_lookup_error_retries_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolved = resolve_with_retry_async(&instant_policy(), {
            let calls = calls.clone();
            move || {
                let c = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if c == 1 {
                        anyhow::bail!("db unreachable")
                    }
                    Ok(target("192.168.1.20", 128))
                }
            }
        })
        .await
        .expect("resolves after transient error");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(resolved.is_complete());
    }

    #[tokio::test]
    async fn async_all_complete_resolves_first_attempt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolved = resolve_all_with_retry_async(&instant_policy(), {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(vec![
                        target("192.168.1.20", 128),
                        target("192.168.1.21", 64),
                    ])
                }
            }
        })
        .await
        .expect("all complete resolves");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved.len(), 2);
    }

    #[tokio::test]
    async fn async_all_retries_while_any_incomplete() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolved = resolve_all_with_retry_async(&instant_policy(), {
            let calls = calls.clone();
            move || {
                let c = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if c < 3 {
                        Ok(vec![target("192.168.1.20", 128), target("", 64)])
                    } else {
                        Ok(vec![
                            target("192.168.1.20", 128),
                            target("192.168.1.21", 64),
                        ])
                    }
                }
            }
        })
        .await
        .expect("resolves once all complete");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(resolved.iter().all(|t| t.is_complete()));
    }
}
