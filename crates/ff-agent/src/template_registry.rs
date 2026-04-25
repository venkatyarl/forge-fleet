//! Template registry — discoverable library of agents, skills, commands, and hooks.
//!
//! Provides a browsable catalog of pre-built ForgeFleet components that users
//! can install and customize for their projects.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::info;

/// A template that can be installed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub category: TemplateCategory,
    pub description: String,
    pub version: String,
    pub author: String,
    pub tags: Vec<String>,
    /// Template content (YAML, TOML, or Markdown).
    pub content: String,
    /// Installation instructions.
    pub install_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateCategory {
    Agent,
    Skill,
    Command,
    Hook,
    McpServer,
    SystemPrompt,
    Workflow,
}

impl TemplateCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Skill => "skill",
            Self::Command => "command",
            Self::Hook => "hook",
            Self::McpServer => "mcp_server",
            Self::SystemPrompt => "system_prompt",
            Self::Workflow => "workflow",
        }
    }
}

/// Template registry.
pub struct TemplateRegistry {
    templates: Vec<Template>,
}

impl TemplateRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            templates: Vec::new(),
        };
        registry.load_builtins();
        registry
    }

    fn load_builtins(&mut self) {
        // Built-in agent templates
        self.templates.push(Template {
            id: "agent-rust-developer".into(),
            name: "Rust Developer Agent".into(),
            category: TemplateCategory::Agent,
            description: "Expert Rust agent with cargo/clippy/test tools".into(),
            version: "1.0.0".into(),
            author: "ForgeFleet".into(),
            tags: vec!["rust".into(), "systems".into(), "backend".into()],
            content: include_str!("agent_roles.rs")
                .lines()
                .take(5)
                .collect::<Vec<_>>()
                .join("\n"),
            install_path: ".forgefleet/agents/rust-developer.toml".into(),
        });

        self.templates.push(Template {
            id: "agent-security-auditor".into(),
            name: "Security Auditor Agent".into(),
            category: TemplateCategory::Agent,
            description: "Security review agent (read-only, no write access)".into(),
            version: "1.0.0".into(),
            author: "ForgeFleet".into(),
            tags: vec!["security".into(), "audit".into(), "review".into()],
            content: "Security auditor with OWASP top 10 analysis".into(),
            install_path: ".forgefleet/agents/security-auditor.toml".into(),
        });

        self.templates.push(Template {
            id: "agent-test-writer".into(),
            name: "Test Writer Agent".into(),
            category: TemplateCategory::Agent,
            description: "Generates comprehensive unit and integration tests".into(),
            version: "1.0.0".into(),
            author: "ForgeFleet".into(),
            tags: vec!["testing".into(), "qa".into(), "tdd".into()],
            content: "Test generation agent with project framework detection".into(),
            install_path: ".forgefleet/agents/test-writer.toml".into(),
        });

        // Built-in workflow templates
        self.templates.push(Template {
            id: "workflow-code-review".into(),
            name: "Code Review Workflow".into(),
            category: TemplateCategory::Workflow,
            description:
                "3-stage review: code agent writes, test agent validates, review agent approves"
                    .into(),
            version: "1.0.0".into(),
            author: "ForgeFleet".into(),
            tags: vec!["review".into(), "workflow".into(), "multi-agent".into()],
            content: "Pipeline: code → test → review across 3 fleet nodes".into(),
            install_path: ".forgefleet/workflows/code-review.toml".into(),
        });

        self.templates.push(Template {
            id: "workflow-consensus-coding".into(),
            name: "Consensus Coding Workflow".into(),
            category: TemplateCategory::Workflow,
            description: "3 agents solve same problem independently, pick best solution".into(),
            version: "1.0.0".into(),
            author: "ForgeFleet".into(),
            tags: vec!["consensus".into(), "parallel".into(), "reliability".into()],
            content: "N-of-M parallel coding with test validation".into(),
            install_path: ".forgefleet/workflows/consensus-coding.toml".into(),
        });

        // Hook templates
        self.templates.push(Template {
            id: "hook-pre-commit-lint".into(), name: "Pre-Commit Lint Hook".into(),
            category: TemplateCategory::Hook,
            description: "Run linter before committing agent changes".into(),
            version: "1.0.0".into(), author: "ForgeFleet".into(),
            tags: vec!["hook".into(), "lint".into(), "pre-commit".into()],
            content: "command = 'cargo clippy -- -D warnings'\nevent = 'pre_tool_use'\ntool_filter = 'Bash'\nblocking = true".into(),
            install_path: ".forgefleet/hooks/pre-commit-lint.toml".into(),
        });

        // MCP server templates
        self.templates.push(Template {
            id: "mcp-filesystem".into(), name: "Filesystem MCP Server".into(),
            category: TemplateCategory::McpServer,
            description: "MCP server for filesystem access (read/write/search)".into(),
            version: "1.0.0".into(), author: "Anthropic".into(),
            tags: vec!["mcp".into(), "filesystem".into()],
            content: "command = 'npx'\nargs = ['-y', '@modelcontextprotocol/server-filesystem', '/path/to/dir']".into(),
            install_path: ".forgefleet/mcp/filesystem.toml".into(),
        });

        self.templates.push(Template {
            id: "mcp-github".into(), name: "GitHub MCP Server".into(),
            category: TemplateCategory::McpServer,
            description: "MCP server for GitHub API access (issues, PRs, repos)".into(),
            version: "1.0.0".into(), author: "GitHub".into(),
            tags: vec!["mcp".into(), "github".into(), "git".into()],
            content: "command = 'npx'\nargs = ['-y', '@modelcontextprotocol/server-github']\nenv = { GITHUB_TOKEN = '$GITHUB_TOKEN' }".into(),
            install_path: ".forgefleet/mcp/github.toml".into(),
        });
    }

    /// Search templates by query.
    pub fn search(&self, query: &str) -> Vec<&Template> {
        let lower = query.to_ascii_lowercase();
        let mut results: Vec<(&Template, f64)> = self
            .templates
            .iter()
            .filter_map(|t| {
                let mut score = 0.0;
                if t.name.to_ascii_lowercase().contains(&lower) {
                    score += 10.0;
                }
                if t.description.to_ascii_lowercase().contains(&lower) {
                    score += 5.0;
                }
                if t.tags
                    .iter()
                    .any(|tag| tag.to_ascii_lowercase().contains(&lower))
                {
                    score += 8.0;
                }
                if t.id.to_ascii_lowercase().contains(&lower) {
                    score += 6.0;
                }
                if score > 0.0 { Some((t, score)) } else { None }
            })
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.into_iter().map(|(t, _)| t).collect()
    }

    /// List templates by category.
    pub fn by_category(&self, category: TemplateCategory) -> Vec<&Template> {
        self.templates
            .iter()
            .filter(|t| t.category == category)
            .collect()
    }

    /// Get a template by ID.
    pub fn get(&self, id: &str) -> Option<&Template> {
        self.templates.iter().find(|t| t.id == id)
    }

    /// Install a template to the project directory.
    pub async fn install(&self, id: &str, project_dir: &Path) -> anyhow::Result<String> {
        let template = self
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Template '{id}' not found"))?;
        let install_path = project_dir.join(&template.install_path);

        if let Some(parent) = install_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        fs::write(&install_path, &template.content).await?;
        info!(id, path = %install_path.display(), "template installed");
        Ok(install_path.to_string_lossy().to_string())
    }

    /// List all templates.
    pub fn list(&self) -> Vec<TemplateSummary> {
        self.templates
            .iter()
            .map(|t| TemplateSummary {
                id: t.id.clone(),
                name: t.name.clone(),
                category: t.category,
                description: t.description.clone(),
                tags: t.tags.clone(),
            })
            .collect()
    }
}

impl Default for TemplateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TemplateSummary {
    pub id: String,
    pub name: String,
    pub category: TemplateCategory,
    pub description: String,
    pub tags: Vec<String>,
}
