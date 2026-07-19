//! Daemon restart handler for the `ff-deploy` crate.
//!
//! Deploy-triggered restarts must be *attempt-neutral*: any work item that was
//! actively leased when the restart started is either allowed to finish (the
//! lease is drained) or is requeued without bumping its retry attempt counter.
//! Bumping attempts for a restart would penalize in-flight work for an event
//! that is not a real failure.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::DeployConfig;

/// How often the restart handler polls active leases while draining.
const LEASE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// An active lease that may be holding one or more work items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveLease {
    /// Opaque identifier for the lease (e.g. a slot id or worker name).
    pub lease_id: String,
    /// Work-item ids pinned to this lease that must be requeued if the lease
    /// cannot be drained before the timeout.
    pub work_item_ids: Vec<String>,
}

/// Outcome of a deploy restart drain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestartReport {
    /// `true` if all active leases released before the drain timeout.
    pub drained: bool,
    /// Work-item ids that were requeued because their leases did not release
    /// in time. Empty when `drained` is `true`.
    pub requeued_item_ids: Vec<String>,
}

/// Attempt-neutral restart handler.
///
/// Polls `active_leases` until all leases release or `config.drain_timeout`
/// elapses. If the timeout is reached, `requeue_items` is invoked for the
/// remaining leases so their work items can be rescheduled without incrementing
/// retry attempt counters.
///
/// # Type parameters
///
/// * `F` / `Fut` — a fallible lease query returning the currently active leases.
/// * `G` / `GFut` — a fallible requeue operation that requeues the leased items
///   without counting the operation as a retry attempt. The leases are passed by
///   value so the future does not borrow from the call site.
pub async fn restart_with_lease_drain<F, Fut, G, GFut>(
    config: &DeployConfig,
    active_leases: F,
    requeue_items: G,
) -> Result<RestartReport>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<ActiveLease>>>,
    G: Fn(Vec<ActiveLease>) -> GFut,
    GFut: std::future::Future<Output = Result<()>>,
{
    let deadline = Instant::now() + config.drain_timeout;

    loop {
        let leases = active_leases()
            .await
            .context("failed to query active leases during restart drain")?;

        if leases.is_empty() {
            return Ok(RestartReport {
                drained: true,
                ..RestartReport::default()
            });
        }

        if Instant::now() >= deadline {
            warn!(
                lease_count = leases.len(),
                "restart handler: drain timeout exceeded; requeueing leased items without bumping attempt counters"
            );

            let requeued_item_ids: Vec<String> = leases
                .iter()
                .flat_map(|lease| lease.work_item_ids.iter().cloned())
                .collect();

            requeue_items(leases)
                .await
                .context("failed to requeue items during restart drain")?;

            return Ok(RestartReport {
                drained: false,
                requeued_item_ids,
            });
        }

        info!(
            lease_count = leases.len(),
            remaining_secs = deadline.saturating_duration_since(Instant::now()).as_secs(),
            "restart handler: waiting for active leases to release"
        );

        tokio::time::sleep(LEASE_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drains_immediately_when_no_active_leases() {
        let config = DeployConfig {
            drain_timeout: Duration::from_secs(1),
        };

        let report = restart_with_lease_drain(
            &config,
            || async { Ok::<_, anyhow::Error>(vec![]) },
            |_leases| async { Ok::<_, anyhow::Error>(()) },
        )
        .await
        .unwrap();

        assert!(report.drained);
        assert!(report.requeued_item_ids.is_empty());
    }

    #[tokio::test]
    async fn drains_after_leases_release() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let config = DeployConfig {
            drain_timeout: Duration::from_secs(5),
        };
        let call_count = Arc::new(AtomicUsize::new(0));
        let requeued = Arc::new(std::sync::Mutex::new(false));

        let cc = call_count.clone();
        let rq = requeued.clone();
        let report = restart_with_lease_drain(
            &config,
            move || {
                let cc = cc.clone();
                async move {
                    if cc.fetch_add(1, Ordering::SeqCst) == 0 {
                        Ok(vec![ActiveLease {
                            lease_id: "slot-1".into(),
                            work_item_ids: vec!["wi-1".into()],
                        }])
                    } else {
                        Ok(vec![])
                    }
                }
            },
            move |_leases| {
                let rq = rq.clone();
                async move {
                    *rq.lock().unwrap() = true;
                    Ok::<_, anyhow::Error>(())
                }
            },
        )
        .await
        .unwrap();

        assert!(report.drained);
        assert!(report.requeued_item_ids.is_empty());
        assert!(!*requeued.lock().unwrap());
    }

    #[tokio::test]
    async fn requeues_items_when_drain_times_out() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let config = DeployConfig {
            drain_timeout: Duration::from_millis(50),
        };
        let requeue_calls = Arc::new(AtomicUsize::new(0));

        let rc = requeue_calls.clone();
        let report = restart_with_lease_drain(
            &config,
            || async {
                Ok(vec![ActiveLease {
                    lease_id: "slot-2".into(),
                    work_item_ids: vec!["wi-2".into(), "wi-3".into()],
                }])
            },
            move |leases| {
                let rc = rc.clone();
                async move {
                    assert_eq!(leases.len(), 1);
                    assert_eq!(leases[0].work_item_ids, &["wi-2", "wi-3"]);
                    rc.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, anyhow::Error>(())
                }
            },
        )
        .await
        .unwrap();

        assert!(!report.drained);
        assert_eq!(report.requeued_item_ids, &["wi-2", "wi-3"]);
        assert_eq!(requeue_calls.load(Ordering::SeqCst), 1);
    }
}
