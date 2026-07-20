use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::{FutureExt, future::BoxFuture};
use sqlx::PgPool;
use tokio::sync::{RwLock, watch};
use tracing::{error, info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TickScope {
    /// Runs on every node — for ticks that only touch node-local state
    /// (e.g. the metrics scraper polling this node's inference servers).
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
            TickDefinition {
                name: "telegram_reply_poller",
                interval: Duration::from_secs(30),
                scope: TickScope::LeaderOnly,
                runner: run_telegram_reply_poller_tick,
            },
            TickDefinition {
                name: "log_analysis_worker",
                interval: crate::log_analysis_worker::DEFAULT_INTERVAL,
                scope: TickScope::LeaderOnly,
                runner: run_log_analysis_tick,
            },
            TickDefinition {
                name: "metrics_scraper",
                interval: crate::metrics_scraper::DEFAULT_INTERVAL,
                scope: TickScope::EveryNode,
                runner: run_metrics_scraper_tick,
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
        match AssertUnwindSafe(runner(pg, worker_name))
            .catch_unwind()
            .await
        {
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
    Box::pin(async move {
        crate::work_item_scheduler::evaluate_work_items(&pg)
            .await
            .map(|_| ())
    })
}

fn run_telegram_reply_poller_tick(
    pg: PgPool,
    _worker_name: String,
) -> BoxFuture<'static, Result<()>> {
    Box::pin(async move {
        crate::telegram_reply_poller::poll_telegram_replies_once(&pg)
            .await
            .map(|_| ())
    })
}

fn run_log_analysis_tick(pg: PgPool, worker_name: String) -> BoxFuture<'static, Result<()>> {
    Box::pin(async move {
        crate::log_analysis_worker::run_log_analysis_tick(&pg, &worker_name)
            .await
            .map(|_| ())
    })
}

fn run_metrics_scraper_tick(pg: PgPool, worker_name: String) -> BoxFuture<'static, Result<()>> {
    Box::pin(async move {
        crate::metrics_scraper::run_metrics_scraper_tick(&pg, &worker_name)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    })
}

/// How often the dispatch-tick watchdog wakes up to check liveness.
pub(crate) const WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum allowed silence from the dispatch-tick scheduler loop before the
/// watchdog considers the daemon stuck and triggers a restart.
pub(crate) const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(300);

pub fn start_tick_scheduler(
    pg: PgPool,
    worker_name: String,
    shutdown_rx: watch::Receiver<bool>,
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

    let start = Instant::now();
    let last_tick_at = Arc::new(AtomicU64::new(start.elapsed().as_secs()));
    let watchdog_last_tick = last_tick_at.clone();
    let watchdog_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(dispatch_tick_watchdog(
        start,
        watchdog_last_tick,
        watchdog_shutdown_rx,
    ));

    tokio::spawn(async move {
        let mut shutdown_rx = shutdown_rx;
        let mut registry = registry;
        let idle_interval = Duration::from_secs(1);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_interval) => {
                    last_tick_at.store(start.elapsed().as_secs(), Ordering::Relaxed);

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

/// Self-watchdog for the dispatch-tick scheduler.
///
/// The scheduler loop bumps a monotonic heartbeat on every idle iteration.
/// If the heartbeat goes stale, the daemon is assumed to be wedged and we
/// trigger an out-of-process restart. Under systemd we ask systemd to restart
/// the canonical unit; otherwise we fall back to `nohup`-re-executing the
/// current binary and then exit.
pub(crate) async fn dispatch_tick_watchdog(
    start: Instant,
    last_tick_at: Arc<AtomicU64>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(WATCHDOG_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let elapsed = start.elapsed().as_secs();
                let last = last_tick_at.load(Ordering::Relaxed);
                if elapsed.saturating_sub(last) > WATCHDOG_TIMEOUT.as_secs() {
                    error!(
                        stale_secs = elapsed.saturating_sub(last),
                        "dispatch tick watchdog: scheduler loop appears stuck; triggering restart"
                    );
                    restart_agent().await;
                    // If restart_agent returns without exiting, the process is in
                    // an unrecoverable state; terminate hard so a wrapper can restart us.
                    std::process::exit(1);
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
    info!("dispatch tick watchdog stopped");
}

/// Restart the agent process.
///
/// On Linux with systemd available, this asks systemd to restart the canonical
/// `forgefleetd.service` unit. On failure, or on non-systemd platforms, it
/// falls back to `nohup`-re-executing the current binary.
pub(crate) async fn restart_agent() {
    if try_systemd_restart().await {
        info!("dispatch tick watchdog: systemd restart triggered; exiting");
        tokio::time::sleep(Duration::from_secs(2)).await;
        std::process::exit(0);
    }

    warn!("dispatch tick watchdog: systemd unavailable or restart failed; falling back to nohup");
    if let Err(err) = try_nohup_restart() {
        error!(error = %err, "dispatch tick watchdog: nohup restart failed");
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    std::process::exit(1);
}

/// Attempt to restart via systemd user units. Returns `true` if the systemctl
/// command reported success. This matches the restart pattern used elsewhere
/// in the fleet (`local_healer`, `revive`, `panic_stop`).
pub(crate) async fn try_systemd_restart() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    // Running under a systemd service manager sets INVOCATION_ID.
    if std::env::var("INVOCATION_ID").is_err() {
        return false;
    }

    let script = "\
        export XDG_RUNTIME_DIR=/run/user/$(id -u); \
        export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; \
        systemctl --user reset-failed forgefleetd.service forgefleet-node.service 2>/dev/null; \
        systemctl --user restart --no-block forgefleetd.service \
            || systemctl --user restart --no-block forgefleet-node.service";

    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) => output.status.success(),
        Err(err) => {
            warn!(error = %err, "dispatch tick watchdog: systemctl invocation failed");
            false
        }
    }
}

/// Fall-back restart: re-execute the current binary with `nohup` so it
/// survives this process exiting, then terminate.
pub(crate) fn try_nohup_restart() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    let _child = std::process::Command::new("nohup")
        .arg(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    Ok(())
}
