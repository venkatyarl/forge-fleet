//! Parallel execution manager — fire subtasks across nodes, track progress,
//! aggregate results.
//!
//! Uses the [`ExecutionPlan`](crate::ExecutionPlan) to dispatch subtasks in
//! stage order.  Within each stage, all subtasks run concurrently via
//! `tokio::spawn`.  Progress is tracked in a [`DashMap`](dashmap::DashMap)
//! for lock-free concurrent access.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::crew::CrewAssignment;
use crate::planner::ExecutionPlan;
use crate::router::RouteDecision;

// ─── Subtask Status ──────────────────────────────────────────────────────────

/// Lifecycle status of a subtask execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubTaskStatus {
    /// Waiting for dependencies to complete.
    Pending,
    /// Dispatched to a node, awaiting result.
    Running,
    /// Completed successfully.
    Completed,
    /// Failed with an error.
    Failed,
    /// Cancelled (e.g., a dependency failed).
    Cancelled,
}

impl std::fmt::Display for SubTaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Running => write!(f, "Running"),
            Self::Completed => write!(f, "Completed"),
            Self::Failed => write!(f, "Failed"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

// ─── SubTask Result ──────────────────────────────────────────────────────────

/// The result of executing a single subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTaskResult {
    /// Which subtask produced this result.
    pub subtask_id: Uuid,
    /// Final status.
    pub status: SubTaskStatus,
    /// Output text (model response, command output, etc.).
    pub output: String,
    /// Error message if failed.
    pub error: Option<String>,
    /// Which model handled this.
    pub model_id: Option<String>,
    /// Which node handled this.
    pub node_name: Option<String>,
    /// When execution started.
    pub started_at: Option<DateTime<Utc>>,
    /// When execution completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Token usage (prompt + completion).
    pub tokens_used: Option<u64>,
}

impl SubTaskResult {
    /// Create a successful result.
    pub fn success(
        subtask_id: Uuid,
        output: impl Into<String>,
        model_id: impl Into<String>,
        node_name: impl Into<String>,
        started_at: DateTime<Utc>,
        duration_ms: u64,
    ) -> Self {
        Self {
            subtask_id,
            status: SubTaskStatus::Completed,
            output: output.into(),
            error: None,
            model_id: Some(model_id.into()),
            node_name: Some(node_name.into()),
            started_at: Some(started_at),
            completed_at: Some(Utc::now()),
            duration_ms: Some(duration_ms),
            tokens_used: None,
        }
    }

    /// Create a failed result.
    pub fn failure(
        subtask_id: Uuid,
        error: impl Into<String>,
        model_id: Option<String>,
        node_name: Option<String>,
    ) -> Self {
        Self {
            subtask_id,
            status: SubTaskStatus::Failed,
            output: String::new(),
            error: Some(error.into()),
            model_id,
            node_name,
            started_at: None,
            completed_at: Some(Utc::now()),
            duration_ms: None,
            tokens_used: None,
        }
    }

    /// Create a cancelled result.
    pub fn cancelled(subtask_id: Uuid, reason: impl Into<String>) -> Self {
        Self {
            subtask_id,
            status: SubTaskStatus::Cancelled,
            output: String::new(),
            error: Some(reason.into()),
            model_id: None,
            node_name: None,
            started_at: None,
            completed_at: Some(Utc::now()),
            duration_ms: None,
            tokens_used: None,
        }
    }

    /// Is this result a success?
    pub fn is_success(&self) -> bool {
        self.status == SubTaskStatus::Completed
    }
}

// ─── Execution Result ────────────────────────────────────────────────────────

/// Aggregated result of executing an entire plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Plan that was executed.
    pub plan_id: Uuid,
    /// Results for each subtask.
    pub results: Vec<SubTaskResult>,
    /// Overall success (all subtasks completed).
    pub success: bool,
    /// Total execution time (ms).
    pub total_duration_ms: u64,
    /// When execution started.
    pub started_at: DateTime<Utc>,
    /// When execution completed.
    pub completed_at: DateTime<Utc>,
}

impl ExecutionResult {
    /// Count of completed subtasks.
    pub fn completed_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == SubTaskStatus::Completed)
            .count()
    }

    /// Count of failed subtasks.
    pub fn failed_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == SubTaskStatus::Failed)
            .count()
    }

    /// Get all successful outputs, concatenated.
    pub fn combined_output(&self) -> String {
        self.results
            .iter()
            .filter(|r| r.is_success())
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }

    /// Get result for a specific subtask.
    pub fn result_for(&self, subtask_id: Uuid) -> Option<&SubTaskResult> {
        self.results.iter().find(|r| r.subtask_id == subtask_id)
    }
}

// ─── Progress Tracking ───────────────────────────────────────────────────────

/// Live progress tracker using DashMap for concurrent access.
#[derive(Debug, Clone)]
pub struct ProgressTracker {
    statuses: Arc<DashMap<Uuid, SubTaskStatus>>,
    total: usize,
}

impl ProgressTracker {
    /// Create a new tracker for the given subtask IDs.
    pub fn new(subtask_ids: &[Uuid]) -> Self {
        let statuses = Arc::new(DashMap::new());
        for &id in subtask_ids {
            statuses.insert(id, SubTaskStatus::Pending);
        }
        Self {
            statuses,
            total: subtask_ids.len(),
        }
    }

    /// Update the status of a subtask.
    pub fn update(&self, subtask_id: Uuid, status: SubTaskStatus) {
        self.statuses.insert(subtask_id, status);
    }

    /// Get the current status of a subtask.
    pub fn status_of(&self, subtask_id: Uuid) -> Option<SubTaskStatus> {
        self.statuses.get(&subtask_id).map(|v| v.clone())
    }

    /// Number of completed subtasks (success + failed + cancelled).
    pub fn finished_count(&self) -> usize {
        self.statuses
            .iter()
            .filter(|entry| {
                matches!(
                    *entry.value(),
                    SubTaskStatus::Completed | SubTaskStatus::Failed | SubTaskStatus::Cancelled
                )
            })
            .count()
    }

    /// Progress as a fraction (0.0–1.0).
    pub fn progress(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        self.finished_count() as f64 / self.total as f64
    }

    /// Whether all subtasks have finished.
    pub fn is_complete(&self) -> bool {
        self.finished_count() == self.total
    }

    /// Get a snapshot of all statuses.
    pub fn snapshot(&self) -> HashMap<Uuid, SubTaskStatus> {
        self.statuses
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect()
    }
}

// ─── Dispatch Function ───────────────────────────────────────────────────────

/// Type alias for the async function that actually executes a subtask.
///
/// In production, this calls the model inference API on the routed node.
/// For testing, it can be a deterministic test double.
pub type DispatchFn = Arc<
    dyn Fn(Uuid, RouteDecision, CrewAssignment) -> tokio::task::JoinHandle<SubTaskResult>
        + Send
        + Sync,
>;

// ─── Parallel Executor ───────────────────────────────────────────────────────

/// The parallel execution engine.
///
/// Executes an [`ExecutionPlan`] stage by stage.  Within each stage, all
/// subtasks are dispatched concurrently.  Progress is tracked live.
pub struct ParallelExecutor {
    /// Route decisions for each subtask.
    routes: HashMap<Uuid, RouteDecision>,
    /// Crew assignments for each subtask.
    assignments: HashMap<Uuid, CrewAssignment>,
    /// Live progress tracker.
    pub tracker: ProgressTracker,
    /// Whether to cancel remaining subtasks on first failure.
    pub fail_fast: bool,
}

impl ParallelExecutor {
    /// Create a new executor.
    pub fn new(
        plan: &ExecutionPlan,
        routes: HashMap<Uuid, RouteDecision>,
        assignments: HashMap<Uuid, CrewAssignment>,
        fail_fast: bool,
    ) -> Self {
        let all_ids: Vec<Uuid> = plan.execution_order();
        Self {
            routes,
            assignments,
            tracker: ProgressTracker::new(&all_ids),
            fail_fast,
        }
    }

    /// Execute the plan using the provided dispatch function.
    ///
    /// Runs stages sequentially; subtasks within each stage run in parallel.
    /// Returns the aggregated result.
    pub async fn execute(&self, plan: &ExecutionPlan, dispatch: DispatchFn) -> ExecutionResult {
        let started_at = Utc::now();
        let mut all_results: Vec<SubTaskResult> = Vec::new();
        let mut overall_success = true;

        // Channel for collecting results from spawned tasks
        let (tx, mut rx) = mpsc::channel::<SubTaskResult>(plan.total_subtasks().max(1));

        for stage in &plan.stages {
            let mut stage_handles = Vec::new();

            for &subtask_id in &stage.subtask_ids {
                // Check if we should cancel (fail-fast mode)
                if self.fail_fast && !overall_success {
                    let cancelled = SubTaskResult::cancelled(
                        subtask_id,
                        "cancelled due to earlier failure (fail-fast mode)",
                    );
                    self.tracker.update(subtask_id, SubTaskStatus::Cancelled);
                    all_results.push(cancelled);
                    continue;
                }

                // Get route and assignment
                let route = match self.routes.get(&subtask_id) {
                    Some(r) => r.clone(),
                    None => {
                        let failed = SubTaskResult::failure(
                            subtask_id,
                            "no route decision available",
                            None,
                            None,
                        );
                        self.tracker.update(subtask_id, SubTaskStatus::Failed);
                        all_results.push(failed);
                        overall_success = false;
                        continue;
                    }
                };

                let assignment = match self.assignments.get(&subtask_id) {
                    Some(a) => a.clone(),
                    None => {
                        let failed = SubTaskResult::failure(
                            subtask_id,
                            "no crew assignment available",
                            None,
                            None,
                        );
                        self.tracker.update(subtask_id, SubTaskStatus::Failed);
                        all_results.push(failed);
                        overall_success = false;
                        continue;
                    }
                };

                self.tracker.update(subtask_id, SubTaskStatus::Running);

                // Dispatch the subtask
                let handle = dispatch(subtask_id, route, assignment);
                let tx = tx.clone();

                // Wrap in a task that sends the result back
                let tracker = self.tracker.clone();
                let wrapper = tokio::spawn(async move {
                    let result = handle.await.unwrap_or_else(|e| {
                        SubTaskResult::failure(
                            subtask_id,
                            format!("task panicked: {e}"),
                            None,
                            None,
                        )
                    });
                    let status = result.status.clone();
                    tracker.update(subtask_id, status);
                    let _ = tx.send(result).await;
                });
                stage_handles.push(wrapper);
            }

            // Wait for all tasks in this stage to complete
            for handle in stage_handles {
                let _ = handle.await;
            }

            // Collect results from channel
            while let Ok(result) = rx.try_recv() {
                if result.status == SubTaskStatus::Failed {
                    overall_success = false;
                }
                all_results.push(result);
            }
        }

        drop(tx);
        // Drain any remaining
        while let Some(result) = rx.recv().await {
            if result.status == SubTaskStatus::Failed {
                overall_success = false;
            }
            all_results.push(result);
        }

        let completed_at = Utc::now();
        let total_duration_ms = (completed_at - started_at)
            .num_milliseconds()
            .unsigned_abs();

        ExecutionResult {
            plan_id: plan.id,
            results: all_results,
            success: overall_success,
            total_duration_ms,
            started_at,
            completed_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::{AgentRole, CrewAssignment};
    use crate::decomposer::{SubTask, SubTaskType, TaskDecomposition};
    use crate::planner::Planner;
    use crate::router::RouteDecision;

    fn make_route(subtask_id: Uuid) -> RouteDecision {
        RouteDecision {
            subtask_id,
            model_id: "test-model".into(),
            node_name: "test-node".into(),
            endpoint: "127.0.0.1:51800".into(),
            score: crate::router::ModelScore {
                model_id: "test-model".into(),
                node_name: "test-node".into(),
                specialty_score: 1.0,
                health_score: 1.0,
                load_score: 1.0,
                hardware_score: 1.0,
                tier_score: 1.0,
                total: 5.0,
            },
            alternatives: vec![],
            decided_at: Utc::now(),
        }
    }

    fn make_assignment(subtask_id: Uuid) -> CrewAssignment {
        CrewAssignment::with_role(subtask_id, AgentRole::Coder)
    }

    /// Mock dispatch function that returns success after a short delay.
    fn mock_dispatch() -> DispatchFn {
        Arc::new(|subtask_id, route, _assignment| {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                SubTaskResult::success(
                    subtask_id,
                    "mock output",
                    route.model_id,
                    route.node_name,
                    Utc::now(),
                    10,
                )
            })
        })
    }

    /// Mock dispatch that always fails.
    fn mock_dispatch_fail() -> DispatchFn {
        Arc::new(|subtask_id, _route, _assignment| {
            tokio::spawn(async move {
                SubTaskResult::failure(subtask_id, "mock error", Some("test-model".into()), None)
            })
        })
    }

    #[tokio::test]
    async fn test_parallel_execution_all_succeed() {
        let mut decomp = TaskDecomposition::new("test");
        let st1 = SubTask::new(0, "a", "do a", SubTaskType::Code);
        let st2 = SubTask::new(1, "b", "do b", SubTaskType::Code);
        let id1 = st1.id;
        let id2 = st2.id;
        decomp.add_subtask(st1);
        decomp.add_subtask(st2);

        let plan = Planner::plan(&decomp).unwrap();

        let routes: HashMap<Uuid, RouteDecision> =
            vec![(id1, make_route(id1)), (id2, make_route(id2))]
                .into_iter()
                .collect();
        let assignments: HashMap<Uuid, CrewAssignment> =
            vec![(id1, make_assignment(id1)), (id2, make_assignment(id2))]
                .into_iter()
                .collect();

        let executor = ParallelExecutor::new(&plan, routes, assignments, false);
        let result = executor.execute(&plan, mock_dispatch()).await;

        assert!(result.success);
        assert_eq!(result.completed_count(), 2);
        assert_eq!(result.failed_count(), 0);
    }

    #[tokio::test]
    async fn test_parallel_execution_fail_fast() {
        let mut decomp = TaskDecomposition::new("fail fast test");
        let st1 = SubTask::new(0, "a", "do a", SubTaskType::Code);
        let id1 = st1.id;
        let st2 = SubTask::new(1, "b", "do b", SubTaskType::Code).depends_on(id1);
        let id2 = st2.id;
        decomp.add_subtask(st1);
        decomp.add_subtask(st2);

        let plan = Planner::plan(&decomp).unwrap();

        let routes: HashMap<Uuid, RouteDecision> =
            vec![(id1, make_route(id1)), (id2, make_route(id2))]
                .into_iter()
                .collect();
        let assignments: HashMap<Uuid, CrewAssignment> =
            vec![(id1, make_assignment(id1)), (id2, make_assignment(id2))]
                .into_iter()
                .collect();

        let executor = ParallelExecutor::new(&plan, routes, assignments, true);
        let result = executor.execute(&plan, mock_dispatch_fail()).await;

        assert!(!result.success);
        // st2 should be cancelled because st1 failed (fail-fast)
        let st2_result = result.result_for(id2).unwrap();
        assert_eq!(st2_result.status, SubTaskStatus::Cancelled);
    }

    #[test]
    fn test_progress_tracker() {
        let ids = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let tracker = ProgressTracker::new(&ids);

        assert_eq!(tracker.progress(), 0.0);
        assert!(!tracker.is_complete());

        tracker.update(ids[0], SubTaskStatus::Completed);
        assert!((tracker.progress() - 1.0 / 3.0).abs() < 0.01);

        tracker.update(ids[1], SubTaskStatus::Failed);
        tracker.update(ids[2], SubTaskStatus::Completed);
        assert!(tracker.is_complete());
        assert!((tracker.progress() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_subtask_result_constructors() {
        let id = Uuid::new_v4();

        let success = SubTaskResult::success(id, "output", "model", "node", Utc::now(), 100);
        assert!(success.is_success());
        assert_eq!(success.status, SubTaskStatus::Completed);

        let failure = SubTaskResult::failure(id, "boom", None, None);
        assert!(!failure.is_success());
        assert_eq!(failure.error, Some("boom".into()));

        let cancelled = SubTaskResult::cancelled(id, "nope");
        assert_eq!(cancelled.status, SubTaskStatus::Cancelled);
    }

    #[test]
    fn test_execution_result_combined_output() {
        let result = ExecutionResult {
            plan_id: Uuid::new_v4(),
            results: vec![
                SubTaskResult::success(Uuid::new_v4(), "part 1", "m", "n", Utc::now(), 10),
                SubTaskResult::failure(Uuid::new_v4(), "err", None, None),
                SubTaskResult::success(Uuid::new_v4(), "part 2", "m", "n", Utc::now(), 10),
            ],
            success: false,
            total_duration_ms: 100,
            started_at: Utc::now(),
            completed_at: Utc::now(),
        };

        let combined = result.combined_output();
        assert!(combined.contains("part 1"));
        assert!(combined.contains("part 2"));
        assert!(!combined.contains("err"));
    }
}
