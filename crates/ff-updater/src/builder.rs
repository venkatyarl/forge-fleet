//! Source builder — pull, compile, test.
//!
//! Runs the full build pipeline:
//! 1. `git pull` to fetch latest source
//! 2. `cargo build --release` to compile
//! 3. `cargo test --workspace --lib` to verify tests pass
//!
//! All output is captured for logging and diagnostics.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::error::{UpdateError, UpdateResult};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the build pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderConfig {
    /// Path to the ForgeFleet source repo.
    pub repo_path: PathBuf,

    /// Git remote to pull from (default: "origin").
    #[serde(default = "default_remote")]
    pub remote: String,

    /// Git branch to pull (default: "main").
    #[serde(default = "default_branch")]
    pub branch: String,

    /// Name of the binary to build (default: "forgefleet").
    #[serde(default = "default_binary_name")]
    pub binary_name: String,

    /// Additional cargo build flags.
    #[serde(default)]
    pub extra_build_flags: Vec<String>,

    /// Whether to run tests before accepting the build (default: true).
    #[serde(default = "default_true")]
    pub run_tests: bool,

    /// Maximum time for the build step (seconds, default: 1800 = 30 min).
    #[serde(default = "default_build_timeout")]
    pub build_timeout_secs: u64,

    /// Maximum time for the test step (seconds, default: 600 = 10 min).
    #[serde(default = "default_test_timeout")]
    pub test_timeout_secs: u64,
}

fn default_remote() -> String {
    "origin".into()
}
fn default_branch() -> String {
    "main".into()
}
fn default_binary_name() -> String {
    "forgefleet".into()
}
fn default_true() -> bool {
    true
}
fn default_build_timeout() -> u64 {
    1800
}
fn default_test_timeout() -> u64 {
    600
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            repo_path: PathBuf::from("."),
            remote: default_remote(),
            branch: default_branch(),
            binary_name: default_binary_name(),
            extra_build_flags: Vec::new(),
            run_tests: true,
            build_timeout_secs: default_build_timeout(),
            test_timeout_secs: default_test_timeout(),
        }
    }
}

// ─── Build result ────────────────────────────────────────────────────────────

/// Outcome of a build attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    /// Whether the build (and tests) succeeded.
    pub success: bool,

    /// Path to the newly built binary (only set on success).
    pub binary_path: Option<PathBuf>,

    /// Git SHA that was built.
    pub built_sha: String,

    /// Time taken for `git pull`.
    pub pull_duration: Duration,

    /// Time taken for `cargo build --release`.
    pub build_duration: Duration,

    /// Time taken for `cargo test` (if run).
    pub test_duration: Option<Duration>,

    /// Combined stdout/stderr from git pull.
    pub pull_log: String,

    /// Combined stdout/stderr from cargo build.
    pub build_log: String,

    /// Combined stdout/stderr from cargo test (if run).
    pub test_log: Option<String>,

    /// Timestamp.
    pub completed_at: DateTime<Utc>,
}

// ─── Builder ─────────────────────────────────────────────────────────────────

/// Builds ForgeFleet from source.
pub struct SourceBuilder {
    config: BuilderConfig,
}

impl SourceBuilder {
    pub fn new(config: BuilderConfig) -> Self {
        Self { config }
    }

    /// Run the full build pipeline: pull → build → test.
    ///
    /// Returns a `BuildResult` with success/failure and all captured logs.
    pub fn build(&self) -> UpdateResult<BuildResult> {
        let repo = &self.config.repo_path;

        info!(repo = %repo.display(), "starting build pipeline");

        // Step 1: git pull
        let (pull_log, pull_duration) = self.git_pull(repo)?;

        // Get the SHA we just pulled
        let built_sha = self.git_head_sha(repo)?;
        info!(sha = %built_sha, "pulled source");

        // Step 2: cargo build --release
        let (build_log, build_duration) = match self.cargo_build(repo) {
            Ok(result) => result,
            Err(e) => {
                error!(error = %e, "cargo build failed");
                return Ok(BuildResult {
                    success: false,
                    binary_path: None,
                    built_sha,
                    pull_duration,
                    build_duration: Duration::ZERO,
                    test_duration: None,
                    pull_log,
                    build_log: e.to_string(),
                    test_log: None,
                    completed_at: Utc::now(),
                });
            }
        };

        // Step 3: cargo test (optional)
        let (test_log, test_duration) = if self.config.run_tests {
            match self.cargo_test(repo) {
                Ok((log, dur)) => (Some(log), Some(dur)),
                Err(e) => {
                    error!(error = %e, "cargo test failed");
                    return Ok(BuildResult {
                        success: false,
                        binary_path: None,
                        built_sha,
                        pull_duration,
                        build_duration,
                        test_duration: Some(Duration::ZERO),
                        pull_log,
                        build_log,
                        test_log: Some(e.to_string()),
                        completed_at: Utc::now(),
                    });
                }
            }
        } else {
            (None, None)
        };

        // Locate the built binary
        let binary_path = repo
            .join("target")
            .join("release")
            .join(&self.config.binary_name);

        let binary_exists = binary_path.exists();
        if !binary_exists {
            warn!(path = %binary_path.display(), "built binary not found at expected path");
        }

        let result = BuildResult {
            success: binary_exists,
            binary_path: if binary_exists {
                Some(binary_path)
            } else {
                None
            },
            built_sha,
            pull_duration,
            build_duration,
            test_duration,
            pull_log,
            build_log,
            test_log,
            completed_at: Utc::now(),
        };

        info!(
            success = result.success,
            build_secs = result.build_duration.as_secs(),
            "build pipeline complete"
        );

        Ok(result)
    }

    // ── Git helpers ──────────────────────────────────────────────────

    fn git_pull(&self, repo: &Path) -> UpdateResult<(String, Duration)> {
        let remote = &self.config.remote;
        let branch = &self.config.branch;

        info!("running git pull {remote} {branch}");
        let start = Instant::now();

        let output = Command::new("git")
            .args(["pull", remote, branch])
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::GitPullFailed {
                stderr: e.to_string(),
            })?;

        let duration = start.elapsed();
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UpdateError::GitPullFailed { stderr });
        }

        debug!(duration_ms = duration.as_millis(), "git pull complete");
        Ok((combined, duration))
    }

    fn git_head_sha(&self, repo: &Path) -> UpdateResult<String> {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::GitCommand {
                command: "git rev-parse HEAD".into(),
                stderr: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(UpdateError::GitCommand {
                command: "git rev-parse HEAD".into(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    // ── Cargo helpers ────────────────────────────────────────────────

    fn cargo_build(&self, repo: &Path) -> UpdateResult<(String, Duration)> {
        let mut args = vec!["build", "--release"];

        // Collect extra flags as owned strings, then reference them
        let extra: Vec<String> = self.config.extra_build_flags.clone();
        for flag in &extra {
            args.push(flag.as_str());
        }

        info!(args = ?args, "running cargo build");
        let start = Instant::now();

        let output = Command::new("cargo")
            .args(&args)
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::BuildFailed {
                exit_code: -1,
                stderr: e.to_string(),
            })?;

        let duration = start.elapsed();
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(UpdateError::BuildFailed {
                exit_code: code,
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        info!(duration_secs = duration.as_secs(), "cargo build succeeded");
        Ok((combined, duration))
    }

    fn cargo_test(&self, repo: &Path) -> UpdateResult<(String, Duration)> {
        let args = ["test", "--workspace", "--lib"];

        info!(args = ?args, "running cargo test");
        let start = Instant::now();

        let output = Command::new("cargo")
            .args(args)
            .current_dir(repo)
            .output()
            .map_err(|e| UpdateError::TestFailed {
                exit_code: -1,
                stderr: e.to_string(),
            })?;

        let duration = start.elapsed();
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(UpdateError::TestFailed {
                exit_code: code,
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        info!(duration_secs = duration.as_secs(), "cargo test passed");
        Ok((combined, duration))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = BuilderConfig::default();
        assert_eq!(cfg.binary_name, "forgefleet");
        assert!(cfg.run_tests);
        assert_eq!(cfg.build_timeout_secs, 1800);
        assert_eq!(cfg.test_timeout_secs, 600);
    }

    #[test]
    fn test_build_result_serialization() {
        let result = BuildResult {
            success: true,
            binary_path: Some(PathBuf::from("/tmp/forgefleet")),
            built_sha: "abc123".into(),
            pull_duration: Duration::from_secs(2),
            build_duration: Duration::from_secs(120),
            test_duration: Some(Duration::from_secs(30)),
            pull_log: "Already up to date.".into(),
            build_log: "Compiling...".into(),
            test_log: Some("test result: ok".into()),
            completed_at: Utc::now(),
        };

        let json = serde_json::to_string(&result).unwrap();
        let parsed: BuildResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.built_sha, "abc123");
    }
}
