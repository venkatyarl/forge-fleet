//! Hive Mind sync — git-based shared memory across the fleet.
//!
//! On startup, pulls the latest shared knowledge from the fleet's hive repo.
//! High-confidence learnings are auto-promoted; low-confidence sit in a queue.
//! Syncs silently — never blocks startup if offline or unconfigured.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use tokio::fs;
use tracing::{debug, info};

/// Result of a hive sync operation.
#[derive(Debug, Clone, Default)]
pub struct SyncResult {
    pub pulled: bool,
    pub was_offline: bool,
    pub last_sync_at: Option<DateTime<Utc>>,
}

/// Manages Hive Mind git sync.
pub struct HiveSync {
    local_path: PathBuf,
}

impl HiveSync {
    pub fn new() -> Self {
        Self {
            local_path: dirs::home_dir()
                .unwrap_or_default()
                .join(".forgefleet")
                .join("hive"),
        }
    }

    /// Ensure hive directory exists with starter files.
    pub async fn ensure_initialized(&self) {
        if self.local_path.exists() {
            return;
        }

        let _ = fs::create_dir_all(&self.local_path).await;

        // Create starter HIVE.md
        let hive_md = self.local_path.join("HIVE.md");
        if !hive_md.exists() {
            let _ = fs::write(
                &hive_md,
                "# Hive Mind\n\n\
                 Shared knowledge across the ForgeFleet.\n\
                 Fleet coding standards, topology facts, and best practices.\n\n\
                 This is auto-populated from high-confidence learnings across all fleet members.\n",
            )
            .await;
        }

        // Create empty learnings.json
        let learnings = self.local_path.join("learnings.json");
        if !learnings.exists() {
            let _ = fs::write(&learnings, "[]").await;
        }

        // Initialize git repo if not already
        let git_dir = self.local_path.join(".git");
        if !git_dir.exists() {
            let _ = tokio::process::Command::new("git")
                .args(["init"])
                .current_dir(&self.local_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            debug!(path = %self.local_path.display(), "initialized hive git repo");
        }

        info!("hive mind initialized at {}", self.local_path.display());
    }

    /// Pull latest from remote (silently no-ops if offline or no remote).
    pub async fn pull(&self) -> SyncResult {
        if !self.local_path.join(".git").exists() {
            return SyncResult::default();
        }

        // Check if remote is configured
        let remote_check = tokio::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(&self.local_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        let has_remote = remote_check.map(|s| s.success()).unwrap_or(false);
        if !has_remote {
            debug!("hive has no remote configured — skipping pull");
            return SyncResult {
                pulled: false,
                was_offline: false,
                last_sync_at: None,
            };
        }

        // Try to pull
        let result = tokio::process::Command::new("git")
            .args(["pull", "--ff-only", "--quiet", "origin", "main"])
            .current_dir(&self.local_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        match result {
            Ok(status) if status.success() => {
                info!("hive mind synced from remote");
                SyncResult {
                    pulled: true,
                    was_offline: false,
                    last_sync_at: Some(Utc::now()),
                }
            }
            Ok(_) => {
                debug!("hive pull failed (merge conflict or branch issue)");
                SyncResult {
                    pulled: false,
                    was_offline: false,
                    last_sync_at: None,
                }
            }
            Err(_) => {
                debug!("hive pull failed (offline)");
                SyncResult {
                    pulled: false,
                    was_offline: true,
                    last_sync_at: None,
                }
            }
        }
    }

    /// Push local hive changes to remote.
    pub async fn push(&self) -> bool {
        if !self.local_path.join(".git").exists() {
            return false;
        }

        // Stage, commit, push
        let _ = tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&self.local_path)
            .status()
            .await;

        let _ = tokio::process::Command::new("git")
            .args(["commit", "-m", "hive: auto-sync learnings"])
            .current_dir(&self.local_path)
            .stdout(std::process::Stdio::null())
            .status()
            .await;

        let result = tokio::process::Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(&self.local_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        result.map(|s| s.success()).unwrap_or(false)
    }

    pub fn local_path(&self) -> &PathBuf {
        &self.local_path
    }

    /// Auto-sync: pull then push if there are local changes.
    /// Called after learning extraction adds new hive entries.
    pub async fn auto_sync(&self) -> SyncResult {
        // Pull first to get latest
        let pull_result = self.pull().await;

        // Check if there are local changes to push
        if self.has_local_changes().await {
            self.push().await;
            info!("hive mind auto-synced (pushed local learnings)");
        }

        pull_result
    }

    async fn has_local_changes(&self) -> bool {
        let result = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.local_path)
            .output()
            .await;

        match result {
            Ok(output) => !output.stdout.is_empty(),
            Err(_) => false,
        }
    }
}
