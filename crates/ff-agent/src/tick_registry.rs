use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::info;

#[derive(Clone, Copy, Debug)]
pub struct TickRun {
    pub started_at: Instant,
    pub last_started_at: Option<Instant>,
}

pub struct TickRegistry;

impl TickRegistry {
    pub fn register<F, Fut>(
        name: &'static str,
        interval: Duration,
        mut shutdown_rx: watch::Receiver<bool>,
        mut tick: F,
    ) -> JoinHandle<()>
    where
        F: FnMut(TickRun) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_started_at = None;

            loop {
                tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        let started_at = Instant::now();
                        let run = TickRun {
                            started_at,
                            last_started_at,
                        };
                        last_started_at = Some(started_at);
                        tick(run).await;
                    }
                }
            }

            info!(tick = name, "tick registry loop stopped");
        })
    }
}
