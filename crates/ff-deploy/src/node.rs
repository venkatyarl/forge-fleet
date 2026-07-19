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

const REQUEUE_CLAIMED_ITEMS_SQL: &str = r#"
WITH released AS (
    UPDATE work_item_leases
       SET lease_state = 'released',
           released_at = NOW(),
           release_reason = 'deploy restart drain'
     WHERE work_item_id = ANY($1)
       AND released_at IS NULL
 RETURNING work_item_id, sub_agent_id
), freed_slots AS (
    UPDATE sub_agents AS sa
       SET current_work_item_id = NULL,
           status = 'idle',
           started_at = NULL,
           last_heartbeat_at = NOW()
     WHERE EXISTS (
           SELECT 1
             FROM released AS r
            WHERE r.sub_agent_id = sa.id
              AND r.work_item_id = sa.current_work_item_id)
), retired_worktrees AS (
    UPDATE work_item_worktrees AS wt
       SET status = 'failed'
     WHERE wt.status IN ('creating', 'active')
       AND EXISTS (
           SELECT 1 FROM released AS r WHERE r.work_item_id = wt.work_item_id)
), requeued AS (
    UPDATE work_items AS wi
       SET status = 'ready',
           assigned_computer = NULL
     WHERE wi.status IN ('claimed', 'building')
       AND EXISTS (
           SELECT 1 FROM released AS r WHERE r.work_item_id = wi.id)
 RETURNING wi.id
)
SELECT COUNT(*) FROM requeued
"#;

/// Requeue work held by active leases because a deploy is restarting the node.
///
/// This is intentionally attempt-neutral: unlike stale-lease recovery, the
/// query does not modify either the work item's `attempts` counter or the
/// lease's `attempt` counter. Lease release, slot cleanup, worktree retirement,
/// and requeue are performed in one statement so no item can be reclaimed
/// between those transitions.
pub async fn requeue_claimed_items(pool: &sqlx::PgPool, leases: Vec<ActiveLease>) -> Result<u64> {
    let work_item_ids = leases
        .into_iter()
        .flat_map(|lease| lease.work_item_ids)
        .map(|id| {
            id.parse::<uuid::Uuid>()
                .with_context(|| format!("invalid work item id in active lease: {id}"))
        })
        .collect::<Result<Vec<_>>>()?;

    if work_item_ids.is_empty() {
        return Ok(0);
    }

    let requeued: i64 = sqlx::query_scalar(REQUEUE_CLAIMED_ITEMS_SQL)
        .bind(&work_item_ids)
        .fetch_one(pool)
        .await
        .context("failed to requeue claimed items for deploy restart")?;

    Ok(requeued as u64)
}

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

    #[test]
    fn deploy_requeue_sql_is_attempt_neutral() {
        assert!(REQUEUE_CLAIMED_ITEMS_SQL.contains("wi.status IN ('claimed', 'building')"));
        assert!(REQUEUE_CLAIMED_ITEMS_SQL.contains("SET status = 'ready'"));
        assert!(!REQUEUE_CLAIMED_ITEMS_SQL.contains("attempts ="));
        assert!(!REQUEUE_CLAIMED_ITEMS_SQL.contains("attempt ="));
    }
}
