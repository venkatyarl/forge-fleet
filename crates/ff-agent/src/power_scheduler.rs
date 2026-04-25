//! Power scheduler — evaluates `computer_schedules` rows once a minute
//! and dispatches sleep / wake / restart actions against fleet members.
//!
//! Only the elected leader should run the spawn loop (conflict-free via
//! the existing `fleet_leader_state` row). Dispatched actions:
//!
//!   - `sleep`:   SSH in and run `pmset sleepnow` (macOS) or
//!                `systemctl suspend` (Linux).
//!   - `wake`:    Wake-on-LAN magic packet (reuses `revive::send_wol`).
//!   - `restart`: SSH in and run `sudo reboot`.
//!
//! The schedule spec supports cron-like expressions in the 5-field
//! Unix form: `m h dom mon dow`. Wildcards `*`, numeric literals, and
//! comma lists are supported; ranges and `*/N` steps are NOT (yet).
//!
//! An optional `condition` expression is evaluated against pulse beats.
//! Supported form: `idle_minutes > N`. Idle is defined as "no pulse
//! beat activity other than the periodic heartbeat" — we use the time
//! since last beat (a crude but workable proxy in v1).

use std::time::Duration;

use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::model_transfer::ssh_exec;
use crate::revive::send_wol;

#[derive(Debug, Error)]
pub enum PowerError {
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    #[error("ff-db: {0}")]
    FfDb(#[from] ff_db::DbError),
    #[error("cron parse: {0}")]
    CronParse(String),
    #[error("ssh: {0}")]
    Ssh(String),
    #[error("wol: {0}")]
    Wol(String),
    #[error("unsupported os: {0}")]
    UnsupportedOs(String),
    #[error("computer not found: {0}")]
    NotFound(String),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScheduleAction {
    pub schedule_id: sqlx::types::Uuid,
    pub computer_name: String,
    pub kind: String,
    pub result: String, // "ok" | "skipped: <reason>" | "error: <msg>"
}

pub struct PowerScheduler {
    pg: PgPool,
}

impl PowerScheduler {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Walk every enabled schedule row and fire any whose cron matches
    /// the current wall-clock minute. Returns a list of actions actually
    /// dispatched (including skips). Safe to call more than once per
    /// minute — the 1-minute floor is enforced via `last_fired_at`.
    pub async fn evaluate_once(&self) -> Result<Vec<ScheduleAction>, PowerError> {
        let now = chrono::Utc::now();
        let rows = ff_db::pg_list_schedules(&self.pg, None, true).await?;

        let mut actions = Vec::new();
        for r in rows {
            // Suppress duplicate firings in the same minute.
            if let Some(last) = r.last_fired_at {
                let since = now.signed_duration_since(last);
                if since.num_seconds() < 55 {
                    continue;
                }
            }

            if !cron_matches(&r.cron_expr, now)? {
                continue;
            }

            // Evaluate optional condition.
            if let Some(cond) = r.condition.as_deref() {
                match self
                    .evaluate_condition(cond, &r.computer_name.clone().unwrap_or_default())
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        let res = format!("skipped: condition failed ({cond})");
                        let _ = ff_db::pg_mark_schedule_fired(&self.pg, r.id, &res).await;
                        actions.push(ScheduleAction {
                            schedule_id: r.id,
                            computer_name: r.computer_name.clone().unwrap_or_default(),
                            kind: r.kind.clone(),
                            result: res,
                        });
                        continue;
                    }
                    Err(e) => {
                        warn!(schedule = %r.id, error = %e, "condition eval failed; skipping");
                        let res = format!("error: condition eval failed: {e}");
                        let _ = ff_db::pg_mark_schedule_fired(&self.pg, r.id, &res).await;
                        actions.push(ScheduleAction {
                            schedule_id: r.id,
                            computer_name: r.computer_name.clone().unwrap_or_default(),
                            kind: r.kind.clone(),
                            result: res,
                        });
                        continue;
                    }
                }
            }

            // Dispatch.
            let result = match self.dispatch(&r).await {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            };
            let _ = ff_db::pg_mark_schedule_fired(&self.pg, r.id, &result).await;
            actions.push(ScheduleAction {
                schedule_id: r.id,
                computer_name: r.computer_name.clone().unwrap_or_default(),
                kind: r.kind.clone(),
                result,
            });
        }

        Ok(actions)
    }

    /// Spawn the minute-tick loop; exits when `shutdown` flips to true.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match self.evaluate_once().await {
                            Ok(actions) if !actions.is_empty() => {
                                info!(count = actions.len(), "power scheduler fired actions");
                            }
                            Ok(_) => {
                                debug!("power scheduler tick: no actions");
                            }
                            Err(e) => {
                                warn!(error = %e, "power scheduler tick failed");
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!("power scheduler shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }

    async fn dispatch(&self, row: &ff_db::ComputerScheduleRow) -> Result<(), PowerError> {
        let target = fetch_target(&self.pg, row.computer_id).await?;
        match row.kind.as_str() {
            "sleep" => dispatch_sleep(&target).await,
            "restart" => dispatch_restart(&target).await,
            "wake" => dispatch_wake(&target).await,
            other => Err(PowerError::UnsupportedOs(format!("unknown kind {other}"))),
        }
    }

    /// Evaluate a condition like `idle_minutes > 120` against pulse beats.
    /// v1 supports one comparator (`>`) and one variable (`idle_minutes`).
    async fn evaluate_condition(
        &self,
        expr: &str,
        computer_name: &str,
    ) -> Result<bool, PowerError> {
        let t = expr.trim();
        // Strip leading "if " if present so "if idle_minutes > 120" works.
        let t = t.strip_prefix("if ").unwrap_or(t).trim();
        if let Some(rest) = t.strip_prefix("idle_minutes") {
            let rest = rest.trim_start();
            if let Some(num_str) = rest.strip_prefix('>') {
                let threshold: i64 = num_str.trim().parse().map_err(|e| {
                    PowerError::CronParse(format!("bad idle threshold '{num_str}': {e}"))
                })?;
                let idle = last_seen_idle_minutes(&self.pg, computer_name).await?;
                return Ok(idle > threshold);
            }
        }
        Err(PowerError::CronParse(format!(
            "unsupported condition expression: {expr}"
        )))
    }
}

#[derive(Debug, Clone)]
struct PowerTarget {
    name: String,
    primary_ip: String,
    ssh_user: String,
    os_family: String,
    mac_addresses: Vec<String>,
}

async fn fetch_target(
    pool: &PgPool,
    computer_id: sqlx::types::Uuid,
) -> Result<PowerTarget, PowerError> {
    let row = sqlx::query(
        "SELECT name, primary_ip, ssh_user, os_family, mac_addresses
         FROM computers WHERE id = $1",
    )
    .bind(computer_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| PowerError::NotFound(computer_id.to_string()))?;

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

    Ok(PowerTarget {
        name: row.get("name"),
        primary_ip: row.get("primary_ip"),
        ssh_user: row.get("ssh_user"),
        os_family: row.get("os_family"),
        mac_addresses,
    })
}

/// Minutes since last pulse beat for the given computer. Returns a very
/// large number if no beat has ever been recorded (treat as "always idle").
async fn last_seen_idle_minutes(pool: &PgPool, computer_name: &str) -> Result<i64, PowerError> {
    let row = sqlx::query(
        "SELECT EXTRACT(EPOCH FROM (NOW() - last_seen_at))::BIGINT as secs
         FROM computers WHERE name = $1",
    )
    .bind(computer_name)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => {
            let secs: i64 = r.try_get("secs").unwrap_or(i64::MAX / 2);
            Ok(secs / 60)
        }
        None => Ok(i64::MAX / 2),
    }
}

async fn dispatch_sleep(t: &PowerTarget) -> Result<(), PowerError> {
    let os = t.os_family.to_lowercase();
    let cmd = if os.starts_with("macos") {
        "pmset sleepnow"
    } else if os.starts_with("linux") {
        // systemctl suspend works for both systemd + recent Ubuntu; no
        // sudo prompt required because Taylor-sudo rule is fleet-wide.
        "sudo systemctl suspend"
    } else {
        return Err(PowerError::UnsupportedOs(os));
    };
    run_ssh(t, cmd).await
}

async fn dispatch_restart(t: &PowerTarget) -> Result<(), PowerError> {
    run_ssh(t, "sudo /sbin/reboot").await
}

async fn dispatch_wake(t: &PowerTarget) -> Result<(), PowerError> {
    if t.mac_addresses.is_empty() {
        return Err(PowerError::Wol(format!(
            "no MAC addresses on record for {}",
            t.name
        )));
    }
    for mac in &t.mac_addresses {
        match send_wol(mac).await {
            Ok(()) => {
                info!(computer = %t.name, mac = %mac, "WoL magic packet sent");
                return Ok(());
            }
            Err(e) => warn!(computer = %t.name, mac = %mac, error = %e, "send_wol failed"),
        }
    }
    Err(PowerError::Wol(format!(
        "all send_wol attempts failed for {}",
        t.name
    )))
}

async fn run_ssh(t: &PowerTarget, cmd: &str) -> Result<(), PowerError> {
    let (code, stdout, stderr) = ssh_exec(&t.ssh_user, &t.primary_ip, cmd)
        .await
        .map_err(PowerError::Ssh)?;
    if code == 0 {
        Ok(())
    } else {
        Err(PowerError::Ssh(format!(
            "{cmd}: exit={code} stdout={} stderr={}",
            stdout.trim_end(),
            stderr.trim_end()
        )))
    }
}

// ─── Minimal cron matcher ──────────────────────────────────────────────────
//
// Supports: "m h dom mon dow" with each field being either `*`, a literal
// integer, or a comma-separated list of integers. Enough for daily sleep
// at midnight, wake at 7am, etc. Ranges + step syntax are deferred.

fn cron_matches(expr: &str, now: chrono::DateTime<chrono::Utc>) -> Result<bool, PowerError> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(PowerError::CronParse(format!(
            "expected 5 fields, got {} for '{}'",
            parts.len(),
            expr
        )));
    }

    let (minute, hour, dom, month, dow) = (parts[0], parts[1], parts[2], parts[3], parts[4]);

    let m = now.format("%M").to_string().parse::<u32>().unwrap_or(0);
    let h = now.format("%H").to_string().parse::<u32>().unwrap_or(0);
    let d = now.format("%d").to_string().parse::<u32>().unwrap_or(0);
    let mo = now.format("%m").to_string().parse::<u32>().unwrap_or(0);
    // Cron dow: 0=Sunday..6=Saturday. chrono's weekday().num_days_from_sunday()
    // returns 0..=6 with same convention.
    let w = chrono::Datelike::weekday(&now).num_days_from_sunday();

    let ok = field_matches(minute, m)?
        && field_matches(hour, h)?
        && field_matches(dom, d)?
        && field_matches(month, mo)?
        && field_matches(dow, w)?;
    Ok(ok)
}

fn field_matches(field: &str, value: u32) -> Result<bool, PowerError> {
    let field = field.trim();
    if field == "*" {
        return Ok(true);
    }
    for token in field.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let parsed: u32 = token
            .parse()
            .map_err(|e| PowerError::CronParse(format!("bad cron token '{token}': {e}")))?;
        if parsed == value {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn cron_wildcard_matches_any() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 4, 18, 23, 45, 0)
            .unwrap();
        assert!(cron_matches("* * * * *", now).unwrap());
    }

    #[test]
    fn cron_literal_matches() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 4, 18, 0, 0, 0).unwrap();
        assert!(cron_matches("0 0 * * *", now).unwrap());
        assert!(!cron_matches("30 0 * * *", now).unwrap());
    }

    #[test]
    fn cron_list_field() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 4, 18, 7, 0, 0).unwrap();
        assert!(cron_matches("0 7,19 * * *", now).unwrap());
    }

    #[test]
    fn bad_field_count_errors() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 4, 18, 0, 0, 0).unwrap();
        let err = cron_matches("0 0 *", now).unwrap_err();
        assert!(matches!(err, PowerError::CronParse(_)));
    }
}
