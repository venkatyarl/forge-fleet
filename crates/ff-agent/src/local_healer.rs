//! Local-host self-heal. Runs inside `ff daemon` and ensures the
//! companion forgefleetd on the SAME host is alive. If missing,
//! restart via launchctl (macOS) or systemctl --user (Linux).
//!
//! Rationale: the cross-computer revive_scan() explicitly skips self —
//! when forgefleetd dies but ff daemon keeps running, peers see the
//! leader heartbeat still fresh and never challenge. Local healer is
//! the only layer that catches this.
//!
//! Strategy per tick:
//!   1. Check `pgrep -f "forgefleetd --node-name {my_name}"` for a live pid.
//!   2. Independently verify that 127.0.0.1:51002 accepts TCP with a 2s timeout.
//!   3. If either check fails, attempt a best-effort restart via the
//!      platform-native user service manager. We do NOT block on the
//!      restart taking effect — the next tick re-evaluates.

use std::process::Stdio;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const GATEWAY_PORT: u16 = 51002;
const PORT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const TICK_INTERVAL: Duration = Duration::from_secs(30);
const STARTUP_GRACE: Duration = Duration::from_secs(60);

/// Per-host self-healer for the companion forgefleetd process.
pub struct LocalHealer {
    pub my_name: String,
}

impl LocalHealer {
    pub fn new(my_name: String) -> Self {
        Self { my_name }
    }

    /// One tick. Never returns Err — healer is best-effort, and a panic
    /// here would silently take down the daemon's other subsystems.
    pub async fn run_once(&self) -> anyhow::Result<()> {
        let proc_alive = self.forgefleetd_running();
        let port_alive = self.gateway_port_listening().await;

        if proc_alive && port_alive {
            tracing::info!(
                node = %self.my_name,
                "local_healer: forgefleetd healthy (pid+port)"
            );
            return Ok(());
        }

        tracing::warn!(
            node = %self.my_name,
            proc_alive,
            port_alive,
            "local_healer: forgefleetd unhealthy — attempting restart"
        );

        match self.attempt_restart().await {
            Ok(summary) => tracing::warn!(
                node = %self.my_name,
                summary = %summary,
                "local_healer: restart command dispatched"
            ),
            Err(err) => tracing::warn!(
                node = %self.my_name,
                error = %err,
                "local_healer: restart command failed"
            ),
        }

        Ok(())
    }

    /// Synchronous pgrep — cheap and avoids pulling in a tokio::process
    /// dependency just for a pid lookup.
    fn forgefleetd_running(&self) -> bool {
        let pattern = format!("forgefleetd --node-name {}", self.my_name);
        match std::process::Command::new("pgrep")
            .arg("-f")
            .arg(&pattern)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) => status.success(),
            Err(_) => false,
        }
    }

    /// Can we open a TCP connection to 127.0.0.1:51002 within the probe
    /// timeout? We don't speak any protocol — a completed handshake is
    /// sufficient proof that the gateway bound its listener.
    async fn gateway_port_listening(&self) -> bool {
        let target = format!("127.0.0.1:{GATEWAY_PORT}");
        match tokio::time::timeout(PORT_PROBE_TIMEOUT, TcpStream::connect(&target)).await {
            Ok(Ok(_stream)) => true,
            _ => false,
        }
    }

    /// Platform-specific restart. macOS uses launchctl kickstart so the
    /// label is re-launched even if launchd thinks it's already running;
    /// Linux uses systemctl --user because forgefleetd is installed as a
    /// per-user service.
    async fn attempt_restart(&self) -> anyhow::Result<String> {
        if cfg!(target_os = "macos") {
            let uid = current_uid().await?;
            let target = format!("gui/{uid}/com.forgefleet.forgefleetd");
            let output = tokio::process::Command::new("launchctl")
                .arg("kickstart")
                .arg("-k")
                .arg(&target)
                .output()
                .await?;
            Ok(format!(
                "launchctl kickstart -k {target} (exit={})",
                output.status.code().unwrap_or(-1)
            ))
        } else {
            let output = tokio::process::Command::new("systemctl")
                .arg("--user")
                .arg("restart")
                .arg("forgefleet-node.service")
                .output()
                .await?;
            Ok(format!(
                "systemctl --user restart forgefleet-node.service (exit={})",
                output.status.code().unwrap_or(-1)
            ))
        }
    }

    /// Spawn the 30s tick. Skips the first 60s after spawn so we don't
    /// race with daemon startup (forgefleetd is often launched in the
    /// same window as `ff daemon`, especially in dev).
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(STARTUP_GRACE) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }

            loop {
                if let Err(err) = self.run_once().await {
                    tracing::warn!(error = %err, "local_healer tick errored");
                }
                tokio::select! {
                    _ = tokio::time::sleep(TICK_INTERVAL) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
            }
        })
    }
}

/// Resolve the current user's numeric uid by shelling out to `id -u`.
/// Avoids a libc dependency for a value we only need when composing the
/// `gui/<uid>/<label>` launchctl target on macOS.
async fn current_uid() -> anyhow::Result<u32> {
    let output = tokio::process::Command::new("id").arg("-u").output().await?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.trim().parse::<u32>()?)
}
