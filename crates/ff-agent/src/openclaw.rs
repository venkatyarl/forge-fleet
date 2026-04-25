//! OpenClaw integration — ForgeFleet leader = OpenClaw gateway.
//!
//! Wired to [`crate::leader_tick::LeaderTick`] callbacks: when a computer
//! becomes leader, its local OpenClaw installation is promoted to gateway
//! mode. When it loses leadership, it is demoted back to node mode pointing
//! at the new leader's OpenClaw gateway.
//!
//! ## Intended usage in forgefleetd startup (src/main.rs):
//!
//! ```ignore
//! use std::sync::Arc;
//! use ff_agent::openclaw::OpenClawManager;
//!
//! let openclaw = Arc::new(OpenClawManager::new(
//!     pg.clone(),
//!     my_computer_id,
//!     my_primary_ip.clone(),
//! ));
//! let oc_on_lead = openclaw.clone();
//! let oc_on_lost = openclaw.clone();
//! let pg_for_url = pg.clone();
//!
//! let on_became: ff_agent::leader_tick::OnBecameLeader = Arc::new(move || {
//!     let oc = oc_on_lead.clone();
//!     tokio::spawn(async move {
//!         if let Err(e) = oc.promote_to_gateway().await {
//!             tracing::error!(error = %e, "openclaw: promote_to_gateway failed");
//!         }
//!     });
//! });
//! let on_lost: ff_agent::leader_tick::OnLostLeader = Arc::new(move |_new_leader_name| {
//!     let oc = oc_on_lost.clone();
//!     let pg = pg_for_url.clone();
//!     tokio::spawn(async move {
//!         // Resolve the new leader's gateway URL from fleet_secrets (written
//!         // by whichever node just promoted itself to gateway).
//!         let url = ff_agent::openclaw::lookup_gateway_url(&pg)
//!             .await
//!             .unwrap_or_default();
//!         if url.is_empty() {
//!             tracing::warn!("openclaw: lost leader but no gateway URL published yet");
//!             return;
//!         }
//!         if let Err(e) = oc.demote_to_node(&url).await {
//!             tracing::error!(error = %e, "openclaw: demote_to_node failed");
//!         }
//!     });
//! });
//!
//! let leader_tick = LeaderTick::new(pg.clone(), pulse, my_id, my_name, my_prio)
//!     .with_on_became_leader(on_became)
//!     .with_on_lost_leader(on_lost);
//! ```

use std::process::{Command, Stdio};
use std::io::Write;

use sqlx::PgPool;
use thiserror::Error;
use tracing::{info, warn};

/// Fleet-secret key under which the gateway publishes its device-pairing
/// export on `on_lost_leader`. The new leader reads it on `on_became_leader`
/// and re-imports it into its freshly-promoted gateway. Transient — cleared
/// after a successful import.
pub const DEVICE_PAIRINGS_SECRET_KEY: &str = "openclaw.device_pairings_export";

#[derive(Debug, Error)]
pub enum OpenClawError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("openclaw cli error: {0}")]
    Cli(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Manages this machine's OpenClaw install — promote/demote driven by
/// ForgeFleet leader-election callbacks.
pub struct OpenClawManager {
    pub pg: PgPool,
    pub my_computer_id: uuid::Uuid,
    pub my_primary_ip: String,
}

impl OpenClawManager {
    pub fn new(pg: PgPool, my_computer_id: uuid::Uuid, my_primary_ip: String) -> Self {
        Self {
            pg,
            my_computer_id,
            my_primary_ip,
        }
    }

    /// Promote this machine's OpenClaw to gateway mode.
    ///
    /// Called on `on_became_leader`. Idempotent — if we're already in
    /// gateway mode per the DB, this no-ops.
    ///
    /// `previous_leader` — if known, this computer's name is used to rsync
    /// the outgoing gateway's paired-device file across so phones/IoT
    /// survive the failover without re-pairing. Best-effort.
    pub async fn promote_to_gateway(
        &self,
        previous_leader: Option<&str>,
    ) -> Result<(), OpenClawError> {
        info!("openclaw: promoting local to gateway mode");

        // Check current mode via DB — idempotent guard.
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT mode FROM openclaw_installations WHERE computer_id = $1",
        )
        .bind(self.my_computer_id)
        .fetch_optional(&self.pg)
        .await?;

        if row.as_ref().map(|r| r.0.as_str()) == Some("gateway") {
            info!("openclaw: already gateway, no-op");
            return Ok(());
        }

        // Run: openclaw config set gateway.mode local
        run_openclaw(&["config", "set", "gateway.mode", "local"])?;

        // Restart via launchd (macOS) or systemd (linux). Best-effort —
        // if the service isn't registered yet we log and continue.
        restart_openclaw_service()?;

        // Sweeten the failover: pull the outgoing gateway's paired-device
        // file across so phones/IoT don't have to re-pair. Runs BEFORE
        // any token rotation so imported devices see a valid gateway
        // state. Best-effort — all failures logged, never propagated.
        if let Some(old) = previous_leader {
            if !old.is_empty() {
                let my_name: String = sqlx::query_scalar(
                    "SELECT name FROM computers WHERE id = $1",
                )
                .bind(self.my_computer_id)
                .fetch_optional(&self.pg)
                .await?
                .unwrap_or_default();
                if old != my_name {
                    let _ = migrate_devices_from(&self.pg, old, &my_name).await;
                }
            }
        }

        let url = format!("ws://{}:50000", self.my_primary_ip);

        // Upsert DB row.
        sqlx::query(
            "INSERT INTO openclaw_installations \
             (computer_id, mode, gateway_url, last_reconfigured_at) \
             VALUES ($1, 'gateway', $2, NOW()) \
             ON CONFLICT (computer_id) DO UPDATE \
             SET mode = 'gateway', gateway_url = $2, last_reconfigured_at = NOW()",
        )
        .bind(self.my_computer_id)
        .bind(&url)
        .execute(&self.pg)
        .await?;

        // Publish gateway URL to fleet_secrets so other members can point at it.
        upsert_secret(&self.pg, "openclaw.gateway_url", &url).await?;

        info!(%url, "openclaw: promoted to gateway");
        Ok(())
    }

    /// Export paired devices from the local OpenClaw gateway.
    ///
    /// Runs `openclaw devices export --format json` and returns stdout.
    /// Called on `on_lost_leader` — the result is written to
    /// `fleet_secrets.openclaw.device_pairings_export` so the incoming
    /// leader can re-import it on `on_became_leader`.
    ///
    /// Returns an empty string if OpenClaw reports no devices (rather
    /// than an error) so the caller can uniformly stash-and-clear.
    pub async fn export_devices(&self) -> Result<String, OpenClawError> {
        info!("openclaw: exporting paired devices via openclaw CLI");
        let output = Command::new("openclaw")
            .args(["devices", "export", "--format", "json"])
            .output()?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(OpenClawError::Cli(format!(
                "devices export failed: {err}"
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        info!(
            bytes = stdout.len(),
            "openclaw: exported paired devices"
        );
        Ok(stdout)
    }

    /// Import paired devices into the local OpenClaw gateway.
    ///
    /// Runs `openclaw devices import --format json` and pipes `json_export`
    /// to stdin. Returns the number of devices imported per OpenClaw's
    /// reported output (best-effort — an opaque 0 is returned if the
    /// output can't be parsed).
    ///
    /// Called on `on_became_leader` after reading the export from
    /// `fleet_secrets`. If `json_export` is empty/whitespace, this is a
    /// no-op returning `Ok(0)` rather than an error.
    pub async fn import_devices(
        &self,
        json_export: &str,
    ) -> Result<usize, OpenClawError> {
        if json_export.trim().is_empty() {
            info!("openclaw: device export is empty — nothing to import");
            return Ok(0);
        }
        info!(
            bytes = json_export.len(),
            "openclaw: importing paired devices via openclaw CLI"
        );
        let mut child = Command::new("openclaw")
            .args(["devices", "import", "--format", "json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(json_export.as_bytes())?;
            // Dropping stdin closes it — openclaw will proceed once EOF
            // arrives.
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(OpenClawError::Cli(format!(
                "devices import failed: {err}"
            )));
        }

        // Best-effort parse: OpenClaw is expected to print a line like
        // "imported 14 device(s)" or emit JSON {"imported":14}. Pull the
        // first integer we find.
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let count = stdout
            .split(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        info!(count, "openclaw: imported paired devices");
        Ok(count)
    }

    /// Demote this machine's OpenClaw to node mode pointing at `leader_url`.
    ///
    /// Called on `on_lost_leader` or when first observing someone else as
    /// leader. Idempotent — always writes the latest URL to disk + DB.
    pub async fn demote_to_node(&self, leader_url: &str) -> Result<(), OpenClawError> {
        info!(leader_url, "openclaw: demoting to node");

        run_openclaw(&["config", "set", "gateway.mode", "remote"])?;
        run_openclaw(&["config", "set", "gateway.remote.url", leader_url])?;

        restart_openclaw_service()?;

        sqlx::query(
            "INSERT INTO openclaw_installations \
             (computer_id, mode, gateway_url, last_reconfigured_at) \
             VALUES ($1, 'node', $2, NOW()) \
             ON CONFLICT (computer_id) DO UPDATE \
             SET mode = 'node', gateway_url = $2, last_reconfigured_at = NOW()",
        )
        .bind(self.my_computer_id)
        .bind(leader_url)
        .execute(&self.pg)
        .await?;

        info!(leader_url, "openclaw: demoted to node");
        Ok(())
    }

    /// Reconcile this machine's OpenClaw role against the durable leader
    /// state in `fleet_leader_state`. Idempotent and safe to call on a
    /// timer.
    ///
    /// Why this exists: `LeaderTick` only fires `on_became_leader` /
    /// `on_lost_leader` on **state transitions**. A node that has been a
    /// non-leader its entire uptime never sees a transition, so its
    /// `demote_to_node` callback never fires and its OpenClaw role stays
    /// whatever it was last manually configured (often: nothing).
    ///
    /// `reconcile_role` closes the gap: read the durable leader, compare
    /// to `self`, and ensure the underlying mode matches. Both
    /// `promote_to_gateway` and `demote_to_node` already no-op when the
    /// DB row matches the desired mode, so calling this every minute is
    /// cheap.
    pub async fn reconcile_role(&self) -> Result<(), OpenClawError> {
        let leader: Option<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT computer_id, member_name FROM fleet_leader_state LIMIT 1",
        )
        .fetch_optional(&self.pg)
        .await?;

        match leader {
            None => {
                tracing::debug!("openclaw: reconcile skipped — no durable leader yet");
                Ok(())
            }
            Some((leader_id, _)) if leader_id == self.my_computer_id => {
                self.promote_to_gateway(None).await
            }
            Some((_, leader_name)) => {
                let url: Option<(String,)> = sqlx::query_as(
                    "SELECT primary_ip FROM computers WHERE name = $1",
                )
                .bind(&leader_name)
                .fetch_optional(&self.pg)
                .await?;
                let Some((leader_ip,)) = url else {
                    tracing::warn!(
                        leader = %leader_name,
                        "openclaw: reconcile can't resolve leader IP"
                    );
                    return Ok(());
                };
                let url = format!("ws://{leader_ip}:50000");
                self.demote_to_node(&url).await
            }
        }
    }

    /// Run `reconcile_role` on a timer until shutdown. Spawned from
    /// `forgefleetd` startup.
    pub async fn run_reconciler(
        self: std::sync::Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        interval: std::time::Duration,
    ) {
        info!(?interval, "openclaw: role reconciler started");
        let mut tick = tokio::time::interval(interval);
        // Skip the immediate fire — first tick happens after `interval`.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Do an initial reconcile so a freshly-booted worker doesn't sit
        // unconfigured for the first interval.
        if let Err(e) = self.reconcile_role().await {
            warn!(error = %e, "openclaw: initial reconcile failed");
        }
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.reconcile_role().await {
                        warn!(error = %e, "openclaw: reconcile_role failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("openclaw: role reconciler shutting down");
                        break;
                    }
                }
            }
        }
    }
}

/// Read the currently-published gateway URL from `fleet_secrets`. Returns
/// `None` if no gateway has ever promoted itself (cold start).
pub async fn lookup_gateway_url(pool: &PgPool) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM fleet_secrets WHERE key = 'openclaw.gateway_url'")
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0))
}

/// Read the transient paired-device export from `fleet_secrets`. Returns
/// `None` if the previous leader never stashed anything (e.g. cold fleet
/// or the old leader crashed before exporting). The new leader reads this
/// during `on_became_leader`; if present, it `import_devices(…)` and then
/// clears the secret via `clear_device_pairings_export`.
pub async fn lookup_device_pairings_export(
    pool: &PgPool,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM fleet_secrets WHERE key = $1",
    )
    .bind(DEVICE_PAIRINGS_SECRET_KEY)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Delete the transient paired-device export secret. Called after a
/// successful import so the next leader change starts from a clean slate.
pub async fn clear_device_pairings_export(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM fleet_secrets WHERE key = $1")
        .bind(DEVICE_PAIRINGS_SECRET_KEY)
        .execute(pool)
        .await?;
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn run_openclaw(args: &[&str]) -> Result<String, OpenClawError> {
    let output = Command::new("openclaw").args(args).output()?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(OpenClawError::Cli(err));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Best-effort service restart: `launchctl kickstart` on macOS, `systemctl
/// restart` on Linux. Failures are logged as warnings, not errors — the
/// DB state is already authoritative and the next `openclaw` invocation
/// will pick up the new config on its own.
fn restart_openclaw_service() -> Result<(), OpenClawError> {
    if std::env::consts::OS == "macos" {
        let uid = current_uid().unwrap_or_else(|| "501".to_string());
        let label = format!("gui/{uid}/ai.openclaw.gateway");
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", &label])
            .status()?;
        if !status.success() {
            warn!(%label, "openclaw: launchctl kickstart failed (service may not be registered); continuing");
        }
    } else {
        // Try systemd --user first, then system-scope via sudo -n (passwordless).
        let user_ok = Command::new("systemctl")
            .args(["--user", "restart", "openclaw-gateway.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !user_ok {
            let system_ok = Command::new("sudo")
                .args(["-n", "systemctl", "restart", "openclaw-gateway.service"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !system_ok {
                warn!("openclaw: systemctl restart failed in both --user and system scope; continuing");
            }
        }
    }
    Ok(())
}

/// Cheap UID lookup without adding a `libc` dep: shell out to `id -u` and
/// fall back to `$UID` / `$SUDO_UID` env vars. Good enough for a launchd
/// GUI domain label.
fn current_uid() -> Option<String> {
    if let Ok(out) = Command::new("id").arg("-u").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    std::env::var("UID").ok().or_else(|| std::env::var("SUDO_UID").ok())
}

/// Best-effort: rsync the previous gateway's `~/.openclaw/data/devices.json`
/// across so paired phones/IoT survive a failover without re-pairing.
///
/// Returns the number of devices imported, or `Ok(0)` on any soft failure.
/// This is sweetener; it must never block a promotion.
async fn migrate_devices_from(
    pool: &PgPool,
    old_leader: &str,
    new_leader: &str,
) -> anyhow::Result<usize> {
    // 1) Look up old leader's ssh_user + ip. Prefer `computers`; fall
    //    back to `fleet_nodes` (legacy terminology still carries data).
    let found: Option<(String, String)> = sqlx::query_as(
        "SELECT ssh_user, primary_ip FROM computers WHERE name = $1",
    )
    .bind(old_leader)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .or(sqlx::query_as::<_, (String, String)>(
        "SELECT ssh_user, ip FROM fleet_nodes WHERE name = $1",
    )
    .bind(old_leader)
    .fetch_optional(pool)
    .await
    .unwrap_or(None));
    let (ssh_user, ip) = match found {
        Some(x) => x,
        None => {
            warn!(%old_leader, "migrate_devices: no ssh_user/ip in computers or fleet_nodes");
            return Ok(0);
        }
    };

    // 2) Cat the file over SSH. Missing/empty → nothing to do.
    let dest = format!("{ssh_user}@{ip}");
    let out = Command::new("ssh")
        .args([
            "-o", "ConnectTimeout=8",
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=accept-new",
            &dest,
            "cat ~/.openclaw/data/devices.json 2>/dev/null",
        ])
        .output();
    let body = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            warn!(%dest, code=?o.status.code(), "migrate_devices: ssh exited non-zero");
            return Ok(0);
        }
        Err(e) => {
            warn!(%dest, error=%e, "migrate_devices: ssh spawn failed");
            return Ok(0);
        }
    };
    if body.trim().is_empty() {
        info!(%old_leader, "migrate_devices: remote devices.json empty or missing");
        return Ok(0);
    }

    // 3) Stage locally.
    let ts = chrono::Utc::now().timestamp();
    let path = format!("/tmp/ff_devices_migration_{ts}.json");
    if let Err(e) = std::fs::write(&path, &body) {
        warn!(%path, error=%e, "migrate_devices: write local stage failed");
        return Ok(0);
    }

    // 4) Count for logging (best-effort parse).
    let count = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("devices").and_then(|d| d.as_array()).map(|a| a.len()))
        .unwrap_or(0);
    info!(%old_leader, %new_leader, count, %path, "migrate_devices: importing paired devices");

    // 5) Import via local openclaw. Fall back to /usr/local/bin/openclaw
    //    if the bare name isn't on $PATH.
    let bin = which_openclaw();
    let status = Command::new(&bin)
        .args(["devices", "import", &path])
        .status();
    match status {
        Ok(s) if s.success() => Ok(count.max(1)),
        Ok(s) => {
            warn!(code=?s.code(), bin=%bin, "migrate_devices: openclaw devices import failed");
            Ok(0)
        }
        Err(e) => {
            warn!(error=%e, bin=%bin, "migrate_devices: openclaw devices import spawn failed");
            Ok(0)
        }
    }
}

/// Resolve the openclaw binary path — prefer `$PATH`, else `/usr/local/bin/openclaw`.
fn which_openclaw() -> String {
    if let Ok(o) = Command::new("which").arg("openclaw").output() {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "/usr/local/bin/openclaw".to_string()
}

async fn upsert_secret(pool: &PgPool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO fleet_secrets (key, value, updated_by, updated_at) \
         VALUES ($1, $2, 'openclaw-manager', NOW()) \
         ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = NOW()",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}
