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

use std::process::Command;

use sqlx::PgPool;
use thiserror::Error;
use tracing::{info, warn};

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
    pub async fn promote_to_gateway(&self) -> Result<(), OpenClawError> {
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
