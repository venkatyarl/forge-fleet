//! Atomic binary swap — safely replace the running binary.
//!
//! Strategy:
//! 1. Rename current binary → `<name>.bak` (backup)
//! 2. Copy new binary → current path
//! 3. Verify permissions (executable bit)
//! 4. On any failure → rollback from `.bak`
//!
//! The `.bak` file is kept for emergency rollback until the next successful update.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::error::{UpdateError, UpdateResult};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the binary swapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapperConfig {
    /// Path to the currently-running binary.
    pub current_binary: PathBuf,

    /// Suffix for backup files (default: ".bak").
    #[serde(default = "default_backup_suffix")]
    pub backup_suffix: String,

    /// Whether to preserve the backup after successful swap (default: true).
    #[serde(default = "default_true")]
    pub keep_backup: bool,

    /// Unix permissions for the new binary (default: 0o755).
    #[serde(default = "default_permissions")]
    pub binary_permissions: u32,
}

fn default_backup_suffix() -> String {
    ".bak".into()
}
fn default_true() -> bool {
    true
}
fn default_permissions() -> u32 {
    0o755
}

impl Default for SwapperConfig {
    fn default() -> Self {
        Self {
            current_binary: PathBuf::from("/usr/local/bin/forgefleet"),
            backup_suffix: default_backup_suffix(),
            keep_backup: true,
            binary_permissions: default_permissions(),
        }
    }
}

// ─── Swap result ─────────────────────────────────────────────────────────────

/// Outcome of a binary swap operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapResult {
    /// Whether the swap completed successfully.
    pub success: bool,

    /// Path to the backup file (if created).
    pub backup_path: Option<PathBuf>,

    /// Size of the old binary (bytes).
    pub old_size: u64,

    /// Size of the new binary (bytes).
    pub new_size: u64,

    /// Whether a rollback was performed due to failure.
    pub rolled_back: bool,

    /// Timestamp of the swap.
    pub swapped_at: DateTime<Utc>,

    /// Human-readable summary.
    pub summary: String,
}

// ─── Swapper ─────────────────────────────────────────────────────────────────

/// Handles atomic binary replacement with rollback on failure.
pub struct BinarySwapper {
    config: SwapperConfig,
}

impl BinarySwapper {
    pub fn new(config: SwapperConfig) -> Self {
        Self { config }
    }

    /// Get the backup path for the current binary.
    pub fn backup_path(&self) -> PathBuf {
        let mut p = self.config.current_binary.as_os_str().to_owned();
        p.push(&self.config.backup_suffix);
        PathBuf::from(p)
    }

    /// Check if a backup exists from a previous swap.
    pub fn has_backup(&self) -> bool {
        self.backup_path().exists()
    }

    /// Perform the atomic swap: current → .bak, new → current.
    ///
    /// If any step fails after the backup rename, we attempt to rollback.
    pub fn swap(&self, new_binary: &Path) -> UpdateResult<SwapResult> {
        let current = &self.config.current_binary;
        let backup = self.backup_path();

        info!(
            current = %current.display(),
            new = %new_binary.display(),
            backup = %backup.display(),
            "starting binary swap"
        );

        // Validate new binary exists
        if !new_binary.exists() {
            return Err(UpdateError::BinaryNotFound {
                path: new_binary.to_path_buf(),
            });
        }

        let new_size = fs::metadata(new_binary)?.len();
        let old_size = if current.exists() {
            fs::metadata(current)?.len()
        } else {
            0
        };

        // Step 1: Backup current binary
        let backup_created = if current.exists() {
            debug!(from = %current.display(), to = %backup.display(), "backing up current binary");

            // Remove old backup if it exists
            if backup.exists() {
                fs::remove_file(&backup).map_err(|e| UpdateError::BackupFailed {
                    reason: format!("could not remove old backup: {e}"),
                })?;
            }

            fs::rename(current, &backup).map_err(|e| UpdateError::BackupFailed {
                reason: format!("rename failed: {e}"),
            })?;

            true
        } else {
            warn!(path = %current.display(), "current binary doesn't exist, skipping backup");
            false
        };

        // Step 2: Copy new binary into place
        if let Err(e) = fs::copy(new_binary, current) {
            error!(error = %e, "failed to copy new binary — rolling back");

            // Rollback: restore from backup
            if backup_created {
                if let Err(rb_err) = fs::rename(&backup, current) {
                    error!(error = %rb_err, "CRITICAL: rollback also failed!");
                    return Err(UpdateError::CopyFailed {
                        reason: format!(
                            "copy failed ({e}) AND rollback failed ({rb_err}) — manual intervention needed"
                        ),
                    });
                }
                info!("rolled back to backup successfully");
            }

            return Ok(SwapResult {
                success: false,
                backup_path: if backup_created { Some(backup) } else { None },
                old_size,
                new_size,
                rolled_back: true,
                swapped_at: Utc::now(),
                summary: format!("copy failed, rolled back: {e}"),
            });
        }

        // Step 3: Set permissions
        if let Err(e) = self.set_permissions(current) {
            error!(error = %e, "failed to set permissions — rolling back");

            // Rollback
            if backup_created {
                let _ = fs::remove_file(current);
                if let Err(rb_err) = fs::rename(&backup, current) {
                    error!(error = %rb_err, "CRITICAL: rollback also failed!");
                    return Err(UpdateError::PermissionFailed {
                        reason: format!("chmod failed ({e}) AND rollback failed ({rb_err})"),
                    });
                }
            }

            return Ok(SwapResult {
                success: false,
                backup_path: if backup_created { Some(backup) } else { None },
                old_size,
                new_size,
                rolled_back: true,
                swapped_at: Utc::now(),
                summary: format!("permission set failed, rolled back: {e}"),
            });
        }

        info!(old_size, new_size, "binary swap complete");

        Ok(SwapResult {
            success: true,
            backup_path: if backup_created && self.config.keep_backup {
                Some(backup)
            } else {
                None
            },
            old_size,
            new_size,
            rolled_back: false,
            swapped_at: Utc::now(),
            summary: format!("swapped {old_size} → {new_size} bytes"),
        })
    }

    fn set_permissions(&self, path: &Path) -> UpdateResult<()> {
        let perms = fs::Permissions::from_mode(self.config.binary_permissions);
        fs::set_permissions(path, perms).map_err(|e| UpdateError::PermissionFailed {
            reason: e.to_string(),
        })?;
        debug!(path = %path.display(), mode = format!("{:#o}", self.config.binary_permissions), "permissions set");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backup_path() {
        let config = SwapperConfig {
            current_binary: PathBuf::from("/usr/local/bin/forgefleet"),
            ..Default::default()
        };
        let swapper = BinarySwapper::new(config);
        assert_eq!(
            swapper.backup_path(),
            PathBuf::from("/usr/local/bin/forgefleet.bak")
        );
    }

    #[test]
    fn test_default_config() {
        let cfg = SwapperConfig::default();
        assert_eq!(cfg.backup_suffix, ".bak");
        assert!(cfg.keep_backup);
        assert_eq!(cfg.binary_permissions, 0o755);
    }

    #[test]
    fn test_swap_nonexistent_source() {
        let config = SwapperConfig {
            current_binary: PathBuf::from("/tmp/test-ff-swapper-current"),
            ..Default::default()
        };
        let swapper = BinarySwapper::new(config);
        let result = swapper.swap(Path::new("/nonexistent/binary"));
        assert!(result.is_err());
    }
}
