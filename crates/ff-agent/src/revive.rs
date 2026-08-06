//! Leader-initiated worker revive.
//!
//! When the leader observes a computer ODOWN for >45s but last_seen_at was
//! within the last 10 minutes, it enqueues a revive_member task that tries
//! to bring the computer back via SSH → daemon restart → WoL → alert.
//!
//! ## Attempt order
//! 1. **SSH probe** — `ssh -o ConnectTimeout=5 -o BatchMode=yes user@host "echo ok"`.
//!    - Process existence is not health: a bounded localhost HTTP probe must
//!      show progress before an active daemon is spared.
//!    - If the daemon is dead or its local health endpoint is proven unhealthy,
//!      we `launchctl kickstart`
//!      (macOS) or `systemctl --user restart` (Linux) the services.
//! 2. **Wake-on-LAN** — if SSH is unreachable and we have MAC addresses on
//!    record, fire a magic packet to the local broadcast on UDP/9.
//! 3. **Failure** — no SSH + no MAC ⇒ record `Failed` so the caller can raise
//!    an alert via Telegram channels.
//!
//! This module is safe to invoke from any node but is only **scheduled** by
//! the current leader (see `leader_tick::revive_scan`).
//!
//! ### Not (yet) implemented
//! - Telegram alert fan-out lives outside this module; the deferred task that
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
const STALE_AFTER: Duration = Duration::from_secs(90);
const RECENTLY_HEALTHY_WINDOW: Duration = Duration::from_secs(15 * 60);
const RESTART_BACKOFF_WINDOW: Duration = Duration::from_secs(30 * 60);
const CONFIRM_INTERVAL: Duration = Duration::from_secs(3);
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonProbe {
    Healthy,
    Unhealthy,
    NotRunning,
}

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
            // Fresh database presence is authoritative and also makes manual
            // `ff fleet revive` safe to run against a healthy member.
            if !self.target_is_stale(target.computer_id).await? {
                return Ok(ReviveOutcome::Skipped("pulse is fresh".into()));
            }
            if self.fleet_ingest_outage().await? {
                return Ok(ReviveOutcome::Skipped(
                    "fleet-wide pulse ingest outage suspected; restart suppressed".into(),
                ));
            }

            // 2. Bounded node-local health/progress probe. An SSH transport
            // failure is deliberately not evidence that the daemon failed.
            match self
                .ssh_daemon_probe(&target.ssh_user, &target.primary_ip, target.ssh_port)
                .await
            {
                Ok(DaemonProbe::Healthy) => {
                    info!(
                        node = %target.name,
                        "daemon localhost health probe is progressing — nothing to restart"
                    );
                    Ok(ReviveOutcome::Skipped(
                        "daemon is locally healthy; pulse transport/ingest issue likely".into(),
                    ))
                }
                Ok(DaemonProbe::Unhealthy | DaemonProbe::NotRunning) => {
                    if !self.claim_restart(target.computer_id).await? {
                        return Ok(ReviveOutcome::Skipped(
                            "targeted restart already attempted in backoff window".into(),
                        ));
                    }
                    info!(
                        node = %target.name,
                        os = %target.os_family,
                        "ssh ok + daemon dead — attempting restart"
                    );
                    let restart_started = chrono::Utc::now();
                    match self.ssh_restart_daemon(target).await {
                        Ok(()) => {
                            if self.confirm_recovered(target, restart_started).await? {
                                Ok(ReviveOutcome::DaemonRestarted)
                            } else {
                                Ok(ReviveOutcome::Failed(
                                    "restart issued, but service and fresh pulse were not confirmed"
                                        .into(),
                                ))
                            }
                        }
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
                    warn!(node = %target.name, error = %e, "daemon probe transport failed");
                    Ok(ReviveOutcome::Skipped(
                        "ambiguous daemon probe transport failure; failing closed".into(),
                    ))
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
    pub async fn load_target(&self, computer_id: uuid::Uuid) -> Result<ReviveTarget, ReviveError> {
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

    async fn target_is_stale(&self, computer_id: uuid::Uuid) -> Result<bool, ReviveError> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT COALESCE(c.last_seen_at < NOW() - make_interval(secs => $2), TRUE)
                    AND (c.status IN ('odown', 'offline', 'sdown') OR EXISTS (
                        SELECT 1 FROM computer_downtime_events d
                         WHERE d.computer_id = c.id AND d.online_at IS NULL
                           AND d.cause IS DISTINCT FROM 'revive_attempt'))
               FROM computers c WHERE c.id = $1",
        )
        .bind(computer_id)
        .bind(STALE_AFTER.as_secs() as i32)
        .fetch_one(&self.pg)
        .await?)
    }

    /// Suppress restart storms when a majority of at least three nodes that
    /// were recently healthy have simultaneously stopped materializing.
    async fn fleet_ingest_outage(&self) -> Result<bool, ReviveError> {
        let (recent, stale): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT,
                    COUNT(*) FILTER (WHERE last_seen_at < NOW() - make_interval(secs => $1))::BIGINT
               FROM computers
              WHERE last_seen_at > NOW() - make_interval(secs => $2)",
        )
        .bind(STALE_AFTER.as_secs() as i32)
        .bind(RECENTLY_HEALTHY_WINDOW.as_secs() as i32)
        .fetch_one(&self.pg)
        .await?;
        Ok(is_ingest_outage(recent, stale))
    }

    /// Atomically claim the one allowed targeted restart for this node/window.
    async fn claim_restart(&self, computer_id: uuid::Uuid) -> Result<bool, ReviveError> {
        let inserted = sqlx::query_scalar::<_, i32>(
            "WITH locked AS (
                 SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))
             ), inserted AS (
                 INSERT INTO computer_downtime_events (computer_id, offline_at, cause)
                 SELECT $1, NOW(), 'revive_attempt' FROM locked
                  WHERE NOT EXISTS (
                    SELECT 1 FROM computer_downtime_events
                     WHERE computer_id = $1 AND cause = 'revive_attempt'
                       AND offline_at > NOW() - make_interval(secs => $2))
                 RETURNING 1
             ) SELECT 1 FROM inserted",
        )
        .bind(computer_id)
        .bind(RESTART_BACKOFF_WINDOW.as_secs() as i32)
        .fetch_optional(&self.pg)
        .await?;
        Ok(inserted.is_some())
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
        run_ssh(cmd).await.unwrap_or(false)
    }

    /// Prove daemon health locally. The HTTP response carries a current
    /// timestamp, so a successful request demonstrates event-loop progress.
    async fn ssh_daemon_probe(
        &self,
        user: &str,
        host: &str,
        port: i32,
    ) -> Result<DaemonProbe, ReviveError> {
        // Note on the pgrep pattern: SSH invokes us via `bash -c <cmd>`,
        // and `pgrep -f` scans full command lines — including our own
        // bash shell's, which contains the literal string "forgefleetd".
        // To avoid a self-match false positive, exclude $$ (our shell pid).
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_base_args(port))
            .arg(format!("{user}@{host}"))
            .arg(
                // Match any forgefleetd binary in the user's PATH, not just
                // ones launched with the `start` arg. Zombie daemons from
                // older deploys (e.g. systemd-launched, no argv suffix) get
                // missed by `forgefleetd.*start` and a revive cycle then
                // tries to spawn a second daemon. Use a broader pattern.
                "if ! pgrep -f '/forgefleetd($| )' | grep -v \"^$$\\$\" >/dev/null; then \
                    echo not_running; \
                 elif curl -fsS --max-time 4 http://127.0.0.1:${FF_AGENT_HTTP_PORT:-51820}/health \
                    | grep -q '\"ok\":true'; then echo healthy; \
                 else echo unhealthy; fi",
            );

        match run_ssh_output(cmd).await {
            Ok(stdout) => parse_daemon_probe(&stdout).ok_or_else(|| {
                ReviveError::Io(std::io::Error::other("unrecognized daemon probe response"))
            }),
            Err(e) => Err(e),
        }
    }

    async fn confirm_recovered(
        &self,
        target: &ReviveTarget,
        restart_started: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, ReviveError> {
        let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
        loop {
            let service_healthy = matches!(
                self.ssh_daemon_probe(&target.ssh_user, &target.primary_ip, target.ssh_port)
                    .await,
                Ok(DaemonProbe::Healthy)
            );
            let pulse_fresh = sqlx::query_scalar::<_, bool>(
                "SELECT COALESCE(last_seen_at >= $2, FALSE) FROM computers WHERE id = $1",
            )
            .bind(target.computer_id)
            .bind(restart_started)
            .fetch_one(&self.pg)
            .await?;
            if recovery_confirmed(service_healthy, pulse_fresh) {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(CONFIRM_INTERVAL).await;
        }
    }

    /// Platform-specific daemon restart issued over SSH.
    ///
    /// On macOS, different nodes register the daemon under different launchd
    /// labels (historical drift across onboarding scripts):
    ///   - `com.forgefleet.forgefleetd` — newer ff-daemon installs
    ///   - `com.forgefleet.node`        — older installs (e.g. Ace)
    ///   - `com.forgefleet.ffdaemon`    — variant used on Vinny
    ///     We try each in order and return on the first success.
    async fn ssh_restart_daemon(&self, target: &ReviveTarget) -> Result<(), ReviveError> {
        match target.os_family.as_str() {
            "macos" => {
                const MAC_LABELS: &[&str] = &[
                    "com.forgefleet.forgefleetd",
                    "com.forgefleet.node",
                    "com.forgefleet.ffdaemon",
                ];
                for label in MAC_LABELS {
                    let restart_cmd = format!("launchctl kickstart -k gui/$(id -u)/{label}");
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
                Err(ReviveError::Io(std::io::Error::other(
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
                    Err(ReviveError::Io(std::io::Error::other(
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
    // `IdentityAgent=none` + `BatchMode=yes` (`crate::ssh_opts::ssh_bypass_args`)
    // defeats a wedged inherited ssh-agent on headless Linux peers — revive runs
    // from the daemon, so it inherits the same socket the wave/backup SSH did.
    let mut args: Vec<String> = crate::ssh_opts::ssh_bypass_args()
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend([
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-p".into(),
        port.to_string(),
    ]);
    args
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
    let bytes =
        parse_mac(mac).ok_or_else(|| ReviveError::InvalidTarget(format!("bad MAC: {mac}")))?;
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
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn is_ingest_outage(recently_healthy: i64, stale: i64) -> bool {
    recently_healthy >= 3 && stale * 2 > recently_healthy
}

fn recovery_confirmed(service_healthy: bool, pulse_fresh: bool) -> bool {
    service_healthy && pulse_fresh
}

fn parse_daemon_probe(stdout: &str) -> Option<DaemonProbe> {
    match stdout.trim() {
        "healthy" => Some(DaemonProbe::Healthy),
        "unhealthy" => Some(DaemonProbe::Unhealthy),
        "not_running" => Some(DaemonProbe::NotRunning),
        _ => None,
    }
}

/// If `fleet_info::resolve_best_ip` knows a better IP (LAN preferred over
/// tailscale), overwrite the target's `primary_ip` so SSH/probe calls hit
/// the right interface. Silently leaves the target unchanged on any error —
/// the stored `primary_ip` is a safe fallback.
async fn rewrite_primary_ip_if_possible(target: &mut ReviveTarget) {
    if let Some((ip, kind)) = crate::fleet_info::resolve_best_ip(&target.name).await
        && ip != target.primary_ip
    {
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
        // Wedged-agent bypass must be present (HA.2).
        assert!(args.iter().any(|a| a == "IdentityAgent=none"));
    }

    #[test]
    fn majority_ingest_outage_requires_three_recent_nodes() {
        assert!(!is_ingest_outage(2, 2));
        assert!(is_ingest_outage(3, 2));
        assert!(is_ingest_outage(10, 6));
        assert!(!is_ingest_outage(10, 5));
    }

    #[test]
    fn restart_is_not_recovered_until_service_and_new_pulse_agree() {
        assert!(!recovery_confirmed(false, false));
        assert!(!recovery_confirmed(true, false));
        assert!(!recovery_confirmed(false, true));
        assert!(recovery_confirmed(true, true));
    }

    #[test]
    fn sophie_shape_active_process_without_local_progress_is_unhealthy() {
        assert_eq!(
            parse_daemon_probe("unhealthy\n"),
            Some(DaemonProbe::Unhealthy)
        );
        assert_ne!(
            parse_daemon_probe("unhealthy\n"),
            Some(DaemonProbe::Healthy)
        );
    }

    #[test]
    fn healthy_local_progress_skips_restart_signal() {
        assert_eq!(parse_daemon_probe("healthy\n"), Some(DaemonProbe::Healthy));
    }

    #[test]
    fn ambiguous_probe_output_fails_closed() {
        assert_eq!(parse_daemon_probe("ssh: connection reset"), None);
    }
}
