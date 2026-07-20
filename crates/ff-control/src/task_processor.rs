//! Task classification performed before work is dispatched.

use ff_core::AgentTaskKind;
use serde::{Deserialize, Serialize};

/// Coarse task complexity used to select an appropriate execution lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplexityFlag {
    Mechanical,
    Moderate,
    Complex,
}

/// Applies control-plane policy to tasks before dispatch.
#[derive(Debug, Default)]
pub struct TaskProcessor;

impl TaskProcessor {
    /// Assess complexity from the task's size, structure, and risk signals.
    pub fn assess_complexity(&self, task: &AgentTaskKind) -> ComplexityFlag {
        match task {
            AgentTaskKind::ShellCommand { command, .. } => assess_text(command, true),
            AgentTaskKind::ModelInference { prompt, .. } => assess_text(prompt, false),
        }
    }
}

fn assess_text(text: &str, shell: bool) -> ComplexityFlag {
    let lower = text.to_ascii_lowercase();
    let word_count = text.split_whitespace().count();
    let complex_signals = [
        "architect",
        "redesign",
        "migration",
        "migrate",
        "security",
        "distributed",
        "multiple files",
        "cross-crate",
    ];
    let moderate_signals = [
        "debug",
        "implement",
        "refactor",
        "optimize",
        "investigate",
        "test",
    ];

    if (shell && ["&&", "||", ";", "\n"].iter().any(|s| text.contains(s)))
        || word_count > 80
        || complex_signals.iter().any(|signal| lower.contains(signal))
    {
        ComplexityFlag::Complex
    } else if word_count > 20 || moderate_signals.iter().any(|signal| lower.contains(signal)) {
        ComplexityFlag::Moderate
    } else {
        ComplexityFlag::Mechanical
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_short_single_step_tasks_as_mechanical() {
        let task = AgentTaskKind::ShellCommand {
            command: "cargo fmt".into(),
            timeout_secs: None,
        };
        assert_eq!(
            TaskProcessor.assess_complexity(&task),
            ComplexityFlag::Mechanical
        );
    }

    #[test]
    fn flags_implementation_tasks_as_moderate() {
        let task = AgentTaskKind::ModelInference {
            model: None,
            prompt: "Implement input validation for the existing endpoint".into(),
            max_tokens: None,
        };
        assert_eq!(
            TaskProcessor.assess_complexity(&task),
            ComplexityFlag::Moderate
        );
    }

    #[test]
    fn flags_cross_cutting_and_chained_tasks_as_complex() {
        let processor = TaskProcessor;
        let prompt = AgentTaskKind::ModelInference {
            model: None,
            prompt: "Redesign the distributed scheduler across multiple files".into(),
            max_tokens: None,
        };
        let command = AgentTaskKind::ShellCommand {
            command: "cargo fmt && cargo test".into(),
            timeout_secs: None,
        };

        assert_eq!(
            processor.assess_complexity(&prompt),
            ComplexityFlag::Complex
        );
        assert_eq!(
            processor.assess_complexity(&command),
            ComplexityFlag::Complex
        );
    }
}
