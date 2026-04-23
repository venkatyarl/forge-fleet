//! Leader-initiated worker revive.
//!
//! When the leader observes a computer ODOWN for >45s but last_seen_at was
//! within the last 10 minutes, it enqueues a revive_member task that tries
//! to bring the computer back via SSH → daemon restart → WoL → alert.
//!
//! ## Attempt order
//! 1. **SSH probe** — `ssh -o ConnectTimeout=5 -o BatchMode=yes user@host "echo ok"`.
//!    - If the probe succeeds and `forgefleetd` is alive, we log and bail
//!      (ODOWN with a healthy daemon usually means Redis/network split — not
//!      something SSH can fix).
//!    - If the probe succeeds and the daemon is dead, we `launchctl kickstart`
//!      (macOS) or `systemctl --user restart` (Linux) the services.
//! 2. **Wake-on-LAN** — if SSH is unreachable and we have MAC addresses on
//!    record, fire a magic packet to the local broadcast on UDP/9.
//! 3. **Failure** — no SSH + no MAC ⇒ record `Failed` so the caller can raise
//!    an alert via OpenClaw channels.
//!
//! This module is safe to invoke from any node but is only **scheduled** by
//! the current leader (see `leader_tick::revive_scan`).
//!
//! ### Not (yet) implemented
//! - OpenClaw alert fan-out lives outside this module; the deferred task that
//!   wraps a revive attempt records `Failed` and the leader escalates from
//!   there. This keeps the revive manager free of Slack/webhook dependencies.

use std::time::Duration;

use sqlx::{PgPool, Row};
use tokio::net::UdpSocket;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Timeout for any single SSH invocation issued by the revive manager.
const SSH_TIMEOUT: Duration = Duration::from_secs(12);
/// Magic-packet destination port (WoL canonical).
const WOL_PORT: u16 = 9;

/// Orchestrates one revive attempt against a target computer.
pub struct ReviveManager {
    pg: PgPool,
}

/// Snapshot of a computer's revive-relevant metadata.
#[derive(Debug, Clone)]
pub struct ReviveTarget {
    pub computer_id: uuid::Uuid,
    pub name: String,
    pub primary_ip: String,
    pub ssh_user: String,
    pub ssh_port: i32,
    pub mac_addresses: Vec<String>,
    pub os_family: String,
    /// One of `lan`, `tailscale_only`, `wan`. Used to decide whether WoL is
    /// a sensible fallback when SSH is unreachable. For tailscale_only or
    /// wan targets we skip WoL entirely — magic packets don't traverse
    /// overlay networks or the public internet.
    pub network_scope: String,
}

/// Terminal outcome of a single `attempt()` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviveOutcome {
    /// SSH worked and we kicked the daemon back to life.
    DaemonRestarted,
    /// SSH worked, daemon is already up — nothing to restart.
    DaemonAlreadyRunning,
    /// SSH unreachable — magic packet sent, awaiting pulse.
    WolSent,
    /// All options exhausted.
    Failed(String),
    /// No-op with a reason (e.g. SSH works but daemon healthy).
    Skipped(String),
}

/// Errors from the revive manager.
#[derive(Debug, thiserror::Error)]
pub enum ReviveError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("target not found: {0}")]
    TargetNotFound(String),
    #[error("target metadata invalid: {0}")]
    InvalidTarget(String),
}

impl ReviveManager {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Try to revive a target. Non-blocking on pulse return — the caller
    /// observes pulse independently.
    pub async fn attempt(&self, target: &ReviveTarget) -> Result<ReviveOutcome, ReviveError> {
        // Record that we're trying, so the backoff check can see it.
        self.record_revive_attempt(target.computer_id).await?;

        info!(
            node = %target.name,
            ip = %target.primary_ip,
            "revive attempt starting"
        );

        // 1. SSH probe — can we reach the box at all?
        let probe_ok = self
            .ssh_probe(&target.ssh_user, &target.primary_ip, target.ssh_port)
            .await;

        if probe_ok {
            // 2. Daemon liveness via pgrep over SSH.
            match self
                .ssh_daemon_running(&target.ssh_user, &target.primary_ip, target.ssh_port)
                .await
            {
                Ok(true) => {
                    info!(
                        node = %target.name,
                        "ssh ok + daemon alive — nothing to restart (likely Redis split)"
                    );
                    Ok(ReviveOutcome::Skipped(
                        "SSH works, daemon running — Redis connectivity issue likely".into(),
                    ))
                }
                Ok(false) => {
                    info!(
                        node = %target.name,
                        os = %target.os_family,
                        "ssh ok + daemon dead — attempting restart"
                    );
                    match self.ssh_restart_daemon(target).await {
                        Ok(()) => Ok(ReviveOutcome::DaemonRestarted),
                        Err(e) => {
                            warn!(
                                node = %target.name,
                                error = %e,
                                "ssh restart failed"
                            );
                            // Fall through to WoL — some boxes have daemon
                            // mode issues the restart call can't unstick.
                            self.try_wol_or_fail(target).await
                        }
                    }
                }
                Err(e) => {
                    warn!(node = %target.name, error = %e, "ssh pgrep failed");
                    self.try_wol_or_fail(target).await
                }
            }
        } else {
            // 3. SSH unreachable — WoL + possibly fail.
            self.try_wol_or_fail(target).await
        }
    }

    /// Load target metadata for a computer id from the DB.
    ///
    /// `primary_ip` is rewritten to the "best reachable" IP — LAN preferred,
    /// Tailscale fallback — via `fleet_info::resolve_best_ip`. This means
    /// SSH probes for a tailscale-only computer automatically target the
    /// 100.64.x address rather than a stale LAN IP.
    pub async fn load_target(
        &self,
        computer_id: uuid::Uuid,
    ) -> Result<ReviveTarget, ReviveError> {
        let row = sqlx::query(
            "SELECT id, name, primary_ip, ssh_user, ssh_port, mac_addresses, os_family,
                    COALESCE(network_scope, 'lan') AS network_scope
             FROM computers
             WHERE id = $1",
        )
        .bind(computer_id)
        .fetch_optional(&self.pg)
        .await?
        .ok_or_else(|| ReviveError::TargetNotFound(computer_id.to_string()))?;

        let mut target = row_to_target(&row)?;
        rewrite_primary_ip_if_possible(&mut target).await;
        Ok(target)
    }

    /// Load target metadata by unique computer name.
    pub async fn load_target_by_name(&self, name: &str) -> Result<ReviveTarget, ReviveError> {
        let row = sqlx::query(
            "SELECT id, name, primary_ip, ssh_user, ssh_port, mac_addresses, os_family,
                    COALESCE(network_scope, 'lan') AS network_scope
             FROM computers
             WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pg)
        .await?
        .ok_or_else(|| ReviveError::TargetNotFound(name.to_string()))?;

        let mut target = row_to_target(&row)?;
        rewrite_primary_ip_if_possible(&mut target).await;
        Ok(target)
    }

    /// Append a `revive_attempt` row to `computer_downtime_events` for
    /// backoff accounting. Never opens a new downtime window — just a marker.
    async fn record_revive_attempt(&self, computer_id: uuid::Uuid) -> Result<(), ReviveError> {
        sqlx::query(
            "INSERT INTO computer_downtime_events (computer_id, offline_at, cause)
             VALUES ($1, NOW(), 'revive_attempt')",
        )
        .bind(computer_id)
        .execute(&self.pg)
        .await?;
        Ok(())
    }

    /// Count revive attempts for a computer in the last `minutes` minutes.
    /// Used by the leader's backoff guard.
    pub async fn recent_attempt_count(
        pg: &PgPool,
        computer_id: uuid::Uuid,
        minutes: i64,
    ) -> Result<i64, ReviveError> {
        let row = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS c
               FROM computer_downtime_events
              WHERE computer_id = $1
                AND cause = 'revive_attempt'
                AND offline_at > NOW() - ($2 || ' minutes')::INTERVAL",
        )
        .bind(computer_id)
        .bind(minutes.to_string())
        .fetch_one(pg)
        .await?;
        Ok(row.get::<i64, _>("c"))
    }

    // ─── SSH helpers ─────────────────────────────────────────────────────────

    /// Single SSH probe: `echo ok` under a short connect timeout.
    async fn ssh_probe(&self, user: &str, host: &str, port: i32) -> bool {
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_base_args(port))
            .arg(format!("{user}@{host}"))
            .arg("echo ok");
        run_ssh(cmd).await.map(|ok| ok).unwrap_or(false)
    }

    /// Check whether `forgefleetd` is alive on the target via pgrep.
    async fn ssh_daemon_running(
        &self,
        user: &str,
        host: &str,
        port: i32,
    ) -> Result<bool, ReviveError> {
        // Note on the pgrep pattern: SSH invokes us via `bash -c <cmd>`,
        // and `pgrep -f` scans full command lines — including our own
        // bash shell's, which contains the literal string "forgefleetd".
        // To avoid a self-match false positive, exclude $$ (our shell pid).
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_base_args(port))
            .arg(format!("{user}@{host}"))
            .arg(
                "if pgrep -f 'forgefleetd.*start' | grep -v \"^$$\\$\" >/dev/null; \
                 then echo yes; else echo no; fi",
            );

        match run_ssh_output(cmd).await {
            Ok(stdout) => Ok(stdout.trim().ends_with("yes")),
            Err(e) => Err(e),
        }
    }

    /// Platform-specific daemon restart issued over SSH.
    ///
    /// On macOS, different nodes register the daemon under different launchd
    /// labels (historical drift across onboarding scripts):
    ///   - `com.forgefleet.forgefleetd` — newer ff-daemon installs
    ///   - `com.forgefleet.node`        — older installs (e.g. Ace)
    ///   - `com.forgefleet.ffdaemon`    — variant used on Taylor
    /// We try each in order and return on the first success.
    async fn ssh_restart_daemon(&self, target: &ReviveTarget) -> Result<(), ReviveError> {
        match target.os_family.as_str() {
            "macos" => {
                const MAC_LABELS: &[&str] = &[
                    "com.forgefleet.forgefleetd",
                    "com.forgefleet.node",
                    "com.forgefleet.ffdaemon",
                ];
                for label in MAC_LABELS {
                    let restart_cmd = format!(
                        "launchctl kickstart -k gui/$(id -u)/{label}"
                    );
                    let mut cmd = Command::new("ssh");
                    cmd.args(ssh_base_args(target.ssh_port))
                        .arg(format!("{}@{}", target.ssh_user, target.primary_ip))
                        .arg(&restart_cmd);

                    if run_ssh(cmd).await.unwrap_or(false) {
                        debug!(
                            node = %target.name,
                            label = %label,
                            "launchctl kickstart succeeded"
                        );
                        return Ok(());
                    }
                    debug!(
                        node = %target.name,
                        label = %label,
                        "launchctl kickstart failed; trying next label"
                    );
                }
                Err(ReviveError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "daemon restart: no macOS launchd label matched",
                )))
            }
            _ => {
                // Linux / DGX: systemd user unit.
                //
                // Headless SSH sessions need XDG_RUNTIME_DIR + DBUS set or
                // `systemctl --user` silently no-ops (tripped the 2026-04-22
                // DGX outage — 4 daemons dead 9+ hours, revive reported ✓).
                //
                // `reset-failed` clears StartLimitBurst trips (a SIGTERM
                // storm during migration can trip systemd into permanent
                // give-up). Installed unit name is `forgefleetd.service`.
                // Old `forgefleet-node.service` kept as a fallback for
                // nodes still on the pre-2026-04 unit layout.
                let restart_cmd = "\
                    export XDG_RUNTIME_DIR=/run/user/$(id -u); \
                    export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; \
                    systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; \
                    systemctl --user restart forgefleetd.service \
                       || systemctl --user restart forgefleet-node.service \
                       || systemctl --user restart forgefleet-daemon.service";
                let mut cmd = Command::new("ssh");
                cmd.args(ssh_base_args(target.ssh_port))
                    .arg(format!("{}@{}", target.ssh_user, target.primary_ip))
                    .arg(restart_cmd);

                if run_ssh(cmd).await.unwrap_or(false) {
                    Ok(())
                } else {
                    Err(ReviveError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "daemon restart ssh call returned non-zero",
                    )))
                }
            }
        }
    }

    /// Send WoL to every known MAC; if we have none, return Failed.
    ///
    /// Skipped entirely for computers whose only reachability is via an
    /// overlay network (Tailscale) or the public internet (WAN). Magic
    /// packets are link-local and won't traverse those paths.
    async fn try_wol_or_fail(&self, target: &ReviveTarget) -> Result<ReviveOutcome, ReviveError> {
        if target.network_scope == "tailscale_only" || target.network_scope == "wan" {
            info!(
                node = %target.name,
                scope = %target.network_scope,
                "skipping WoL — target reachable only via overlay/WAN, magic packets won't help"
            );
            return Ok(ReviveOutcome::Failed(format!(
                "SSH unreachable, WoL not applicable for network_scope='{}'",
                target.network_scope
            )));
        }
        if target.mac_addresses.is_empty() {
            return Ok(ReviveOutcome::Failed(
                "SSH unreachable and no MAC for WoL".into(),
            ));
        }
        let mut sent_any = false;
        for mac in &target.mac_addresses {
            match send_wol(mac).await {
                Ok(()) => {
                    info!(node = %target.name, mac = %mac, "WoL magic packet sent");
                    sent_any = true;
                }
                Err(e) => warn!(node = %target.name, mac = %mac, error = %e, "WoL send failed"),
            }
        }
        if sent_any {
            Ok(ReviveOutcome::WolSent)
        } else {
            Ok(ReviveOutcome::Failed(
                "SSH unreachable and all WoL sends failed".into(),
            ))
        }
    }
}

// ─── Module helpers ────────────────────────────────────────────────────────

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

/// Run an SSH command under a timeout; true iff exit 0.
async fn run_ssh(mut cmd: Command) -> Result<bool, ReviveError> {
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    match timeout(SSH_TIMEOUT, cmd.status()).await {
        Ok(Ok(s)) => Ok(s.success()),
        Ok(Err(e)) => Err(ReviveError::Io(e)),
        Err(_) => {
            debug!("ssh timed out after {:?}", SSH_TIMEOUT);
            Ok(false)
        }
    }
}

/// Run an SSH command under a timeout; return stdout as UTF-8.
async fn run_ssh_output(mut cmd: Command) -> Result<String, ReviveError> {
    cmd.stdin(std::process::Stdio::null());
    match timeout(SSH_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Err(e)) => Err(ReviveError::Io(e)),
        Err(_) => Err(ReviveError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "ssh output timed out",
        ))),
    }
}

/// Send a WoL magic packet to `mac` via UDP broadcast.
pub async fn send_wol(mac: &str) -> Result<(), ReviveError> {
    let bytes = parse_mac(mac)
        .ok_or_else(|| ReviveError::InvalidTarget(format!("bad MAC: {mac}")))?;
    let mut packet = Vec::with_capacity(6 + 16 * 6);
    packet.extend_from_slice(&[0xFFu8; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }

    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.set_broadcast(true)?;
    sock.send_to(&packet, ("255.255.255.255", WOL_PORT)).await?;
    Ok(())
}

/// Parse a 6-byte MAC from "aa:bb:cc:dd:ee:ff" or "aa-bb-..." etc.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if cleaned.len() != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// If `fleet_info::resolve_best_ip` knows a better IP (LAN preferred over
/// tailscale), overwrite the target's `primary_ip` so SSH/probe calls hit
/// the right interface. Silently leaves the target unchanged on any error —
/// the stored `primary_ip` is a safe fallback.
async fn rewrite_primary_ip_if_possible(target: &mut ReviveTarget) {
    if let Some((ip, kind)) = crate::fleet_info::resolve_best_ip(&target.name).await {
        if ip != target.primary_ip {
            debug!(
                node = %target.name,
                old_ip = %target.primary_ip,
                new_ip = %ip,
                kind = %kind,
                "revive: resolved better IP for target"
            );
            target.primary_ip = ip;
        }
    }
}

/// Shared row-extraction helper — pulls a `ReviveTarget` from a selected row.
fn row_to_target(row: &sqlx::postgres::PgRow) -> Result<ReviveTarget, ReviveError> {
    let mac_json: serde_json::Value = row
        .try_get::<serde_json::Value, _>("mac_addresses")
        .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
    let mac_addresses: Vec<String> = mac_json
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default();

    let network_scope: String = row
        .try_get::<String, _>("network_scope")
        .unwrap_or_else(|_| "lan".to_string());

    Ok(ReviveTarget {
        computer_id: row.get("id"),
        name: row.get("name"),
        primary_ip: row.get("primary_ip"),
        ssh_user: row.get("ssh_user"),
        ssh_port: row.get("ssh_port"),
        mac_addresses,
        os_family: row.get("os_family"),
        network_scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mac_canonical() {
        let m = parse_mac("aa:bb:cc:dd:ee:ff").unwrap();
        assert_eq!(m, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn parse_mac_dashes_and_case() {
        let m = parse_mac("AA-BB-CC-dd-ee-FF").unwrap();
        assert_eq!(m, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn parse_mac_rejects_short() {
        assert!(parse_mac("aa:bb:cc").is_none());
    }

    #[test]
    fn ssh_base_args_includes_port() {
        let args = ssh_base_args(2222);
        assert!(args.iter().any(|a| a == "2222"));
        assert!(args.iter().any(|a| a == "ConnectTimeout=5"));
        assert!(args.iter().any(|a| a == "BatchMode=yes"));
    }
}
