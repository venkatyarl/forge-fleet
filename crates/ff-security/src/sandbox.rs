use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// No filesystem access.
    Locked,
    /// Read-only operations in scoped paths.
    ReadOnly,
    /// Read-write operations in scoped paths.
    WorkspaceWrite,
    /// Full access (highest trust, should be rare).
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProfile {
    pub name: String,
    pub mode: SandboxMode,
    pub allowed_paths: Vec<String>,
    pub denied_paths: Vec<String>,
    pub allow_network: bool,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub max_runtime_secs: u64,
    pub max_memory_mb: Option<u64>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SandboxValidationError {
    #[error("profile name cannot be empty")]
    EmptyName,

    #[error("allowed path cannot be empty")]
    EmptyAllowedPath,

    #[error("denied path cannot be empty")]
    EmptyDeniedPath,

    #[error("max_runtime_secs must be > 0")]
    InvalidRuntime,

    #[error("max_memory_mb must be > 0 when provided")]
    InvalidMemory,

    #[error("path appears in both allow and deny lists: {0}")]
    ConflictingPath(String),

    #[error("tool appears in both allow and deny lists: {0}")]
    ConflictingTool(String),

    #[error("locked mode cannot include allowed paths")]
    LockedModeHasAllowedPaths,
}

impl SandboxProfile {
    pub fn validate(&self) -> Result<(), SandboxValidationError> {
        if self.name.trim().is_empty() {
            return Err(SandboxValidationError::EmptyName);
        }

        if self.max_runtime_secs == 0 {
            return Err(SandboxValidationError::InvalidRuntime);
        }

        if self.max_memory_mb.is_some_and(|mb| mb == 0) {
            return Err(SandboxValidationError::InvalidMemory);
        }

        for p in &self.allowed_paths {
            if p.trim().is_empty() {
                return Err(SandboxValidationError::EmptyAllowedPath);
            }
        }

        for p in &self.denied_paths {
            if p.trim().is_empty() {
                return Err(SandboxValidationError::EmptyDeniedPath);
            }
            if self.allowed_paths.iter().any(|ap| ap == p) {
                return Err(SandboxValidationError::ConflictingPath(p.clone()));
            }
        }

        for tool in &self.allowed_tools {
            if self.denied_tools.iter().any(|t| t == tool) {
                return Err(SandboxValidationError::ConflictingTool(tool.clone()));
            }
        }

        if self.mode == SandboxMode::Locked && !self.allowed_paths.is_empty() {
            return Err(SandboxValidationError::LockedModeHasAllowedPaths);
        }

        Ok(())
    }
}
