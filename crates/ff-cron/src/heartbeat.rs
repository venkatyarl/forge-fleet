use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, watch};
use tracing::{error, info};

pub type HeartbeatFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;
pub type HeartbeatAction = Arc<dyn Fn() -> HeartbeatFuture + Send + Sync + 'static>;

#[derive(Clone)]
pub struct HeartbeatTask {
    pub name: String,
    pub action: HeartbeatAction,
}

impl std::fmt::Debug for HeartbeatTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeartbeatTask")
            .field("name", &self.name)
            .finish()
    }
}

/// Periodic maintenance runner for cron internals.
#[derive(Clone)]
pub struct HeartbeatRunner {
    interval: Duration,
    tasks: Arc<RwLock<Vec<HeartbeatTask>>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl HeartbeatRunner {
    pub fn new(interval: Duration) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Self {
            interval,
            tasks: Arc::new(RwLock::new(Vec::new())),
            shutdown_tx,
            shutdown_rx,
        }
    }

    pub async fn register_task(&self, name: impl Into<String>, action: HeartbeatAction) {
        let mut tasks = self.tasks.write().await;
        tasks.push(HeartbeatTask {
            name: name.into(),
            action,
        });
    }

    pub async fn run_once(&self) {
        let tasks = self.tasks.read().await.clone();

        for task in tasks {
            match (task.action)().await {
                Ok(()) => {
                    info!(task = %task.name, "heartbeat maintenance task completed");
                }
                Err(err) => {
                    error!(task = %task.name, error = %err, "heartbeat maintenance task failed");
                }
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            let mut shutdown_rx = self.shutdown_rx.clone();

            info!(
                interval_secs = self.interval.as_secs(),
                "heartbeat runner started"
            );

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        self.run_once().await;
                    }
                    _ = shutdown_rx.changed() => {
                        info!("heartbeat runner shutting down");
                        break;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn runner_executes_registered_tasks() {
        let runner = HeartbeatRunner::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);

        runner
            .register_task(
                "increment",
                Arc::new(move || {
                    let calls_inner = Arc::clone(&calls_clone);
                    Box::pin(async move {
                        calls_inner.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
            )
            .await;

        runner.run_once().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
