//! Shared NFS storage manager — declares fleet-wide exported volumes and
//! tracks which computers have them mounted.
//!
//! ## Scope note (v1)
//!
//! Real NFS setup is platform-specific:
//!   - **macOS**: the NFS server is `nfsd`. Exports live in `/etc/exports`
//!     (system-wide) or `/etc/nfs.conf` (per-user). After editing, run
//!     `sudo nfsd update`. `showmount -e` lists active exports.
//!   - **Linux (Ubuntu/DGX OS)**: the NFS server is `nfs-kernel-server`
//!     (apt). Exports live in `/etc/exports`. After editing, run
//!     `sudo exportfs -ra`.
//!   - **DGX OS / Ubuntu server**: same as Ubuntu; make sure the firewall
//!     opens ports 111/2049 (NFSv4) if enabled.
//!
//! This module writes the DB rows and (best-effort) attempts the shell
//! invocations, but operators should verify manually on the first run.
//! Client-side `mount -t nfs` syntax also differs between macOS and Linux;
//! we shell out through SSH and capture failures to the `last_error` column
//! on `shared_volume_mounts` for human review.
//!
//! ## Manual setup procedure
//!
//! 1. On the host (e.g. `taylor`):
//!    ```text
//!    sudo sh -c 'echo "/Users/venkat/models -network 192.168.5.0 -mask 255.255.255.0" >> /etc/exports'
//!    sudo nfsd update     # macOS
//!    # or:
//!    sudo exportfs -ra    # Linux
//!    ```
//! 2. On each client:
//!    ```text
//!    sudo mkdir -p ~/models
//!    sudo mount -t nfs taylor:/Users/venkat/models ~/models
//!    ```
//! 3. Then: `ff storage share create --host taylor --path /Users/venkat/models --name fleet-models --purpose models`

use sqlx::{PgPool, Row};
use thiserror::Error;
use tracing::{info, warn};

use crate::model_transfer::ssh_exec;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    #[error("ff-db: {0}")]
    FfDb(#[from] ff_db::DbError),
    #[error("host computer '{0}' not found")]
    HostNotFound(String),
    #[error("target computer '{0}' not found")]
    TargetNotFound(String),
    #[error("shared volume '{0}' not found")]
    VolumeNotFound(String),
    #[error("ssh: {0}")]
    Ssh(String),
    #[error("os-unsupported: {0}")]
    UnsupportedOs(String),
}

/// NFS client mount options shared by every code path that performs a client
/// mount.  `soft` causes NFS calls to return ETIMEDOUT instead of wedging the
/// process in uninterruptible D-state; `timeo=50` (5s) + `retrans=3` fail fast
/// enough that autofs can recover when the peer comes back.
const NFS_CLIENT_OPTS: &str = "soft,intr,timeo=50,retrans=3";

/// Info about a share + its mount fan-out.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareInfo {
    pub name: String,
    pub host: String,
    pub export_path: String,
    pub mount_path: String,
    pub nfs_version: String,
    pub purpose: Option<String>,
    pub read_only: bool,
    pub mounts: Vec<(String, String)>, // (computer_name, status)
}

pub struct SharedStorageManager {
    pg: PgPool,
}

impl SharedStorageManager {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Register (and best-effort configure) an NFS export on `host_name`.
    ///
    /// Writes the `shared_volumes` row, then optionally shells out via SSH
    /// to append `/etc/exports` and kick the NFS daemon. OS-specific logic
    /// is documented in the module-level comment.
    pub async fn create_share(
        &self,
        name: &str,
        host_name: &str,
        export_path: &str,
        mount_path: &str,
        purpose: Option<&str>,
        read_only: bool,
    ) -> Result<sqlx::types::Uuid, StorageError> {
        // Look up host computer id + ssh details.
        let host = fetch_computer(&self.pg, host_name)
            .await?
            .ok_or_else(|| StorageError::HostNotFound(host_name.into()))?;

        let id = ff_db::pg_create_shared_volume(
            &self.pg,
            name,
            host.id,
            export_path,
            mount_path,
            purpose,
            read_only,
        )
        .await?;

        info!(
            share = name,
            host = host_name,
            export_path,
            mount_path,
            "registered shared volume row; attempting NFS export (best effort)"
        );

        // Best-effort: attempt the platform-specific NFS export.
        // Failures are non-fatal — DB row exists either way so operators
        // can retry manually.
        if let Err(e) = configure_nfs_export(&host, export_path).await {
            warn!(
                share = name,
                host = host_name,
                error = %e,
                "NFS export setup failed — operator must configure /etc/exports manually"
            );
        }

        Ok(id)
    }

    /// Mount a named share on `computer_name`. Attempts the OS-specific
    /// `mount -t nfs` command over SSH and records the outcome in
    /// `shared_volume_mounts.status` + `.last_error`.
    pub async fn mount(
        &self,
        volume_name: &str,
        computer_name: &str,
        override_path: Option<&str>,
    ) -> Result<String, StorageError> {
        let volume = ff_db::pg_get_shared_volume(&self.pg, volume_name)
            .await?
            .ok_or_else(|| StorageError::VolumeNotFound(volume_name.into()))?;
        let target = fetch_computer(&self.pg, computer_name)
            .await?
            .ok_or_else(|| StorageError::TargetNotFound(computer_name.into()))?;
        let host = fetch_computer_by_id(&self.pg, volume.host_computer_id)
            .await?
            .ok_or_else(|| {
                StorageError::HostNotFound(format!(
                    "host computer {} for share {}",
                    volume.host_computer_id, volume.name
                ))
            })?;

        let mount_path = override_path
            .map(str::to_string)
            .unwrap_or_else(|| volume.mount_path.clone());

        // Record "mounting" so observers see the intent.
        ff_db::pg_upsert_shared_volume_mount(
            &self.pg,
            volume.id,
            target.id,
            Some(&mount_path),
            "mounting",
            None,
        )
        .await?;

        match attempt_client_mount(&target, &host, &volume.export_path, &mount_path).await {
            Ok(_) => {
                ff_db::pg_upsert_shared_volume_mount(
                    &self.pg,
                    volume.id,
                    target.id,
                    Some(&mount_path),
                    "mounted",
                    None,
                )
                .await?;
                info!(
                    share = %volume.name,
                    computer = %computer_name,
                    mount_path = %mount_path,
                    "NFS mount succeeded"
                );
                Ok(mount_path)
            }
            Err(e) => {
                let msg = e.to_string();
                ff_db::pg_upsert_shared_volume_mount(
                    &self.pg,
                    volume.id,
                    target.id,
                    Some(&mount_path),
                    "stale",
                    Some(&msg),
                )
                .await?;
                Err(e)
            }
        }
    }

    /// Unmount a named share on `computer_name`. Attempts `umount` over SSH
    /// and drops the `shared_volume_mounts` row on success.
    pub async fn unmount(
        &self,
        volume_name: &str,
        computer_name: &str,
    ) -> Result<(), StorageError> {
        let volume = ff_db::pg_get_shared_volume(&self.pg, volume_name)
            .await?
            .ok_or_else(|| StorageError::VolumeNotFound(volume_name.into()))?;
        let target = fetch_computer(&self.pg, computer_name)
            .await?
            .ok_or_else(|| StorageError::TargetNotFound(computer_name.into()))?;

        // Fetch effective mount_path from row, falling back to volume default.
        let mounts = ff_db::pg_list_shared_volume_mounts(&self.pg, Some(volume.id)).await?;
        let mount_path = mounts
            .iter()
            .find(|m| m.computer_id == target.id)
            .and_then(|m| m.mount_path.clone())
            .unwrap_or_else(|| volume.mount_path.clone());

        let cmd = format!("umount {} 2>&1 || true", shell_quote(&mount_path));
        let (code, stdout, stderr) = ssh_exec(&target.ssh_user, &target.primary_ip, &cmd)
            .await
            .map_err(StorageError::Ssh)?;

        if code == 0 {
            ff_db::pg_delete_shared_volume_mount(&self.pg, volume.id, target.id).await?;
            info!(
                share = %volume.name,
                computer = %computer_name,
                "NFS unmount succeeded"
            );
            Ok(())
        } else {
            let err = format!(
                "umount exit={code}: {}",
                stdout.trim_end().to_string() + &stderr
            );
            ff_db::pg_upsert_shared_volume_mount(
                &self.pg,
                volume.id,
                target.id,
                Some(&mount_path),
                "stale",
                Some(&err),
            )
            .await?;
            Err(StorageError::Ssh(err))
        }
    }

    /// List every registered share with its current mount fan-out.
    pub async fn list(&self) -> Result<Vec<ShareInfo>, StorageError> {
        let volumes = ff_db::pg_list_shared_volumes(&self.pg).await?;
        let all_mounts = ff_db::pg_list_shared_volume_mounts(&self.pg, None).await?;

        let mut out = Vec::with_capacity(volumes.len());
        for v in volumes {
            let mounts: Vec<(String, String)> = all_mounts
                .iter()
                .filter(|m| m.volume_id == v.id)
                .map(|m| {
                    (
                        m.computer_name.clone().unwrap_or_else(|| "?".into()),
                        m.status.clone(),
                    )
                })
                .collect();

            out.push(ShareInfo {
                name: v.name,
                host: v.host_name.unwrap_or_else(|| "?".into()),
                export_path: v.export_path,
                mount_path: v.mount_path,
                nfs_version: v.nfs_version,
                purpose: v.purpose,
                read_only: v.read_only,
                mounts,
            });
        }
        Ok(out)
    }
}

// ─── Internals ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ComputerInfo {
    id: sqlx::types::Uuid,
    name: String,
    primary_ip: String,
    ssh_user: String,
    os_family: String,
}

async fn fetch_computer(pool: &PgPool, name: &str) -> Result<Option<ComputerInfo>, StorageError> {
    let row = sqlx::query(
        "SELECT id, name, primary_ip, ssh_user, os_family
         FROM computers WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ComputerInfo {
        id: r.get("id"),
        name: r.get("name"),
        primary_ip: r.get("primary_ip"),
        ssh_user: r.get("ssh_user"),
        os_family: r.get("os_family"),
    }))
}

async fn fetch_computer_by_id(
    pool: &PgPool,
    id: sqlx::types::Uuid,
) -> Result<Option<ComputerInfo>, StorageError> {
    let row = sqlx::query(
        "SELECT id, name, primary_ip, ssh_user, os_family
         FROM computers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ComputerInfo {
        id: r.get("id"),
        name: r.get("name"),
        primary_ip: r.get("primary_ip"),
        ssh_user: r.get("ssh_user"),
        os_family: r.get("os_family"),
    }))
}

/// Platform-aware NFS export config. Best-effort; on failure, the error is
/// logged and operators must configure /etc/exports manually. Requires
/// passwordless `sudo` on the host (Taylor is the only fleet node without
/// this — see memory).
async fn configure_nfs_export(host: &ComputerInfo, export_path: &str) -> Result<(), StorageError> {
    let os = host.os_family.to_lowercase();
    let subnet = "192.168.5.0/24"; // conservative default; override via metadata later.

    let line = if os.starts_with("macos") {
        // macOS /etc/exports syntax: path -network <net> -mask <mask>
        format!("{export_path} -network 192.168.5.0 -mask 255.255.255.0")
    } else if os.starts_with("linux") {
        // Linux /etc/exports syntax: path client(options)
        format!("{export_path} {subnet}(rw,sync,no_subtree_check,no_root_squash)")
    } else {
        return Err(StorageError::UnsupportedOs(os));
    };

    // Idempotent append. We use `grep -qxF` so we don't double-add the same
    // line; the full reload-daemon command differs per OS.
    let reload = if os.starts_with("macos") {
        "sudo nfsd update"
    } else {
        "sudo exportfs -ra"
    };

    let cmd = format!(
        "grep -qxF {line_q} /etc/exports 2>/dev/null || (echo {line_q} | sudo tee -a /etc/exports >/dev/null) && {reload}",
        line_q = shell_quote(&line),
    );

    let (code, stdout, stderr) = ssh_exec(&host.ssh_user, &host.primary_ip, &cmd)
        .await
        .map_err(StorageError::Ssh)?;
    if code == 0 {
        info!(
            host = %host.name,
            export_path,
            "NFS export configured or already present"
        );
        Ok(())
    } else {
        Err(StorageError::Ssh(format!(
            "configure_nfs_export exit={code}: {}{}",
            stdout.trim_end(),
            stderr
        )))
    }
}

/// SSH into the target and run `mount -t nfs`. OS-specific syntax handled.
async fn attempt_client_mount(
    target: &ComputerInfo,
    host: &ComputerInfo,
    export_path: &str,
    mount_path: &str,
) -> Result<(), StorageError> {
    let os = target.os_family.to_lowercase();

    // Make sure mount point exists + mount. Commands differ per OS only in
    // that macOS accepts `-o resvport` (required by macOS NFS v3 clients on
    // privileged ports); NFSv4 over TCP port 2049 works on both platforms.
    let extra_opts = if os.starts_with("macos") {
        format!("-o nfsvers=4,resvport,{NFS_CLIENT_OPTS}")
    } else {
        format!("-o nfsvers=4,{NFS_CLIENT_OPTS}")
    };

    let cmd = format!(
        "mkdir -p {mp_q} && sudo mount -t nfs {opts} {src_q} {mp_q}",
        mp_q = shell_quote(mount_path),
        src_q = shell_quote(&format!("{}:{}", host.primary_ip, export_path)),
        opts = extra_opts,
    );

    let (code, stdout, stderr) = ssh_exec(&target.ssh_user, &target.primary_ip, &cmd)
        .await
        .map_err(StorageError::Ssh)?;
    if code == 0 {
        Ok(())
    } else {
        Err(StorageError::Ssh(format!(
            "mount exit={code}: {}{}",
            stdout.trim_end(),
            stderr
        )))
    }
}

/// Minimal POSIX single-quote escape for shell args.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ─── Peer-mount inventory + D-state checks ──────────────────────────────────

#[derive(Debug, Clone)]
struct ParsedPeerMount {
    peer_name: String,
    source: String,
    mount_path: String,
    fs_type: String,
    mount_options: String,
}

impl SharedStorageManager {
    /// Scan every fleet node for currently-mounted NFS peer mounts (autofs or
    /// manual) and upsert them into `node_peer_mounts`.  Returns
    /// `(recorded_mounts, failed_nodes)`.
    pub async fn inventory_peer_mounts(&self) -> Result<(usize, usize), StorageError> {
        let nodes = ff_db::pg_list_nodes(&self.pg).await?;
        let mut recorded = 0usize;
        let mut failed = 0usize;

        for node in &nodes {
            let Some(computer) = fetch_computer(&self.pg, &node.name).await? else {
                continue;
            };
            let probe = "cat /proc/mounts 2>/dev/null || mount";
            match ssh_exec(&node.ssh_user, &node.ip, probe).await {
                Ok((0, stdout, _stderr)) => {
                    for m in parse_mount_text(&stdout, &nodes) {
                        ff_db::pg_upsert_node_peer_mount(
                            &self.pg,
                            computer.id,
                            &m.peer_name,
                            &m.source,
                            &m.mount_path,
                            &m.fs_type,
                            Some(&m.mount_options),
                        )
                        .await?;
                        recorded += 1;
                    }
                }
                Ok((code, stdout, stderr)) => {
                    failed += 1;
                    warn!(
                        node = %node.name,
                        code,
                        output = %format!("{}{}", stdout.trim_end(), stderr),
                        "peer-mount inventory failed"
                    );
                }
                Err(e) => {
                    failed += 1;
                    warn!(node = %node.name, error = %e, "peer-mount inventory ssh failed");
                }
            }
        }

        Ok((recorded, failed))
    }
}

/// Count local processes in uninterruptible sleep (`D` state).  This is the
/// symptom class that makes a node look runaway while CPU is idle — a pileup of
/// NFS waiters on a hard-mounted dead peer.  Returns `None` on non-Linux
/// platforms (no `/proc`).
pub fn local_dstate_waiter_count() -> Option<i64> {
    let mut count = 0i64;
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stat_path = format!("/proc/{pid}/stat");
        let Ok(content) = std::fs::read_to_string(&stat_path) else {
            continue;
        };
        if let Some(state) = parse_proc_stat_state(&content) {
            if state == 'D' {
                count += 1;
            }
        }
    }
    Some(count)
}

/// Parse the one-character process state out of `/proc/<pid>/stat`.  The file
/// format is `pid (comm) state ...`; `comm` may contain spaces, so we locate the
/// first ')' and read the next non-whitespace character.
fn parse_proc_stat_state(content: &str) -> Option<char> {
    let close = content.find(')')?;
    content[close + 1..].trim_start().chars().next()
}

/// Resolve a mount source hostname (e.g. `james` or `10.44.0.2`) to a fleet
/// worker name.  Falls back to the raw source when the host is not a known
/// fleet node.
fn resolve_peer_name(source_host: &str, nodes: &[ff_db::FleetNodeRow]) -> String {
    let host = source_host.to_lowercase();
    for node in nodes {
        if node.name.to_lowercase() == host {
            return node.name.clone();
        }
        if node.ip.to_lowercase() == host {
            return node.name.clone();
        }
        if let Some(arr) = node.alt_ips.as_array() {
            for alt in arr {
                if let Some(s) = alt.as_str() {
                    if s.to_lowercase() == host {
                        return node.name.clone();
                    }
                }
            }
        }
    }
    source_host.to_string()
}

/// Parse both Linux `/proc/mounts` text and macOS `mount` output, returning
/// only NFS-family mounts whose source looks like `host:/path`.
fn parse_mount_text(text: &str, nodes: &[ff_db::FleetNodeRow]) -> Vec<ParsedPeerMount> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Linux /proc/mounts: device mnt fs opts dump pass
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 && parts[0].contains(':') && parts[2].starts_with("nfs") {
            let source = parts[0].to_string();
            let host = source.split(':').next().unwrap_or(&source);
            out.push(ParsedPeerMount {
                peer_name: resolve_peer_name(host, nodes),
                source,
                mount_path: unescape_mount_path(parts[1]),
                fs_type: parts[2].to_string(),
                mount_options: parts[3].to_string(),
            });
            continue;
        }

        // macOS `mount` output: host:/path on /mnt (nfs4, read-only, ...)
        if let Some((device, rest)) = line.split_once(" on ") {
            if !device.contains(':') {
                continue;
            }
            let (mount_path, opts) = rest.rsplit_once(" (").unwrap_or((rest, ""));
            let fs_type = opts
                .trim_start_matches('(')
                .trim_end_matches(')')
                .split(',')
                .next()
                .unwrap_or("nfs")
                .trim()
                .to_string();
            if fs_type.starts_with("nfs") {
                let host = device.split(':').next().unwrap_or(device);
                out.push(ParsedPeerMount {
                    peer_name: resolve_peer_name(host, nodes),
                    source: device.to_string(),
                    mount_path: mount_path.to_string(),
                    fs_type,
                    mount_options: opts
                        .trim_start_matches('(')
                        .trim_end_matches(')')
                        .to_string(),
                });
            }
        }
    }
    out
}

/// Undo the octal escapes used by the kernel in `/proc/mounts` (`\040` → space).
fn unescape_mount_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'0') {
            chars.next();
            if chars.peek() == Some(&'4') {
                chars.next();
                if chars.peek() == Some(&'0') {
                    chars.next();
                    out.push(' ');
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `printf %s <shell_quote(s)>` through a real `/bin/sh` and return what
    /// the shell actually produced. The security contract is that this equals
    /// `s` EXACTLY — no expansion, word-splitting, or command execution.
    fn sh_roundtrip(s: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(s)))
            .output()
            .expect("spawn sh");
        assert!(out.status.success(), "sh failed for {s:?}");
        String::from_utf8(out.stdout).expect("utf8")
    }

    #[test]
    fn shell_quote_neutralises_injection_in_a_real_shell() {
        // Payloads are side-effect-free (`echo`) so a quoting REGRESSION shows up
        // as a mismatched round-trip rather than as a destructive side effect.
        for s in [
            "plain",
            "/srv/forgefleet/models",
            "path with spaces",
            "has'apostrophe",
            "$(echo pwned)",
            "`echo pwned`",
            "; echo INJECTED ;",
            "a|b&c;d>e<f",
            "$HOME ${X}",
            "tab\tand\nnewline",
            "",
        ] {
            assert_eq!(sh_roundtrip(s), s, "quoting failed to neutralise {s:?}");
        }
    }

    #[test]
    fn shell_quote_structure() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote(""), "''");
        // A single quote becomes close-quote, escaped-quote, reopen-quote.
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    fn test_node(name: &str, ip: &str, alt_ips: &[&str]) -> ff_db::FleetNodeRow {
        ff_db::FleetNodeRow {
            name: name.to_string(),
            ip: ip.to_string(),
            ssh_user: "root".to_string(),
            ram_gb: 0,
            cpu_cores: 0,
            os: "linux".to_string(),
            role: "worker".to_string(),
            election_priority: 0,
            hardware: String::new(),
            alt_ips: serde_json::json!(alt_ips),
            capabilities: serde_json::json!({}),
            preferences: serde_json::json!({}),
            resources: serde_json::json!({}),
            status: "online".to_string(),
            runtime: "unknown".to_string(),
            models_dir: "~/models".to_string(),
            disk_quota_pct: 80,
            sub_agent_count: 1,
            gh_account: None,
            tooling: serde_json::json!({}),
            gpu_kind: None,
            gpu_model: None,
            gpu_vram_gb: None,
            gpu_total_vram_gb: None,
            has_gpu: None,
            computer_ram_gb: None,
            computer_cpu_cores: None,
            computer_status: None,
        }
    }

    #[test]
    fn parse_proc_mounts_linux() {
        let nodes = vec![test_node("james", "10.44.0.2", &[])];
        let text = "james:/mnt/james /mnt/james nfs4 rw,relatime,vers=4.2,hard,timeo=600,retrans=2 0 0\n/dev/sda1 / ext4 rw 0 0\n";
        let mounts = parse_mount_text(text, &nodes);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].peer_name, "james");
        assert_eq!(mounts[0].mount_path, "/mnt/james");
        assert_eq!(mounts[0].fs_type, "nfs4");
        assert!(mounts[0].mount_options.contains("hard"));
    }

    #[test]
    fn parse_mount_macos() {
        let nodes = vec![test_node("taylor", "10.44.0.1", &[])];
        let text =
            "10.44.0.1:/Users/venkat/models on /Volumes/taylor-models (nfs4, read-only, ...)\n";
        let mounts = parse_mount_text(text, &nodes);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].peer_name, "taylor");
        assert_eq!(mounts[0].mount_path, "/Volumes/taylor-models");
        assert_eq!(mounts[0].fs_type, "nfs4");
    }

    #[test]
    fn resolve_peer_name_matches_alt_ip() {
        let nodes = vec![test_node("marcus", "10.44.0.3", &["192.168.5.33"])];
        assert_eq!(resolve_peer_name("marcus", &nodes), "marcus");
        assert_eq!(resolve_peer_name("10.44.0.3", &nodes), "marcus");
        assert_eq!(resolve_peer_name("192.168.5.33", &nodes), "marcus");
        assert_eq!(resolve_peer_name("sophie", &nodes), "sophie");
    }

    #[test]
    fn parse_proc_stat_state_extracts_dstate() {
        assert_eq!(parse_proc_stat_state("123 (bash) S 0 1 1 ..."), Some('S'));
        assert_eq!(parse_proc_stat_state("456 (nfs-io) D 0 1 1 ..."), Some('D'));
        assert_eq!(
            parse_proc_stat_state("789 (comm with spaces) D 0"),
            Some('D')
        );
    }
}
