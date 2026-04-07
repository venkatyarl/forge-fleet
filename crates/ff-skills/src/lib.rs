//! `ff-skills` — ForgeFleet universal skill system.
//!
//! This crate provides a unified interface for discovering, loading, adapting,
//! executing, and selecting skills from multiple sources:
//!
//! - **OpenClaw** — SKILL.md directory-based skills
//! - **Claude** — Anthropic tool definitions (`input_schema`)
//! - **MCP** — Model Context Protocol JSON-RPC tools (`inputSchema`)
//! - **Custom** — User-defined / programmatic tool registrations
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────┐     ┌──────────┐     ┌───────────┐
//! │  Discovery   │────▶│ Registry │◀────│  Loader   │
//! │ (filesystem) │     │ (DashMap)│     │ (parsers) │
//! └─────────────┘     └────┬─────┘     └───────────┘
//!                          │
//!                     ┌────▼─────┐
//!                     │ Selector │  ← query → ranked skills
//!                     └────┬─────┘
//!                          │
//!                     ┌────▼─────┐
//!                     │ Executor │  ← sandbox, timeout, permissions
//!                     └──────────┘
//! ```
//!
//! ## Adapters
//!
//! Each adapter implements `SkillAdapter` for bidirectional conversion between
//! a provider-specific format and ForgeFleet's universal `SkillMetadata` model.

pub mod adapters;
pub mod error;
pub mod executor;
pub mod loader;
pub mod registry;
pub mod selector;
pub mod types;

// ── Re-exports for convenience ───────────────────────────────────────────────

pub use error::{Result, SkillError};
pub use executor::{ExecutorConfig, SkillExecutor};
pub use registry::SkillRegistry;
pub use selector::{ScoredSkill, SelectorConfig, SkillSelector};
pub use types::{
    SkillMetadata, SkillOrigin, SkillPermission, ToolDefinition, ToolExecutionResult,
    ToolInvocation, ToolParameter,
};

// ── Adapter re-exports ───────────────────────────────────────────────────────

pub use adapters::SkillAdapter;
pub use adapters::claude::ClaudeAdapter;
pub use adapters::custom::CustomAdapter;
pub use adapters::mcp::McpAdapter;
pub use adapters::openclaw::OpenClawAdapter;

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Auto-detect the format of a raw JSON skill definition and import it using
/// the appropriate adapter.
pub fn auto_import(raw: &serde_json::Value) -> Result<SkillMetadata> {
    // Try adapters in specificity order: MCP → Claude → OpenClaw → Custom.
    let mcp = McpAdapter::new();
    if mcp.can_handle(raw) {
        return mcp.import(raw);
    }

    let claude = ClaudeAdapter::new();
    if claude.can_handle(raw) {
        return claude.import(raw);
    }

    let openclaw = OpenClawAdapter::new();
    if openclaw.can_handle(raw) {
        return openclaw.import(raw);
    }

    // Fallback to custom.
    let custom = CustomAdapter::new();
    custom.import(raw)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_import_mcp() {
        let raw = serde_json::json!({
            "name": "search",
            "description": "Search",
            "inputSchema": {
                "type": "object",
                "properties": { "q": { "type": "string" } }
            }
        });
        let skill = auto_import(&raw).unwrap();
        assert_eq!(skill.origin, SkillOrigin::Mcp);
    }

    #[test]
    fn test_auto_import_claude() {
        let raw = serde_json::json!({
            "name": "greet",
            "description": "Greet",
            "input_schema": {
                "type": "object",
                "properties": { "name": { "type": "string" } }
            }
        });
        let skill = auto_import(&raw).unwrap();
        assert_eq!(skill.origin, SkillOrigin::Claude);
    }

    #[test]
    fn test_auto_import_openclaw() {
        let raw = serde_json::json!({
            "name": "weather",
            "location": "/skills/weather",
            "tools": [{ "name": "get", "command": "echo hi" }]
        });
        let skill = auto_import(&raw).unwrap();
        assert_eq!(skill.origin, SkillOrigin::OpenClaw);
    }

    #[test]
    fn test_auto_import_custom_fallback() {
        let raw = serde_json::json!({ "name": "mystery" });
        let skill = auto_import(&raw).unwrap();
        assert_eq!(skill.origin, SkillOrigin::Custom);
    }

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
    }
}
