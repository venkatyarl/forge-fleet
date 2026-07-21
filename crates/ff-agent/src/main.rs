mod activity;
mod executor;
mod http;
mod leader;
mod state;

use crate::{
    activity::{decide_activity_level, should_yield_resources},
    executor::{run_task_executor, run_task_poller},
    http::{AppContext, build_router},
    leader::LeaderClient,
    state::{AgentState, SharedState},
};
use ff_agent::config::AgentConfig;
use ff_discovery::{collect_health_snapshot, detect_hardware_profile, read_activity_signals};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let mut config = AgentConfig::from_env();
    if let Some(mem_budget_mb) = config.slm_mem_budget_mb {
        ff_agent::slm::validate_memory_budget_mb(mem_budget_mb).map_err(anyhow::Error::msg)?;
    }
    info!(node_id = %config.node_id, "starting ff-agent daemon");

    let hardware = detect_hardware_profile();
    let shared_state: SharedState = Arc::new(RwLock::new(AgentState::new(
        config.node_id.clone(),
        hardware.clone(),
    )));

    let leader = LeaderClient::new(config.leader_url.clone(), config.node_id.clone());
    let registration = leader.register(&hardware).await;

    {
        let mut locked = shared_state.write().await;
        locked.role = registration.role;
    }

    if registration.heartbeat_interval_secs > 0 {
        config.heartbeat_interval_secs = registration.heartbeat_interval_secs;
    }

    info!(
        role = ?registration.role,
        heartbeat_secs = config.heartbeat_interval_secs,
        "registration complete"
    );

    let (task_tx, task_rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();

    let http_ctx = AppContext {
        state: shared_state.clone(),
        task_tx: task_tx.clone(),
    };

    let http_cancel = cancel.clone();

    let http_handle = tokio::spawn(run_http_server(config.http_port, http_ctx, http_cancel));

    let registry_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let registry_handle = tokio::spawn(run_tool_registry_reporter(
        config.node_id.clone(),
        config.leader_url.clone(),
        cancel.clone(),
        registry_client,
    ));

    let health_handle = tokio::spawn(run_health_reporter(
        shared_state.clone(),
        leader.clone(),
        config.heartbeat_interval_secs,
        cancel.clone(),
    ));

    let activity_handle = tokio::spawn(run_activity_monitor(
        shared_state.clone(),
        config.activity_override,
        config.activity_poll_interval_secs,
        cancel.clone(),
    ));

    let poller_handle = tokio::spawn(run_task_poller(
        task_tx,
        leader.clone(),
        config.task_poll_interval_secs,
    ));

    let build_monitor_handle = tokio::spawn(run_build_timeout_monitor(
        shared_state.clone(),
        config.max_build_duration_secs,
        config.build_monitor_poll_secs,
        cancel.clone(),
    ));

    let executor_handle = tokio::spawn(run_task_executor(
        shared_state,
        task_rx,
        leader,
        config.runtime_url,
    ));

    info!("ff-agent is running. press Ctrl+C to stop");
    tokio::signal::ctrl_c().await?;
    info!("ff-agent shutdown signal received");

    // Signal graceful shutdown to cancellable tasks.
    cancel.cancel();

    // Abort the rest and wait up to 5s for cleanup.
    let timeout = Duration::from_secs(5);
    let _ = tokio::time::timeout(timeout, http_handle).await;
    registry_handle.abort();
    health_handle.abort();
    activity_handle.abort();
    poller_handle.abort();
    build_monitor_handle.abort();
    executor_handle.abort();

    Ok(())
}

async fn run_http_server(port: u16, ctx: AppContext, cancel: CancellationToken) {
    let app = build_router(ctx);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            info!(%addr, "status endpoint listening");
            let serve = axum::serve(listener, app);
            if let Err(err) = serve
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
            {
                error!(error = %err, "http server stopped");
            }
        }
        Err(err) => error!(error = %err, %addr, "failed to bind http status endpoint"),
    }
}

async fn run_health_reporter(
    state: SharedState,
    leader: LeaderClient,
    interval_secs: u64,
    cancel: CancellationToken,
) {
    let interval = Duration::from_secs(interval_secs.max(5));

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = cancel.cancelled() => {
                info!("health reporter shutting down");
                break;
            }
        }

        let (active_tasks, running_models, role, activity_level) = {
            let locked = state.read().await;
            (
                locked.active_tasks.len(),
                locked.running_models.clone(),
                locked.role,
                locked.activity_level,
            )
        };

        let health = collect_health_snapshot(active_tasks, running_models);

        {
            let mut locked = state.write().await;
            locked.last_health = Some(health.clone());
        }

        if let Err(err) = leader.send_heartbeat(role, activity_level, &health).await {
            warn!(error = %err, "heartbeat report failed");
        }
    }
}

async fn run_tool_registry_reporter(
    node_id: String,
    leader_url: String,
    cancel: CancellationToken,
    client: reqwest::Client,
) {
    // Wait a bit for the gateway to be fully up
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        _ = cancel.cancelled() => return,
    }

    let gateway = if leader_url.contains(':') {
        // Convert leader URL (e.g., http://192.168.5.100:50001) to gateway URL
        leader_url
            .replace(":50001", ":51002")
            .replace(":50000", ":51002")
    } else {
        "http://192.168.5.100:51002".to_string()
    };

    // Build tool registration payload from actual tool implementations
    let tools: Vec<serde_json::Value> = ff_agent::tools::all_tools_arc()
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name(),
                "description": tool.description(),
                "parameters_schema": tool.parameters_schema(),
                "capabilities_required": [],
            })
        })
        .collect();

    let register_body = serde_json::json!({
        "worker_name": node_id,
        "tools": tools,
    });

    let register_url = format!("{gateway}/api/tools/register");
    match client.post(&register_url).json(&register_body).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                info!(count = tools.len(), "fleet tools registered");
            } else {
                warn!(status = %resp.status(), "fleet tool registration returned non-success");
            }
        }
        Err(e) => {
            warn!(error = %e, "fleet tool registration failed");
        }
    }

    // Periodic heartbeat to keep tools healthy
    let heartbeat_interval = Duration::from_secs(60);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(heartbeat_interval) => {}
            _ = cancel.cancelled() => {
                info!("tool registry reporter shutting down");
                break;
            }
        }

        let heartbeat_body = serde_json::json!({"worker_name": node_id});
        let heartbeat_url = format!("{gateway}/api/tools/heartbeat");
        match client
            .post(&heartbeat_url)
            .json(&heartbeat_body)
            .send()
            .await
        {
            Ok(resp) => {
                if !resp.status().is_success() {
                    warn!(status = %resp.status(), "fleet tool heartbeat returned non-success");
                }
            }
            Err(e) => {
                warn!(error = %e, "fleet tool heartbeat failed");
            }
        }
    }
}

async fn run_activity_monitor(
    state: SharedState,
    override_level: Option<ff_core::ActivityLevel>,
    interval_secs: u64,
    cancel: CancellationToken,
) {
    let interval = Duration::from_secs(interval_secs.max(2));

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = cancel.cancelled() => {
                info!("activity monitor shutting down");
                break;
            }
        }

        let signals = read_activity_signals();
        let level = decide_activity_level(&signals, override_level);
        let yield_resources = should_yield_resources(level);

        {
            let mut locked = state.write().await;
            locked.activity_level = level;
            locked.yield_resources = yield_resources;
        }
    }
}

/// Background watchdog that kills builds exceeding `max_build_duration`.
///
/// The executor registers a `BuildWatch` (start time + cancellation token) for
/// every build shell-command it runs. This task periodically scans those
/// watches and, for any build whose elapsed wall-clock has passed
/// `max_build_duration`, fires its cancellation token — the executor is
/// `select!`ing on that token and drops the child, killing the stuck build.
///
/// A `max_build_duration_secs` of `0` disables the monitor entirely.
async fn run_build_timeout_monitor(
    state: SharedState,
    max_build_duration_secs: u64,
    poll_interval_secs: u64,
    cancel: CancellationToken,
) {
    if max_build_duration_secs == 0 {
        info!("build timeout monitor disabled (max_build_duration = 0)");
        return;
    }

    let max = Duration::from_secs(max_build_duration_secs);
    let interval = Duration::from_secs(poll_interval_secs.max(1));

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = cancel.cancelled() => {
                info!("build timeout monitor shutting down");
                break;
            }
        }

        // Collect stuck builds under a read lock, then cancel outside it.
        let stuck: Vec<_> = {
            let locked = state.read().await;
            locked
                .build_watches
                .iter()
                .filter(|(_, watch)| watch.started_at.elapsed() >= max)
                .map(|(id, watch)| (*id, watch.cancel.clone()))
                .collect()
        };

        for (task_id, token) in stuck {
            warn!(
                %task_id,
                max_secs = max_build_duration_secs,
                "build exceeded max_build_duration; killing"
            );
            token.cancel();
        }
    }
}
