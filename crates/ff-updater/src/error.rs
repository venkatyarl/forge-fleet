//! Update-specific error types.

use std::path::PathBuf;

/// All errors that can occur during the update lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    // ── Check ────────────────────────────────────────────────────────
    #[error("update check failed: {reason}")]
    CheckFailed { reason: String },

    #[error("git command failed: {command} — {stderr}")]
    GitCommand { command: String, stderr: String },

    // ── Build ────────────────────────────────────────────────────────
    #[error("git pull failed: {stderr}")]
    GitPullFailed { stderr: String },

    #[error("cargo build failed (exit {exit_code}): {stderr}")]
    BuildFailed { exit_code: i32, stderr: String },

    #[error("cargo test failed (exit {exit_code}): {stderr}")]
    TestFailed { exit_code: i32, stderr: String },

    // ── Verify ───────────────────────────────────────────────────────
    #[error("binary not found at {}", path.display())]
    BinaryNotFound { path: PathBuf },

    #[error("binary too small ({size_bytes} bytes, min {min_bytes})")]
    BinaryTooSmall { size_bytes: u64, min_bytes: u64 },

    #[error("binary version check failed: {reason}")]
    VersionCheckFailed { reason: String },

    #[error("smoke test failed: {command} — {stderr}")]
    SmokeTestFailed { command: String, stderr: String },

    // ── Swap ─────────────────────────────────────────────────────────
    #[error("swap failed: could not rename current binary to .bak: {reason}")]
    BackupFailed { reason: String },

    #[error("swap failed: could not copy new binary into place: {reason}")]
    CopyFailed { reason: String },

    #[error("swap failed: permission fix failed: {reason}")]
    PermissionFailed { reason: String },

    // ── Rollback ─────────────────────────────────────────────────────
    #[error("rollback failed: no .bak file at {}", path.display())]
    NoBackup { path: PathBuf },

    #[error("rollback failed: {reason}")]
    RollbackFailed { reason: String },

    #[error("health check failed after rollback: {reason}")]
    HealthCheckFailed { reason: String },

    // ── Orchestration ────────────────────────────────────────────────
    #[error("update already in progress (state: {state})")]
    AlreadyInProgress { state: String },

    #[error("fleet coordination error: {reason}")]
    FleetCoordination { reason: String },

    // ── General ──────────────────────────────────────────────────────
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("{0}")]
    Other(String),
}

/// Convenience alias.
pub type UpdateResult<T> = std::result::Result<T, UpdateError>;
