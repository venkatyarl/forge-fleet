//! Leader daemon — accepts worker registrations, tracks fleet state,
//! assigns tasks, monitors health, promotes backup on failure.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use uuid::Uuid;

use ff_core::activity::YieldMode;
use ff_core::config::FleetConfig;
use ff_core::leader::{ElectionResult, check_failover};
use ff_core::task::{AgentTask, TaskResult};
use ff_core::types::{Hardware, Node, NodeStatus, Role};

use crate::election::ElectionManager;
use crate::resource_pool::ResourcePool;
use crate::scheduler::TaskScheduler;
use crate::work_queue::WorkQueue;

// ─── Worker registration messages ────────────────────────────────────────────

/// Registration request sent by a worker to the leader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRegistration {
    /// Worker's self-assigned node ID.
    pub node_id: Uuid,
    /// Worker's human-readable name.
    pub name: String,
    /// Worker's hostname / IP.
    pub host: String,
    /// Worker's API port.
    pub port: u16,
    /// Hardware profile.
    pub hardware: Hardware,
    /// Models currently loaded.
    pub models: Vec<String>,
    /// Whether this node is Taylor.
    pub is_taylor: bool,
}

/// Response to a worker registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationResponse {
    /// Whether the registration was accepted.
    pub accepted: bool,
    /// Heartbeat interval the worker should use (seconds).
    pub heartbeat_interval_secs: u64,
    /// Reason if rejected.
    pub reason: Option<String>,
}

/// Heartbeat sent by a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
    /// Worker's node ID.
    pub node_id: Uuid,
    /// Worker's current status.
    pub status: NodeStatus,
    /// Current yield mode (Taylor only, None for others).
    pub yield_mode: Option<YieldMode>,
    /// Current CPU load (0.0–100.0).
    pub cpu_load: f32,
    /// Current memory usage (0.0–100.0).
    pub memory_usage: f32,
    /// Current GPU usage (0.0–100.0).
    pub gpu_usage: f32,
    /// Active task count.
    pub active_tasks: u32,
    /// Models currently loaded.
    pub models: Vec<String>,
}

/// A tracked worker in the leader's state.
#[derive(Debug, Clone)]
pub struct TrackedWorker {
    /// Node definition.
    pub node: Node,
    /// Whether this is a Taylor node.
    pub is_taylor: bool,
    /// Current yield mode (Taylor only).
    pub yield_mode: YieldMode,
    /// Last heartbeat time.
    pub last_heartbeat: DateTime<Utc>,
    /// CPU load from last heartbeat.
    pub cpu_load: f32,
    /// Memory usage from last heartbeat.
    pub memory_usage: f32,
    /// GPU usage from last heartbeat.
    pub gpu_usage: f32,
    /// Active task count.
    pub active_tasks: u32,
}

// ─── Leader Daemon ───────────────────────────────────────────────────────────

/// The leader daemon — central coordinator of the ForgeFleet mesh.
pub struct LeaderDaemon {
    /// Fleet configuration.
    config: Arc<FleetConfig>,
    /// This node's name.
    node_name: String,
    /// Registered workers, keyed by node ID.
    workers: Arc<DashMap<Uuid, TrackedWorker>>,
    /// Resource pool for fleet-wide resource tracking.
    resource_pool: Arc<ResourcePool>,
    /// Task scheduler.
    scheduler: Arc<TaskScheduler>,
    /// Work queue.
    work_queue: Arc<WorkQueue>,
    /// Election manager.
    election_manager: Arc<ElectionManager>,
    /// Shutdown signal.
    shutdown_tx: watch::Sender<bool>,
    /// Shutdown receiver (clonable).
    shutdown_rx: watch::Receiver<bool>,
}

impl LeaderDaemon {
    /// Create a new leader daemon.
    pub fn new(config: FleetConfig, node_name: String) -> Self {
        let config = Arc::new(config);
        let workers: Arc<DashMap<Uuid, TrackedWorker>> = Arc::new(DashMap::new());
        let resource_pool = Arc::new(ResourcePool::new());
        let work_queue = Arc::new(WorkQueue::new());
        let scheduler = Arc::new(TaskScheduler::new(
            Arc::clone(&workers),
            Arc::clone(&resource_pool),
        ));
        let election_manager =
            Arc::new(ElectionManager::new(Arc::clone(&config), node_name.clone()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Self {
            config,
            node_name,
            workers,
            resource_pool,
            scheduler,
            work_queue,
            election_manager,
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Register a new worker. Returns a registration response.
    pub fn register_worker(&self, reg: WorkerRegistration) -> RegistrationResponse {
        let node = Node {
            id: reg.node_id,
            name: reg.name.clone(),
            host: reg.host.clone(),
            port: reg.port,
            role: Role::Worker,
            election_priority: if reg.is_taylor { 1 } else { 100 },
            status: NodeStatus::Online,
            hardware: reg.hardware.clone(),
            models: reg.models.clone(),
            last_heartbeat: Some(Utc::now()),
            registered_at: Utc::now(),
        };

        let worker = TrackedWorker {
            node: node.clone(),
            is_taylor: reg.is_taylor,
            yield_mode: YieldMode::Idle,
            last_heartbeat: Utc::now(),
            cpu_load: 0.0,
            memory_usage: 0.0,
            gpu_usage: 0.0,
            active_tasks: 0,
        };

        // Update resource pool with new worker.
        self.resource_pool.add_node(
            reg.node_id,
            reg.hardware.memory_gib,
            reg.hardware.cpu_cores,
            reg.hardware.has_gpu(),
        );

        self.workers.insert(reg.node_id, worker);

        info!(
            worker_name = %reg.name,
            worker_id = %reg.node_id,
            memory_gib = reg.hardware.memory_gib,
            cpu_cores = reg.hardware.cpu_cores,
            models = ?reg.models,
            "worker registered"
        );

        RegistrationResponse {
            accepted: true,
            heartbeat_interval_secs: self.config.fleet.heartbeat_interval_secs,
            reason: None,
        }
    }

    /// Process a worker heartbeat. Returns `true` if the worker is known.
    pub fn process_heartbeat(&self, hb: WorkerHeartbeat) -> bool {
        if let Some(mut worker) = self.workers.get_mut(&hb.node_id) {
            worker.last_heartbeat = Utc::now();
            worker.node.status = hb.status;
            worker.node.last_heartbeat = Some(Utc::now());
            worker.cpu_load = hb.cpu_load;
            worker.memory_usage = hb.memory_usage;
            worker.gpu_usage = hb.gpu_usage;
            worker.active_tasks = hb.active_tasks;
            worker.node.models = hb.models;

            if let Some(mode) = hb.yield_mode {
                worker.yield_mode = mode;
            }

            // Update resource pool usage.
            self.resource_pool
                .update_usage(hb.node_id, hb.memory_usage, hb.cpu_load, hb.gpu_usage);

            debug!(
                worker_id = %hb.node_id,
                status = ?hb.status,
                cpu = hb.cpu_load,
                mem = hb.memory_usage,
                tasks = hb.active_tasks,
                "heartbeat processed"
            );
            true
        } else {
            warn!(worker_id = %hb.node_id, "heartbeat from unknown worker");
            false
        }
    }

    /// Check for timed-out workers and mark them offline.
    pub fn check_worker_health(&self) {
        let timeout = Duration::from_secs(self.config.fleet.heartbeat_timeout_secs);
        let now = Utc::now();

        for mut entry in self.workers.iter_mut() {
            let worker = entry.value_mut();
            let elapsed = now
                .signed_duration_since(worker.last_heartbeat)
                .to_std()
                .unwrap_or(Duration::ZERO);

            if elapsed > timeout && worker.node.status != NodeStatus::Offline {
                warn!(
                    worker = %worker.node.name,
                    elapsed_secs = elapsed.as_secs(),
                    "worker timed out — marking offline"
                );
                worker.node.status = NodeStatus::Offline;

                self.resource_pool.mark_offline(worker.node.id);
            }
        }
    }

    /// Run a leader election check, potentially triggering failover.
    pub fn run_election_check(&self) -> Option<ElectionResult> {
        let node_health: Vec<(String, bool, bool)> = self
            .workers
            .iter()
            .map(|entry| {
                let w = entry.value();
                let is_healthy = w.node.status == NodeStatus::Online;
                let is_yielding = w.is_taylor
                    && matches!(w.yield_mode, YieldMode::Interactive | YieldMode::Protected);
                (w.node.name.clone(), is_healthy, is_yielding)
            })
            .collect();

        check_failover(&self.node_name, &self.config, &node_health)
    }

    /// Submit a task to the work queue and attempt to schedule it.
    pub fn submit_task(&self, task: AgentTask) -> Uuid {
        let task_id = task.id;
        self.work_queue.submit(task);

        // Try to schedule any pending tasks.
        self.try_schedule_pending();

        task_id
    }

    /// Record a task result from a worker.
    pub fn record_task_result(&self, result: TaskResult) {
        let task_id = result.task_id;
        let success = result.success;

        self.work_queue.complete(task_id, success);

        if success {
            info!(task_id = %task_id, "task completed successfully");
        } else {
            warn!(task_id = %task_id, "task failed — checking for retry");
            // The work queue handles retry logic internally.
        }
    }

    /// Try to schedule pending tasks from the queue.
    fn try_schedule_pending(&self) {
        while let Some(entry) = self.work_queue.peek_pending() {
            let task = &entry.task;

            if let Some(worker_id) = self.scheduler.select_worker(task) {
                if let Some(claimed) = self.work_queue.claim(entry.task.id, worker_id) {
                    info!(
                        task_id = %claimed.task.id,
                        worker_id = %worker_id,
                        "task scheduled to worker"
                    );
                } else {
                    break; // Couldn't claim — something changed.
                }
            } else {
                debug!("no suitable worker for pending task — will retry later");
                break;
            }
        }
    }

    /// Get a snapshot of all tracked workers.
    pub fn worker_snapshot(&self) -> Vec<TrackedWorker> {
        self.workers.iter().map(|e| e.value().clone()).collect()
    }

    /// Remove a worker (unregister).
    pub fn remove_worker(&self, node_id: &Uuid) -> bool {
        if let Some((_, worker)) = self.workers.remove(node_id) {
            self.resource_pool.remove_node(*node_id);
            info!(worker = %worker.node.name, "worker unregistered");
            true
        } else {
            false
        }
    }

    /// Get number of registered workers.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Get the election manager.
    pub fn election_manager(&self) -> &ElectionManager {
        &self.election_manager
    }

    /// Get the work queue.
    pub fn work_queue(&self) -> &WorkQueue {
        &self.work_queue
    }

    /// Get the resource pool.
    pub fn resource_pool(&self) -> &ResourcePool {
        &self.resource_pool
    }

    /// Signal shutdown.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        info!("leader daemon shutting down");
    }

    /// Start the background health-check and election loop.
    ///
    /// This spawns a tokio task that runs until shutdown is signaled.
    pub fn start_background_loops(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let daemon = Arc::clone(self);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(
                daemon.config.fleet.heartbeat_interval_secs,
            ));
            let mut shutdown = daemon.shutdown_rx.clone();

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Check worker health.
                        daemon.check_worker_health();

                        // Check if election failover is needed.
                        if let Some(result) = daemon.run_election_check()
                            && let Some(ref new_leader) = result.elected {
                                info!(
                                    new_leader = %new_leader,
                                    reason = %result.reason,
                                    "election triggered — new leader elected"
                                );
                                // In a full implementation, this would notify
                                // all workers about the leadership change.
                            }

                        // Try to schedule any pending tasks.
                        daemon.try_schedule_pending();
                    }
                    _ = shutdown.changed() => {
                        info!("leader background loop shutting down");
                        break;
                    }
                }
            }
        })
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::task::AgentTaskKind;
    use ff_core::types::*;

    fn test_config() -> FleetConfig {
        FleetConfig {
            fleet: ff_core::config::FleetSettings {
                name: "test-fleet".into(),
                heartbeat_interval_secs: 5,
                heartbeat_timeout_secs: 15,
                api_port: 51800,
                ..Default::default()
            },
            nodes: std::collections::HashMap::new(),
            models: vec![],
            leader: ff_core::config::LeaderConfig {
                preferred: "taylor".into(),
                fallback_order: vec!["james".into()],
                election_interval_secs: 10,
            },
            ..Default::default()
        }
    }

    fn test_hardware() -> Hardware {
        Hardware {
            os: OsType::Linux,
            cpu_model: "AMD Ryzen 7".into(),
            cpu_cores: 16,
            gpu: GpuType::None,
            gpu_model: None,
            memory_gib: 64,
            memory_type: MemoryType::Ddr5,
            interconnect: Interconnect::Ethernet10g,
            runtimes: vec![Runtime::LlamaCpp],
        }
    }

    #[test]
    fn test_register_worker() {
        let daemon = LeaderDaemon::new(test_config(), "taylor".into());
        let reg = WorkerRegistration {
            node_id: Uuid::new_v4(),
            name: "james".into(),
            host: "192.168.5.101".into(),
            port: 51800,
            hardware: test_hardware(),
            models: vec!["qwen3-9b".into()],
            is_taylor: false,
        };

        let resp = daemon.register_worker(reg.clone());
        assert!(resp.accepted);
        assert_eq!(daemon.worker_count(), 1);
    }

    #[test]
    fn test_heartbeat_updates_worker() {
        let daemon = LeaderDaemon::new(test_config(), "taylor".into());
        let node_id = Uuid::new_v4();

        // Register.
        daemon.register_worker(WorkerRegistration {
            node_id,
            name: "james".into(),
            host: "192.168.5.101".into(),
            port: 51800,
            hardware: test_hardware(),
            models: vec![],
            is_taylor: false,
        });

        // Heartbeat.
        let found = daemon.process_heartbeat(WorkerHeartbeat {
            node_id,
            status: NodeStatus::Online,
            yield_mode: None,
            cpu_load: 45.0,
            memory_usage: 60.0,
            gpu_usage: 0.0,
            active_tasks: 2,
            models: vec!["qwen3-9b".into()],
        });
        assert!(found);

        let snapshot = daemon.worker_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].cpu_load, 45.0);
        assert_eq!(snapshot[0].active_tasks, 2);
    }

    #[test]
    fn test_remove_worker() {
        let daemon = LeaderDaemon::new(test_config(), "taylor".into());
        let node_id = Uuid::new_v4();

        daemon.register_worker(WorkerRegistration {
            node_id,
            name: "james".into(),
            host: "192.168.5.101".into(),
            port: 51800,
            hardware: test_hardware(),
            models: vec![],
            is_taylor: false,
        });

        assert_eq!(daemon.worker_count(), 1);
        assert!(daemon.remove_worker(&node_id));
        assert_eq!(daemon.worker_count(), 0);
    }

    #[test]
    fn test_submit_and_record_task() {
        let daemon = LeaderDaemon::new(test_config(), "taylor".into());
        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "echo hello".into(),
                timeout_secs: Some(30),
            },
        };

        let task_id = daemon.submit_task(task);
        let stats = daemon.work_queue().stats();
        assert_eq!(stats.total_submitted, 1);

        daemon.record_task_result(TaskResult {
            task_id,
            success: true,
            output: "hello".into(),
            completed_at: Utc::now(),
            duration_ms: 50,
        });

        let stats = daemon.work_queue().stats();
        assert_eq!(stats.total_completed, 1);
    }
}
