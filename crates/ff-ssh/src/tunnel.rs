use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::SshNodeConfig;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelDirection {
    LocalToRemote,
    RemoteToLocal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelSpec {
    pub node: SshNodeConfig,
    pub direction: TunnelDirection,
    pub local_host: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    #[serde(default)]
    pub retry_delay_secs: u64,
}

impl TunnelSpec {
    pub fn local_to_remote(
        node: SshNodeConfig,
        local_host: impl Into<String>,
        local_port: u16,
        remote_host: impl Into<String>,
        remote_port: u16,
    ) -> Self {
        Self {
            node,
            direction: TunnelDirection::LocalToRemote,
            local_host: local_host.into(),
            local_port,
            remote_host: remote_host.into(),
            remote_port,
            retry_delay_secs: 3,
        }
    }

    pub fn remote_to_local(
        node: SshNodeConfig,
        local_host: impl Into<String>,
        local_port: u16,
        remote_host: impl Into<String>,
        remote_port: u16,
    ) -> Self {
        Self {
            node,
            direction: TunnelDirection::RemoteToLocal,
            local_host: local_host.into(),
            local_port,
            remote_host: remote_host.into(),
            remote_port,
            retry_delay_secs: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelHandle {
    pub id: Uuid,
    pub spec: TunnelSpec,
    pub auto_reconnect: bool,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug)]
struct ManagedTunnel {
    handle: TunnelHandle,
    stop_flag: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("tunnel not found: {0}")]
    NotFound(Uuid),

    #[error("password auth requested but `sshpass` is missing")]
    MissingSshPass,

    #[error("failed to spawn tunnel process: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Manages SSH port-forward tunnels and optional auto-reconnect loops.
#[derive(Debug, Default, Clone)]
pub struct TunnelManager {
    tunnels: Arc<Mutex<HashMap<Uuid, ManagedTunnel>>>,
}

impl TunnelManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn start_tunnel(
        &self,
        spec: TunnelSpec,
        auto_reconnect: bool,
    ) -> Result<TunnelHandle, TunnelError> {
        let handle = TunnelHandle {
            id: Uuid::new_v4(),
            spec: spec.clone(),
            auto_reconnect,
            started_at: Utc::now(),
        };

        let stop_flag = Arc::new(AtomicBool::new(false));
        let worker_stop_flag = stop_flag.clone();
        let worker_spec = spec.clone();

        let join = tokio::spawn(async move {
            run_tunnel_loop(worker_spec, auto_reconnect, worker_stop_flag).await;
        });

        let managed = ManagedTunnel {
            handle: handle.clone(),
            stop_flag,
            join,
        };

        self.tunnels.lock().await.insert(handle.id, managed);

        Ok(handle)
    }

    pub async fn stop_tunnel(&self, id: Uuid) -> Result<(), TunnelError> {
        let managed = self
            .tunnels
            .lock()
            .await
            .remove(&id)
            .ok_or(TunnelError::NotFound(id))?;

        managed.stop_flag.store(true, Ordering::SeqCst);
        let _ = managed.join.await;
        Ok(())
    }

    pub async fn list_tunnels(&self) -> Vec<TunnelHandle> {
        self.tunnels
            .lock()
            .await
            .values()
            .map(|t| t.handle.clone())
            .collect()
    }

    pub async fn stop_all(&self) {
        let ids: Vec<Uuid> = self.tunnels.lock().await.keys().copied().collect();
        for id in ids {
            let _ = self.stop_tunnel(id).await;
        }
    }
}

async fn run_tunnel_loop(spec: TunnelSpec, auto_reconnect: bool, stop_flag: Arc<AtomicBool>) {
    let retry_delay = Duration::from_secs(spec.retry_delay_secs.max(1));

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        let once_result = tokio::task::spawn_blocking({
            let spec = spec.clone();
            let stop_flag = stop_flag.clone();
            move || run_tunnel_once(&spec, &stop_flag)
        })
        .await;

        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        let should_reconnect =
            auto_reconnect && matches!(once_result, Ok(Ok(_)) | Ok(Err(_)) | Err(_));

        if !should_reconnect {
            break;
        }

        tokio::time::sleep(retry_delay).await;
    }
}

fn run_tunnel_once(spec: &TunnelSpec, stop_flag: &AtomicBool) -> Result<(), TunnelError> {
    let mut cmd = build_tunnel_command(spec)?;
    let mut child = cmd.spawn()?;

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }

        if child.try_wait()?.is_some() {
            return Ok(());
        }

        std::thread::sleep(Duration::from_millis(250));
    }
}

fn build_tunnel_command(spec: &TunnelSpec) -> Result<Command, TunnelError> {
    let mut cmd = if spec.node.password.is_some() {
        if !command_exists("sshpass") {
            return Err(TunnelError::MissingSshPass);
        }

        let mut c = Command::new("sshpass");
        c.arg("-p")
            .arg(spec.node.password.as_deref().unwrap_or_default())
            .arg("ssh");
        c
    } else {
        Command::new("ssh")
    };

    cmd.arg("-N");
    cmd.arg("-p").arg(spec.node.port.to_string());
    cmd.arg("-o").arg("ExitOnForwardFailure=yes");
    cmd.arg("-o")
        .arg(format!("BatchMode={}", yes_no(spec.node.batch_mode)));

    if let Some(timeout) = spec.node.connect_timeout_secs {
        cmd.arg("-o").arg(format!("ConnectTimeout={timeout}"));
    }

    if let Some(key_path) = &spec.node.key_path {
        cmd.arg("-i").arg(key_path);
    }

    if let Some(known_hosts) = &spec.node.known_hosts_path {
        cmd.arg("-o")
            .arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    }

    let forward = match spec.direction {
        TunnelDirection::LocalToRemote => format!(
            "{}:{}:{}:{}",
            spec.local_host, spec.local_port, spec.remote_host, spec.remote_port
        ),
        TunnelDirection::RemoteToLocal => format!(
            "{}:{}:{}:{}",
            spec.remote_host, spec.remote_port, spec.local_host, spec.local_port
        ),
    };

    match spec.direction {
        TunnelDirection::LocalToRemote => {
            cmd.arg("-L").arg(forward);
        }
        TunnelDirection::RemoteToLocal => {
            cmd.arg("-R").arg(forward);
        }
    }

    cmd.arg(format!("{}@{}", spec.node.username, spec.node.host));

    Ok(cmd)
}

fn command_exists(binary: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {binary} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn yes_no(enabled: bool) -> &'static str {
    if enabled { "yes" } else { "no" }
}
