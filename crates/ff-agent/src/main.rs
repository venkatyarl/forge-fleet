mod activity;
mod config;
mod executor;
mod http;
mod leader;
mod state;

use crate::{
    activity::{decide_activity_level, should_yield_resources},
    config::AgentConfig,
    executor::{run_task_executor, run_task_poller},
    http::{AppContext, build_router},
    leader::LeaderClient,
    state::{AgentState, SharedState},
};
use ff_discovery::{collect_health_snapshot, detect_hardware_profile, read_activity_signals};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{RwLock, mpsc};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let mut config = AgentConfig::from_env();
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

    let http_ctx = AppContext {
        state: shared_state.clone(),
        task_tx: task_tx.clone(),
    };

    tokio::spawn(run_http_server(config.http_port, http_ctx));

    tokio::spawn(run_health_reporter(
        shared_state.clone(),
        leader.clone(),
        config.heartbeat_interval_secs,
    ));

    tokio::spawn(run_activity_monitor(
        shared_state.clone(),
        config.activity_override,
        config.activity_poll_interval_secs,
    ));

    tokio::spawn(run_task_poller(
        task_tx,
        leader.clone(),
        config.task_poll_interval_secs,
    ));

    tokio::spawn(run_task_executor(
        shared_state,
        task_rx,
        leader,
        config.runtime_url,
    ));

    info!("ff-agent is running. press Ctrl+C to stop");
    tokio::signal::ctrl_c().await?;
    info!("ff-agent shutdown signal received");

    Ok(())
}

async fn run_http_server(port: u16, ctx: AppContext) {
    let app = build_router(ctx);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            info!(%addr, "status endpoint listening");
            if let Err(err) = axum::serve(listener, app).await {
                error!(error = %err, "http server stopped");
            }
        }
        Err(err) => error!(error = %err, %addr, "failed to bind http status endpoint"),
    }
}

async fn run_health_reporter(state: SharedState, leader: LeaderClient, interval_secs: u64) {
    let interval = Duration::from_secs(interval_secs.max(5));

    loop {
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

        tokio::time::sleep(interval).await;
    }
}

async fn run_activity_monitor(
    state: SharedState,
    override_level: Option<ff_core::ActivityLevel>,
    interval_secs: u64,
) {
    let interval = Duration::from_secs(interval_secs.max(2));

    loop {
        let signals = read_activity_signals();
        let level = decide_activity_level(&signals, override_level);
        let yield_resources = should_yield_resources(level);

        {
            let mut locked = state.write().await;
            locked.activity_level = level;
            locked.yield_resources = yield_resources;
        }

        tokio::time::sleep(interval).await;
    }
}
