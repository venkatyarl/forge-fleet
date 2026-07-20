//! Slot-manager retry-jitter logic for git fetch operations.
//!
//! Git fetches are a common transient failure point for sub-agent slots
//! (network blip, GitHub rate-limit window, SSH handshake stall). This module
//! wraps `git fetch` with a bounded exponential-backoff retry decorated with
//! uniform random jitter so that many retrying slots do not hammer the remote
//! in lockstep.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use rand::Rng;
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

/// Default maximum number of fetch attempts.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Base delay for exponential backoff.
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(500);

/// Cap on the raw exponential delay before jitter is applied.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(10);

/// Default per-attempt timeout for `git fetch`.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Run `git fetch` in `repo_path` with exponential-backoff retry and random
/// jitter.
///
/// `args` are appended after `git fetch`; pass `&["origin", "main"]` etc. A
/// successful attempt returns immediately. After `max_attempts` failures the
/// last error is returned with context.
///
/// # Example
///
/// ```no_run
/// use ff_agent::ha::slot_manager::git_fetch_with_retry;
/// # async fn example() -> anyhow::Result<()> {
/// git_fetch_with_retry("/tmp/repo", &["origin", "main"], 3, None).await?;
/// # Ok(())
/// # }
/// ```
pub async fn git_fetch_with_retry<P, I, S>(
    repo_path: P,
    args: I,
    max_attempts: u32,
    timeout_override: Option<Duration>,
) -> Result<()>
where
    P: AsRef<Path>,
    I: IntoIterator<Item = S> + Clone,
    S: AsRef<std::ffi::OsStr>,
{
    let repo_path = repo_path.as_ref();
    let per_attempt_timeout = timeout_override.unwrap_or(DEFAULT_FETCH_TIMEOUT);
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..max_attempts {
        debug!(
            repo_path = %repo_path.display(),
            attempt = attempt + 1,
            max_attempts,
            "running git fetch"
        );

        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo_path)
            .arg("fetch")
            .args(args.clone())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match timeout(per_attempt_timeout, cmd.output()).await {
            Ok(Ok(output)) if output.status.success() => {
                debug!(
                    repo_path = %repo_path.display(),
                    attempt = attempt + 1,
                    "git fetch succeeded"
                );
                return Ok(());
            }
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let msg = format!("git fetch exited with {}: {}", output.status, stderr.trim());
                warn!(
                    repo_path = %repo_path.display(),
                    attempt = attempt + 1,
                    error = %msg,
                    "git fetch failed"
                );
                last_err = Some(anyhow::anyhow!(msg));
            }
            Ok(Err(e)) => {
                warn!(
                    repo_path = %repo_path.display(),
                    attempt = attempt + 1,
                    error = %e,
                    "git fetch failed to spawn"
                );
                last_err = Some(e.into());
            }
            Err(_) => {
                warn!(
                    repo_path = %repo_path.display(),
                    attempt = attempt + 1,
                    timeout_secs = per_attempt_timeout.as_secs(),
                    "git fetch timed out"
                );
                last_err = Some(anyhow::anyhow!(
                    "git fetch timed out after {}s",
                    per_attempt_timeout.as_secs()
                ));
            }
        }

        if attempt + 1 < max_attempts {
            let delay = backoff_with_jitter(attempt);
            debug!(
                repo_path = %repo_path.display(),
                attempt = attempt + 1,
                delay_ms = delay.as_millis() as u64,
                "sleeping before git fetch retry"
            );
            sleep(delay).await;
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("git fetch failed"))).with_context(|| {
        format!(
            "git fetch failed after {} attempts in {}",
            max_attempts,
            repo_path.display()
        )
    })
}

/// Compute the backoff delay for `attempt` (0-indexed) using exponential
/// backoff capped at [`DEFAULT_MAX_DELAY`] plus uniform random jitter in
/// `[0, computed_delay)`.
fn backoff_with_jitter(attempt: u32) -> Duration {
    let base = DEFAULT_BASE_DELAY.as_millis() as u64;
    let max = DEFAULT_MAX_DELAY.as_millis() as u64;
    let raw = max.min(base.saturating_mul(1u64 << attempt));
    let jitter = rand::thread_rng().gen_range(0..raw.max(1));
    Duration::from_millis(jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_jitter_stays_within_exponential_cap() {
        // Base 500ms: attempt 0 -> [0, 500), 1 -> [0, 1000), 2 -> [0, 2000),
        // attempt 5 -> capped to [0, 10000).
        for attempt in 0..=6 {
            let delay = backoff_with_jitter(attempt);
            let max_expected_ms = (500u64 << attempt).min(10_000);
            assert!(
                delay.as_millis() as u64 <= max_expected_ms,
                "attempt {attempt}: delay {}ms exceeds cap {max_expected_ms}ms",
                delay.as_millis()
            );
        }
    }
}
