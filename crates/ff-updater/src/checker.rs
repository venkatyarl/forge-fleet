//! Update availability checker.
//!
//! Compares the local git SHA against the remote (origin/main) to determine
//! whether a new version is available. Supports both `git fetch` + `rev-parse`
//! (preferred, no network dependency beyond git remote) and GitHub API fallback.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{UpdateError, UpdateResult};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the update checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckerConfig {
    /// Path to the ForgeFleet source repo.
    pub repo_path: PathBuf,

    /// Git remote name (default: "origin").
    #[serde(default = "default_remote")]
    pub remote: String,

    /// Git branch to track (default: "main").
    #[serde(default = "default_branch")]
    pub branch: String,

    /// How often to check for updates (seconds).
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,

    /// Optional GitHub owner/repo for API-based checks.
    /// e.g. "venkat/forge-fleet-rs"
    pub github_repo: Option<String>,

    /// Optional GitHub token for higher rate limits.
    pub github_token: Option<String>,
}

fn default_remote() -> String {
    "origin".into()
}
fn default_branch() -> String {
    "main".into()
}
fn default_check_interval() -> u64 {
    3600 // 1 hour
}

impl Default for CheckerConfig {
    fn default() -> Self {
        Self {
            repo_path: PathBuf::from("."),
            remote: default_remote(),
            branch: default_branch(),
            check_interval_secs: default_check_interval(),
            github_repo: None,
            github_token: None,
        }
    }
}

// ─── Check result ────────────────────────────────────────────────────────────

/// Result of an update availability check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// Whether an update is available.
    pub update_available: bool,

    /// Current local commit SHA.
    pub local_sha: String,

    /// Latest remote commit SHA.
    pub remote_sha: String,

    /// Number of commits behind (0 = up to date).
    pub commits_behind: u64,

    /// Timestamp of this check.
    pub checked_at: DateTime<Utc>,
}

// ─── Checker ─────────────────────────────────────────────────────────────────

/// The update checker — compares local vs remote git state.
pub struct UpdateChecker {
    config: CheckerConfig,
    last_check: Option<CheckResult>,
}

impl UpdateChecker {
    /// Create a new checker with the given config.
    pub fn new(config: CheckerConfig) -> Self {
        Self {
            config,
            last_check: None,
        }
    }

    /// Return the last cached check result, if any.
    pub fn last_check(&self) -> Option<&CheckResult> {
        self.last_check.as_ref()
    }

    /// Check interval as a `Duration`.
    pub fn check_interval(&self) -> Duration {
        Duration::from_secs(self.config.check_interval_secs)
    }

    /// Whether enough time has elapsed since the last check.
    pub fn should_check(&self) -> bool {
        match &self.last_check {
            None => true,
            Some(last) => {
                let elapsed = Utc::now()
                    .signed_duration_since(last.checked_at)
                    .num_seconds();
                elapsed >= self.config.check_interval_secs as i64
            }
        }
    }

    /// Perform an update check using git fetch + rev-parse.
    ///
    /// 1. `git fetch <remote>` — update remote refs
    /// 2. `git rev-parse HEAD` — local SHA
    /// 3. `git rev-parse <remote>/<branch>` — remote SHA
    /// 4. `git rev-list HEAD..<remote>/<branch> --count` — commits behind
    pub fn check_git(&mut self) -> UpdateResult<CheckResult> {
        let repo = &self.config.repo_path;
        let remote = &self.config.remote;
        let branch = &self.config.branch;

        info!(repo = %repo.display(), "checking for updates via git");

        // 1. Fetch remote
        self.git_fetch(repo, remote)?;

        // 2. Local SHA
        let local_sha = self.git_rev_parse(repo, "HEAD")?;
        debug!(local_sha = %local_sha, "local HEAD");

        // 3. Remote SHA
        let remote_ref = format!("{remote}/{branch}");
        if !self.git_remote_ref_exists(repo, remote, branch)? {
            warn!(
                remote = %remote,
                branch = %branch,
                "git remote ref unavailable; updater check skipped"
            );

            let result = CheckResult {
                update_available: false,
                local_sha: local_sha.clone(),
                remote_sha: local_sha,
                commits_behind: 0,
                checked_at: Utc::now(),
            };

            self.last_check = Some(result.clone());
            return Ok(result);
        }

        let remote_sha = self.git_rev_parse(repo, &remote_ref)?;
        debug!(remote_sha = %remote_sha, "remote HEAD");

        // 4. Commits behind
        let commits_behind = self.git_rev_list_count(repo, "HEAD", &remote_ref)?;

        let result = CheckResult {
            update_available: local_sha != remote_sha,
            local_sha,
            remote_sha,
            commits_behind,
            checked_at: Utc::now(),
        };

        info!(
            update_available = result.update_available,
            commits_behind = result.commits_behind,
            "update check complete"
        );

        self.last_check = Some(result.clone());
        Ok(result)
    }

    /// Check using the GitHub API (fallback when git fetch isn't practical).
    pub async fn check_github_api(&mut self) -> UpdateResult<CheckResult> {
        let github_repo =
            self.config
                .github_repo
                .as_deref()
                .ok_or_else(|| UpdateError::CheckFailed {
                    reason: "github_repo not configured".into(),
                })?;

        let repo_path = &self.config.repo_path;
        let branch = &self.config.branch;

        info!(github_repo, "checking for updates via GitHub API");

        // Get local SHA
        let local_sha = self.git_rev_parse(repo_path, "HEAD")?;

        // Fetch remote SHA from GitHub
        let url = format!("https://api.github.com/repos/{github_repo}/commits/{branch}");

        let client = reqwest::Client::new();
        let mut req = client
            .get(&url)
            .header("User-Agent", "ForgeFleet-Updater")
            .header("Accept", "application/vnd.github.v3+json");

        if let Some(token) = &self.config.github_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        let resp = req.send().await.map_err(|e| UpdateError::CheckFailed {
            reason: format!("GitHub API request failed: {e}"),
        })?;

        if !resp.status().is_success() {
            return Err(UpdateError::CheckFailed {
                reason: format!("GitHub API returned {}", resp.status()),
            });
        }

        let body: serde_json::Value = resp.json().await.map_err(|e| UpdateError::CheckFailed {
            reason: format!("failed to parse GitHub response: {e}"),
        })?;

        let remote_sha = body["sha"]
            .as_str()
            .ok_or_else(|| UpdateError::CheckFailed {
                reason: "no 'sha' field in GitHub response".into(),
            })?
            .to_string();

        let result = CheckResult {
            update_available: local_sha != remote_sha,
            local_sha,
            remote_sha,
            commits_behind: 0, // GitHub API doesn't give count easily
            checked_at: Utc::now(),
        };

        self.last_check = Some(result.clone());
        Ok(result)
    }

    // ── Git helpers ──────────────────────────────────────────────────

    fn git_fetch(&self, repo: &Path, remote: &str) -> UpdateResult<()> {
        let output = Command::new("git")
            .args(["fetch", remote, "--quiet"])
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::GitCommand {
                command: format!("git fetch {remote}"),
                stderr: e.to_string(),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            warn!(stderr = %stderr, "git fetch failed (non-fatal, using cached refs)");
            // Non-fatal — we can still compare cached remote refs
        }

        Ok(())
    }

    fn git_remote_ref_exists(&self, repo: &Path, remote: &str, branch: &str) -> UpdateResult<bool> {
        let remote_ref = format!("refs/remotes/{remote}/{branch}");
        let output = Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &remote_ref])
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::GitCommand {
                command: format!("git show-ref --verify --quiet {remote_ref}"),
                stderr: e.to_string(),
            })?;

        Ok(output.status.success())
    }

    fn git_rev_parse(&self, repo: &Path, rev: &str) -> UpdateResult<String> {
        let output = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::GitCommand {
                command: format!("git rev-parse {rev}"),
                stderr: e.to_string(),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UpdateError::GitCommand {
                command: format!("git rev-parse {rev}"),
                stderr,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn git_rev_list_count(&self, repo: &Path, from: &str, to: &str) -> UpdateResult<u64> {
        let range = format!("{from}..{to}");
        let output = Command::new("git")
            .args(["rev-list", &range, "--count"])
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::GitCommand {
                command: format!("git rev-list {range} --count"),
                stderr: e.to_string(),
            })?;

        if !output.status.success() {
            // If the range is invalid, default to 0
            return Ok(0);
        }

        let count_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        count_str
            .parse::<u64>()
            .map_err(|_| UpdateError::GitCommand {
                command: format!("git rev-list {range} --count"),
                stderr: format!("could not parse count: {count_str}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = CheckerConfig::default();
        assert_eq!(cfg.remote, "origin");
        assert_eq!(cfg.branch, "main");
        assert_eq!(cfg.check_interval_secs, 3600);
        assert!(cfg.github_repo.is_none());
    }

    #[test]
    fn test_should_check_when_never_checked() {
        let checker = UpdateChecker::new(CheckerConfig::default());
        assert!(checker.should_check());
    }

    #[test]
    fn test_check_result_serialization() {
        let result = CheckResult {
            update_available: true,
            local_sha: "abc123".into(),
            remote_sha: "def456".into(),
            commits_behind: 3,
            checked_at: Utc::now(),
        };

        let json = serde_json::to_string(&result).unwrap();
        let parsed: CheckResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.update_available);
        assert_eq!(parsed.commits_behind, 3);
    }
}
