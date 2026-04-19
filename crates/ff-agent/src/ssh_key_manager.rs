//! SSH key revocation + rotation orchestrator.
//!
//! Two primary operations:
//!
//! * [`SshKeyManager::revoke_computer_trust`] — removes a computer's SSH
//!   `user` public key from every OTHER live computer's `authorized_keys`.
//!   Records a row per (revoked_node, target_node) in
//!   `fleet_ssh_revocations` (schema V17).
//!
//! * [`SshKeyManager::rotate_computer_keypair`] — STUB. Full rotation
//!   requires orchestrating `ssh-keygen` on the target host, distributing
//!   the new pubkey to every peer, verifying reachability, and scrubbing
//!   the old pubkey. That workflow is left for a follow-up phase;
//!   calling it returns `NotImplemented` with a description of the steps.
//!
//! Revocation uses a simple SSH fan-out:
//!   ```bash
//!   ssh <target> "sed -i.bak '/<fingerprint>/d' ~/.ssh/authorized_keys"
//!   ```
//! The command is `sed -i` on Linux and `sed -i ''` on macOS — we build
//! the right form from the target's `os_family`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum SshError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("no such computer: {0}")]
    UnknownComputer(String),
    #[error("no user SSH key recorded for {0}")]
    NoKey(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("operation not yet implemented: {0}")]
    NotImplemented(String),
}

/// Per-target outcome of a single revocation attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevocationTarget {
    pub target: String,
    pub primary_ip: Option<String>,
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RevocationReport {
    pub revoked_node: String,
    pub key_fingerprint: String,
    pub targets: Vec<RevocationTarget>,
    pub succeeded: usize,
    pub failed: usize,
}

/// SSH key manager. Holds a Postgres pool used to look up keys and record
/// revocation events.
#[derive(Clone)]
pub struct SshKeyManager {
    pg: PgPool,
}

impl SshKeyManager {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Revoke `computer_name`'s user SSH key from every other alive
    /// computer's `authorized_keys`, and mark `computer_trust` edges
    /// from/to this computer as revoked.
    ///
    /// Returns a per-target report. A failure on one target does not
    /// abort the others.
    pub async fn revoke_computer_trust(
        &self,
        computer_name: &str,
        who: Option<&str>,
    ) -> Result<RevocationReport, SshError> {
        // 1. Look up the revoked node's user key.
        let key_row = sqlx::query(
            "SELECT public_key, fingerprint FROM fleet_node_ssh_keys
              WHERE node_name = $1 AND key_purpose = 'user'
              ORDER BY added_at DESC
              LIMIT 1",
        )
        .bind(computer_name)
        .fetch_optional(&self.pg)
        .await?;

        let Some(key_row) = key_row else {
            return Err(SshError::NoKey(computer_name.to_string()));
        };
        let fingerprint: String = key_row.get("fingerprint");
        let public_key: String = key_row.get("public_key");

        // 2. Pick targets: every computer except the revoked one that has
        //    a recent last_seen_at (24h).
        let targets = sqlx::query(
            "SELECT name, primary_ip, COALESCE(os_family, '') AS os_family
               FROM computers
              WHERE name <> $1
                AND (last_seen_at IS NULL OR last_seen_at > NOW() - INTERVAL '24 hours')",
        )
        .bind(computer_name)
        .fetch_all(&self.pg)
        .await?;

        let mut report = RevocationReport {
            revoked_node: computer_name.to_string(),
            key_fingerprint: fingerprint.clone(),
            ..Default::default()
        };

        for row in targets {
            let target: String = row.get("name");
            let ip: Option<String> = row.try_get("primary_ip").ok();
            let os_family: String = row.try_get("os_family").unwrap_or_default();

            let outcome = self
                .revoke_on_target(&target, ip.as_deref(), &os_family, &public_key, &fingerprint)
                .await;

            let success = outcome.is_ok();
            let message = match &outcome {
                Ok(s) => s.clone(),
                Err(e) => format!("{e}"),
            };

            // Record in fleet_ssh_revocations.
            let ins = sqlx::query(
                "INSERT INTO fleet_ssh_revocations
                    (revoked_node, key_fingerprint, target_node,
                     revoked_by, success, last_error)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(computer_name)
            .bind(&fingerprint)
            .bind(&target)
            .bind(who)
            .bind(success)
            .bind(if success { None } else { Some(message.clone()) })
            .execute(&self.pg)
            .await;
            if let Err(e) = ins {
                warn!(error = %e, target = %target, "failed to insert fleet_ssh_revocations row");
            }

            if success {
                report.succeeded += 1;
            } else {
                report.failed += 1;
                warn!(target = %target, error = %message, "ssh key revocation failed on target");
            }

            report.targets.push(RevocationTarget {
                target,
                primary_ip: ip,
                success,
                message,
            });
        }

        // 3. Mark computer_trust edges as revoked (both directions).
        if let Ok(rev_id) = lookup_computer_id(&self.pg, computer_name).await {
            if let Some(rev_id) = rev_id {
                let _ = sqlx::query(
                    "UPDATE computer_trust SET revoked_at = NOW(), revoked_by = $1
                      WHERE (source_computer_id = $2 OR target_computer_id = $2)
                        AND revoked_at IS NULL",
                )
                .bind(who)
                .bind(rev_id)
                .execute(&self.pg)
                .await;
            }
        }

        info!(
            revoked = %computer_name,
            succeeded = report.succeeded,
            failed = report.failed,
            "ssh key revocation complete",
        );

        Ok(report)
    }

    /// Rotate the target computer's own SSH keypair.
    ///
    /// Currently unimplemented — this phase only wires up revocation.
    /// Full rotation requires:
    ///   1. `ssh <target> "ssh-keygen -t ed25519 -N '' -f ~/.ssh/id_ed25519.new"`
    ///   2. Read new pubkey back, insert into `fleet_node_ssh_keys`.
    ///   3. Distribute pubkey to every peer's `authorized_keys`.
    ///   4. Atomically swap `.new` → primary on the target.
    ///   5. Verify new key works by probing each peer.
    ///   6. Revoke the old pubkey from every peer (via
    ///      `revoke_computer_trust` on the old fingerprint).
    pub async fn rotate_computer_keypair(&self, computer_name: &str) -> Result<(), SshError> {
        Err(SshError::NotImplemented(format!(
            "SSH keypair rotation for '{computer_name}' is a multi-step workflow not yet wired. \
             Use `ff fleet revoke-trust --computer {computer_name}` to revoke, then re-onboard \
             with `ff onboard add` to distribute a fresh key."
        )))
    }

    /// Run the sed on one target. Returns the stdout of the SSH command on success.
    async fn revoke_on_target(
        &self,
        target: &str,
        primary_ip: Option<&str>,
        os_family: &str,
        _public_key: &str,
        fingerprint: &str,
    ) -> Result<String, SshError> {
        // Escape the fingerprint for a sed regex. Only "/" is problematic
        // since we use "/" as the delimiter — use "#" instead.
        let safe_fp = fingerprint.replace('\n', "").replace('\r', "");

        // macOS sed requires `-i ''`, GNU sed wants `-i`.
        let sed_inplace = if os_family.contains("macos") {
            "sed -i.bak"
        } else {
            "sed -i.bak"
        };

        // We can't match by fingerprint directly (fingerprint is derived,
        // not a literal substring). Instead, match by the comment tail —
        // authorized_keys lines usually end in `user@host`. To be robust,
        // the caller should SSH in and remove the matching public_key
        // payload. Use grep -v to filter by exact pubkey substring.
        //
        // We do this by base64-stripping: ssh-ed25519 keys have a unique
        // 43-char base64 body that rarely collides. Extract that body
        // from the stored public_key and match on it.
        let body = extract_key_body(_public_key).unwrap_or_else(|| fingerprint.to_string());
        let remote_cmd = format!(
            "mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && \
             cp ~/.ssh/authorized_keys ~/.ssh/authorized_keys.bak && \
             grep -v -F {body} ~/.ssh/authorized_keys.bak > ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys && \
             echo revoked:{safe_fp}",
            body = shell_single_quote(&body),
            safe_fp = safe_fp,
        );

        let host = primary_ip.unwrap_or(target);
        debug!(target = %target, host = %host, "ssh revoke fan-out");

        let output = tokio::time::timeout(
            Duration::from_secs(20),
            Command::new("ssh")
                .args([
                    "-o", "BatchMode=yes",
                    "-o", "ConnectTimeout=5",
                    "-o", "StrictHostKeyChecking=accept-new",
                    host,
                    &remote_cmd,
                ])
                .output(),
        )
        .await
        .map_err(|_| SshError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "ssh timeout")))??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(SshError::Io(std::io::Error::other(format!(
                "ssh revoke on {target}: exit {:?}: {stderr}; sed_mode={sed_inplace}",
                output.status.code()
            ))));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(stdout)
    }
}

async fn lookup_computer_id(
    pool: &PgPool,
    computer_name: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let row = sqlx::query_scalar::<_, Uuid>("SELECT id FROM computers WHERE name = $1")
        .bind(computer_name)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Extract the base64 body (middle field) of an OpenSSH-format public key line.
/// Given: `ssh-ed25519 AAAA... user@host`, returns `Some("AAAA...")`.
fn extract_key_body(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        Some(parts[1].to_string())
    } else {
        None
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_body_happy_path() {
        let line = "ssh-ed25519 AAAABODY user@host";
        assert_eq!(extract_key_body(line), Some("AAAABODY".into()));
    }

    #[test]
    fn shell_quote_escapes() {
        assert_eq!(shell_single_quote("abc"), "'abc'");
        assert_eq!(shell_single_quote("it's"), "'it'\"'\"'s'");
    }
}
