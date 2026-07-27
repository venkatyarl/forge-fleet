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
pub mod alerts;
pub mod cascade_strategy;
pub mod confidence;
pub mod crew;
pub mod decomposer;
pub mod leader;
pub mod llm_router;
pub mod merge_train;
pub mod node_manager;
pub mod parallel;
pub mod placement;
pub mod planner;
pub mod project_handler;
pub mod project_policy;
pub mod queue;
pub mod router;
#[path = "ff-orchestrator.rs"]
pub mod rpc_480b;
pub mod scheduler;
pub mod task_decomposer;
pub mod train_branch;

#[cfg(test)]
mod tests;

// Re-export primary types at crate root for ergonomic use.
pub use agent_team::{AgentAssignment, ModelPreference, TeamConfig, TeamTemplates};
pub use alerts::{AlertForwarder, AlertSink};
pub use confidence::{
    ConfidenceAssessment, ConfidenceExtractor, ConfidenceScore, ConfidenceTracker,
    EscalationConfig, EscalationDecision,
};
pub use crew::{AgentRole, CrewAssignment, CrewDefinition};
pub use decomposer::{SubTask, SubTaskType, TaskDecomposition};
pub use leader::{
    AgentHeartbeatResult, AgentTask, LeaderCoordinator, Preemption, SubmissionAction,
    SubmissionResult, TickResult,
};
pub use llm_router::{LlmCandidate, LlmFailureKind, LlmRouter};
pub use merge_train::MergeTrainConfig;
pub use node_manager::NodeManager;
pub use parallel::{ExecutionResult, ParallelExecutor, SubTaskResult};
pub use placement::{AntiAffinityRule, NodeWorkloadPreference, PlacementEngine, PlacementPolicy};
pub use planner::{ExecutionPlan, PlanNode, PlanStage};
pub use project_handler::ProjectHandler;
pub use project_policy::{
    ApprovalTrigger, ComplianceFlag, DataSensitivity, DeploymentTarget, ExecutionPolicy,
    HumanApprovalLevel, HumanApprovalPolicy, ProjectExecutionProfile, ProjectPolicyEngine,
    ReviewRequirements, ReviewStrictness, RolloutPolicy, RolloutStrategy, RoutingPolicy,
    TestRequirements, TierAccessPolicy,
};
pub use queue::{PriorityQueue, QueuedTask};
pub use router::{ModelScore, RouteDecision, TaskRouter};
pub use rpc_480b::{
    DEFAULT_480B_CTX_SIZE, DEFAULT_480B_ENDPOINT_URL, DEFAULT_480B_MODEL, DEFAULT_480B_PARALLEL,
    DEFAULT_480B_PORT, DEFAULT_480B_RPC_RING_TOPOLOGY, DEFAULT_480B_RPC_SHARD_COUNT,
    Rpc480bRecipeError, Rpc480bRingConfig, Rpc480bRingRecipe, Rpc480bShardRecipe,
    orchestrator_480b_ring_rpc,
};
pub use scheduler::{
    NodeCapacity, ResourceRequirements, RunningTask, ScheduleDecision, ScheduledTask, Scheduler,
    TaskPriority,
};
pub use task_decomposer::{
    DecomposedSubTask, DecompositionStrategy, TaskPattern, TemplateDecomposer,
};
pub use train_branch::{QueuedPr, TrainBranch, TrainBranchError, create_train_branch};

/// Run the native deployment convergence step for one orchestrator tick.
///
/// `ff-deploy` groups targets by their architecture string. Deriving that
/// value from the selected deployment profile here keeps the orchestrator's
/// profile selection authoritative and avoids the retired shell reconciler.
pub async fn run_native_deployment_tick<O>(
    profile: ff_core::DeploymentProfile,
    target_names: impl IntoIterator<Item = String>,
    operations: &O,
) -> ff_deploy::NativeDeployReport
where
    O: ff_deploy::NativeDeployOperations + Sync,
{
    let architecture = match profile {
        ff_core::DeploymentProfile::LinuxX86 => "linux-x86",
        ff_core::DeploymentProfile::LinuxAarch64Dgx => "linux-aarch64-dgx",
        ff_core::DeploymentProfile::MacosAarch64 => "macos-aarch64",
        ff_core::DeploymentProfile::Windows => "windows",
    };
    let targets = target_names
        .into_iter()
        .map(|name| ff_deploy::NativeDeployTarget {
            name,
            architecture: architecture.to_owned(),
        });

    ff_deploy::run_native_deploy_tick(targets, operations).await
}
