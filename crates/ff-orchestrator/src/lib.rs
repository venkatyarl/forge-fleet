//! `ff-orchestrator` — ForgeFleet task decomposition and multi-model orchestration.
//!
//! Inspired by Perplexity Computer's 19-model orchestration pattern, this crate
//! decomposes complex tasks into subtasks, routes each to the optimal model/node,
//! and executes them in parallel where the dependency graph allows.
//!
//! # Modules
//!
//! - [`decomposer`] — Break complex tasks into typed subtasks
//! - [`router`] — Select the best model/node for each subtask (Perplexity pattern)
//! - [`parallel`] — Fire subtasks across nodes, track progress, aggregate results
//! - [`crew`] — CrewAI-inspired role definitions (researcher, coder, reviewer, writer)
//! - [`planner`] — DAG-based execution planning with dependency resolution
//! - [`agent_team`] — Composable agent team management with tier-aware templates
//! - [`task_decomposer`] — Template-based task decomposition (build/fix/review patterns)
//! - [`confidence`] — Confidence-based escalation and trend tracking

pub mod agent_team;
pub mod confidence;
pub mod crew;
pub mod decomposer;
pub mod parallel;
pub mod planner;
pub mod project_policy;
pub mod router;
pub mod task_decomposer;

// Re-export primary types at crate root for ergonomic use.
pub use agent_team::{AgentAssignment, ModelPreference, TeamConfig, TeamTemplates};
pub use confidence::{
    ConfidenceAssessment, ConfidenceExtractor, ConfidenceScore, ConfidenceTracker,
    EscalationConfig, EscalationDecision,
};
pub use crew::{AgentRole, CrewAssignment, CrewDefinition};
pub use decomposer::{SubTask, SubTaskType, TaskDecomposition};
pub use parallel::{ExecutionResult, ParallelExecutor, SubTaskResult};
pub use planner::{ExecutionPlan, PlanNode, PlanStage};
pub use project_policy::{
    ApprovalTrigger, ComplianceFlag, DataSensitivity, DeploymentTarget, ExecutionPolicy,
    HumanApprovalLevel, HumanApprovalPolicy, ProjectExecutionProfile, ProjectPolicyEngine,
    ReviewRequirements, ReviewStrictness, RolloutPolicy, RolloutStrategy, RoutingPolicy,
    TestRequirements, TierAccessPolicy,
};
pub use router::{ModelScore, RouteDecision, TaskRouter};
pub use task_decomposer::{
    DecomposedSubTask, DecompositionStrategy, TaskPattern, TemplateDecomposer,
};
