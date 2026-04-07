//! Crew/team definitions — CrewAI-inspired role-based agent composition.
//!
//! Define agent roles (researcher, coder, reviewer, writer), assign them to
//! subtasks, and compose crews for multi-step workflows.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ff_core::Tier;

use crate::decomposer::SubTaskType;

// ─── Agent Role ──────────────────────────────────────────────────────────────

/// A role an agent can play in a crew.
///
/// Inspired by CrewAI — each role has a specialty, preferred model tier,
/// and description of what it does well.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Gathers information: web search, document retrieval, fact-checking.
    Researcher,
    /// Writes code: implementation, refactoring, debugging.
    Coder,
    /// Reviews output: code review, quality checks, verification.
    Reviewer,
    /// Writes prose: documentation, reports, creative content.
    Writer,
    /// Plans and orchestrates: architecture, strategy, decomposition.
    Planner,
    /// Quick tasks: translations, lookups, formatting.
    Assistant,
    /// Executes tools: shell commands, API calls, deployments.
    Executor,
    /// Writes and runs tests: unit tests, integration tests, test plans.
    Tester,
}

impl AgentRole {
    /// Minimum tier this role should use.
    pub fn min_tier(&self) -> Tier {
        match self {
            Self::Assistant | Self::Executor => Tier::Tier1,
            Self::Coder | Self::Writer | Self::Tester => Tier::Tier2,
            Self::Researcher | Self::Reviewer | Self::Planner => Tier::Tier2,
        }
    }

    /// Ideal tier for this role to produce best results.
    pub fn ideal_tier(&self) -> Tier {
        match self {
            Self::Assistant => Tier::Tier1,
            Self::Executor => Tier::Tier1,
            Self::Coder => Tier::Tier2,
            Self::Writer => Tier::Tier2,
            Self::Researcher => Tier::Tier3,
            Self::Reviewer => Tier::Tier3,
            Self::Planner => Tier::Tier3,
            Self::Tester => Tier::Tier2,
        }
    }

    /// System prompt preamble for this role.
    pub fn system_prompt(&self) -> &'static str {
        match self {
            Self::Researcher => {
                "You are a research specialist. Gather comprehensive, accurate information. \
                 Cite sources when possible. Focus on relevance and completeness."
            }
            Self::Coder => {
                "You are an expert software engineer. Write clean, well-tested, production-quality \
                 code. Follow best practices. Include error handling and documentation."
            }
            Self::Reviewer => {
                "You are a senior code reviewer. Identify bugs, security issues, performance \
                 problems, and style violations. Be thorough but constructive."
            }
            Self::Writer => {
                "You are a technical writer. Produce clear, well-structured documentation and \
                 prose. Adapt your tone to the audience."
            }
            Self::Planner => {
                "You are a system architect and planner. Break complex problems into manageable \
                 pieces. Consider trade-offs, dependencies, and risks."
            }
            Self::Assistant => {
                "You are a helpful assistant. Answer quickly and accurately. Be concise."
            }
            Self::Executor => {
                "You are a task executor. Run commands and tools precisely as instructed. \
                 Report results clearly, including any errors."
            }
            Self::Tester => {
                "You are a testing specialist. Write comprehensive tests covering edge cases, \
                 error conditions, and happy paths. Identify untested code paths and verify \
                 correctness through systematic testing."
            }
        }
    }

    /// Which subtask types this role handles best.
    pub fn handles(&self) -> Vec<SubTaskType> {
        match self {
            Self::Researcher => vec![SubTaskType::Research],
            Self::Coder => vec![SubTaskType::Code, SubTaskType::ToolUse],
            Self::Reviewer => vec![SubTaskType::Review],
            Self::Writer => vec![SubTaskType::Creative, SubTaskType::Summarize],
            Self::Planner => vec![SubTaskType::Planning, SubTaskType::Analysis],
            Self::Assistant => vec![SubTaskType::FastLookup],
            Self::Executor => vec![SubTaskType::ToolUse],
            Self::Tester => vec![SubTaskType::Code, SubTaskType::Review],
        }
    }

    /// Map a subtask type to the best role for it.
    pub fn best_for(task_type: SubTaskType) -> Self {
        match task_type {
            SubTaskType::Code => Self::Coder,
            SubTaskType::Research => Self::Researcher,
            SubTaskType::Analysis => Self::Planner,
            SubTaskType::Creative => Self::Writer,
            SubTaskType::FastLookup => Self::Assistant,
            SubTaskType::Review => Self::Reviewer,
            SubTaskType::Summarize => Self::Writer,
            SubTaskType::Planning => Self::Planner,
            SubTaskType::ToolUse => Self::Executor,
        }
    }
}

impl std::fmt::Display for AgentRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Researcher => write!(f, "Researcher"),
            Self::Coder => write!(f, "Coder"),
            Self::Reviewer => write!(f, "Reviewer"),
            Self::Writer => write!(f, "Writer"),
            Self::Planner => write!(f, "Planner"),
            Self::Assistant => write!(f, "Assistant"),
            Self::Executor => write!(f, "Executor"),
            Self::Tester => write!(f, "Tester"),
        }
    }
}

// ─── Crew Assignment ─────────────────────────────────────────────────────────

/// Assignment of a role to a specific subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewAssignment {
    /// The subtask being assigned.
    pub subtask_id: Uuid,
    /// The role handling this subtask.
    pub role: AgentRole,
    /// Optional: specific model to use (overrides router selection).
    pub model_override: Option<String>,
    /// Optional: specific node to use.
    pub node_override: Option<String>,
    /// Custom system prompt (appended to role's default).
    pub extra_instructions: Option<String>,
}

impl CrewAssignment {
    /// Create a new assignment with automatic role selection.
    pub fn auto(subtask_id: Uuid, task_type: SubTaskType) -> Self {
        Self {
            subtask_id,
            role: AgentRole::best_for(task_type),
            model_override: None,
            node_override: None,
            extra_instructions: None,
        }
    }

    /// Create a new assignment with a specific role.
    pub fn with_role(subtask_id: Uuid, role: AgentRole) -> Self {
        Self {
            subtask_id,
            role,
            model_override: None,
            node_override: None,
            extra_instructions: None,
        }
    }

    /// Builder: override the model.
    pub fn model(mut self, model_id: impl Into<String>) -> Self {
        self.model_override = Some(model_id.into());
        self
    }

    /// Builder: override the node.
    pub fn node(mut self, node_name: impl Into<String>) -> Self {
        self.node_override = Some(node_name.into());
        self
    }

    /// Builder: add extra instructions.
    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.extra_instructions = Some(text.into());
        self
    }

    /// Compose the full system prompt for this assignment.
    pub fn full_system_prompt(&self) -> String {
        let base = self.role.system_prompt();
        match &self.extra_instructions {
            Some(extra) => format!("{base}\n\nAdditional instructions: {extra}"),
            None => base.to_string(),
        }
    }
}

// ─── Crew Definition ─────────────────────────────────────────────────────────

/// A crew definition — a named collection of role assignments for a workflow.
///
/// Example crews:
/// - **Code Crew**: Planner → Coder → Reviewer
/// - **Research Crew**: Researcher → Writer → Reviewer
/// - **Full Stack**: Planner → Researcher → Coder → Reviewer → Writer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewDefinition {
    /// Unique crew ID.
    pub id: Uuid,
    /// Human-readable crew name.
    pub name: String,
    /// Description of what this crew does.
    pub description: String,
    /// Role assignments (in execution order for sequential crews).
    pub assignments: Vec<CrewAssignment>,
}

impl CrewDefinition {
    /// Create a new empty crew.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            description: description.into(),
            assignments: Vec::new(),
        }
    }

    /// Add an assignment to the crew.
    pub fn assign(&mut self, assignment: CrewAssignment) {
        self.assignments.push(assignment);
    }

    /// Roles used in this crew (deduplicated).
    pub fn roles(&self) -> Vec<&AgentRole> {
        let mut roles: Vec<&AgentRole> = self.assignments.iter().map(|a| &a.role).collect();
        roles.dedup();
        roles
    }

    /// Number of assignments.
    pub fn len(&self) -> usize {
        self.assignments.len()
    }

    /// Whether the crew has no assignments.
    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }
}

// ─── Preset Crews ────────────────────────────────────────────────────────────

/// Common crew presets.
pub struct CrewPresets;

impl CrewPresets {
    /// Code crew: Context Engineer (research) → Coder → Reviewer.
    ///
    /// Matches ForgeFleet's existing `fleet_crew` pattern.
    pub fn code_crew(subtask_ids: &[Uuid; 3]) -> CrewDefinition {
        let mut crew = CrewDefinition::new(
            "Code Crew",
            "Research → Implement → Review pipeline for coding tasks",
        );
        crew.assign(CrewAssignment::with_role(
            subtask_ids[0],
            AgentRole::Researcher,
        ));
        crew.assign(CrewAssignment::with_role(subtask_ids[1], AgentRole::Coder));
        crew.assign(CrewAssignment::with_role(
            subtask_ids[2],
            AgentRole::Reviewer,
        ));
        crew
    }

    /// Research crew: Researcher → Writer → Reviewer.
    pub fn research_crew(subtask_ids: &[Uuid; 3]) -> CrewDefinition {
        let mut crew = CrewDefinition::new(
            "Research Crew",
            "Research → Write → Review pipeline for knowledge tasks",
        );
        crew.assign(CrewAssignment::with_role(
            subtask_ids[0],
            AgentRole::Researcher,
        ));
        crew.assign(CrewAssignment::with_role(subtask_ids[1], AgentRole::Writer));
        crew.assign(CrewAssignment::with_role(
            subtask_ids[2],
            AgentRole::Reviewer,
        ));
        crew
    }

    /// Full stack crew: Planner → Researcher → Coder → Reviewer → Writer.
    pub fn full_stack_crew(subtask_ids: &[Uuid; 5]) -> CrewDefinition {
        let mut crew = CrewDefinition::new(
            "Full Stack Crew",
            "Complete pipeline: Plan → Research → Code → Review → Document",
        );
        crew.assign(CrewAssignment::with_role(
            subtask_ids[0],
            AgentRole::Planner,
        ));
        crew.assign(CrewAssignment::with_role(
            subtask_ids[1],
            AgentRole::Researcher,
        ));
        crew.assign(CrewAssignment::with_role(subtask_ids[2], AgentRole::Coder));
        crew.assign(CrewAssignment::with_role(
            subtask_ids[3],
            AgentRole::Reviewer,
        ));
        crew.assign(CrewAssignment::with_role(subtask_ids[4], AgentRole::Writer));
        crew
    }

    /// Quick crew: single Assistant for simple tasks.
    pub fn quick_crew(subtask_id: Uuid) -> CrewDefinition {
        let mut crew = CrewDefinition::new("Quick Crew", "Single assistant for fast, simple tasks");
        crew.assign(CrewAssignment::with_role(subtask_id, AgentRole::Assistant));
        crew
    }
}

/// Auto-assign roles to a set of subtasks based on their types.
pub fn auto_assign_crew(subtasks: &[crate::decomposer::SubTask]) -> Vec<CrewAssignment> {
    subtasks
        .iter()
        .map(|st| CrewAssignment::auto(st.id, st.task_type))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_for_task_type() {
        assert_eq!(AgentRole::best_for(SubTaskType::Code), AgentRole::Coder);
        assert_eq!(
            AgentRole::best_for(SubTaskType::Research),
            AgentRole::Researcher
        );
        assert_eq!(
            AgentRole::best_for(SubTaskType::FastLookup),
            AgentRole::Assistant
        );
        assert_eq!(
            AgentRole::best_for(SubTaskType::Review),
            AgentRole::Reviewer
        );
        assert_eq!(
            AgentRole::best_for(SubTaskType::Planning),
            AgentRole::Planner
        );
    }

    #[test]
    fn test_role_tiers() {
        assert!(AgentRole::Coder.ideal_tier() <= AgentRole::Reviewer.ideal_tier());
        assert_eq!(AgentRole::Assistant.ideal_tier(), Tier::Tier1);
    }

    #[test]
    fn test_crew_assignment_auto() {
        let id = Uuid::new_v4();
        let assignment = CrewAssignment::auto(id, SubTaskType::Code);
        assert_eq!(assignment.role, AgentRole::Coder);
        assert!(assignment.model_override.is_none());
    }

    #[test]
    fn test_crew_assignment_builder() {
        let id = Uuid::new_v4();
        let assignment = CrewAssignment::with_role(id, AgentRole::Reviewer)
            .model("qwen3-72b")
            .node("taylor")
            .instructions("Focus on security");
        assert_eq!(assignment.model_override, Some("qwen3-72b".into()));
        assert_eq!(assignment.node_override, Some("taylor".into()));
        assert!(assignment.full_system_prompt().contains("security"));
    }

    #[test]
    fn test_code_crew_preset() {
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let crew = CrewPresets::code_crew(&ids);
        assert_eq!(crew.len(), 3);
        assert_eq!(crew.assignments[0].role, AgentRole::Researcher);
        assert_eq!(crew.assignments[1].role, AgentRole::Coder);
        assert_eq!(crew.assignments[2].role, AgentRole::Reviewer);
    }

    #[test]
    fn test_full_stack_crew_preset() {
        let ids = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        let crew = CrewPresets::full_stack_crew(&ids);
        assert_eq!(crew.len(), 5);
        assert_eq!(crew.name, "Full Stack Crew");
    }

    #[test]
    fn test_auto_assign_crew() {
        use crate::decomposer::SubTask;
        let subtasks = vec![
            SubTask::new(0, "research", "find info", SubTaskType::Research),
            SubTask::new(1, "code", "implement it", SubTaskType::Code),
            SubTask::new(2, "review", "check it", SubTaskType::Review),
        ];
        let assignments = auto_assign_crew(&subtasks);
        assert_eq!(assignments.len(), 3);
        assert_eq!(assignments[0].role, AgentRole::Researcher);
        assert_eq!(assignments[1].role, AgentRole::Coder);
        assert_eq!(assignments[2].role, AgentRole::Reviewer);
    }

    #[test]
    fn test_system_prompts_non_empty() {
        let roles = [
            AgentRole::Researcher,
            AgentRole::Coder,
            AgentRole::Reviewer,
            AgentRole::Writer,
            AgentRole::Planner,
            AgentRole::Assistant,
            AgentRole::Executor,
            AgentRole::Tester,
        ];
        for role in &roles {
            assert!(!role.system_prompt().is_empty(), "empty prompt for {role}");
        }
    }
}
