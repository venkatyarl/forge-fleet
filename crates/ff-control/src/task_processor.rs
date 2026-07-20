//! Task processing logic with complexity flagging.
//!
//! Each incoming [`AgentTask`] is classified as mechanical, moderate, or complex
//! based on the shape of its payload. The flag can be used downstream for
//! scheduling, routing, and resource estimation.

use ff_core::{AgentTask, AgentTaskKind};
use serde::{Deserialize, Serialize};

/// Complexity classification for an [`AgentTask`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    /// Simple, predictable work: short commands, short prompts, bounded
    /// execution windows.
    Mechanical,
    /// Standard work that does not fit the extremes.
    Moderate,
    /// Work that likely needs extra time, resources, or oversight: long
    /// commands, multi-step shell pipelines, large inference prompts, or
    /// unbounded execution windows.
    Complex,
}

/// Processes agent tasks and assesses their complexity.
#[derive(Debug, Clone, Default)]
pub struct TaskProcessor;

impl TaskProcessor {
    pub fn new() -> Self {
        Self
    }

    /// Assess the complexity of a task based on its characteristics.
    ///
    /// Current heuristics:
    /// - [`AgentTaskKind::ShellCommand`]: considers command length, shell
    ///   operators, and timeout bounds.
    /// - [`AgentTaskKind::ModelInference`]: considers prompt length and token
    ///   budget.
    pub fn assess_complexity(task: &AgentTask) -> TaskComplexity {
        match &task.kind {
            AgentTaskKind::ShellCommand {
                command,
                timeout_secs,
            } => Self::assess_shell_complexity(command, *timeout_secs),
            AgentTaskKind::ModelInference {
                prompt, max_tokens, ..
            } => Self::assess_inference_complexity(prompt, *max_tokens),
        }
    }

    fn assess_shell_complexity(command: &str, timeout_secs: Option<u64>) -> TaskComplexity {
        let command = command.trim();
        let len = command.len();

        // Mechanical: short, single-step command with a tight timeout.
        if len <= 50
            && timeout_secs.map_or(false, |t| t <= 60)
            && !Self::has_shell_operators(command)
        {
            return TaskComplexity::Mechanical;
        }

        // Complex: long command, unbounded timeout, or multi-step shell logic.
        if len > 200
            || timeout_secs.is_none()
            || Self::has_shell_operators(command)
            || Self::looks_like_script(command)
        {
            return TaskComplexity::Complex;
        }

        TaskComplexity::Moderate
    }

    fn assess_inference_complexity(prompt: &str, max_tokens: Option<u32>) -> TaskComplexity {
        let len = prompt.len();

        if len <= 100 && max_tokens.map_or(false, |t| t <= 256) {
            return TaskComplexity::Mechanical;
        }

        if len > 1000 || max_tokens.map_or(true, |t| t > 2048) {
            return TaskComplexity::Complex;
        }

        TaskComplexity::Moderate
    }

    fn has_shell_operators(command: &str) -> bool {
        [" && ", " || ", " | ", ";", "`", "$()"]
            .iter()
            .any(|op| command.contains(op))
    }

    fn looks_like_script(command: &str) -> bool {
        command.contains('\n') || command.contains("for ") || command.contains("while ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn task(kind: AgentTaskKind) -> AgentTask {
        AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind,
        }
    }

    #[test]
    fn short_shell_command_with_timeout_is_mechanical() {
        let t = task(AgentTaskKind::ShellCommand {
            command: "echo hello".to_string(),
            timeout_secs: Some(30),
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Mechanical
        );
    }

    #[test]
    fn long_shell_command_is_complex() {
        let t = task(AgentTaskKind::ShellCommand {
            command: "a".repeat(201),
            timeout_secs: Some(30),
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Complex
        );
    }

    #[test]
    fn shell_command_without_timeout_is_complex() {
        let t = task(AgentTaskKind::ShellCommand {
            command: "sleep 5".to_string(),
            timeout_secs: None,
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Complex
        );
    }

    #[test]
    fn shell_command_with_operators_is_complex() {
        let t = task(AgentTaskKind::ShellCommand {
            command: "cargo build && cargo test".to_string(),
            timeout_secs: Some(120),
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Complex
        );
    }

    #[test]
    fn medium_shell_command_is_moderate() {
        let t = task(AgentTaskKind::ShellCommand {
            command: "cargo check --workspace".to_string(),
            timeout_secs: Some(120),
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Moderate
        );
    }

    #[test]
    fn short_prompt_is_mechanical() {
        let t = task(AgentTaskKind::ModelInference {
            model: None,
            prompt: "hi".to_string(),
            max_tokens: Some(100),
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Mechanical
        );
    }

    #[test]
    fn long_prompt_is_complex() {
        let t = task(AgentTaskKind::ModelInference {
            model: None,
            prompt: "x".repeat(1001),
            max_tokens: Some(500),
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Complex
        );
    }

    #[test]
    fn unbounded_max_tokens_is_complex() {
        let t = task(AgentTaskKind::ModelInference {
            model: None,
            prompt: "summarize".to_string(),
            max_tokens: None,
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Complex
        );
    }

    #[test]
    fn moderate_inference_is_moderate() {
        let t = task(AgentTaskKind::ModelInference {
            model: None,
            prompt: "Explain Rust ownership in one paragraph.".to_string(),
            max_tokens: Some(512),
        });
        assert_eq!(
            TaskProcessor::assess_complexity(&t),
            TaskComplexity::Moderate
        );
    }
}
