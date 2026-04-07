//! Core skill types — the universal data model for skills across all adapters.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ─── Skill Origin ────────────────────────────────────────────────────────────

/// Where a skill was discovered / imported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillOrigin {
    /// OpenClaw SKILL.md format (directory with SKILL.md + optional scripts).
    OpenClaw,
    /// Claude Code / Anthropic tool definitions.
    Claude,
    /// Model Context Protocol JSON tool definitions.
    Mcp,
    /// Custom / user-defined skill format.
    Custom,
    /// Discovered from filesystem scan.
    Filesystem,
}

impl std::fmt::Display for SkillOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenClaw => write!(f, "OpenClaw"),
            Self::Claude => write!(f, "Claude"),
            Self::Mcp => write!(f, "MCP"),
            Self::Custom => write!(f, "Custom"),
            Self::Filesystem => write!(f, "Filesystem"),
        }
    }
}

// ─── Permission ──────────────────────────────────────────────────────────────

/// Permissions a skill may request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPermission {
    /// Read files from the workspace.
    FileRead,
    /// Write files to the workspace.
    FileWrite,
    /// Execute shell commands.
    ShellExec,
    /// Make outbound network requests.
    Network,
    /// Access environment variables.
    EnvAccess,
    /// Access secrets / credentials.
    Secrets,
    /// Spawn sub-processes.
    ProcessSpawn,
    /// Custom named permission.
    Custom(String),
}

impl std::fmt::Display for SkillPermission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileRead => write!(f, "file:read"),
            Self::FileWrite => write!(f, "file:write"),
            Self::ShellExec => write!(f, "shell:exec"),
            Self::Network => write!(f, "network"),
            Self::EnvAccess => write!(f, "env:access"),
            Self::Secrets => write!(f, "secrets"),
            Self::ProcessSpawn => write!(f, "process:spawn"),
            Self::Custom(s) => write!(f, "custom:{s}"),
        }
    }
}

// ─── Tool Parameter ──────────────────────────────────────────────────────────

/// Schema for a single tool parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParameter {
    /// Parameter name.
    pub name: String,
    /// JSON Schema type (string, number, boolean, array, object).
    pub param_type: String,
    /// Human description.
    #[serde(default)]
    pub description: String,
    /// Whether this parameter is required.
    #[serde(default)]
    pub required: bool,
    /// Default value (JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// Enum constraints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enum_values: Vec<serde_json::Value>,
}

// ─── Tool Definition ─────────────────────────────────────────────────────────

/// A single executable tool within a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name (unique within the skill).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Input parameters schema.
    #[serde(default)]
    pub parameters: Vec<ToolParameter>,
    /// How to invoke this tool.
    pub invocation: ToolInvocation,
    /// Permissions required by this tool.
    #[serde(default)]
    pub permissions: Vec<SkillPermission>,
    /// Maximum execution time in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

/// How a tool is invoked at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ToolInvocation {
    /// Run a shell command / script.
    Shell {
        command: String,
        #[serde(default)]
        working_dir: Option<PathBuf>,
    },
    /// Call an HTTP endpoint (MCP-style).
    Http {
        url: String,
        #[serde(default = "default_method")]
        method: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Inline function reference (Rust callback).
    Builtin { handler: String },
    /// Prompt injection — the tool is a prompt template for the LLM.
    Prompt { template: String },
}

fn default_method() -> String {
    "POST".into()
}

// ─── Skill Metadata ──────────────────────────────────────────────────────────

/// Complete metadata for a registered skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Unique skill identifier (derived from directory name / manifest).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Description of what this skill does.
    pub description: String,
    /// Where the skill was loaded from.
    pub origin: SkillOrigin,
    /// Filesystem path to the skill root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<PathBuf>,
    /// Version string (if available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Author / maintainer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Tags for search / categorization.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tools provided by this skill.
    pub tools: Vec<ToolDefinition>,
    /// Permissions the skill as a whole requires.
    #[serde(default)]
    pub permissions: Vec<SkillPermission>,
    /// When the skill was registered.
    pub registered_at: DateTime<Utc>,
    /// Internal UUID for dedup.
    pub uuid: Uuid,
    /// Keywords extracted from description + tags for search.
    #[serde(default)]
    pub search_keywords: Vec<String>,
}

impl SkillMetadata {
    /// Build the keyword index from name, description, and tags.
    pub fn rebuild_keywords(&mut self) {
        let mut words: Vec<String> = Vec::new();
        // Add name tokens.
        words.extend(
            self.name
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| s.len() >= 2)
                .map(|s| s.to_lowercase()),
        );
        // Add description tokens.
        words.extend(
            self.description
                .split_whitespace()
                .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()))
                .filter(|s| s.len() >= 3)
                .map(|s| s.to_lowercase()),
        );
        // Add tags as-is.
        words.extend(self.tags.iter().map(|t| t.to_lowercase()));
        words.sort();
        words.dedup();
        self.search_keywords = words;
    }

    /// Number of tools in this skill.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Find a tool by name.
    pub fn find_tool(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.iter().find(|t| t.name == name)
    }
}

// ─── Execution Result ────────────────────────────────────────────────────────

/// Result of executing a skill tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionResult {
    /// Which skill this came from.
    pub skill_id: String,
    /// Which tool was executed.
    pub tool_name: String,
    /// Whether execution succeeded.
    pub success: bool,
    /// Output content (stdout / response body / result).
    pub output: String,
    /// Error message if failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Exit code for shell tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// When execution completed.
    pub completed_at: DateTime<Utc>,
}
