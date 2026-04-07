//! Skill-specific error types.

use std::path::PathBuf;

/// Errors specific to the skill system.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    // ── Loading ──────────────────────────────────────────────────────
    #[error("skill not found: {name}")]
    NotFound { name: String },

    #[error("skill file not found: {}", path.display())]
    FileNotFound { path: PathBuf },

    #[error("failed to parse SKILL.md at {}: {reason}", path.display())]
    MarkdownParse { path: PathBuf, reason: String },

    #[error("failed to parse MCP tool definition: {reason}")]
    McpParse { reason: String },

    #[error("invalid skill manifest: {reason}")]
    InvalidManifest { reason: String },

    // ── Registry ─────────────────────────────────────────────────────
    #[error("skill already registered: {name}")]
    AlreadyRegistered { name: String },

    #[error("skill directory not found: {}", path.display())]
    DirectoryNotFound { path: PathBuf },

    // ── Execution ────────────────────────────────────────────────────
    #[error("tool not found in skill {skill}: {tool}")]
    ToolNotFound { skill: String, tool: String },

    #[error("execution timed out after {timeout_secs}s for tool {tool}")]
    ExecutionTimeout { tool: String, timeout_secs: u64 },

    #[error("permission denied: skill {skill} lacks permission {permission}")]
    PermissionDenied { skill: String, permission: String },

    #[error("sandbox violation: {reason}")]
    SandboxViolation { reason: String },

    #[error("execution failed for tool {tool}: {reason}")]
    ExecutionFailed { tool: String, reason: String },

    // ── Adapter ──────────────────────────────────────────────────────
    #[error("adapter error ({adapter}): {reason}")]
    AdapterError { adapter: String, reason: String },

    // ── Wrapped ──────────────────────────────────────────────────────
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, SkillError>;
