//! Emergency halt/resume + quarantine helpers.
//!
//! These are the "break glass" operator tools. They fan out over SSH to
//! stop or start every daemon on the fleet, or to isolate a single misbehaving
//! computer without removing it from the registry.
//!
//! ## Panic-stop flow
//! 1. Read every row from `computers`.
//! 2. For the local computer, run stop commands inline (no SSH hop — the
//!    SSH server may itself be on the same host we're halting).
//! 3. For every remote, SSH in and run the same stop commands. Remotes are
//!    dispatched concurrently via `tokio::join_all`.
//! 4. Return a `HaltReport` the caller can render.
//!
//! ## Quarantine flow
//! 1. SSH to the named computer and stop both daemon services.
//! 2. `UPDATE computers SET status='maintenance'` so leader election and
//!    LLM routing skip the node on their next scan.
//! 3. Flip `openclaw_installations.mode='node'` and clear `gateway_url`
//!    (if a row exists) so the quarantined computer can't serve gateway
//!    traffic while isolated.
//! 4. Publish `fleet.events.quarantine` on NATS for dashboards/log sinks.
//!
//! `unquarantine` is the symmetric reverse — SSH in, restart the services,
//! bump status back to 'pending'. The next pulse flips it to 'online'.
//!
//! ### Why not reuse `revive.rs`?
//! Revive is specifically about bringing a dead box back. Quarantine stops
//! a live box; panic-stop halts every box. The SSH fan-out primitives look
//! similar but the control flow + DB side effects are different enough that
//! keeping them separate is clearer.

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Timeout applied to every SSH invocation fired by panic-stop / quarantine.
/// Chosen to match `revive::SSH_TIMEOUT` — longer than a healthy round-trip,
/// short enough that a hanging host doesn't stall the whole fan-out.
const SSH_TIMEOUT: Duration = Duration::from_secs(12);

/// One row per computer touched by panic-stop / resume.
#[derive(Debug, Clone)]
pub struct HaltEntry {
    pub name: String,
    pub ok: bool,
    /// Human-readable detail (method used, or the error message).
    pub detail: String,
}

/// Aggregate result of a fleet panic-stop / resume fan-out.
#[derive(Debug, Clone)]
pub struct HaltReport {
    pub entries: Vec<HaltEntry>,
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl HaltReport {
    fn from_entries(entries: Vec<HaltEntry>) -> Self {
        let total = entries.len();
        let succeeded = entries.iter().filter(|e| e.ok).count();
        let failed = total - succeeded;
        Self { entries, total, succeeded, failed }
    }
}

/// Snapshot of the SSH-relevant columns for a computer.
#[derive(Debug, Clone)]
struct RemoteTarget {
    name: String,
    primary_ip: String,
    ssh_user: String,
    ssh_port: i32,
    os_family: String,
}

/// Load every computer. Status filter is intentionally absent — we want
/// panic-stop to hit every known box, even 'maintenance' or 'offline'
/// ones (stopping a dead daemon is a no-op but harmless).
async fn load_all_computers(pg: &PgPool) -> Result<Vec<RemoteTarget>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT name, primary_ip, ssh_user, ssh_port, os_family \
           FROM computers ORDER BY name",
    )
    .fetch_all(pg)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| RemoteTarget {
            name: r.get("name"),
            primary_ip: r.get("primary_ip"),
            ssh_user: r.get("ssh_user"),
            ssh_port: r.get("ssh_port"),
            os_family: r.get("os_family"),
        })
        .collect())
}

fn ssh_base_args(port: i32) -> Vec<String> {
    vec![
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-p".into(),
        port.to_string(),
    ]
}

/// Run an SSH command under a timeout; returns (success, combined_output).
async fn run_ssh_collect(
    user: &str,
    host: &str,
    port: i32,
    remote_cmd: &str,
) -> (bool, String) {
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args(port))
        .arg(format!("{user}@{host}"))
        .arg(remote_cmd);
    cmd.stdin(std::process::Stdio::null());

    match timeout(SSH_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => {
            let combined = String::from_utf8_lossy(&out.stdout).to_string()
                + String::from_utf8_lossy(&out.stderr).as_ref();
            (out.status.success(), combined.trim().to_string())
        }
        Ok(Err(e)) => (false, format!("ssh spawn failed: {e}")),
        Err(_) => (false, format!("ssh timed out after {:?}", SSH_TIMEOUT)),
    }
}

/// Run a local shell command under a timeout; returns (success, combined_output).
async fn run_local_collect(shell_cmd: &str) -> (bool, String) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(shell_cmd);
    cmd.stdin(std::process::Stdio::null());
    match timeout(SSH_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => {
            let combined = String::from_utf8_lossy(&out.stdout).to_string()
                + String::from_utf8_lossy(&out.stderr).as_ref();
            (out.status.success(), combined.trim().to_string())
        }
        Ok(Err(e)) => (false, format!("local spawn failed: {e}")),
        Err(_) => (false, "local command timed out".into()),
    }
}

/// OS-specific stop command. macOS tries every known launchd label in
/// sequence (same set as `revive::ssh_restart_daemon`). Linux stops both
/// user services. Trailing `|| true` makes the whole pipeline succeed even
/// if a unit doesn't exist on that host.
fn build_stop_command(os_family: &str) -> String {
    match os_family {
        "macos" => {
            // Try every historical plist name; launchctl bootout is quieter
            // than unload for services that aren't loaded.
            r#"for label in com.forgefleet.forgefleetd com.forgefleet.node com.forgefleet.ffdaemon; do
                 launchctl bootout gui/$(id -u)/$label 2>/dev/null || true
                 launchctl unload ~/Library/LaunchAgents/${label}.plist 2>/dev/null || true
               done; echo stopped"#
                .to_string()
        }
        _ => {
            // Linux / DGX — user systemd units.
            "systemctl --user stop forgefleet-node.service forgefleet-daemon.service 2>/dev/null; \
             systemctl --user stop forgefleet-node.service 2>/dev/null; \
             echo stopped"
                .to_string()
        }
    }
}

/// OS-specific start command. Mirrors `build_stop_command`.
fn build_start_command(os_family: &str) -> String {
    match os_family {
        "macos" => {
            r#"for label in com.forgefleet.forgefleetd com.forgefleet.node com.forgefleet.ffdaemon; do
                 launchctl load -w ~/Library/LaunchAgents/${label}.plist 2>/dev/null || true
                 launchctl kickstart -k gui/$(id -u)/$label 2>/dev/null || true
               done; echo started"#
                .to_string()
        }
        _ => {
            "systemctl --user start forgefleet-node.service forgefleet-daemon.service 2>/dev/null; \
             systemctl --user start forgefleet-node.service 2>/dev/null; \
             echo started"
                .to_string()
        }
    }
}

/// Stop daemons on every computer in the `computers` table.
///
/// `local_name` identifies the computer running this process — that row
/// is executed inline instead of via SSH (reliable even if sshd itself is
/// flaky). Remotes are dispatched concurrently.
pub async fn fleet_panic_stop(pg: &PgPool, local_name: &str) -> Result<HaltReport, sqlx::Error> {
    let targets = load_all_computers(pg).await?;
    let (local, remote): (Vec<_>, Vec<_>) = targets
        .into_iter()
        .partition(|t| t.name.eq_ignore_ascii_case(local_name));

    let mut entries: Vec<HaltEntry> = Vec::with_capacity(local.len() + remote.len());

    // Run the local stop inline.
    for t in local {
        let cmd = build_stop_command(&t.os_family);
        let (ok, detail) = run_local_collect(&cmd).await;
        debug!(node = %t.name, ok, %detail, "local stop complete");
        entries.push(HaltEntry {
            name: t.name,
            ok,
            detail: if ok { "local stop ok".into() } else { detail },
        });
    }

    // Dispatch remotes in parallel.
    let mut handles = Vec::with_capacity(remote.len());
    for t in remote {
        handles.push(tokio::spawn(async move {
            let cmd = build_stop_command(&t.os_family);
            let (ok, detail) = run_ssh_collect(&t.ssh_user, &t.primary_ip, t.ssh_port, &cmd).await;
            HaltEntry {
                name: t.name,
                ok,
                detail: if ok { "ssh stop ok".into() } else { detail },
            }
        }));
    }

    for h in handles {
        match h.await {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                // Task join failure — should be unreachable but we still
                // record it so the caller can see a discrepancy.
                warn!(error = %e, "panic-stop remote task join failed");
                entries.push(HaltEntry {
                    name: "?".into(),
                    ok: false,
                    detail: format!("task join failed: {e}"),
                });
            }
        }
    }

    Ok(HaltReport::from_entries(entries))
}

/// Start daemons on every computer in the `computers` table.
pub async fn fleet_resume(pg: &PgPool, local_name: &str) -> Result<HaltReport, sqlx::Error> {
    let targets = load_all_computers(pg).await?;
    let (local, remote): (Vec<_>, Vec<_>) = targets
        .into_iter()
        .partition(|t| t.name.eq_ignore_ascii_case(local_name));

    let mut entries: Vec<HaltEntry> = Vec::with_capacity(local.len() + remote.len());

    for t in local {
        let cmd = build_start_command(&t.os_family);
        let (ok, detail) = run_local_collect(&cmd).await;
        entries.push(HaltEntry {
            name: t.name,
            ok,
            detail: if ok { "local start ok".into() } else { detail },
        });
    }

    let mut handles = Vec::with_capacity(remote.len());
    for t in remote {
        handles.push(tokio::spawn(async move {
            let cmd = build_start_command(&t.os_family);
            let (ok, detail) = run_ssh_collect(&t.ssh_user, &t.primary_ip, t.ssh_port, &cmd).await;
            HaltEntry {
                name: t.name,
                ok,
                detail: if ok { "ssh start ok".into() } else { detail },
            }
        }));
    }

    for h in handles {
        match h.await {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                warn!(error = %e, "resume remote task join failed");
                entries.push(HaltEntry {
                    name: "?".into(),
                    ok: false,
                    detail: format!("task join failed: {e}"),
                });
            }
        }
    }

    Ok(HaltReport::from_entries(entries))
}

/// Docker stack halt — used by `--halt-dbs`. Only meaningful on Taylor,
/// where the compose project lives. Silently stops the three data-plane
/// containers; each is no-op-safe if the container doesn't exist.
pub async fn stop_taylor_docker_stack() -> (bool, String) {
    const CONTAINERS: &[&str] = &[
        "forgefleet-postgres",
        "forgefleet-redis",
        // forgefleet-sentinel removed — Pulse P2P replaces Redis Sentinel.
        // Left in a historical comment so readers looking for it find the
        // rationale without chasing git blame.
        "forgefleet-nats",
    ];

    let mut all_ok = true;
    let mut detail = String::new();
    for c in CONTAINERS {
        let (ok, out) = run_local_collect(&format!("docker stop {c} 2>&1 || true")).await;
        if !ok {
            all_ok = false;
        }
        detail.push_str(&format!("{c}: {out}\n"));
    }
    (all_ok, detail.trim_end().to_string())
}

/// Quarantine a computer: SSH stop + DB flip + NATS event.
///
/// Returns a short status string the CLI can render. Does *not* print
/// directly — that's the caller's job.
pub async fn quarantine_computer(
    pg: &PgPool,
    computer: &str,
) -> Result<QuarantineResult, QuarantineError> {
    // 1. Look up the target.
    let row = sqlx::query(
        "SELECT name, primary_ip, ssh_user, ssh_port, os_family \
           FROM computers WHERE LOWER(name) = LOWER($1)",
    )
    .bind(computer)
    .fetch_optional(pg)
    .await?
    .ok_or_else(|| QuarantineError::NotFound(computer.to_string()))?;

    let target = RemoteTarget {
        name: row.get("name"),
        primary_ip: row.get("primary_ip"),
        ssh_user: row.get("ssh_user"),
        ssh_port: row.get("ssh_port"),
        os_family: row.get("os_family"),
    };

    // 2. SSH stop. We record the outcome but don't bail on failure —
    //    even if SSH is down, we still want the DB flip so the rest of
    //    the fleet stops talking to this node.
    let cmd = build_stop_command(&target.os_family);
    let (ssh_ok, ssh_detail) =
        run_ssh_collect(&target.ssh_user, &target.primary_ip, target.ssh_port, &cmd).await;
    if !ssh_ok {
        warn!(
            node = %target.name,
            detail = %ssh_detail,
            "quarantine: ssh stop failed; continuing with DB flip anyway"
        );
    } else {
        info!(node = %target.name, "quarantine: ssh stop succeeded");
    }

    // 3. DB flip. Single transaction so we don't end up with a
    //    maintenance-status computer still serving gateway traffic.
    let mut tx = pg.begin().await?;
    let res = sqlx::query(
        "UPDATE computers \
            SET status = 'maintenance', \
                status_changed_at = NOW(), \
                last_seen_at = NOW() \
          WHERE LOWER(name) = LOWER($1)",
    )
    .bind(&target.name)
    .execute(&mut *tx)
    .await?;

    if res.rows_affected() == 0 {
        return Err(QuarantineError::NotFound(target.name));
    }

    // Only rewrite openclaw row if it exists — computers without an
    // OpenClaw install don't get a phantom row.
    let _oc = sqlx::query(
        "UPDATE openclaw_installations oi \
            SET mode = 'node', gateway_url = NULL \
           FROM computers c \
          WHERE oi.computer_id = c.id AND LOWER(c.name) = LOWER($1)",
    )
    .bind(&target.name)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // 4. NATS event. Best-effort — no-op if NATS unavailable.
    let payload = json!({
        "event": "quarantine",
        "computer": target.name,
        "ssh_stop_ok": ssh_ok,
        "ssh_detail": if ssh_ok { "" } else { ssh_detail.as_str() },
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    crate::nats_client::publish_json("fleet.events.quarantine", &payload).await;

    Ok(QuarantineResult {
        name: target.name,
        ssh_stop_ok: ssh_ok,
        ssh_detail,
    })
}

/// Unquarantine a computer: SSH start + DB flip back to 'pending'.
pub async fn unquarantine_computer(
    pg: &PgPool,
    computer: &str,
) -> Result<QuarantineResult, QuarantineError> {
    let row = sqlx::query(
        "SELECT name, primary_ip, ssh_user, ssh_port, os_family \
           FROM computers WHERE LOWER(name) = LOWER($1)",
    )
    .bind(computer)
    .fetch_optional(pg)
    .await?
    .ok_or_else(|| QuarantineError::NotFound(computer.to_string()))?;

    let target = RemoteTarget {
        name: row.get("name"),
        primary_ip: row.get("primary_ip"),
        ssh_user: row.get("ssh_user"),
        ssh_port: row.get("ssh_port"),
        os_family: row.get("os_family"),
    };

    let cmd = build_start_command(&target.os_family);
    let (ssh_ok, ssh_detail) =
        run_ssh_collect(&target.ssh_user, &target.primary_ip, target.ssh_port, &cmd).await;

    let res = sqlx::query(
        "UPDATE computers \
            SET status = 'pending', \
                status_changed_at = NOW() \
          WHERE LOWER(name) = LOWER($1)",
    )
    .bind(&target.name)
    .execute(pg)
    .await?;
    if res.rows_affected() == 0 {
        return Err(QuarantineError::NotFound(target.name));
    }

    let payload = json!({
        "event": "unquarantine",
        "computer": target.name,
        "ssh_start_ok": ssh_ok,
        "ssh_detail": if ssh_ok { "" } else { ssh_detail.as_str() },
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    crate::nats_client::publish_json("fleet.events.quarantine", &payload).await;

    Ok(QuarantineResult {
        name: target.name,
        ssh_stop_ok: ssh_ok,
        ssh_detail,
    })
}

/// Outcome of a quarantine / unquarantine invocation.
#[derive(Debug, Clone)]
pub struct QuarantineResult {
    pub name: String,
    /// True if the SSH stop (quarantine) / start (unquarantine) command
    /// exited 0. False means we flipped the DB anyway but the target
    /// still needs a manual check.
    pub ssh_stop_ok: bool,
    pub ssh_detail: String,
}

/// Errors specific to quarantine / unquarantine operations.
#[derive(Debug, thiserror::Error)]
pub enum QuarantineError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("no computer named '{0}' in the fleet")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halt_report_counts() {
        let r = HaltReport::from_entries(vec![
            HaltEntry { name: "a".into(), ok: true, detail: "ok".into() },
            HaltEntry { name: "b".into(), ok: false, detail: "boom".into() },
            HaltEntry { name: "c".into(), ok: true, detail: "ok".into() },
        ]);
        assert_eq!(r.total, 3);
        assert_eq!(r.succeeded, 2);
        assert_eq!(r.failed, 1);
    }

    #[test]
    fn stop_cmd_macos_mentions_all_labels() {
        let s = build_stop_command("macos");
        assert!(s.contains("com.forgefleet.forgefleetd"));
        assert!(s.contains("com.forgefleet.node"));
        assert!(s.contains("com.forgefleet.ffdaemon"));
    }

    #[test]
    fn stop_cmd_linux_uses_systemctl_user() {
        let s = build_stop_command("linux-ubuntu");
        assert!(s.contains("systemctl --user stop"));
        assert!(s.contains("forgefleet-node.service"));
    }

    #[test]
    fn start_cmd_matches_stop_cmd_labels() {
        let start = build_start_command("macos");
        let stop = build_stop_command("macos");
        // both must reference identical set of launchd labels
        for label in ["com.forgefleet.forgefleetd", "com.forgefleet.node", "com.forgefleet.ffdaemon"] {
            assert!(start.contains(label));
            assert!(stop.contains(label));
        }
    }
}
