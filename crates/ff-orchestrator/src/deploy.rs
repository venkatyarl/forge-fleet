//! Graceful, batched fleet deployment coordination.

use anyhow::Result;
use async_trait::async_trait;

/// Operations supplied by the deploy transport/state owner.
#[async_trait]
pub trait GracefulDeployHooks<T, O>: Send {
    /// Stop new work and wait for/requeue work already running on this batch.
    async fn drain(&mut self, batch: &[T]) -> Result<()>;

    /// Update and restart every member of this batch.
    async fn update_and_restart(&mut self, batch: Vec<T>) -> Result<Vec<O>>;

    /// Verify that every restarted member is healthy before rollout continues.
    async fn health_check(&mut self, results: &[O]) -> Result<bool>;

    /// Restore the batch's pre-deploy scheduling state.
    async fn reenable(&mut self, batch: &[T]) -> Result<()>;
}

/// Results from the batches that were attempted.
#[derive(Debug, PartialEq, Eq)]
pub struct GracefulDeployOutcome<O> {
    pub results: Vec<O>,
    /// True when a failed health check prevented later batches from starting.
    pub halted: bool,
}

/// Run `drain -> update/restart -> health check -> re-enable` for each batch.
///
/// Re-enable is attempted after every successful drain, including when update,
/// restart, or health checking fails. A failed health check stops the rollout
/// before the next batch is drained.
pub async fn run_graceful_deploy<T, O, H>(
    targets: Vec<T>,
    batch_size: usize,
    hooks: &mut H,
) -> Result<GracefulDeployOutcome<O>>
where
    T: Clone + Send + Sync,
    O: Send + Sync,
    H: GracefulDeployHooks<T, O>,
{
    let mut all_results = Vec::new();

    for batch in targets.chunks(batch_size.max(1)) {
        let batch = batch.to_vec();
        hooks.drain(&batch).await?;

        let deployment = hooks.update_and_restart(batch.clone()).await;
        let (results, healthy) = match deployment {
            Ok(results) => {
                let healthy = hooks.health_check(&results).await;
                match healthy {
                    Ok(healthy) => (results, healthy),
                    Err(error) => {
                        let cleanup = hooks.reenable(&batch).await;
                        if let Err(cleanup_error) = cleanup {
                            return Err(error.context(format!(
                                "health check failed; re-enable also failed: {cleanup_error}"
                            )));
                        }
                        return Err(error);
                    }
                }
            }
            Err(error) => {
                let cleanup = hooks.reenable(&batch).await;
                if let Err(cleanup_error) = cleanup {
                    return Err(error.context(format!(
                        "update/restart failed; re-enable also failed: {cleanup_error}"
                    )));
                }
                return Err(error);
            }
        };

        hooks.reenable(&batch).await?;
        all_results.extend(results);
        if !healthy {
            return Ok(GracefulDeployOutcome {
                results: all_results,
                halted: true,
            });
        }
    }

    Ok(GracefulDeployOutcome {
        results: all_results,
        halted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Hooks {
        events: Vec<String>,
        unhealthy: Option<u8>,
        fail_update: bool,
    }

    #[async_trait]
    impl GracefulDeployHooks<u8, u8> for Hooks {
        async fn drain(&mut self, batch: &[u8]) -> Result<()> {
            self.events.push(format!("drain:{batch:?}"));
            Ok(())
        }

        async fn update_and_restart(&mut self, batch: Vec<u8>) -> Result<Vec<u8>> {
            self.events.push(format!("update-restart:{batch:?}"));
            if self.fail_update {
                anyhow::bail!("update failed");
            }
            Ok(batch)
        }

        async fn health_check(&mut self, results: &[u8]) -> Result<bool> {
            self.events.push(format!("health:{results:?}"));
            Ok(!results.iter().any(|node| Some(*node) == self.unhealthy))
        }

        async fn reenable(&mut self, batch: &[u8]) -> Result<()> {
            self.events.push(format!("reenable:{batch:?}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn sequences_each_batch_before_starting_the_next() {
        let mut hooks = Hooks::default();
        let outcome = run_graceful_deploy(vec![1, 2, 3], 2, &mut hooks)
            .await
            .unwrap();

        assert!(!outcome.halted);
        assert_eq!(outcome.results, vec![1, 2, 3]);
        assert_eq!(
            hooks.events,
            [
                "drain:[1, 2]",
                "update-restart:[1, 2]",
                "health:[1, 2]",
                "reenable:[1, 2]",
                "drain:[3]",
                "update-restart:[3]",
                "health:[3]",
                "reenable:[3]",
            ]
        );
    }

    #[tokio::test]
    async fn unhealthy_batch_is_reenabled_and_stops_later_batches() {
        let mut hooks = Hooks {
            unhealthy: Some(2),
            ..Hooks::default()
        };
        let outcome = run_graceful_deploy(vec![1, 2, 3, 4], 2, &mut hooks)
            .await
            .unwrap();

        assert!(outcome.halted);
        assert_eq!(outcome.results, vec![1, 2]);
        assert_eq!(
            hooks.events,
            [
                "drain:[1, 2]",
                "update-restart:[1, 2]",
                "health:[1, 2]",
                "reenable:[1, 2]",
            ]
        );
    }

    #[tokio::test]
    async fn update_failure_reenables_the_drained_batch() {
        let mut hooks = Hooks {
            fail_update: true,
            ..Hooks::default()
        };
        let error = run_graceful_deploy(vec![1, 2, 3], 2, &mut hooks)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "update failed");
        assert_eq!(
            hooks.events,
            ["drain:[1, 2]", "update-restart:[1, 2]", "reenable:[1, 2]"]
        );
    }
}
