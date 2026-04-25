//! Plugin/skill ecosystem — dynamic loading, discovery, and management.
//!
//! Supports loading plugins from:
//! 1. Local filesystem (~/.forgefleet/plugins/)
//! 2. Project directory (.forgefleet/plugins/)
//! 3. Skill manifest files (SKILL.md format)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, info, warn};

/// Plugin manifest (loaded from plugin.toml or plugin.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Commands this plugin provides.
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    /// Tools this plugin provides.
    #[serde(default)]
    pub tools: Vec<PluginTool>,
    /// Required permissions.
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    /// Shell script to execute.
    pub script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTool {
    pub name: String,
    pub description: String,
    /// JSON Schema for input parameters.
    pub input_schema: serde_json::Value,
    /// Shell command template with $PARAM substitution.
    pub command_template: String,
}

/// Plugin registry.
#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    pub plugins: HashMap<String, LoadedPlugin>,
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub path: PathBuf,
    pub enabled: bool,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discover and load plugins from standard directories.
    pub async fn discover(&mut self, working_dir: &Path) {
        // 1. Global plugins
        if let Some(home) = dirs::home_dir() {
            let global_dir = home.join(".forgefleet").join("plugins");
            self.load_directory(&global_dir).await;
        }

        // 2. Project plugins
        let project_dir = working_dir.join(".forgefleet").join("plugins");
        self.load_directory(&project_dir).await;

        info!(count = self.plugins.len(), "plugins discovered");
    }

    async fn load_directory(&mut self, dir: &Path) {
        let mut entries = match fs::read_dir(dir).await {
            Ok(e) => e,
            Err(_) => return,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Try loading manifest
            for manifest_name in ["plugin.toml", "plugin.json", "SKILL.md"] {
                let manifest_path = path.join(manifest_name);
                if manifest_path.exists() {
                    match self.load_plugin(&path, &manifest_path).await {
                        Ok(plugin) => {
                            debug!(name = %plugin.manifest.name, "loaded plugin");
                            self.plugins.insert(plugin.manifest.name.clone(), plugin);
                        }
                        Err(e) => {
                            warn!(path = %path.display(), error = %e, "failed to load plugin");
                        }
                    }
                    break;
                }
            }
        }
    }

    async fn load_plugin(&self, dir: &Path, manifest_path: &Path) -> anyhow::Result<LoadedPlugin> {
        let content = fs::read_to_string(manifest_path).await?;

        let manifest: PluginManifest =
            if manifest_path.extension().and_then(|e| e.to_str()) == Some("toml") {
                toml::from_str(&content)?
            } else if manifest_path.extension().and_then(|e| e.to_str()) == Some("json") {
                serde_json::from_str(&content)?
            } else {
                // Parse SKILL.md format
                parse_skill_md(&content, dir)?
            };

        Ok(LoadedPlugin {
            manifest,
            path: dir.to_path_buf(),
            enabled: true,
        })
    }

    /// Get all tools from loaded plugins.
    pub fn all_tools(&self) -> Vec<(&str, &PluginTool)> {
        self.plugins
            .iter()
            .filter(|(_, p)| p.enabled)
            .flat_map(|(name, p)| p.manifest.tools.iter().map(move |t| (name.as_str(), t)))
            .collect()
    }

    /// Get all commands from loaded plugins.
    pub fn all_commands(&self) -> Vec<(&str, &PluginCommand)> {
        self.plugins
            .iter()
            .filter(|(_, p)| p.enabled)
            .flat_map(|(name, p)| p.manifest.commands.iter().map(move |c| (name.as_str(), c)))
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&LoadedPlugin> {
        self.plugins.get(name)
    }

    pub fn enable(&mut self, name: &str) -> bool {
        if let Some(p) = self.plugins.get_mut(name) {
            p.enabled = true;
            true
        } else {
            false
        }
    }

    pub fn disable(&mut self, name: &str) -> bool {
        if let Some(p) = self.plugins.get_mut(name) {
            p.enabled = false;
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .values()
            .map(|p| PluginInfo {
                name: p.manifest.name.clone(),
                version: p.manifest.version.clone(),
                description: p.manifest.description.clone(),
                enabled: p.enabled,
                tools: p.manifest.tools.len(),
                commands: p.manifest.commands.len(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    pub tools: usize,
    pub commands: usize,
}

/// Parse a SKILL.md format manifest.
fn parse_skill_md(content: &str, dir: &Path) -> anyhow::Result<PluginManifest> {
    let mut name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut description = String::new();
    let tools = Vec::new();

    for line in content.lines() {
        if line.starts_with("# ") {
            name = line[2..].trim().to_string();
        } else if description.is_empty() && !line.starts_with('#') && !line.trim().is_empty() {
            description = line.trim().to_string();
        }
    }

    Ok(PluginManifest {
        name,
        version: "0.1.0".into(),
        description,
        author: String::new(),
        commands: Vec::new(),
        tools,
        permissions: Vec::new(),
    })
}
