//! Agent team management — composable, tier-aware team configurations.
//!
//! Builds on top of [`crate::crew`] to provide:
//! - [`ModelPreference`] — express tier or specific-model preferences
//! - [`AgentAssignment`] — role + model preference + node preference
//! - [`TeamConfig`] — ordered list of assignments with metadata
//! - Pre-built team templates (`code_team`, `review_team`, `research_team`)
//!
//! Teams are designed to be converted into [`crate::planner::ExecutionPlan`]s
//! for actual execution.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ff_core::Tier;

use crate::crew::AgentRole;

// ─── Model Preference ────────────────────────────────────────────────────────

/// How an agent assignment specifies which model to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelPreference {
    /// Use any model in this tier.
    Tier { tier: Tier },
    /// Use a specific model by name (e.g. "qwen3-72b-q4").
    Specific { model_id: String },
    /// Let the router decide (default).
    Auto,
}

impl ModelPreference {
    /// Shorthand for a tier preference.
    pub fn tier(tier: Tier) -> Self {
        Self::Tier { tier }
    }

    /// Shorthand for a specific model preference.
    pub fn specific(model_id: impl Into<String>) -> Self {
        Self::Specific {
            model_id: model_id.into(),
        }
    }

    /// Shorthand for auto (router decides).
    pub fn auto() -> Self {
        Self::Auto
    }

    /// Extract the preferred tier, if any.
    pub fn preferred_tier(&self) -> Option<Tier> {
        match self {
            Self::Tier { tier } => Some(*tier),
            _ => None,
        }
    }
}

impl Default for ModelPreference {
    fn default() -> Self {
        Self::Auto
    }
}

impl std::fmt::Display for ModelPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tier { tier } => write!(f, "{tier}"),
            Self::Specific { model_id } => write!(f, "model:{model_id}"),
            Self::Auto => write!(f, "auto"),
        }
    }
}

// ─── Agent Assignment ────────────────────────────────────────────────────────

/// A single agent assignment within a team.
///
/// Ties together a role, model preference, optional node preference,
/// and custom instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignment {
    /// Unique ID for this assignment.
    pub id: Uuid,
    /// The role this agent plays.
    pub role: AgentRole,
    /// Model preference: tier, specific model, or auto.
    pub model_preference: ModelPreference,
    /// Optional: prefer a specific node (by name).
    pub node_preference: Option<String>,
    /// Optional custom instructions appended to the role's system prompt.
    pub instructions: Option<String>,
}

impl AgentAssignment {
    /// Create a new assignment with auto model selection.
    pub fn new(role: AgentRole) -> Self {
        Self {
            id: Uuid::new_v4(),
            role,
            model_preference: ModelPreference::Auto,
            node_preference: None,
            instructions: None,
        }
    }

    /// Create an assignment with a specific tier preference.
    pub fn with_tier(role: AgentRole, tier: Tier) -> Self {
        Self {
            id: Uuid::new_v4(),
            role,
            model_preference: ModelPreference::tier(tier),
            node_preference: None,
            instructions: None,
        }
    }

    /// Create an assignment with a specific model.
    pub fn with_model(role: AgentRole, model_id: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            role,
            model_preference: ModelPreference::specific(model_id),
            node_preference: None,
            instructions: None,
        }
    }

    /// Builder: set node preference.
    pub fn on_node(mut self, node_name: impl Into<String>) -> Self {
        self.node_preference = Some(node_name.into());
        self
    }

    /// Builder: add custom instructions.
    pub fn with_instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = Some(text.into());
        self
    }

    /// Compose the full system prompt (role default + custom instructions).
    pub fn full_system_prompt(&self) -> String {
        let base = self.role.system_prompt();
        match &self.instructions {
            Some(extra) => format!("{base}\n\nAdditional instructions: {extra}"),
            None => base.to_string(),
        }
    }
}

// ─── Team Config ─────────────────────────────────────────────────────────────

/// A team configuration: an ordered list of agent assignments for a task.
///
/// Teams define *who* does what (roles + model preferences). The planner
/// converts a team config + decomposed subtasks into an execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    /// Unique team ID.
    pub id: Uuid,
    /// Human-readable team name.
    pub name: String,
    /// Description of the team's purpose.
    pub description: String,
    /// Ordered agent assignments (execution order for sequential teams).
    pub assignments: Vec<AgentAssignment>,
}

impl TeamConfig {
    /// Create a new empty team.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            description: description.into(),
            assignments: Vec::new(),
        }
    }

    /// Add an agent assignment to the team.
    pub fn add(&mut self, assignment: AgentAssignment) {
        self.assignments.push(assignment);
    }

    /// Builder: add an assignment and return self.
    pub fn with(mut self, assignment: AgentAssignment) -> Self {
        self.assignments.push(assignment);
        self
    }

    /// Number of agents in the team.
    pub fn len(&self) -> usize {
        self.assignments.len()
    }

    /// Whether the team has no agents.
    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }

    /// Get all unique roles in the team.
    pub fn roles(&self) -> Vec<&AgentRole> {
        let mut seen = std::collections::HashSet::new();
        self.assignments
            .iter()
            .filter(|a| seen.insert(&a.role))
            .map(|a| &a.role)
            .collect()
    }

    /// Get assignments for a specific role.
    pub fn agents_with_role(&self, role: &AgentRole) -> Vec<&AgentAssignment> {
        self.assignments
            .iter()
            .filter(|a| &a.role == role)
            .collect()
    }
}

// ─── Pre-built Team Templates ────────────────────────────────────────────────

/// Factory for common team configurations.
pub struct TeamTemplates;

impl TeamTemplates {
    /// Code team: Planner(tier1) → Coder(tier2) → Reviewer(tier3).
    ///
    /// The planner researches context quickly at tier1, the coder implements
    /// at tier2, and a higher-tier reviewer catches issues at tier3.
    pub fn code_team() -> TeamConfig {
        TeamConfig::new(
            "Code Team",
            "Planner(T1) → Coder(T2) → Reviewer(T3) for coding tasks",
        )
        .with(AgentAssignment::with_tier(AgentRole::Planner, Tier::Tier1))
        .with(AgentAssignment::with_tier(AgentRole::Coder, Tier::Tier2))
        .with(AgentAssignment::with_tier(AgentRole::Reviewer, Tier::Tier3))
    }

    /// Review team: Coder(tier2) → Reviewer(tier3) → Reviewer(tier4).
    ///
    /// The coder analyses the code at tier2, a first reviewer at tier3,
    /// then a second expert-level reviewer at tier4 for maximum scrutiny.
    pub fn review_team() -> TeamConfig {
        TeamConfig::new(
            "Review Team",
            "Coder(T2) → Reviewer(T3) → Reviewer(T4) for thorough reviews",
        )
        .with(AgentAssignment::with_tier(AgentRole::Coder, Tier::Tier2))
        .with(AgentAssignment::with_tier(AgentRole::Reviewer, Tier::Tier3))
        .with(
            AgentAssignment::with_tier(AgentRole::Reviewer, Tier::Tier4)
                .with_instructions("You are the final reviewer. Be extra thorough — check for security, performance, and architectural issues."),
        )
    }

    /// Research team: Researcher(tier1) → Researcher(tier2) → Summarizer(tier3).
    ///
    /// A fast researcher gathers breadth at tier1, a deeper researcher
    /// at tier2 adds depth, and a tier3 writer summarizes/synthesizes.
    pub fn research_team() -> TeamConfig {
        TeamConfig::new(
            "Research Team",
            "Researcher(T1) → Researcher(T2) → Summarizer(T3) for knowledge tasks",
        )
        .with(
            AgentAssignment::with_tier(AgentRole::Researcher, Tier::Tier1).with_instructions(
                "Gather broad information quickly. Focus on breadth over depth.",
            ),
        )
        .with(
            AgentAssignment::with_tier(AgentRole::Researcher, Tier::Tier2).with_instructions(
                "Deep-dive into the most relevant findings. Verify facts and add detail.",
            ),
        )
        .with(
            AgentAssignment::with_tier(AgentRole::Writer, Tier::Tier3).with_instructions(
                "Synthesize all research into a clear, well-structured summary.",
            ),
        )
    }

    /// Full development team: Planner → Researcher → Coder → Tester → Reviewer.
    pub fn full_dev_team() -> TeamConfig {
        TeamConfig::new(
            "Full Dev Team",
            "Planner(T1) → Researcher(T2) → Coder(T2) → Tester(T2) → Reviewer(T3)",
        )
        .with(AgentAssignment::with_tier(AgentRole::Planner, Tier::Tier1))
        .with(AgentAssignment::with_tier(
            AgentRole::Researcher,
            Tier::Tier2,
        ))
        .with(AgentAssignment::with_tier(AgentRole::Coder, Tier::Tier2))
        .with(AgentAssignment::with_tier(AgentRole::Tester, Tier::Tier2))
        .with(AgentAssignment::with_tier(AgentRole::Reviewer, Tier::Tier3))
    }

    /// Quick single-agent team for simple tasks.
    pub fn quick_team() -> TeamConfig {
        TeamConfig::new("Quick Team", "Single assistant for fast tasks").with(
            AgentAssignment::with_tier(AgentRole::Assistant, Tier::Tier1),
        )
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_preference_tier() {
        let pref = ModelPreference::tier(Tier::Tier2);
        assert_eq!(pref.preferred_tier(), Some(Tier::Tier2));
        assert_eq!(pref.to_string(), "Tier 2 (32B code)");
    }

    #[test]
    fn test_model_preference_specific() {
        let pref = ModelPreference::specific("qwen3-72b");
        assert_eq!(pref.preferred_tier(), None);
        assert!(pref.to_string().contains("qwen3-72b"));
    }

    #[test]
    fn test_model_preference_auto() {
        let pref = ModelPreference::auto();
        assert_eq!(pref, ModelPreference::Auto);
        assert_eq!(pref.to_string(), "auto");
    }

    #[test]
    fn test_model_preference_default() {
        let pref = ModelPreference::default();
        assert_eq!(pref, ModelPreference::Auto);
    }

    #[test]
    fn test_agent_assignment_basic() {
        let a = AgentAssignment::new(AgentRole::Coder);
        assert_eq!(a.role, AgentRole::Coder);
        assert_eq!(a.model_preference, ModelPreference::Auto);
        assert!(a.node_preference.is_none());
    }

    #[test]
    fn test_agent_assignment_with_tier() {
        let a = AgentAssignment::with_tier(AgentRole::Reviewer, Tier::Tier3);
        assert_eq!(a.model_preference, ModelPreference::tier(Tier::Tier3));
    }

    #[test]
    fn test_agent_assignment_with_model() {
        let a = AgentAssignment::with_model(AgentRole::Coder, "qwen3-32b");
        assert_eq!(a.model_preference, ModelPreference::specific("qwen3-32b"));
    }

    #[test]
    fn test_agent_assignment_builder_chain() {
        let a = AgentAssignment::with_tier(AgentRole::Reviewer, Tier::Tier4)
            .on_node("taylor")
            .with_instructions("Check for security issues");
        assert_eq!(a.node_preference, Some("taylor".into()));
        assert!(a.full_system_prompt().contains("security"));
    }

    #[test]
    fn test_team_config_basic() {
        let team = TeamConfig::new("Test Team", "A test team")
            .with(AgentAssignment::new(AgentRole::Coder))
            .with(AgentAssignment::new(AgentRole::Reviewer));
        assert_eq!(team.len(), 2);
        assert!(!team.is_empty());
        assert_eq!(team.roles().len(), 2);
    }

    #[test]
    fn test_team_config_agents_with_role() {
        let team = TeamConfig::new("Multi-Reviewer", "Test")
            .with(AgentAssignment::new(AgentRole::Reviewer))
            .with(AgentAssignment::new(AgentRole::Reviewer))
            .with(AgentAssignment::new(AgentRole::Coder));
        assert_eq!(team.agents_with_role(&AgentRole::Reviewer).len(), 2);
        assert_eq!(team.agents_with_role(&AgentRole::Coder).len(), 1);
        assert_eq!(team.agents_with_role(&AgentRole::Planner).len(), 0);
    }

    #[test]
    fn test_code_team_template() {
        let team = TeamTemplates::code_team();
        assert_eq!(team.len(), 3);
        assert_eq!(team.assignments[0].role, AgentRole::Planner);
        assert_eq!(team.assignments[1].role, AgentRole::Coder);
        assert_eq!(team.assignments[2].role, AgentRole::Reviewer);
        assert_eq!(
            team.assignments[0].model_preference,
            ModelPreference::tier(Tier::Tier1)
        );
        assert_eq!(
            team.assignments[1].model_preference,
            ModelPreference::tier(Tier::Tier2)
        );
        assert_eq!(
            team.assignments[2].model_preference,
            ModelPreference::tier(Tier::Tier3)
        );
    }

    #[test]
    fn test_review_team_template() {
        let team = TeamTemplates::review_team();
        assert_eq!(team.len(), 3);
        assert_eq!(team.assignments[0].role, AgentRole::Coder);
        assert_eq!(team.assignments[1].role, AgentRole::Reviewer);
        assert_eq!(team.assignments[2].role, AgentRole::Reviewer);
        // Final reviewer has extra instructions
        assert!(team.assignments[2].instructions.is_some());
    }

    #[test]
    fn test_research_team_template() {
        let team = TeamTemplates::research_team();
        assert_eq!(team.len(), 3);
        assert_eq!(team.assignments[0].role, AgentRole::Researcher);
        assert_eq!(team.assignments[1].role, AgentRole::Researcher);
        assert_eq!(team.assignments[2].role, AgentRole::Writer);
        // Both researchers have custom instructions
        assert!(team.assignments[0].instructions.is_some());
        assert!(team.assignments[1].instructions.is_some());
    }

    #[test]
    fn test_full_dev_team_template() {
        let team = TeamTemplates::full_dev_team();
        assert_eq!(team.len(), 5);
        assert_eq!(team.assignments[3].role, AgentRole::Tester);
    }

    #[test]
    fn test_quick_team_template() {
        let team = TeamTemplates::quick_team();
        assert_eq!(team.len(), 1);
        assert_eq!(team.assignments[0].role, AgentRole::Assistant);
    }

    #[test]
    fn test_team_serialization_roundtrip() {
        let team = TeamTemplates::code_team();
        let json = serde_json::to_string(&team).unwrap();
        let back: TeamConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, team.name);
        assert_eq!(back.len(), team.len());
    }
}
