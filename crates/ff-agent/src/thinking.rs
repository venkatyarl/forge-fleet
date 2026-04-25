//! Extended thinking / reasoning support for agent sessions.
//!
//! When enabled, the system prompt instructs the LLM to think through problems
//! step-by-step before acting. Configurable per-session.

use serde::{Deserialize, Serialize};

/// Thinking mode configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    /// No thinking blocks.
    Off,
    /// Always include thinking.
    On,
    /// Adaptively enable thinking based on task complexity.
    Adaptive,
}

impl Default for ThinkingMode {
    fn default() -> Self {
        Self::Adaptive
    }
}

/// Thinking configuration for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    pub mode: ThinkingMode,
    /// Maximum thinking tokens (default 4096).
    pub budget_tokens: u32,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self {
            mode: ThinkingMode::Adaptive,
            budget_tokens: 4096,
        }
    }
}

impl ThinkingConfig {
    /// Should thinking be enabled for the current context?
    pub fn should_enable(&self, turn: u32, has_tool_calls: bool) -> bool {
        match self.mode {
            ThinkingMode::Off => false,
            ThinkingMode::On => true,
            ThinkingMode::Adaptive => {
                // Enable thinking on first turn and after complex tool results
                turn <= 1 || has_tool_calls
            }
        }
    }
}

/// Build the thinking section of the system prompt.
pub fn thinking_prompt_section(config: &ThinkingConfig) -> String {
    match config.mode {
        ThinkingMode::Off => String::new(),
        ThinkingMode::On | ThinkingMode::Adaptive => {
            r#"
## Thinking Process

Before taking action, think through the problem:
1. What is being asked?
2. What do I already know from context?
3. What information do I need to gather (read files, run commands)?
4. What's the best approach?
5. What could go wrong?

When the task is complex, break it into steps and tackle each one. Verify your work after making changes.
"#.to_string()
        }
    }
}

/// Estimate if a user prompt is "complex" enough to warrant thinking.
pub fn estimate_complexity(prompt: &str) -> Complexity {
    let lower = prompt.to_ascii_lowercase();
    let word_count = prompt.split_whitespace().count();

    // Simple heuristics for complexity estimation
    let complex_keywords = [
        "refactor",
        "redesign",
        "architect",
        "migrate",
        "implement",
        "debug",
        "investigate",
        "optimize",
        "security",
        "performance",
        "multiple files",
        "entire",
        "all of",
        "comprehensive",
    ];

    let complex_count = complex_keywords
        .iter()
        .filter(|k| lower.contains(*k))
        .count();

    if complex_count >= 2 || word_count > 50 {
        Complexity::High
    } else if complex_count >= 1 || word_count > 20 {
        Complexity::Medium
    } else {
        Complexity::Low
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complexity {
    Low,
    Medium,
    High,
}
