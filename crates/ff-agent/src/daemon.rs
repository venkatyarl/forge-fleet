use std::{
    panic::AssertUnwindSafe,
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::{FutureExt, future::BoxFuture};
use sqlx::PgPool;
use tokio::sync::{RwLock, watch};
use tracing::{error, info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TickScope {
    EveryNode,
    LeaderOnly,
}

struct TickDefinition {
    name: &'static str,
    interval: Duration,
    scope: TickScope,
    runner: fn(PgPool, String) -> BoxFuture<'static, Result<()>>,
}

struct TickState {
    next_run_at: Instant,
}

struct RegisteredTick {
    definition: TickDefinition,
    state: TickState,
}

struct TickRegistry {
    ticks: Vec<RegisteredTick>,
}

impl TickRegistry {
    fn new() -> Self {
        let now = Instant::now();
        let ticks = [
            TickDefinition {
                name: "project_scheduler",
                interval: Duration::from_secs(60),
                scope: TickScope::LeaderOnly,
                runner: run_project_scheduler_tick,
            },
            TickDefinition {
                name: "work_item_scheduler",
                interval: Duration::from_secs(15),
                scope: TickScope::LeaderOnly,
                runner: run_work_item_scheduler_tick,
            },
        ]
        .into_iter()
        .map(|definition| RegisteredTick {
            state: TickState {
                next_run_at: now + definition.interval,
            },
            definition,
        })
        .collect();

        Self { ticks }
    }
}

struct LeaderCache {
    worker_name: String,
    ttl: Duration,
    cached: RwLock<Option<(Instant, bool)>>,
}

impl LeaderCache {
    fn new(worker_name: String) -> Self {
        Self {
            worker_name,
            ttl: Duration::from_secs(5),
            cached: RwLock::new(None),
        }
    }

    async fn is_leader(&self, pg: &PgPool) -> bool {
        {
            let guard = self.cached.read().await;
            if let Some((checked_at, is_leader)) = *guard
                && checked_at.elapsed() < self.ttl
            {
                return is_leader;
            }
        }

        let is_leader: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM fleet_leader_state
                WHERE member_name = $1
                  AND heartbeat_at > NOW() - INTERVAL '60 seconds'
            )
            "#,
        )
        .bind(&self.worker_name)
        .fetch_one(pg)
        .await
        .unwrap_or(false);

        let mut guard = self.cached.write().await;
        *guard = Some((Instant::now(), is_leader));
        is_leader
    }
}

struct PanicIsolatingWrapper;

impl PanicIsolatingWrapper {
    fn new() -> Self {
        Self
    }

    async fn invoke(
        &self,
        name: &'static str,
        runner: fn(PgPool, String) -> BoxFuture<'static, Result<()>>,
        pg: PgPool,
        worker_name: String,
    ) {
        match AssertUnwindSafe(runner(pg, worker_name)).catch_unwind().await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(tick = name, error = %err, "daemon tick failed"),
            Err(panic) => {
                let payload = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("non-string panic payload");
                error!(tick = name, payload, "daemon tick panicked");
            }
        }
    }
}

fn run_project_scheduler_tick(pg: PgPool, worker_name: String) -> BoxFuture<'static, Result<()>> {
    Box::pin(async move {
        crate::scheduler_tick::evaluate_schedules(&pg, &worker_name)
            .await
            .map(|_| ())
    })
}

fn run_work_item_scheduler_tick(
    pg: PgPool,
    _worker_name: String,
) -> BoxFuture<'static, Result<()>> {
    Box::pin(async move { crate::work_item_scheduler::evaluate_work_items(&pg).await.map(|_| ()) })
}

pub fn start_tick_scheduler(
    pg: PgPool,
    worker_name: String,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let registry = TickRegistry::new();
    let leader_cache = LeaderCache::new(worker_name.clone());
    let wrapper = PanicIsolatingWrapper::new();

    for tick in &registry.ticks {
        info!(
            tick = tick.definition.name,
            interval_secs = tick.definition.interval.as_secs(),
            scope = ?tick.definition.scope,
            "initialized daemon tick"
        );
    }

    tokio::spawn(async move {
        let mut registry = registry;
        let idle_interval = Duration::from_secs(1);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_interval) => {
                    let now = Instant::now();
                    for tick in &mut registry.ticks {
                        if now < tick.state.next_run_at {
                            continue;
                        }

                        tick.state.next_run_at = now + tick.definition.interval;

                        if tick.definition.scope == TickScope::LeaderOnly
                            && !leader_cache.is_leader(&pg).await
                        {
                            continue;
                        }

                        wrapper
                            .invoke(
                                tick.definition.name,
                                tick.definition.runner,
                                pg.clone(),
                                worker_name.clone(),
                            )
                            .await;
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }

        info!("daemon tick scheduler stopped");
    })
}
