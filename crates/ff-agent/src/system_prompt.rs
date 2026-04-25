//! Modular system prompt builder — assembles the system prompt from sections.
//!
//! Sections:
//! 1. Core identity and capabilities
//! 2. Tool descriptions (auto-generated from registered tools)
//! 3. Guidelines and rules
//! 4. Project memory (FORGEFLEET.md)
//! 5. Dynamic context (git status, project info)
//! 6. Custom system prompt (user override)

use std::path::Path;

use crate::memory;
use crate::tools::AgentTool;

/// Output style for agent responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStyle {
    Concise,
    Normal,
    Verbose,
}

/// Effort level that affects model behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortLevel {
    Low,
    Medium,
    High,
}

impl EffortLevel {
    pub fn temperature(&self) -> f32 {
        match self {
            Self::Low => 0.2,
            Self::Medium => 0.3,
            Self::High => 0.5,
        }
    }
}

/// Configuration for building the system prompt.
pub struct SystemPromptConfig<'a> {
    pub working_dir: &'a Path,
    pub tools: &'a [Box<dyn AgentTool>],
    pub memory_files: &'a [memory::MemoryFile],
    pub git_status: Option<&'a str>,
    pub custom_prompt: Option<&'a str>,
    pub output_style: OutputStyle,
    pub effort_level: EffortLevel,
}

/// Build the full system prompt from all sections.
pub fn build_system_prompt(config: &SystemPromptConfig) -> String {
    let mut prompt = String::with_capacity(4096);

    // Section 1: Core identity
    prompt.push_str(&core_identity(config.working_dir));

    // Section 2: Tool descriptions
    prompt.push_str(&tool_section(config.tools));

    // Section 3: Guidelines
    prompt.push_str(&guidelines_section(config.output_style));

    // Section 4: Project memory
    let memory_ctx = memory::build_memory_context(config.memory_files);
    if !memory_ctx.is_empty() {
        prompt.push_str(&memory_ctx);
    }

    // Section 5: Dynamic context
    if let Some(git) = config.git_status {
        if !git.trim().is_empty() {
            prompt.push_str("\n\n## Git Status\n\n```\n");
            prompt.push_str(git.trim());
            prompt.push_str("\n```\n");
        }
    }

    // Section 6: Custom prompt (appended)
    if let Some(custom) = config.custom_prompt {
        if !custom.trim().is_empty() {
            prompt.push_str("\n\n## Additional Instructions\n\n");
            prompt.push_str(custom.trim());
            prompt.push('\n');
        }
    }

    prompt
}

fn core_identity(working_dir: &Path) -> String {
    format!(
        r#"You are a ForgeFleet coding agent — an autonomous AI assistant that uses tools to accomplish software engineering tasks. You are part of a distributed fleet of AI agents running on local hardware.

Working directory: {working_dir}

You have access to tools for reading, writing, and editing files, running shell commands, searching codebases, fetching web content, spawning sub-agents, and managing tasks. Use tools to interact with the codebase — never guess at file contents or command outputs.
"#,
        working_dir = working_dir.display()
    )
}

fn tool_section(tools: &[Box<dyn AgentTool>]) -> String {
    let mut section = String::from("\n## Available Tools\n\n");
    for tool in tools {
        section.push_str(&format!("- **{}**: {}\n", tool.name(), tool.description()));
    }
    section
}

fn guidelines_section(style: OutputStyle) -> String {
    let style_note = match style {
        OutputStyle::Concise => {
            "Keep responses extremely concise. Lead with actions, not explanations."
        }
        OutputStyle::Normal => "Keep responses concise but informative.",
        OutputStyle::Verbose => "Provide detailed explanations and reasoning.",
    };

    format!(
        r#"
## Guidelines

- Always read a file before editing it.
- Use Edit for modifying existing files (not Write, which overwrites entirely).
- Use Bash for running builds, tests, and git commands.
- Be precise with Edit — old_string must match exactly including whitespace.
- When investigating issues, read the relevant code first before making changes.
- After making changes, verify them by reading the modified file or running tests.
- Use Agent tool to delegate complex subtasks to sub-agents on fleet nodes.
- Use TaskCreate to track multi-step work.
- {style_note}
"#
    )
}

/// Get git status for the working directory (for dynamic context injection).
pub async fn get_git_status(working_dir: &Path) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(["status", "--short", "--branch"])
        .current_dir(working_dir)
        .output()
        .await
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}
