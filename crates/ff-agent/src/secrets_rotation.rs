//! Secrets rotation — scans `fleet_secrets` for near-expiry keys, dispatches
//! alert events, and supports manual or automatic rotation.
//!
//! Extended by schema V17 with:
//!   - `expires_at`          — NULL = never expires
//!   - `rotate_before_days`  — warning window before expiry (default 90)
//!   - `rotation_count`      — incremented on each rotation
//!   - `last_rotated_at`     — when we last rotated
//!
//! The rotator runs on the leader only (daily check). Rotation is triggered
//! manually via `ff secrets rotate <key>` — the loop only emits alerts.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("no such secret: {0}")]
    NotFound(String),
    #[error("cannot generate value for unknown key type: {0}")]
    UnknownKeyType(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A single near-expiry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpiringSecret {
    pub key: String,
    pub description: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub rotate_before_days: i32,
    pub days_remaining: Option<i64>,
    pub rotation_count: i32,
    pub last_rotated_at: Option<DateTime<Utc>>,
}

/// Result of one `check_expirations` pass.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RotationReport {
    pub near_expiry: Vec<ExpiringSecret>,
    pub already_expired: Vec<ExpiringSecret>,
    pub alerts_dispatched: usize,
}

/// Periodic rotator. `check_expirations` is idempotent.
#[derive(Clone)]
pub struct SecretsRotator {
    pg: PgPool,
}

impl SecretsRotator {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Scan `fleet_secrets` for rows within `expires_at < NOW() + rotate_before_days`.
    /// For each one, insert an `alert_events` row pointing at the
    /// `secret_expiring_soon` policy (if it exists).
    pub async fn check_expirations(&self) -> Result<RotationReport, SecretsError> {
        let rows = sqlx::query(
            "SELECT key, description, expires_at, rotate_before_days,
                    rotation_count, last_rotated_at,
                    EXTRACT(EPOCH FROM (expires_at - NOW())) / 86400.0 AS days_remaining
             FROM fleet_secrets
             WHERE expires_at IS NOT NULL
               AND expires_at < NOW() + make_interval(days => rotate_before_days)
             ORDER BY expires_at ASC",
        )
        .fetch_all(&self.pg)
        .await?;

        let mut report = RotationReport::default();
        let now = Utc::now();

        // Find policy id once (may be missing until seed runs).
        let policy_id: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM alert_policies WHERE name = 'secret_expiring_soon' LIMIT 1",
        )
        .fetch_optional(&self.pg)
        .await
        .ok()
        .flatten();

        for r in rows {
            let expires_at: Option<DateTime<Utc>> = r.try_get("expires_at").ok();
            let days_remaining: Option<f64> = r.try_get("days_remaining").ok();
            let days_remaining_i: Option<i64> = days_remaining.map(|d| d as i64);

            let row = ExpiringSecret {
                key: r.get("key"),
                description: r.try_get("description").ok(),
                expires_at,
                rotate_before_days: r.get("rotate_before_days"),
                days_remaining: days_remaining_i,
                rotation_count: r.get("rotation_count"),
                last_rotated_at: r.try_get("last_rotated_at").ok(),
            };

            let expired = expires_at.map(|t| t < now).unwrap_or(false);

            warn!(
                key = %row.key,
                days_remaining = ?row.days_remaining,
                expired,
                "fleet secret near/past expiry — rotate with `ff secrets rotate {}`",
                row.key,
            );

            // Dispatch an alert_event if policy is seeded.
            if let Some(pid) = policy_id {
                let msg = format!(
                    "Secret '{}' expires in {} days (at {})",
                    row.key,
                    row.days_remaining.unwrap_or(0),
                    row.expires_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "-".into()),
                );
                let insert = sqlx::query(
                    "INSERT INTO alert_events (policy_id, value, value_text, message, channel_result)
                     VALUES ($1, $2, $3, $4, 'pending')",
                )
                .bind(pid)
                .bind(row.days_remaining.map(|v| v as f64))
                .bind(&row.key)
                .bind(&msg)
                .execute(&self.pg)
                .await;
                if let Err(e) = insert {
                    debug!(error = %e, "failed to insert alert_event for expiring secret");
                } else {
                    report.alerts_dispatched += 1;
                }
            }

            if expired {
                report.already_expired.push(row);
            } else {
                report.near_expiry.push(row);
            }
        }
        Ok(report)
    }

    /// Rotate a single secret. If `new_value` is None, a fresh value is
    /// generated based on the key's shape (see [`generate_for_key`]).
    pub async fn rotate(
        &self,
        key: &str,
        new_value: Option<String>,
    ) -> Result<RotationOutcome, SecretsError> {
        let current = sqlx::query(
            "SELECT rotate_before_days, rotation_count FROM fleet_secrets WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pg)
        .await?;

        let Some(row) = current else {
            return Err(SecretsError::NotFound(key.to_string()));
        };
        let rotate_before_days: i32 = row.get("rotate_before_days");

        let (value, kind) = match new_value {
            Some(v) => (v, "provided"),
            None => {
                let generated = generate_for_key(key)?;
                (generated.value, generated.kind)
            }
        };

        // Store the rotated value and bump bookkeeping.
        sqlx::query(
            "UPDATE fleet_secrets SET
                value            = $1,
                rotation_count   = rotation_count + 1,
                last_rotated_at  = NOW(),
                expires_at       = NOW() + make_interval(days => $2),
                updated_at       = NOW(),
                updated_by       = COALESCE($3, updated_by)
             WHERE key = $4",
        )
        .bind(&value)
        .bind(rotate_before_days)
        .bind(Some(format!("secrets_rotation:{kind}")))
        .bind(key)
        .execute(&self.pg)
        .await?;

        // Compute a fingerprint for logs (never log value).
        let mut hasher = Sha256::new();
        hasher.update(value.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let short = digest[..12].to_string();

        info!(
            key = %key,
            kind = kind,
            bytes = value.len(),
            sha12 = %short,
            "secret rotated",
        );

        Ok(RotationOutcome {
            key: key.to_string(),
            kind: kind.to_string(),
            new_len: value.len(),
            new_fingerprint: short,
        })
    }

    /// Spawn the daily check loop. Runs `check_expirations` every
    /// `interval_hours`. Only runs on the leader — callers should gate
    /// with `pg_get_current_leader` before spawning.
    pub fn spawn(self, interval_hours: u64, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Start with a short delay to avoid racing migrations at boot.
            tokio::time::sleep(Duration::from_secs(30)).await;
            let period = Duration::from_secs(interval_hours.max(1) * 3600);
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match self.check_expirations().await {
                            Ok(report) => {
                                info!(
                                    near = report.near_expiry.len(),
                                    expired = report.already_expired.len(),
                                    alerts = report.alerts_dispatched,
                                    "secrets_rotation tick",
                                );
                            }
                            Err(e) => {
                                error!(error = %e, "secrets_rotation check failed");
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!("secrets_rotation shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}

/// Outcome of a single `rotate()`. `new_fingerprint` is the first 12 hex
/// chars of SHA-256(value) — safe to log.
#[derive(Debug, Clone, Serialize)]
pub struct RotationOutcome {
    pub key: String,
    pub kind: String,
    pub new_len: usize,
    pub new_fingerprint: String,
}

/// Generated-value result.
pub struct Generated {
    pub value: String,
    pub kind: &'static str,
}

/// Generate a new value for a secret key based on its name.
///
/// - Keys ending in `.ssh_pubkey` / `.ssh_privkey` → delegate caller (we
///   cannot generate a standalone pubkey without a matching privkey).
/// - Keys ending in `.hmac_key` → 32 bytes, hex.
/// - Keys ending in `.token`, `.api_key`, `.password`, `.secret` → 32 bytes hex.
/// - Anything else → 32 bytes hex (safe default).
pub fn generate_for_key(key: &str) -> Result<Generated, SecretsError> {
    if key.ends_with(".ssh_privkey") || key.ends_with(".ssh_pubkey") {
        // SSH rotation is driven by ssh_key_manager; refuse to generate
        // a standalone half-pair.
        return Err(SecretsError::UnknownKeyType(
            "ssh keypair — use `ff fleet rotate-ssh-key` instead".into(),
        ));
    }

    // Default: 32-byte random, hex-encoded (64 chars).
    let kind = if key.ends_with(".hmac_key") {
        "random_hmac"
    } else if key.ends_with(".token") || key.ends_with(".api_key") {
        "random_token"
    } else if key.ends_with(".password") || key.ends_with(".secret") {
        "random_password"
    } else {
        "random_generic"
    };

    let hex = random_hex(32);
    Ok(Generated { value: hex, kind })
}

/// Produce `n` random bytes, hex-encoded. Uses /dev/urandom on Unix; falls
/// back to a SHA256 PRNG seeded with (nanos, pid) if that fails.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if read_urandom(&mut buf).is_err() {
        // Fallback PRNG — not cryptographically strong but better than
        // panicking in a dev environment.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        let mut seed = Sha256::new();
        seed.update(now.to_le_bytes());
        seed.update(pid.to_le_bytes());
        let mut state = seed.finalize().to_vec();
        let mut i = 0;
        while i < buf.len() {
            let mut h = Sha256::new();
            h.update(&state);
            state = h.finalize().to_vec();
            let take = state.len().min(buf.len() - i);
            buf[i..i + take].copy_from_slice(&state[..take]);
            i += take;
        }
    }
    hex_encode(&buf)
}

fn read_urandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_has_correct_length() {
        let h = random_hex(32);
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_for_key_refuses_ssh() {
        assert!(generate_for_key("foo.ssh_privkey").is_err());
    }

    #[test]
    fn generate_for_key_defaults_to_random_hex() {
        let g = generate_for_key("github.token").unwrap();
        assert_eq!(g.value.len(), 64);
        assert_eq!(g.kind, "random_token");
    }
}
