//! Daemon restart coordination with active-lease draining.
//!
//! Before restarting a daemon, callers should ensure that no active work-item
//! leases are held by the target. Restarting while leases are active can orphan
//! in-flight work and wedge fleet slots. This module provides a small, testable
//! coordinator that polls a [`LeaseSource`] until leases drain (or a timeout
//! expires) and only then invokes the restart action.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::DeployConfig;

// Re-export git helpers so the deploy module exposes the full dirty-tree reset
// workflow: detect dirty trees, stash them with a labeled ref, then reset.
pub use crate::git_utils::{git_fetch_and_reset_hard, git_stash_dirty_tree, git_tree_is_dirty};

/// Default sleep interval between lease polls.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Source of truth for active leases on the target being restarted.
pub trait LeaseSource {
    /// Return the current number of active leases.
    fn active_leases(&self) -> Result<usize>;
}

/// Coordinates daemon restart with lease draining.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartCoordinator {
    /// Maximum time to wait for leases to drain before failing.
    pub drain_timeout: Duration,
    /// Sleep interval between lease polls.
    pub poll_interval: Duration,
}

impl Default for RestartCoordinator {
    fn default() -> Self {
        Self {
            drain_timeout: Duration::from_secs(300),
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

impl RestartCoordinator {
    /// Create a coordinator from the top-level [`DeployConfig`], using the
    /// default poll interval.
    pub fn from_config(config: &DeployConfig) -> Self {
        Self {
            drain_timeout: config.drain_timeout,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// Create a coordinator with explicit timeout and poll interval.
    pub fn new(drain_timeout: Duration, poll_interval: Duration) -> Self {
        Self {
            drain_timeout,
            poll_interval,
        }
    }

    /// Wait for active leases to drain, then perform the restart.
    ///
    /// `perform_restart` is called exactly once after the lease count reaches
    /// zero (or immediately if it is already zero). If leases do not drain
    /// within `drain_timeout`, the function returns an error and the restart is
    /// **not** performed.
    pub fn restart<S, F>(&self, source: &S, perform_restart: F) -> Result<()>
    where
        S: LeaseSource,
        F: FnOnce() -> Result<()>,
    {
        let start = Instant::now();

        loop {
            let active = source
                .active_leases()
                .context("failed to query active leases")?;

            if active == 0 {
                info!("no active leases remaining; proceeding with daemon restart");
                break;
            }

            if start.elapsed() >= self.drain_timeout {
                return Err(anyhow::anyhow!(
                    "timed out after {:?} waiting for {} active lease(s) to drain",
                    self.drain_timeout,
                    active
                ));
            }

            warn!(
                active_leases = active,
                elapsed_ms = start.elapsed().as_millis(),
                "active leases still held; waiting before daemon restart"
            );

            let remaining = self.drain_timeout.saturating_sub(start.elapsed());
            std::thread::sleep(self.poll_interval.min(remaining));
        }

        perform_restart().context("daemon restart failed after leases drained")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FakeSource {
        counts: Vec<usize>,
        idx: AtomicUsize,
    }

    impl LeaseSource for FakeSource {
        fn active_leases(&self) -> Result<usize> {
            let i = self.idx.fetch_add(1, Ordering::SeqCst);
            Ok(*self
                .counts
                .get(i)
                .or_else(|| self.counts.last())
                .unwrap_or(&0))
        }
    }

    #[test]
    fn restart_succeeds_when_no_leases() {
        let source = FakeSource {
            counts: vec![0],
            idx: AtomicUsize::new(0),
        };
        let restarted = AtomicBool::new(false);
        let coord = RestartCoordinator::default();

        coord
            .restart(&source, || {
                restarted.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();

        assert!(restarted.load(Ordering::SeqCst));
    }

    #[test]
    fn restart_waits_for_leases_to_drain() {
        let source = FakeSource {
            counts: vec![3, 2, 1, 0],
            idx: AtomicUsize::new(0),
        };
        let restarted = AtomicBool::new(false);
        let coord = RestartCoordinator::new(Duration::from_secs(10), Duration::from_millis(1));

        coord
            .restart(&source, || {
                restarted.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();

        assert!(restarted.load(Ordering::SeqCst));
        assert_eq!(source.idx.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn restart_times_out_if_leases_never_drain() {
        let source = FakeSource {
            counts: vec![usize::MAX],
            idx: AtomicUsize::new(0),
        };
        let coord = RestartCoordinator::new(Duration::from_millis(5), Duration::from_millis(1));

        let err = coord.restart(&source, || Ok(())).unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn restart_not_run_when_drain_times_out() {
        let source = FakeSource {
            counts: vec![usize::MAX],
            idx: AtomicUsize::new(0),
        };
        let restarted = AtomicBool::new(false);
        let coord = RestartCoordinator::new(Duration::from_millis(5), Duration::from_millis(1));

        coord
            .restart(&source, || {
                restarted.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap_err();

        assert!(!restarted.load(Ordering::SeqCst));
    }

    #[test]
    fn drain_timeout_is_not_extended_by_poll_interval() {
        let source = FakeSource {
            counts: vec![usize::MAX],
            idx: AtomicUsize::new(0),
        };
        let coord = RestartCoordinator::new(Duration::from_millis(5), Duration::from_secs(1));
        let start = Instant::now();

        coord.restart(&source, || Ok(())).unwrap_err();

        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn from_config_uses_deploy_config_timeout() {
        let config = DeployConfig {
            drain_timeout: Duration::from_secs(120),
        };
        let coord = RestartCoordinator::from_config(&config);
        assert_eq!(coord.drain_timeout, Duration::from_secs(120));
        assert_eq!(coord.poll_interval, DEFAULT_POLL_INTERVAL);
    }
}
