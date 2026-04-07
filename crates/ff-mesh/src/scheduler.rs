//! Task scheduler — assigns tasks to workers based on hardware capability,
//! current load, activity level (yield when user active), model availability.

use std::sync::Arc;

use dashmap::DashMap;
use tracing::debug;
use uuid::Uuid;

use ff_core::activity::YieldMode;
use ff_core::task::{AgentTask, AgentTaskKind};
use ff_core::types::NodeStatus;

use crate::leader::TrackedWorker;
use crate::resource_pool::ResourcePool;

// ─── Scoring ─────────────────────────────────────────────────────────────────

/// Score components for a worker evaluating a task.
#[derive(Debug, Clone)]
struct WorkerScore {
    /// Worker's node ID.
    node_id: Uuid,
    /// Worker name (for logging).
    name: String,
    /// Whether the worker can handle this task at all.
    eligible: bool,
    /// Lower is better. 0 = best.
    score: f64,
    /// Reason for ineligibility (if not eligible).
    #[allow(dead_code)]
    reason: Option<String>,
}

// ─── Scheduler ───────────────────────────────────────────────────────────────

/// Task scheduler that selects the best worker for a given task.
pub struct TaskScheduler {
    /// Reference to the live worker map.
    workers: Arc<DashMap<Uuid, TrackedWorker>>,
    /// Resource pool for fleet-wide capacity checks (used by advanced scheduling).
    #[allow(dead_code)]
    resource_pool: Arc<ResourcePool>,
}

impl TaskScheduler {
    /// Create a new task scheduler.
    pub fn new(
        workers: Arc<DashMap<Uuid, TrackedWorker>>,
        resource_pool: Arc<ResourcePool>,
    ) -> Self {
        Self {
            workers,
            resource_pool,
        }
    }

    /// Select the best worker for a task. Returns `None` if no worker is suitable.
    pub fn select_worker(&self, task: &AgentTask) -> Option<Uuid> {
        let mut scores: Vec<WorkerScore> = self
            .workers
            .iter()
            .map(|entry| {
                let worker = entry.value();
                self.score_worker(worker, task)
            })
            .collect();

        // Filter to eligible, then sort by score (ascending — lower is better).
        scores.retain(|s| s.eligible);
        scores.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(best) = scores.first() {
            debug!(
                worker = %best.name,
                score = best.score,
                task_id = %task.id,
                "selected worker for task"
            );
            Some(best.node_id)
        } else {
            debug!(task_id = %task.id, "no eligible worker found");
            None
        }
    }

    /// Score a worker for a given task. Lower score = better fit.
    fn score_worker(&self, worker: &TrackedWorker, task: &AgentTask) -> WorkerScore {
        let name = worker.node.name.clone();
        let node_id = worker.node.id;

        // ── Eligibility checks ───────────────────────────────────────

        // Must be online.
        if worker.node.status != NodeStatus::Online {
            return WorkerScore {
                node_id,
                name,
                eligible: false,
                score: f64::MAX,
                reason: Some("node not online".into()),
            };
        }

        // Taylor yield mode check.
        if worker.is_taylor {
            match worker.yield_mode {
                YieldMode::Protected => {
                    return WorkerScore {
                        node_id,
                        name,
                        eligible: false,
                        score: f64::MAX,
                        reason: Some("taylor in Protected mode".into()),
                    };
                }
                YieldMode::Interactive => {
                    // Only allow lightweight tasks (not inference).
                    if matches!(task.kind, AgentTaskKind::ModelInference { .. }) {
                        return WorkerScore {
                            node_id,
                            name,
                            eligible: false,
                            score: f64::MAX,
                            reason: Some("taylor in Interactive mode — no inference".into()),
                        };
                    }
                }
                _ => {} // Assist and Idle are fine.
            }
        }

        // Model availability check for inference tasks.
        if let AgentTaskKind::ModelInference {
            model: Some(ref model_id),
            ..
        } = task.kind
            && !worker.node.models.contains(model_id)
        {
            return WorkerScore {
                node_id,
                name,
                eligible: false,
                score: f64::MAX,
                reason: Some(format!("model '{}' not loaded", model_id)),
            };
        }

        // ── Scoring (lower = better) ────────────────────────────────

        let mut score = 0.0;

        // Load factor: prefer less loaded workers.
        // CPU load (0–100) contributes heavily.
        score += worker.cpu_load as f64 * 1.0;

        // Memory pressure.
        score += worker.memory_usage as f64 * 0.5;

        // GPU usage.
        score += worker.gpu_usage as f64 * 0.8;

        // Active task count penalty (prefer workers with fewer tasks).
        score += worker.active_tasks as f64 * 15.0;

        // Taylor yield mode penalty — prefer non-Taylor when user is somewhat active.
        if worker.is_taylor {
            match worker.yield_mode {
                YieldMode::Interactive => score += 500.0, // Heavy penalty.
                YieldMode::Assist => score += 100.0,      // Moderate penalty.
                YieldMode::Idle => score += 0.0,          // No penalty.
                YieldMode::Protected => score += 1000.0,  // Should never get here (filtered above).
            }
        }

        // Hardware bonus for inference tasks: prefer GPU nodes.
        if matches!(task.kind, AgentTaskKind::ModelInference { .. }) {
            if worker.node.hardware.has_gpu() {
                score -= 50.0; // GPU bonus.
            }
            // Prefer nodes with more memory for large models.
            score -= (worker.node.hardware.memory_gib as f64) * 0.5;
        }

        WorkerScore {
            node_id,
            name,
            eligible: true,
            score,
            reason: None,
        }
    }

    /// Get a ranked list of workers for a task (for debugging/display).
    pub fn rank_workers(&self, task: &AgentTask) -> Vec<(Uuid, String, f64, bool)> {
        let mut scores: Vec<WorkerScore> = self
            .workers
            .iter()
            .map(|entry| self.score_worker(entry.value(), task))
            .collect();

        scores.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        scores
            .into_iter()
            .map(|s| (s.node_id, s.name, s.score, s.eligible))
            .collect()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ff_core::types::*;

    fn make_worker(
        name: &str,
        memory_gib: u64,
        has_gpu: bool,
        cpu_load: f32,
        active_tasks: u32,
        is_taylor: bool,
        yield_mode: YieldMode,
        models: Vec<String>,
    ) -> (Uuid, TrackedWorker) {
        let id = Uuid::new_v4();
        let gpu = if has_gpu {
            GpuType::AppleSilicon
        } else {
            GpuType::None
        };
        let worker = TrackedWorker {
            node: Node {
                id,
                name: name.into(),
                host: "127.0.0.1".into(),
                port: 51800,
                role: Role::Worker,
                election_priority: 10,
                status: NodeStatus::Online,
                hardware: Hardware {
                    os: OsType::Linux,
                    cpu_model: "test".into(),
                    cpu_cores: 16,
                    gpu,
                    gpu_model: None,
                    memory_gib,
                    memory_type: MemoryType::Ddr5,
                    interconnect: Interconnect::Ethernet10g,
                    runtimes: vec![Runtime::LlamaCpp],
                },
                models,
                last_heartbeat: Some(Utc::now()),
                registered_at: Utc::now(),
            },
            is_taylor,
            yield_mode,
            last_heartbeat: Utc::now(),
            cpu_load,
            memory_usage: 30.0,
            gpu_usage: 10.0,
            active_tasks,
        };
        (id, worker)
    }

    #[test]
    fn test_select_least_loaded() {
        let workers = Arc::new(DashMap::new());
        let pool = Arc::new(ResourcePool::new());

        let (id1, w1) = make_worker("james", 64, false, 80.0, 3, false, YieldMode::Idle, vec![]);
        let (id2, w2) = make_worker("marcus", 64, false, 20.0, 0, false, YieldMode::Idle, vec![]);

        workers.insert(id1, w1);
        workers.insert(id2, w2);

        let scheduler = TaskScheduler::new(workers, pool);

        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "echo hi".into(),
                timeout_secs: None,
            },
        };

        let selected = scheduler.select_worker(&task).unwrap();
        assert_eq!(selected, id2); // Marcus is less loaded.
    }

    #[test]
    fn test_taylor_avoided_in_interactive() {
        let workers = Arc::new(DashMap::new());
        let pool = Arc::new(ResourcePool::new());

        let (id1, w1) = make_worker(
            "taylor",
            128,
            true,
            10.0,
            0,
            true,
            YieldMode::Interactive,
            vec!["qwen3-32b".into()],
        );
        let (id2, w2) = make_worker(
            "james",
            64,
            false,
            50.0,
            1,
            false,
            YieldMode::Idle,
            vec!["qwen3-32b".into()],
        );

        workers.insert(id1, w1);
        workers.insert(id2, w2);

        let scheduler = TaskScheduler::new(workers, pool);

        // Inference task — Taylor in Interactive should be avoided.
        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ModelInference {
                model: Some("qwen3-32b".into()),
                prompt: "test".into(),
                max_tokens: None,
            },
        };

        let selected = scheduler.select_worker(&task).unwrap();
        assert_eq!(selected, id2); // James selected, Taylor skipped.
    }

    #[test]
    fn test_model_availability_filter() {
        let workers = Arc::new(DashMap::new());
        let pool = Arc::new(ResourcePool::new());

        // James has the model, Marcus doesn't.
        let (id1, w1) = make_worker(
            "james",
            64,
            false,
            50.0,
            0,
            false,
            YieldMode::Idle,
            vec!["qwen3-32b".into()],
        );
        let (id2, w2) = make_worker("marcus", 64, false, 10.0, 0, false, YieldMode::Idle, vec![]);

        workers.insert(id1, w1);
        workers.insert(id2, w2);

        let scheduler = TaskScheduler::new(workers, pool);

        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ModelInference {
                model: Some("qwen3-32b".into()),
                prompt: "test".into(),
                max_tokens: None,
            },
        };

        let selected = scheduler.select_worker(&task).unwrap();
        assert_eq!(selected, id1); // Only James has the model.
    }

    #[test]
    fn test_gpu_preferred_for_inference() {
        let workers = Arc::new(DashMap::new());
        let pool = Arc::new(ResourcePool::new());

        let (id1, w1) = make_worker("james", 64, false, 20.0, 0, false, YieldMode::Idle, vec![]);
        let (id2, w2) = make_worker("taylor", 128, true, 20.0, 0, false, YieldMode::Idle, vec![]);

        workers.insert(id1, w1);
        workers.insert(id2, w2);

        let scheduler = TaskScheduler::new(workers, pool);

        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ModelInference {
                model: None,
                prompt: "test".into(),
                max_tokens: None,
            },
        };

        let selected = scheduler.select_worker(&task).unwrap();
        assert_eq!(selected, id2); // Taylor has GPU + more memory.
    }

    #[test]
    fn test_no_eligible_workers() {
        let workers = Arc::new(DashMap::new());
        let pool = Arc::new(ResourcePool::new());

        // Single worker is offline.
        let (id1, mut w1) =
            make_worker("james", 64, false, 20.0, 0, false, YieldMode::Idle, vec![]);
        w1.node.status = NodeStatus::Offline;
        workers.insert(id1, w1);

        let scheduler = TaskScheduler::new(workers, pool);

        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "echo hi".into(),
                timeout_secs: None,
            },
        };

        assert!(scheduler.select_worker(&task).is_none());
    }

    #[test]
    fn test_taylor_protected_excluded() {
        let workers = Arc::new(DashMap::new());
        let pool = Arc::new(ResourcePool::new());

        let (id1, w1) = make_worker(
            "taylor",
            128,
            true,
            0.0,
            0,
            true,
            YieldMode::Protected,
            vec![],
        );
        workers.insert(id1, w1);

        let scheduler = TaskScheduler::new(workers, pool);

        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "echo hi".into(),
                timeout_secs: None,
            },
        };

        assert!(scheduler.select_worker(&task).is_none());
    }
}
