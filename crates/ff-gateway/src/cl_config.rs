//! Claude Code configuration for newly enrolled nodes.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Ensure the enrolled runtime user's Claude Code configuration includes the
/// local ForgeFleet MCP server.
pub fn ensure_claude_config(runtime_user: &str) -> Result<()> {
    let home = if cfg!(target_os = "macos") {
        PathBuf::from("/Users").join(runtime_user)
    } else {
        PathBuf::from("/home").join(runtime_user)
    };
    let path = home.join(".claude").join("settings.json");

    let mut settings: Value = if path.exists() {
        let contents =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&contents).with_context(|| format!("parse {}", path.display()))?
    } else {
        json!({})
    };

    let servers = settings
        .as_object_mut()
        .context("Claude settings must be a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    servers
        .as_object_mut()
        .context("Claude mcpServers must be a JSON object")?
        .insert(
            "forgefleet".into(),
            json!({ "command": "forgefleetd", "args": ["mcp", "--stdio"] }),
        );

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)
        .with_context(|| format!("write {}", path.display()))
}
