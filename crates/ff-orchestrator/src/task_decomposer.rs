//! Template-based task decomposition — high-level tasks to subtask DAGs.
//!
//! While [`crate::decomposer`] does keyword-based classification of free-text,
//! this module provides *template-based* decomposition for common task patterns:
//!
//! - "build feature X" → research → code → test → review
//! - "fix bug X" → reproduce → research → fix → test → review
//! - "review PR X" → read diff → check tests → check style → write review
//!
//! It also provides [`DecompositionStrategy`] to control how subtasks relate
//! (sequential, parallel, or full DAG) and a bridge to convert decomposed
//! tasks into [`crate::planner::ExecutionPlan`]s.

use serde::{Deserialize, Serialize};

use crate::crew::AgentRole;
use crate::decomposer::{SubTask, SubTaskType, TaskDecomposition};
use crate::planner::{ExecutionPlan, Planner};

// ─── Decomposition Strategy ──────────────────────────────────────────────────

/// How subtasks should relate to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecompositionStrategy {
    /// Each subtask depends on the previous one (A → B → C → D).
    Sequential,
    /// All subtasks are independent and can run in parallel.
    Parallel,
    /// Custom dependency graph — subtasks declare their own dependencies.
    Dag,
}

impl std::fmt::Display for DecompositionStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sequential => write!(f, "Sequential"),
            Self::Parallel => write!(f, "Parallel"),
            Self::Dag => write!(f, "DAG"),
        }
    }
}

// ─── Task Pattern ────────────────────────────────────────────────────────────

/// A recognized high-level task pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPattern {
    /// "build feature X", "implement X", "add X"
    BuildFeature,
    /// "fix bug X", "fix issue X", "debug X"
    FixBug,
    /// "review PR X", "review code X", "code review X"
    ReviewPr,
    /// Unknown pattern — will use generic decomposition.
    Generic,
}

impl TaskPattern {
    /// Try to detect the pattern from a task description.
    pub fn detect(task: &str) -> Self {
        let lower = task.to_lowercase();

        // Check for review patterns first (most specific)
        if lower.contains("review pr")
            || lower.contains("review pull request")
            || lower.contains("code review")
            || lower.contains("review code")
        {
            return Self::ReviewPr;
        }

        // Bug fix patterns
        if lower.contains("fix bug")
            || lower.contains("fix issue")
            || lower.contains("debug")
            || lower.contains("fix error")
            || lower.contains("fix crash")
            || lower.contains("hotfix")
        {
            return Self::FixBug;
        }

        // Build/implement patterns
        if lower.contains("build")
            || lower.contains("implement")
            || lower.contains("create")
            || lower.contains("add feature")
            || lower.contains("develop")
            || lower.contains("write a")
        {
            return Self::BuildFeature;
        }

        Self::Generic
    }

    /// Default strategy for this pattern.
    pub fn default_strategy(&self) -> DecompositionStrategy {
        match self {
            // Build/fix are sequential pipelines
            Self::BuildFeature | Self::FixBug => DecompositionStrategy::Sequential,
            // PR review stages have some parallelism (check tests || check style)
            Self::ReviewPr => DecompositionStrategy::Dag,
            // Generic defaults to sequential (safest)
            Self::Generic => DecompositionStrategy::Sequential,
        }
    }
}

impl std::fmt::Display for TaskPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuildFeature => write!(f, "Build Feature"),
            Self::FixBug => write!(f, "Fix Bug"),
            Self::ReviewPr => write!(f, "Review PR"),
            Self::Generic => write!(f, "Generic"),
        }
    }
}

// ─── Decomposed SubTask (with role) ─────────────────────────────────────────

/// A subtask enriched with the suggested agent role and estimated complexity.
///
/// This is a higher-level view than [`SubTask`] — it adds role assignment
/// and complexity estimation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposedSubTask {
    /// Human-readable title.
    pub title: String,
    /// Full description / prompt.
    pub description: String,
    /// Suggested agent role.
    pub role: AgentRole,
    /// Subtask type (for routing).
    pub task_type: SubTaskType,
    /// IDs of subtasks this depends on (by index in the list).
    pub dependency_indices: Vec<usize>,
    /// Estimated complexity (1–10).
    pub estimated_complexity: u8,
}

// ─── Template Decomposer ─────────────────────────────────────────────────────

/// Template-based task decomposer.
///
/// Detects the task pattern and applies a pre-defined template to decompose
/// it into subtasks with roles, dependencies, and complexity estimates.
pub struct TemplateDecomposer;

impl TemplateDecomposer {
    /// Decompose a task into enriched subtasks based on pattern detection.
    ///
    /// Returns the detected pattern, strategy, and list of subtasks.
    pub fn decompose(task: &str) -> (TaskPattern, DecompositionStrategy, Vec<DecomposedSubTask>) {
        let pattern = TaskPattern::detect(task);
        let strategy = pattern.default_strategy();

        let subtasks = match pattern {
            TaskPattern::BuildFeature => Self::build_feature_template(task),
            TaskPattern::FixBug => Self::fix_bug_template(task),
            TaskPattern::ReviewPr => Self::review_pr_template(task),
            TaskPattern::Generic => Self::generic_template(task),
        };

        (pattern, strategy, subtasks)
    }

    /// Convert decomposed subtasks into a [`TaskDecomposition`] suitable for
    /// the planner.
    pub fn to_task_decomposition(task: &str, subtasks: &[DecomposedSubTask]) -> TaskDecomposition {
        let mut decomposition = TaskDecomposition::new(task);

        // First pass: create all SubTask objects to get their UUIDs
        let mut created: Vec<SubTask> = subtasks
            .iter()
            .enumerate()
            .map(|(idx, ds)| {
                SubTask::new(idx, &ds.title, &ds.description, ds.task_type)
                    .with_complexity(ds.estimated_complexity)
            })
            .collect();

        // Second pass: wire up dependencies using the UUIDs
        for (idx, ds) in subtasks.iter().enumerate() {
            for &dep_idx in &ds.dependency_indices {
                if dep_idx < created.len() {
                    let dep_id = created[dep_idx].id;
                    created[idx].depends_on.push(dep_id);
                }
            }
        }

        for st in created {
            decomposition.add_subtask(st);
        }

        decomposition
    }

    /// Full pipeline: decompose → plan → execution plan.
    ///
    /// Convenience method that chains decomposition with the planner.
    pub fn decompose_and_plan(
        task: &str,
    ) -> Result<(TaskPattern, ExecutionPlan), crate::planner::PlanError> {
        let (pattern, _strategy, subtasks) = Self::decompose(task);
        let decomposition = Self::to_task_decomposition(task, &subtasks);
        let plan = Planner::plan(&decomposition)?;
        Ok((pattern, plan))
    }

    // ── Templates ────────────────────────────────────────────────────────

    /// "build feature X" → [research context, write code, write tests, review]
    fn build_feature_template(task: &str) -> Vec<DecomposedSubTask> {
        vec![
            DecomposedSubTask {
                title: "Research context".into(),
                description: format!("Research the codebase context and requirements for: {task}"),
                role: AgentRole::Researcher,
                task_type: SubTaskType::Research,
                dependency_indices: vec![],
                estimated_complexity: 3,
            },
            DecomposedSubTask {
                title: "Write code".into(),
                description: format!("Implement the feature based on research findings: {task}"),
                role: AgentRole::Coder,
                task_type: SubTaskType::Code,
                dependency_indices: vec![0], // depends on research
                estimated_complexity: 7,
            },
            DecomposedSubTask {
                title: "Write tests".into(),
                description: format!(
                    "Write comprehensive tests for the implemented feature: {task}"
                ),
                role: AgentRole::Tester,
                task_type: SubTaskType::Code,
                dependency_indices: vec![1], // depends on code
                estimated_complexity: 5,
            },
            DecomposedSubTask {
                title: "Code review".into(),
                description: format!(
                    "Review the implementation and tests for quality, security, and correctness: {task}"
                ),
                role: AgentRole::Reviewer,
                task_type: SubTaskType::Review,
                dependency_indices: vec![1, 2], // depends on code + tests
                estimated_complexity: 5,
            },
        ]
    }

    /// "fix bug X" → [reproduce, research, fix, test, review]
    fn fix_bug_template(task: &str) -> Vec<DecomposedSubTask> {
        vec![
            DecomposedSubTask {
                title: "Reproduce bug".into(),
                description: format!("Identify and reproduce the reported issue: {task}"),
                role: AgentRole::Tester,
                task_type: SubTaskType::Research,
                dependency_indices: vec![],
                estimated_complexity: 4,
            },
            DecomposedSubTask {
                title: "Research root cause".into(),
                description: format!(
                    "Investigate the root cause based on reproduction results: {task}"
                ),
                role: AgentRole::Researcher,
                task_type: SubTaskType::Research,
                dependency_indices: vec![0], // depends on reproduce
                estimated_complexity: 5,
            },
            DecomposedSubTask {
                title: "Implement fix".into(),
                description: format!("Write the fix based on root cause analysis: {task}"),
                role: AgentRole::Coder,
                task_type: SubTaskType::Code,
                dependency_indices: vec![1], // depends on research
                estimated_complexity: 6,
            },
            DecomposedSubTask {
                title: "Test fix".into(),
                description: format!(
                    "Verify the fix resolves the issue and doesn't introduce regressions: {task}"
                ),
                role: AgentRole::Tester,
                task_type: SubTaskType::Code,
                dependency_indices: vec![2], // depends on fix
                estimated_complexity: 4,
            },
            DecomposedSubTask {
                title: "Review fix".into(),
                description: format!(
                    "Review the bug fix for correctness and potential side effects: {task}"
                ),
                role: AgentRole::Reviewer,
                task_type: SubTaskType::Review,
                dependency_indices: vec![2, 3], // depends on fix + tests
                estimated_complexity: 4,
            },
        ]
    }

    /// "review PR X" → [read diff, check tests, check style, write review]
    fn review_pr_template(task: &str) -> Vec<DecomposedSubTask> {
        vec![
            DecomposedSubTask {
                title: "Read diff".into(),
                description: format!("Read and understand the changes in the pull request: {task}"),
                role: AgentRole::Researcher,
                task_type: SubTaskType::Research,
                dependency_indices: vec![],
                estimated_complexity: 4,
            },
            DecomposedSubTask {
                title: "Check tests".into(),
                description: format!(
                    "Verify test coverage and correctness for the PR changes: {task}"
                ),
                role: AgentRole::Tester,
                task_type: SubTaskType::Review,
                dependency_indices: vec![0], // depends on reading the diff
                estimated_complexity: 4,
            },
            DecomposedSubTask {
                title: "Check style".into(),
                description: format!(
                    "Review code style, naming, documentation, and best practices: {task}"
                ),
                role: AgentRole::Reviewer,
                task_type: SubTaskType::Review,
                dependency_indices: vec![0], // depends on reading the diff (parallel with check tests)
                estimated_complexity: 3,
            },
            DecomposedSubTask {
                title: "Write review".into(),
                description: format!(
                    "Synthesize findings into a comprehensive review with actionable feedback: {task}"
                ),
                role: AgentRole::Reviewer,
                task_type: SubTaskType::Creative,
                dependency_indices: vec![1, 2], // depends on both checks
                estimated_complexity: 5,
            },
        ]
    }

    /// Generic decomposition for unrecognized patterns.
    fn generic_template(task: &str) -> Vec<DecomposedSubTask> {
        vec![
            DecomposedSubTask {
                title: "Analyze task".into(),
                description: format!("Understand the requirements and context: {task}"),
                role: AgentRole::Planner,
                task_type: SubTaskType::Planning,
                dependency_indices: vec![],
                estimated_complexity: 4,
            },
            DecomposedSubTask {
                title: "Execute task".into(),
                description: format!("Carry out the main work: {task}"),
                role: AgentRole::Coder,
                task_type: SubTaskType::Code,
                dependency_indices: vec![0],
                estimated_complexity: 6,
            },
            DecomposedSubTask {
                title: "Verify results".into(),
                description: format!("Check the output for correctness and quality: {task}"),
                role: AgentRole::Reviewer,
                task_type: SubTaskType::Review,
                dependency_indices: vec![1],
                estimated_complexity: 4,
            },
        ]
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pattern Detection ────────────────────────────────────────────────

    #[test]
    fn test_detect_build_feature() {
        assert_eq!(
            TaskPattern::detect("build a REST API for user management"),
            TaskPattern::BuildFeature
        );
        assert_eq!(
            TaskPattern::detect("implement login flow"),
            TaskPattern::BuildFeature
        );
        assert_eq!(
            TaskPattern::detect("create a new dashboard widget"),
            TaskPattern::BuildFeature
        );
    }

    #[test]
    fn test_detect_fix_bug() {
        assert_eq!(
            TaskPattern::detect("fix bug in the authentication module"),
            TaskPattern::FixBug
        );
        assert_eq!(
            TaskPattern::detect("debug the memory leak in worker pool"),
            TaskPattern::FixBug
        );
        assert_eq!(
            TaskPattern::detect("fix error 500 on login page"),
            TaskPattern::FixBug
        );
    }

    #[test]
    fn test_detect_review_pr() {
        assert_eq!(
            TaskPattern::detect("review PR #42 for the new API"),
            TaskPattern::ReviewPr
        );
        assert_eq!(
            TaskPattern::detect("code review of the refactored module"),
            TaskPattern::ReviewPr
        );
    }

    #[test]
    fn test_detect_generic() {
        assert_eq!(
            TaskPattern::detect("optimize database queries"),
            TaskPattern::Generic
        );
    }

    // ── Template Decomposition ───────────────────────────────────────────

    #[test]
    fn test_build_feature_decomposition() {
        let (pattern, strategy, subtasks) =
            TemplateDecomposer::decompose("build a REST API for users");
        assert_eq!(pattern, TaskPattern::BuildFeature);
        assert_eq!(strategy, DecompositionStrategy::Sequential);
        assert_eq!(subtasks.len(), 4);

        assert_eq!(subtasks[0].role, AgentRole::Researcher);
        assert_eq!(subtasks[1].role, AgentRole::Coder);
        assert_eq!(subtasks[2].role, AgentRole::Tester);
        assert_eq!(subtasks[3].role, AgentRole::Reviewer);

        // Code depends on research
        assert_eq!(subtasks[1].dependency_indices, vec![0]);
        // Tests depend on code
        assert_eq!(subtasks[2].dependency_indices, vec![1]);
        // Review depends on code + tests
        assert_eq!(subtasks[3].dependency_indices, vec![1, 2]);
    }

    #[test]
    fn test_fix_bug_decomposition() {
        let (pattern, _, subtasks) = TemplateDecomposer::decompose("fix bug in auth module");
        assert_eq!(pattern, TaskPattern::FixBug);
        assert_eq!(subtasks.len(), 5);

        assert_eq!(subtasks[0].title, "Reproduce bug");
        assert_eq!(subtasks[4].title, "Review fix");
    }

    #[test]
    fn test_review_pr_decomposition() {
        let (pattern, strategy, subtasks) = TemplateDecomposer::decompose("review PR #42");
        assert_eq!(pattern, TaskPattern::ReviewPr);
        assert_eq!(strategy, DecompositionStrategy::Dag);
        assert_eq!(subtasks.len(), 4);

        // Check tests and check style are both dependent on read diff only
        assert_eq!(subtasks[1].dependency_indices, vec![0]);
        assert_eq!(subtasks[2].dependency_indices, vec![0]);
        // Write review depends on both checks
        assert_eq!(subtasks[3].dependency_indices, vec![1, 2]);
    }

    // ── Conversion to TaskDecomposition ──────────────────────────────────

    #[test]
    fn test_to_task_decomposition() {
        let (_, _, subtasks) = TemplateDecomposer::decompose("build feature X");
        let decomposition = TemplateDecomposer::to_task_decomposition("build feature X", &subtasks);

        assert_eq!(decomposition.len(), 4);
        // First subtask should be a root (no deps)
        assert!(decomposition.subtasks[0].is_root());
        // Second subtask depends on first
        assert_eq!(decomposition.subtasks[1].depends_on.len(), 1);
        assert_eq!(
            decomposition.subtasks[1].depends_on[0],
            decomposition.subtasks[0].id
        );
    }

    #[test]
    fn test_decompose_and_plan() {
        let (pattern, plan) =
            TemplateDecomposer::decompose_and_plan("build a new API endpoint").unwrap();
        assert_eq!(pattern, TaskPattern::BuildFeature);
        // Should have multiple stages (research is stage 0, code is stage 1, etc.)
        assert!(plan.num_stages() >= 2);
        assert_eq!(plan.total_subtasks(), 4);
    }

    #[test]
    fn test_review_pr_plan_has_parallelism() {
        let (_, plan) = TemplateDecomposer::decompose_and_plan("review PR #99").unwrap();
        // Stage 0: read diff
        // Stage 1: check tests + check style (parallel!)
        // Stage 2: write review
        assert_eq!(plan.num_stages(), 3);
        // Stage 1 should have 2 parallel subtasks
        assert_eq!(plan.stages[1].parallelism(), 2);
    }

    // ── Strategy ─────────────────────────────────────────────────────────

    #[test]
    fn test_strategy_display() {
        assert_eq!(DecompositionStrategy::Sequential.to_string(), "Sequential");
        assert_eq!(DecompositionStrategy::Parallel.to_string(), "Parallel");
        assert_eq!(DecompositionStrategy::Dag.to_string(), "DAG");
    }

    #[test]
    fn test_pattern_display() {
        assert_eq!(TaskPattern::BuildFeature.to_string(), "Build Feature");
        assert_eq!(TaskPattern::FixBug.to_string(), "Fix Bug");
        assert_eq!(TaskPattern::ReviewPr.to_string(), "Review PR");
        assert_eq!(TaskPattern::Generic.to_string(), "Generic");
    }

    // ── Serialization ────────────────────────────────────────────────────

    #[test]
    fn test_strategy_serde_roundtrip() {
        let s = DecompositionStrategy::Dag;
        let json = serde_json::to_string(&s).unwrap();
        let back: DecompositionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn test_pattern_serde_roundtrip() {
        let p = TaskPattern::FixBug;
        let json = serde_json::to_string(&p).unwrap();
        let back: TaskPattern = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
