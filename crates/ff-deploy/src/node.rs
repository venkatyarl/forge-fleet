//! Node-level `forgefleetd` restart with a drain timeout.
//!
//! Restarting the daemon while sub-agents hold active work-item leases would
//! orphan in-flight work, and letting stale-lease recovery reclaim it later
//! would burn a retry attempt for an event that is not a real failure. This
//! module gates the restart behind the attempt-neutral drain loop in
//! [`crate::daemon`]: wait up to [`DeployConfig::drain_timeout`] for active
//! leases to release, requeue whatever is still claimed without bumping
//! attempt counters, and only then dispatch the actual daemon restart.

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::DeployConfig;
use crate::daemon::{ActiveLease, RestartReport, restart_with_lease_drain};

/// Drain active leases, then restart `forgefleetd`.
///
/// The drain phase reuses [`restart_with_lease_drain`]: it polls
/// `active_leases` until every lease releases or `config.drain_timeout`
/// elapses, at which point `requeue_items` requeues the remaining claimed
/// items without incrementing their retry attempt counters. Only after the
/// drain phase completes is `restart_daemon` invoked — if the drain itself
/// errors (lease query or requeue failure) the restart is NOT dispatched, so
/// a daemon is never bounced while leases are in an unknown state.
///
/// Returns the drain [`RestartReport`] so callers can log whether the node
/// drained cleanly or timed out and requeued.
pub async fn restart_forgefleetd_with_drain<F, Fut, G, GFut, R, RFut>(
    config: &DeployConfig,
    active_leases: F,
    requeue_items: G,
    restart_daemon: R,
) -> Result<RestartReport>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<ActiveLease>>>,
    G: Fn(Vec<ActiveLease>) -> GFut,
    GFut: std::future::Future<Output = Result<()>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<()>>,
{
    let report = restart_with_lease_drain(config, active_leases, requeue_items)
        .await
        .context("lease drain failed; forgefleetd restart not dispatched")?;

    if report.drained {
        info!("node restart: all leases drained; restarting forgefleetd");
    } else {
        warn!(
            requeued = report.requeued_item_ids.len(),
            "node restart: drain timeout hit; leased items requeued attempt-neutrally; restarting forgefleetd"
        );
    }

    restart_daemon()
        .await
        .context("failed to dispatch forgefleetd restart after drain")?;

    Ok(report)
}

/// Build the local shell command that restarts `forgefleetd` on this node.
///
/// Mirrors the OS-aware restart bodies in `ff-agent` (`task_runner`,
/// `upgrade_playbooks`): launchd `kickstart -k` on macOS, systemd `--user`
/// on Linux. The Linux path uses `setsid` + `--no-block` with detached stdio
/// because the daemon may be restarting itself — a blocking `systemctl
/// restart` from inside the unit never returns.
pub fn forgefleetd_restart_command(os_family: &str) -> String {
    if os_family.starts_with("macos") {
        "USER_ID=$(id -u); \
         launchctl kickstart -k \"gui/${USER_ID}/com.forgefleet.forgefleetd\" 2>/dev/null \
         || launchctl kickstart -k \"user/${USER_ID}/com.forgefleet.forgefleetd\" 2>/dev/null \
         || launchctl kickstart -k \"system/com.forgefleet.forgefleetd\""
            .to_string()
    } else {
        "export XDG_RUNTIME_DIR=\"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}\"; \
         setsid systemctl --user restart --no-block forgefleetd.service </dev/null >/dev/null 2>&1"
            .to_string()
    }
}

/// Restart `forgefleetd` on the local node using the OS-appropriate command.
pub async fn restart_forgefleetd_local() -> Result<()> {
    let cmd = forgefleetd_restart_command(std::env::consts::OS);

    let status = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .status()
        .await
        .context("failed to spawn forgefleetd restart command")?;

    if !status.success() {
        anyhow::bail!("forgefleetd restart command exited with {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn config(drain_timeout: Duration) -> DeployConfig {
        DeployConfig { drain_timeout }
    }

    #[tokio::test]
    async fn restarts_after_clean_drain() {
        let restarts = Arc::new(AtomicUsize::new(0));

        let r = restarts.clone();
        let report = restart_forgefleetd_with_drain(
            &config(Duration::from_secs(1)),
            || async { Ok::<_, anyhow::Error>(vec![]) },
            |_leases| async { Ok(()) },
            move || async move {
                r.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert!(report.drained);
        assert!(report.requeued_item_ids.is_empty());
        assert_eq!(restarts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn requeues_then_restarts_when_drain_times_out() {
        let restarts = Arc::new(AtomicUsize::new(0));
        let requeues = Arc::new(AtomicUsize::new(0));

        let rs = restarts.clone();
        let rq = requeues.clone();
        let report = restart_forgefleetd_with_drain(
            &config(Duration::from_millis(50)),
            || async {
                Ok(vec![ActiveLease {
                    lease_id: "slot-1".into(),
                    work_item_ids: vec!["wi-1".into()],
                }])
            },
            move |_leases| {
                let rq = rq.clone();
                async move {
                    rq.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            move || async move {
                rs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert!(!report.drained);
        assert_eq!(report.requeued_item_ids, &["wi-1"]);
        assert_eq!(requeues.load(Ordering::SeqCst), 1);
        assert_eq!(restarts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_restart_when_drain_errors() {
        let restarts = Arc::new(AtomicUsize::new(0));

        let r = restarts.clone();
        let result = restart_forgefleetd_with_drain(
            &config(Duration::from_secs(1)),
            || async { Err::<Vec<ActiveLease>, _>(anyhow::anyhow!("lease query down")) },
            |_leases| async { Ok(()) },
            move || async move {
                r.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(restarts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn restart_command_uses_launchctl_on_macos() {
        let cmd = forgefleetd_restart_command("macos");
        assert!(cmd.contains("launchctl kickstart -k"));
        assert!(cmd.contains("com.forgefleet.forgefleetd"));
    }

    #[test]
    fn restart_command_uses_detached_nonblocking_systemctl_on_linux() {
        let cmd = forgefleetd_restart_command("linux");
        assert!(cmd.contains("systemctl --user restart --no-block forgefleetd.service"));
        assert!(cmd.contains("setsid"));
        assert!(cmd.contains("XDG_RUNTIME_DIR"));
    }
}
