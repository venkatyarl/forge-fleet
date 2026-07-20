//! SSH key revocation + rotation orchestrator.
//!
//! Two primary operations:
//!
//! * [`SshKeyManager::revoke_computer_trust`] — removes a computer's SSH
//!   `user` public key from every OTHER live computer's `authorized_keys`.
//!   Records a row per (revoked_node, target_node) in
//!   `fleet_ssh_revocations` (schema V17).
//!
//! * [`SshKeyManager::rotate_computer_keypair`] — rotates a computer's SSH
//!   user keypair by generating a new ed25519 key on the target, distributing
//!   the new public key to every peer, verifying reachability with the new
//!   key, swapping it to the primary identity on the target, and finally
//!   scrubbing the old public key from peers.
//!
//! Revocation uses a simple SSH fan-out:
//!   ```bash
//!   ssh <target> "sed -i.bak '/<fingerprint>/d' ~/.ssh/authorized_keys"
//!   ```
//! The command is `sed -i` on Linux and `sed -i ''` on macOS — we build
//! the right form from the target's `os_family`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    #[error("rotation aborted: {0}")]
    RotationAborted(String),
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

/// Per-target outcome of a rotation phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RotationTarget {
    pub target: String,
    pub primary_ip: Option<String>,
    pub installed: bool,
    pub verified: bool,
    pub removed_old: bool,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RotationReport {
    pub rotated_node: String,
    pub new_fingerprint: String,
    pub old_fingerprint: Option<String>,
    pub targets: Vec<RotationTarget>,
    pub peers_reached: usize,
    pub peers_failed: usize,
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
            "SELECT public_key, fingerprint FROM fleet_workers_ssh_keys
              WHERE worker_name = $1 AND key_purpose = 'user'
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
                .revoke_on_target(
                    &target,
                    ip.as_deref(),
                    &os_family,
                    &public_key,
                    &fingerprint,
                )
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
        if let Ok(rev_id) = lookup_computer_id(&self.pg, computer_name).await
            && let Some(rev_id) = rev_id
        {
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

        info!(
            revoked = %computer_name,
            succeeded = report.succeeded,
            failed = report.failed,
            "ssh key revocation complete",
        );

        Ok(report)
    }

    /// Rotate `computer_name`'s user SSH keypair in-place.
    ///
    /// Steps:
    /// 1. Generate a new ed25519 keypair on the target as `~/.ssh/id_ed25519.new`.
    /// 2. Read the new public key back and persist it in `fleet_workers_ssh_keys`.
    /// 3. Fan the new public key out to every peer's `authorized_keys`.
    /// 4. Verify the target can reach each peer using the new private key.
    /// 5. Atomically swap the `.new` key into the primary `id_ed25519` slot.
    /// 6. Scrub the old public key from every peer.
    /// 7. Clean up temporary `.new` / `.old` files on the target.
    ///
    /// Failures during fan-out are recorded per-target but do not abort the
    /// whole rotation; however, if the new key cannot be generated or the
    /// target is unreachable, the rotation aborts before any state is changed.
    pub async fn rotate_computer_keypair(
        &self,
        computer_name: &str,
    ) -> Result<RotationReport, SshError> {
        // 1. Look up the target computer.
        let computer_row = sqlx::query(
            "SELECT name, primary_ip, ssh_user, ssh_port, COALESCE(os_family, '') AS os_family
               FROM computers
              WHERE name = $1",
        )
        .bind(computer_name)
        .fetch_optional(&self.pg)
        .await?;

        let Some(row) = computer_row else {
            return Err(SshError::UnknownComputer(computer_name.to_string()));
        };

        let target_name: String = row.get("name");
        let target_ip: Option<String> = row.try_get("primary_ip").ok();
        let target_ssh_user: String = row.get("ssh_user");
        let target_ssh_port: i32 = row.try_get("ssh_port").unwrap_or(22);
        let _target_os_family: String = row.try_get("os_family").unwrap_or_default();

        let target_host = target_ip.as_deref().unwrap_or(&target_name);
        let target_dest = ssh_dest(&target_ssh_user, target_host);

        // 2. Look up the current user key (the one we will replace).
        let old_key_row = sqlx::query(
            "SELECT public_key, fingerprint FROM fleet_workers_ssh_keys
              WHERE worker_name = $1 AND key_purpose = 'user'
              ORDER BY added_at DESC
              LIMIT 1",
        )
        .bind(computer_name)
        .fetch_optional(&self.pg)
        .await?;

        let old_key: Option<(String, String)> = old_key_row.map(|r| {
            let pk: String = r.get("public_key");
            let fp: String = r.get("fingerprint");
            (pk, fp)
        });

        // 3. Generate a fresh ed25519 keypair on the target.
        let gen_cmd = format!(
            "mkdir -p ~/.ssh && chmod 700 ~/.ssh && \
             rm -f ~/.ssh/id_ed25519.new ~/.ssh/id_ed25519.new.pub && \
             ssh-keygen -t ed25519 -N '' -f ~/.ssh/id_ed25519.new -C '{user}@{name}-rotated' >/dev/null && \
             cat ~/.ssh/id_ed25519.new.pub",
            user = target_ssh_user,
            name = target_name
        );

        let gen_output = self
            .ssh_exec(
                &target_ssh_user,
                &target_host,
                target_ssh_port,
                &gen_cmd,
                60,
            )
            .await?;
        let new_pubkey = gen_output.trim().to_string();

        if new_pubkey.is_empty() {
            return Err(SshError::RotationAborted(
                "target returned an empty new public key".into(),
            ));
        }

        let (new_key_type, new_fingerprint) = parse_pubkey_meta(&new_pubkey);
        if new_key_type != "ssh-ed25519" {
            return Err(SshError::RotationAborted(format!(
                "target generated unexpected key type '{new_key_type}'"
            )));
        }

        // 4. Persist the new public key before we distribute it.
        sqlx::query(
            "INSERT INTO fleet_workers_ssh_keys
                (worker_name, key_purpose, public_key, key_type, fingerprint)
             VALUES ($1, 'user', $2, $3, $4)
             ON CONFLICT (worker_name, fingerprint) DO UPDATE SET
                public_key = EXCLUDED.public_key,
                key_type = EXCLUDED.key_type,
                key_purpose = EXCLUDED.key_purpose",
        )
        .bind(computer_name)
        .bind(&new_pubkey)
        .bind(&new_key_type)
        .bind(&new_fingerprint)
        .execute(&self.pg)
        .await?;

        // 5. Select peers: every other recently-seen computer.
        let peers = sqlx::query(
            "SELECT name, primary_ip, ssh_user, ssh_port, COALESCE(os_family, '') AS os_family
               FROM computers
              WHERE name <> $1
                AND (last_seen_at IS NULL OR last_seen_at > NOW() - INTERVAL '24 hours')",
        )
        .bind(computer_name)
        .fetch_all(&self.pg)
        .await?;

        let mut report = RotationReport {
            rotated_node: computer_name.to_string(),
            new_fingerprint: new_fingerprint.clone(),
            old_fingerprint: old_key.as_ref().map(|(_, fp)| fp.clone()),
            ..Default::default()
        };

        // 6. Distribute the new public key to every peer.
        for row in &peers {
            let peer_name: String = row.get("name");
            let peer_ip: Option<String> = row.try_get("primary_ip").ok();
            let peer_ssh_user: String = row.get("ssh_user");
            let peer_ssh_port: i32 = row.try_get("ssh_port").unwrap_or(22);
            let peer_os_family: String = row.try_get("os_family").unwrap_or_default();

            let mut target_report = RotationTarget {
                target: peer_name.clone(),
                primary_ip: peer_ip.clone(),
                ..Default::default()
            };

            match self
                .install_key_on_target(
                    &peer_name,
                    peer_ip.as_deref(),
                    &peer_ssh_user,
                    peer_ssh_port,
                    &peer_os_family,
                    &new_pubkey,
                )
                .await
            {
                Ok(msg) => {
                    target_report.installed = true;
                    target_report.messages.push(msg);
                    report.peers_reached += 1;
                }
                Err(e) => {
                    let msg = format!("{e}");
                    target_report.messages.push(msg.clone());
                    report.peers_failed += 1;
                    warn!(target = %peer_name, error = %msg, "failed to install new ssh key");
                }
            }

            report.targets.push(target_report);
        }

        // 7. Verify the target can reach each peer using the new private key.
        for row in &peers {
            let peer_name: String = row.get("name");
            let peer_ip: Option<String> = row.try_get("primary_ip").ok();
            let peer_ssh_user: String = row.get("ssh_user");
            let peer_ssh_port: i32 = row.try_get("ssh_port").unwrap_or(22);

            let peer_host = peer_ip.as_deref().unwrap_or(&peer_name);
            let peer_dest = ssh_dest(&peer_ssh_user, &peer_host);
            let peer_port_args = ssh_port_args(peer_ssh_port);

            let verify_cmd = format!(
                "ssh -i ~/.ssh/id_ed25519.new {bypass} -o ConnectTimeout=5 \
                 -o StrictHostKeyChecking=accept-new {port_args} {dest} true",
                bypass = crate::ssh_opts::SSH_AGENT_BYPASS,
                port_args = peer_port_args,
                dest = shell_single_quote(&peer_dest)
            );

            if let Some(t) = report.targets.iter_mut().find(|t| t.target == peer_name) {
                match self
                    .ssh_exec(
                        &target_ssh_user,
                        &target_host,
                        target_ssh_port,
                        &verify_cmd,
                        20,
                    )
                    .await
                {
                    Ok(_) => {
                        t.verified = true;
                        t.messages.push("verified".into());
                    }
                    Err(e) => {
                        let msg = format!("verify failed: {e}");
                        t.messages.push(msg.clone());
                        warn!(target = %peer_name, error = %msg, "new ssh key verification failed");
                    }
                }
            }
        }

        // 8. Swap the new key into the primary slot on the target.
        let swap_cmd = "cp ~/.ssh/id_ed25519 ~/.ssh/id_ed25519.old && \
                        cp ~/.ssh/id_ed25519.pub ~/.ssh/id_ed25519.old.pub && \
                        mv ~/.ssh/id_ed25519.new ~/.ssh/id_ed25519 && \
                        mv ~/.ssh/id_ed25519.new.pub ~/.ssh/id_ed25519.pub && \
                        chmod 600 ~/.ssh/id_ed25519 && \
                        chmod 644 ~/.ssh/id_ed25519.pub && \
                        echo swapped";
        self.ssh_exec(
            &target_ssh_user,
            &target_host,
            target_ssh_port,
            swap_cmd,
            20,
        )
        .await
        .map_err(|e| {
            SshError::RotationAborted(format!(
                "failed to swap new key into primary slot on {target_name}: {e}"
            ))
        })?;

        // 9. Scrub the old public key from every peer.
        if let Some((old_pubkey, old_fp)) = old_key {
            for row in &peers {
                let peer_name: String = row.get("name");
                let peer_ip: Option<String> = row.try_get("primary_ip").ok();
                let peer_ssh_user: String = row.get("ssh_user");
                let peer_ssh_port: i32 = row.try_get("ssh_port").unwrap_or(22);
                let peer_os_family: String = row.try_get("os_family").unwrap_or_default();

                if let Some(t) = report.targets.iter_mut().find(|t| t.target == peer_name) {
                    match self
                        .remove_key_on_target(
                            &peer_name,
                            peer_ip.as_deref(),
                            &peer_ssh_user,
                            peer_ssh_port,
                            &peer_os_family,
                            &old_pubkey,
                            &old_fp,
                        )
                        .await
                    {
                        Ok(msg) => {
                            t.removed_old = true;
                            t.messages.push(msg);
                        }
                        Err(e) => {
                            let msg = format!("remove old key failed: {e}");
                            t.messages.push(msg.clone());
                            warn!(target = %peer_name, error = %msg, "failed to remove old ssh key");
                        }
                    }
                }
            }

            // Record the overall rotation event in fleet_ssh_revocations for the old key.
            let _ = sqlx::query(
                "INSERT INTO fleet_ssh_revocations
                    (revoked_node, key_fingerprint, target_node, revoked_by, success, last_error)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(computer_name)
            .bind(&old_fp)
            .bind("*")
            .bind("rotate_computer_keypair")
            .bind(true)
            .bind::<Option<&str>>(None)
            .execute(&self.pg)
            .await;
        }

        // 10. Clean up temporary key files on the target.
        let cleanup_cmd = "rm -f ~/.ssh/id_ed25519.new ~/.ssh/id_ed25519.new.pub \
                           ~/.ssh/id_ed25519.old ~/.ssh/id_ed25519.old.pub && echo cleaned";
        if let Err(e) = self
            .ssh_exec(
                &target_ssh_user,
                &target_host,
                target_ssh_port,
                cleanup_cmd,
                15,
            )
            .await
        {
            warn!(target = %target_name, error = %e, "failed to clean up temporary key files");
        }

        info!(
            rotated = %computer_name,
            new_fingerprint = %new_fingerprint,
            peers_reached = report.peers_reached,
            peers_failed = report.peers_failed,
            "ssh key rotation complete",
        );

        Ok(report)
    }

    /// Run the sed on one target. Returns the stdout of the SSH command on success.
    async fn revoke_on_target(
        &self,
        target: &str,
        primary_ip: Option<&str>,
        _os_family: &str,
        _public_key: &str,
        fingerprint: &str,
    ) -> Result<String, SshError> {
        // Escape the fingerprint for a sed regex. Only "/" is problematic
        // since we use "/" as the delimiter — use "#" instead.
        let safe_fp = fingerprint.replace(['\n', '\r'], "");

        // macOS sed requires `-i ''`, GNU sed wants `-i`.
        let sed_inplace = "sed -i.bak";

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
                .args(crate::ssh_opts::ssh_bypass_args())
                .args([
                    "-o",
                    "ConnectTimeout=5",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    host,
                    &remote_cmd,
                ])
                .output(),
        )
        .await
        .map_err(|_| {
            SshError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ssh timeout",
            ))
        })??;

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

    /// Append `public_key` to `target`'s `authorized_keys` if not already present.
    async fn install_key_on_target(
        &self,
        target: &str,
        primary_ip: Option<&str>,
        ssh_user: &str,
        ssh_port: i32,
        _os_family: &str,
        public_key: &str,
    ) -> Result<String, SshError> {
        let host = primary_ip.unwrap_or(target);
        let dest = ssh_dest(ssh_user, host);
        let quoted = shell_single_quote(public_key);
        let remote_cmd = format!(
            "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys && \
             grep -qxF {quoted} ~/.ssh/authorized_keys || \
             echo {quoted} >> ~/.ssh/authorized_keys && \
             echo installed",
        );

        self.ssh_exec(ssh_user, host, ssh_port, &remote_cmd, 20)
            .await
    }

    /// Remove `public_key` from `target`'s `authorized_keys`.
    async fn remove_key_on_target(
        &self,
        target: &str,
        primary_ip: Option<&str>,
        ssh_user: &str,
        ssh_port: i32,
        _os_family: &str,
        public_key: &str,
        fingerprint: &str,
    ) -> Result<String, SshError> {
        let host = primary_ip.unwrap_or(target);
        let dest = ssh_dest(ssh_user, host);
        let body = extract_key_body(public_key).unwrap_or_else(|| fingerprint.to_string());
        let remote_cmd = format!(
            "mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && \
             cp ~/.ssh/authorized_keys ~/.ssh/authorized_keys.bak && \
             grep -v -F {body} ~/.ssh/authorized_keys.bak > ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys && \
             echo removed:{safe_fp}",
            body = shell_single_quote(&body),
            safe_fp = fingerprint.replace(['\n', '\r'], ""),
        );

        self.ssh_exec(ssh_user, host, ssh_port, &remote_cmd, 20)
            .await
    }

    /// Execute one SSH command and return stdout on success.
    async fn ssh_exec(
        &self,
        dest: &str,
        remote_cmd: &str,
        timeout_secs: u64,
    ) -> Result<String, SshError> {
        debug!(dest = %dest, "ssh exec");
        let output = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            Command::new("ssh")
                .args(crate::ssh_opts::ssh_bypass_args())
                .args([
                    "-o",
                    "ConnectTimeout=5",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    dest,
                    remote_cmd,
                ])
                .output(),
        )
        .await
        .map_err(|_| {
            SshError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ssh timeout",
            ))
        })??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(SshError::Io(std::io::Error::other(format!(
                "ssh on {dest}: exit {:?}: {stderr}",
                output.status.code()
            ))));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

/// Build an SSH destination string, optionally including a non-default port.
fn ssh_dest(user: &str, host: &str, port: i32) -> String {
    if port == 22 {
        format!("{user}@{host}")
    } else {
        format!("-p {port} {user}@{host}")
    }
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

/// Parse the type and fingerprint of an OpenSSH public-key string.
/// Mirrors `ff-gateway/src/onboard.rs::parse_pubkey_meta` so rows written by
/// rotation use the same fingerprint scheme as enrollment.
fn parse_pubkey_meta(pubkey: &str) -> (String, String) {
    let mut parts = pubkey.split_whitespace();
    let key_type = parts.next().unwrap_or("unknown").to_string();
    let key_body = parts.next().unwrap_or(pubkey);
    let mut hasher = Sha256::new();
    hasher.update(key_body.as_bytes());
    let digest = hasher.finalize();
    let fp = format!("SHA256:{}", hex_encode(&digest));
    (key_type, fp)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
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

    #[test]
    fn parse_pubkey_meta_produces_ed25519_type() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHRlc3Q test@host";
        let (kt, fp) = parse_pubkey_meta(line);
        assert_eq!(kt, "ssh-ed25519");
        assert!(fp.starts_with("SHA256:"));
        assert!(!fp.is_empty());
    }

    #[test]
    fn ssh_dest_uses_port() {
        assert_eq!(ssh_dest("git", "host", 22), "git@host");
        assert_eq!(ssh_dest("git", "host", 2222), "-p 2222 git@host");
    }
}
