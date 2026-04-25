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
//!    ```
//!    sudo sh -c 'echo "/Users/venkat/models -network 192.168.5.0 -mask 255.255.255.0" >> /etc/exports'
//!    sudo nfsd update     # macOS
//!    # or:
//!    sudo exportfs -ra    # Linux
//!    ```
//! 2. On each client:
//!    ```
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
        format!("{} -network 192.168.5.0 -mask 255.255.255.0", export_path)
    } else if os.starts_with("linux") {
        // Linux /etc/exports syntax: path client(options)
        format!(
            "{} {}(rw,sync,no_subtree_check,no_root_squash)",
            export_path, subnet
        )
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
        "-o nfsvers=4,resvport"
    } else {
        "-o nfsvers=4"
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
