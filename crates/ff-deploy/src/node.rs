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
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::DeployConfig;
use crate::daemon::{ActiveLease, RestartReport, restart_with_lease_drain};

/// Release active leases and return their claimed/building work items to `ready`.
///
/// This is used when a deploy forces a daemon restart, so it intentionally does
/// not modify `work_items.attempts`: a deploy is not a failed build attempt.
pub async fn requeue_claimed_items(pool: &PgPool, leases: &[ActiveLease]) -> Result<u64> {
    let lease_ids: Vec<Uuid> = leases
        .iter()
        .map(|lease| Uuid::parse_str(&lease.lease_id))
        .collect::<std::result::Result<_, _>>()
        .context("invalid work-item lease id returned by drain query")?;

    let result = sqlx::query(
        "WITH drained AS (
             UPDATE work_item_leases
                SET lease_state = 'released',
                    released_at = NOW(),
                    release_reason = 'deploy restart drain'
              WHERE id = ANY($1)
                AND released_at IS NULL
          RETURNING work_item_id, sub_agent_id
         ), freed_slots AS (
             UPDATE sub_agents AS sa
                SET current_work_item_id = NULL,
                    status = 'idle',
                    started_at = NULL,
                    last_heartbeat_at = NOW()
              WHERE EXISTS (
                    SELECT 1 FROM drained d
                     WHERE d.sub_agent_id = sa.id
                       AND d.work_item_id = sa.current_work_item_id)
         ), retired_worktrees AS (
             UPDATE work_item_worktrees AS wt
                SET status = 'failed'
              WHERE wt.status IN ('creating', 'active')
                AND EXISTS (
                    SELECT 1 FROM drained d WHERE d.work_item_id = wt.work_item_id)
         )
         UPDATE work_items AS wi
            SET status = 'ready',
                assigned_computer = NULL
          WHERE wi.status IN ('claimed', 'building')
            AND EXISTS (
                SELECT 1 FROM drained d WHERE d.work_item_id = wi.id)",
    )
    .bind(&lease_ids)
    .execute(pool)
    .await
    .context("failed to requeue work items after lease drain timeout")?;

    Ok(result.rows_affected())
}

/// Drain this node's in-flight work-item leases from the canonical Postgres store.
///
/// Active leases are allowed to finish until `config.drain_timeout`. Any leases
/// still held at the deadline are released and their work items are atomically
/// returned to `ready` without incrementing `work_items.attempts`.
pub async fn drain_active_work_item_leases(
    pool: &PgPool,
    computer_id: Uuid,
    config: &DeployConfig,
) -> Result<RestartReport> {
    restart_with_lease_drain(
        config,
        || async {
            let rows = sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT l.id, l.work_item_id
                   FROM work_item_leases l
                   JOIN work_items wi ON wi.id = l.work_item_id
                  WHERE l.computer_id = $1
                    AND l.released_at IS NULL
                    AND wi.status IN ('claimed', 'building')",
            )
            .bind(computer_id)
            .fetch_all(pool)
            .await
            .context("failed to load in-flight work-item leases")?;

            Ok(rows
                .into_iter()
                .map(|(lease_id, work_item_id)| ActiveLease {
                    lease_id: lease_id.to_string(),
                    work_item_ids: vec![work_item_id.to_string()],
                })
                .collect())
        },
        |leases| async move {
            requeue_claimed_items(pool, &leases).await?;
            Ok(())
        },
    )
    .await
}

/// Drain this node's real work-item leases, then restart local `forgefleetd`.
pub async fn restart_forgefleetd_local_with_drain(
    pool: &PgPool,
    computer_id: Uuid,
    config: &DeployConfig,
) -> Result<RestartReport> {
    let report = drain_active_work_item_leases(pool, computer_id, config)
        .await
        .context("lease drain failed; forgefleetd restart not dispatched")?;
    restart_forgefleetd_local().await?;
    Ok(report)
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
    // Drain active leases first — `restart_with_lease_drain` waits for
    // in-flight items to complete, then requeues anything still claimed at
    // the timeout deadline.  The requeue path sets status='ready' without
    // touching `work_items.attempts`, so retry counters are preserved.
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

    // Only dispatch the actual daemon restart after the drain phase
    // completes (success or timeout), never while leases are in an
    // unknown state.
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
mod tests;
