//! Rollback — restore a previous binary from `.bak` after a failed update.
//!
//! Used when a new binary passes verification but fails health checks
//! after being deployed and started.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{UpdateError, UpdateResult};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the rollback system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackConfig {
    /// Path to the running binary.
    pub binary_path: PathBuf,

    /// Suffix for backup files (default: ".bak").
    #[serde(default = "default_backup_suffix")]
    pub backup_suffix: String,

    /// Health check URL to verify the service is running after rollback.
    /// e.g. "http://localhost:51800/health"
    pub health_check_url: Option<String>,

    /// Health check timeout (seconds, default: 30).
    #[serde(default = "default_health_timeout")]
    pub health_check_timeout_secs: u64,

    /// Number of health check retries (default: 5).
    #[serde(default = "default_retries")]
    pub health_check_retries: u32,

    /// Delay between retries (seconds, default: 3).
    #[serde(default = "default_retry_delay")]
    pub retry_delay_secs: u64,

    /// Binary permissions (default: 0o755).
    #[serde(default = "default_permissions")]
    pub binary_permissions: u32,
}

fn default_backup_suffix() -> String {
    ".bak".into()
}
fn default_health_timeout() -> u64 {
    30
}
fn default_retries() -> u32 {
    5
}
fn default_retry_delay() -> u64 {
    3
}
fn default_permissions() -> u32 {
    0o755
}

impl Default for RollbackConfig {
    fn default() -> Self {
        Self {
            binary_path: PathBuf::from("/usr/local/bin/forgefleet"),
            backup_suffix: default_backup_suffix(),
            health_check_url: None,
            health_check_timeout_secs: default_health_timeout(),
            health_check_retries: default_retries(),
            retry_delay_secs: default_retry_delay(),
            binary_permissions: default_permissions(),
        }
    }
}

// ─── Rollback result ─────────────────────────────────────────────────────────

/// Outcome of a rollback operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackResult {
    /// Whether the rollback succeeded.
    pub success: bool,

    /// Whether the health check passed after rollback.
    pub health_check_passed: Option<bool>,

    /// The SHA or version we rolled back from (if known).
    pub rolled_back_from: Option<String>,

    /// Timestamp of the rollback.
    pub rolled_back_at: DateTime<Utc>,

    /// Human-readable summary.
    pub summary: String,
}

// ─── Rollback manager ────────────────────────────────────────────────────────

/// Manages rollback to a previous binary version.
pub struct RollbackManager {
    config: RollbackConfig,
}

impl RollbackManager {
    pub fn new(config: RollbackConfig) -> Self {
        Self { config }
    }

    /// Get the backup file path.
    pub fn backup_path(&self) -> PathBuf {
        let mut p = self.config.binary_path.as_os_str().to_owned();
        p.push(&self.config.backup_suffix);
        PathBuf::from(p)
    }

    /// Check whether a backup file exists for rollback.
    pub fn can_rollback(&self) -> bool {
        self.backup_path().exists()
    }

    /// Perform rollback: restore the `.bak` binary.
    ///
    /// 1. Verify `.bak` exists
    /// 2. Remove current binary
    /// 3. Rename `.bak` → current
    /// 4. Set permissions
    /// 5. Optionally run health check
    pub fn rollback(&self) -> UpdateResult<RollbackResult> {
        let binary = &self.config.binary_path;
        let backup = self.backup_path();

        info!(
            binary = %binary.display(),
            backup = %backup.display(),
            "starting rollback"
        );

        // 1. Verify backup exists
        if !backup.exists() {
            return Err(UpdateError::NoBackup {
                path: backup.clone(),
            });
        }

        // 2. Remove current binary (it's the bad one)
        if binary.exists() {
            debug!("removing current (bad) binary");
            fs::remove_file(binary).map_err(|e| UpdateError::RollbackFailed {
                reason: format!("could not remove current binary: {e}"),
            })?;
        }

        // 3. Rename backup → current
        fs::rename(&backup, binary).map_err(|e| UpdateError::RollbackFailed {
            reason: format!("could not rename backup to current: {e}"),
        })?;

        // 4. Set permissions
        let perms = fs::Permissions::from_mode(self.config.binary_permissions);
        fs::set_permissions(binary, perms).map_err(|e| UpdateError::RollbackFailed {
            reason: format!("could not set permissions: {e}"),
        })?;

        info!("binary rollback complete, backup restored");

        Ok(RollbackResult {
            success: true,
            health_check_passed: None,
            rolled_back_from: None,
            rolled_back_at: Utc::now(),
            summary: "rolled back to previous binary".into(),
        })
    }

    /// Perform rollback and verify the service starts correctly.
    ///
    /// This is the async version that includes health checking after rollback.
    pub async fn rollback_and_verify(&self) -> UpdateResult<RollbackResult> {
        let mut result = self.rollback()?;

        // Run health check if configured
        if let Some(url) = &self.config.health_check_url {
            info!(url, "running post-rollback health check");
            let healthy = self.health_check(url).await;

            result.health_check_passed = Some(healthy);
            if !healthy {
                result.success = false;
                result.summary =
                    format!("rollback binary restored but health check failed at {url}");
                warn!(url, "post-rollback health check FAILED");
            } else {
                info!("post-rollback health check passed");
                result.summary = "rolled back and health check passed".into();
            }
        }

        Ok(result)
    }

    /// Run health checks with retries.
    async fn health_check(&self, url: &str) -> bool {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.config.health_check_timeout_secs))
            .build()
            .unwrap_or_default();

        for attempt in 1..=self.config.health_check_retries {
            debug!(attempt, url, "health check attempt");

            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(attempt, "health check passed");
                    return true;
                }
                Ok(resp) => {
                    warn!(attempt, status = %resp.status(), "health check returned non-200");
                }
                Err(e) => {
                    warn!(attempt, error = %e, "health check failed");
                }
            }

            if attempt < self.config.health_check_retries {
                tokio::time::sleep(Duration::from_secs(self.config.retry_delay_secs)).await;
            }
        }

        false
    }

    /// Quick version check on the restored binary.
    pub fn check_restored_version(&self) -> UpdateResult<String> {
        let binary = &self.config.binary_path;

        let output = Command::new(binary)
            .arg("--version")
            .output()
            .map_err(|e| UpdateError::HealthCheckFailed {
                reason: format!("could not execute restored binary: {e}"),
            })?;

        if !output.status.success() {
            return Err(UpdateError::HealthCheckFailed {
                reason: format!(
                    "restored binary --version failed with exit code {}",
                    output.status.code().unwrap_or(-1)
                ),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backup_path() {
        let config = RollbackConfig {
            binary_path: PathBuf::from("/usr/local/bin/forgefleet"),
            ..Default::default()
        };
        let mgr = RollbackManager::new(config);
        assert_eq!(
            mgr.backup_path(),
            PathBuf::from("/usr/local/bin/forgefleet.bak")
        );
    }

    #[test]
    fn test_cannot_rollback_without_backup() {
        let config = RollbackConfig {
            binary_path: PathBuf::from("/tmp/nonexistent-ff-binary"),
            ..Default::default()
        };
        let mgr = RollbackManager::new(config);
        assert!(!mgr.can_rollback());
    }

    #[test]
    fn test_rollback_no_backup_errors() {
        let config = RollbackConfig {
            binary_path: PathBuf::from("/tmp/nonexistent-ff-binary"),
            ..Default::default()
        };
        let mgr = RollbackManager::new(config);
        let result = mgr.rollback();
        assert!(result.is_err());
    }

    #[test]
    fn test_default_config() {
        let cfg = RollbackConfig::default();
        assert_eq!(cfg.health_check_retries, 5);
        assert_eq!(cfg.retry_delay_secs, 3);
        assert_eq!(cfg.binary_permissions, 0o755);
    }
}
