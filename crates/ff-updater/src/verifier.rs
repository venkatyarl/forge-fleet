//! Binary verification — ensure a newly built binary is sane before swapping.
//!
//! Checks:
//! 1. Binary exists and has reasonable size
//! 2. Binary executes `--version` successfully
//! 3. Optional smoke test commands pass

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{UpdateError, UpdateResult};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for binary verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifierConfig {
    /// Minimum acceptable binary size in bytes (default: 1 MiB).
    /// Catches truncated or empty builds.
    #[serde(default = "default_min_size")]
    pub min_binary_size: u64,

    /// Maximum acceptable binary size in bytes (default: 500 MiB).
    /// Catches absurdly large builds that might indicate debug symbols left in.
    #[serde(default = "default_max_size")]
    pub max_binary_size: u64,

    /// The version flag to pass (default: "--version").
    #[serde(default = "default_version_flag")]
    pub version_flag: String,

    /// Optional smoke test commands to run against the new binary.
    /// Each string is a full command where `{binary}` is replaced with the path.
    /// e.g. `["{binary} --health-check", "{binary} config validate"]`
    #[serde(default)]
    pub smoke_tests: Vec<String>,

    /// Timeout for each verification command (seconds, default: 30).
    #[serde(default = "default_verify_timeout")]
    pub verify_timeout_secs: u64,
}

fn default_min_size() -> u64 {
    1_048_576 // 1 MiB
}
fn default_max_size() -> u64 {
    524_288_000 // 500 MiB
}
fn default_version_flag() -> String {
    "--version".into()
}
fn default_verify_timeout() -> u64 {
    30
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            min_binary_size: default_min_size(),
            max_binary_size: default_max_size(),
            version_flag: default_version_flag(),
            smoke_tests: Vec::new(),
            verify_timeout_secs: default_verify_timeout(),
        }
    }
}

// ─── Verification result ─────────────────────────────────────────────────────

/// Outcome of binary verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    /// Whether all checks passed.
    pub passed: bool,

    /// Binary size in bytes.
    pub binary_size: u64,

    /// Output of `--version` (if it ran).
    pub version_output: Option<String>,

    /// Results of smoke tests (command → pass/fail + output).
    pub smoke_test_results: Vec<SmokeTestResult>,

    /// Human-readable summary of what was checked.
    pub summary: String,
}

/// Result of a single smoke test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmokeTestResult {
    pub command: String,
    pub passed: bool,
    pub output: String,
}

// ─── Verifier ────────────────────────────────────────────────────────────────

/// Verifies a newly built binary before it replaces the current one.
pub struct BinaryVerifier {
    config: VerifierConfig,
}

impl BinaryVerifier {
    pub fn new(config: VerifierConfig) -> Self {
        Self { config }
    }

    /// Run all verification checks on the binary at `binary_path`.
    pub fn verify(&self, binary_path: &Path) -> UpdateResult<VerifyResult> {
        info!(path = %binary_path.display(), "verifying binary");

        let mut issues: Vec<String> = Vec::new();

        // 1. Check existence
        if !binary_path.exists() {
            return Err(UpdateError::BinaryNotFound {
                path: binary_path.to_path_buf(),
            });
        }

        // 2. Check size
        let metadata = std::fs::metadata(binary_path)?;
        let binary_size = metadata.len();
        debug!(binary_size, "binary size");

        if binary_size < self.config.min_binary_size {
            return Err(UpdateError::BinaryTooSmall {
                size_bytes: binary_size,
                min_bytes: self.config.min_binary_size,
            });
        }

        if binary_size > self.config.max_binary_size {
            issues.push(format!(
                "binary is unusually large ({binary_size} bytes, max {})",
                self.config.max_binary_size
            ));
            warn!(
                binary_size,
                max = self.config.max_binary_size,
                "binary larger than expected"
            );
        }

        // 3. Version check
        let version_output = match self.check_version(binary_path) {
            Ok(output) => {
                info!(version = %output.trim(), "version check passed");
                Some(output)
            }
            Err(e) => {
                issues.push(format!("version check failed: {e}"));
                None
            }
        };

        // 4. Smoke tests
        let mut smoke_test_results = Vec::new();
        for smoke_cmd in &self.config.smoke_tests {
            let result = self.run_smoke_test(binary_path, smoke_cmd);
            if !result.passed {
                issues.push(format!("smoke test failed: {}", smoke_cmd));
            }
            smoke_test_results.push(result);
        }

        let passed = issues.is_empty() && version_output.is_some();
        let summary = if passed {
            format!(
                "All checks passed. Binary: {} bytes, {} smoke tests OK.",
                binary_size,
                smoke_test_results.len()
            )
        } else {
            format!("Verification issues: {}", issues.join("; "))
        };

        info!(passed, summary = %summary, "verification complete");

        Ok(VerifyResult {
            passed,
            binary_size,
            version_output,
            smoke_test_results,
            summary,
        })
    }

    // ── Helpers ──────────────────────────────────────────────────────

    fn check_version(&self, binary_path: &Path) -> UpdateResult<String> {
        let output = Command::new(binary_path)
            .arg(&self.config.version_flag)
            .output()
            .map_err(|e| UpdateError::VersionCheckFailed {
                reason: format!("failed to execute: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UpdateError::VersionCheckFailed {
                reason: format!("exit code {}: {stderr}", output.status.code().unwrap_or(-1)),
            });
        }

        // Version string is usually on stdout
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if stdout.trim().is_empty() {
            // Some programs print version to stderr
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if stderr.trim().is_empty() {
                return Err(UpdateError::VersionCheckFailed {
                    reason: "no output from --version".into(),
                });
            }
            return Ok(stderr);
        }

        Ok(stdout)
    }

    fn run_smoke_test(&self, binary_path: &Path, command_template: &str) -> SmokeTestResult {
        let command = command_template.replace("{binary}", &binary_path.display().to_string());
        debug!(command = %command, "running smoke test");

        // Split command into program + args
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return SmokeTestResult {
                command: command.clone(),
                passed: false,
                output: "empty command".into(),
            };
        }

        let result = Command::new(parts[0]).args(&parts[1..]).output();

        match result {
            Ok(output) => {
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
                SmokeTestResult {
                    command,
                    passed: output.status.success(),
                    output: combined,
                }
            }
            Err(e) => SmokeTestResult {
                command,
                passed: false,
                output: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = VerifierConfig::default();
        assert_eq!(cfg.min_binary_size, 1_048_576);
        assert_eq!(cfg.version_flag, "--version");
        assert!(cfg.smoke_tests.is_empty());
    }

    #[test]
    fn test_verify_nonexistent_binary() {
        let verifier = BinaryVerifier::new(VerifierConfig::default());
        let result = verifier.verify(Path::new("/nonexistent/binary"));
        assert!(result.is_err());
    }

    #[test]
    fn test_smoke_test_result_serialization() {
        let result = SmokeTestResult {
            command: "test --health".into(),
            passed: true,
            output: "OK".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SmokeTestResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.passed);
    }
}
