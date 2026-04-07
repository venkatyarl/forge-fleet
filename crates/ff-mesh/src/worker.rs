//! Worker role — registers with leader, accepts tasks, reports results,
//! advertises resources and current load.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, watch};
use tracing::{debug, info, warn};
use uuid::Uuid;

use ff_core::activity::ActivityState;
use ff_core::task::{AgentTask, AgentTaskKind, TaskResult};
use ff_core::types::{Hardware, NodeStatus};

use crate::leader::{RegistrationResponse, WorkerHeartbeat, WorkerRegistration};

// ─── Worker State ────────────────────────────────────────────────────────────

/// Current state of the worker agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// Not yet registered with a leader.
    Unregistered,
    /// Registered and operational.
    Active,
    /// Lost connection to leader, attempting reconnection.
    Disconnected,
    /// Gracefully shutting down.
    Draining,
}

/// Configuration for the worker agent.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// This worker's node ID.
    pub node_id: Uuid,
    /// This worker's name.
    pub name: String,
    /// This worker's hostname / IP.
    pub host: String,
    /// This worker's API port.
    pub port: u16,
    /// Hardware profile.
    pub hardware: Hardware,
    /// Leader's address (host:port).
    pub leader_addr: String,
    /// Optional override for model inference API base URL.
    ///
    /// When unset, inference requests are sent to `http://{host}:{port}`.
    pub inference_base_url: Option<String>,
    /// Whether this is a Taylor node.
    pub is_taylor: bool,
    /// Max concurrent tasks this worker handles.
    pub max_concurrent_tasks: u32,
}

// ─── Worker Agent ────────────────────────────────────────────────────────────

/// The worker agent — runs on each fleet node to participate in the mesh.
pub struct WorkerAgent {
    /// Worker configuration.
    config: WorkerConfig,
    /// Current state.
    state: Arc<RwLock<WorkerState>>,
    /// Models currently loaded.
    models: Arc<RwLock<Vec<String>>>,
    /// Active tasks being executed.
    active_tasks: Arc<RwLock<Vec<ActiveTask>>>,
    /// Heartbeat interval (set by leader on registration).
    heartbeat_interval: Arc<RwLock<Duration>>,
    /// Activity state (Taylor nodes only).
    activity_state: Arc<RwLock<ActivityState>>,
    /// Shutdown signal.
    shutdown_tx: watch::Sender<bool>,
    /// Shutdown receiver (clonable).
    shutdown_rx: watch::Receiver<bool>,
}

/// A task actively being processed by this worker.
#[derive(Debug, Clone)]
pub struct ActiveTask {
    /// The task being executed.
    pub task: AgentTask,
    /// When execution started.
    pub started_at: DateTime<Utc>,
    /// Assigned by the leader.
    pub assigned_by: Option<String>,
}

impl WorkerAgent {
    /// Create a new worker agent.
    pub fn new(config: WorkerConfig) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Self {
            config,
            state: Arc::new(RwLock::new(WorkerState::Unregistered)),
            models: Arc::new(RwLock::new(Vec::new())),
            active_tasks: Arc::new(RwLock::new(Vec::new())),
            heartbeat_interval: Arc::new(RwLock::new(Duration::from_secs(15))),
            activity_state: Arc::new(RwLock::new(ActivityState::initial())),
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Build a registration request for the leader.
    pub async fn build_registration(&self) -> WorkerRegistration {
        let models = self.models.read().await;
        WorkerRegistration {
            node_id: self.config.node_id,
            name: self.config.name.clone(),
            host: self.config.host.clone(),
            port: self.config.port,
            hardware: self.config.hardware.clone(),
            models: models.clone(),
            is_taylor: self.config.is_taylor,
        }
    }

    /// Process a registration response from the leader.
    pub async fn handle_registration_response(&self, resp: RegistrationResponse) -> bool {
        if resp.accepted {
            *self.state.write().await = WorkerState::Active;
            *self.heartbeat_interval.write().await =
                Duration::from_secs(resp.heartbeat_interval_secs);
            info!(
                name = %self.config.name,
                heartbeat_secs = resp.heartbeat_interval_secs,
                "registered with leader"
            );
            true
        } else {
            warn!(
                name = %self.config.name,
                reason = ?resp.reason,
                "registration rejected by leader"
            );
            false
        }
    }

    /// Build a heartbeat message.
    pub async fn build_heartbeat(&self) -> WorkerHeartbeat {
        let active_tasks = self.active_tasks.read().await;
        let models = self.models.read().await;
        let activity = self.activity_state.read().await;

        WorkerHeartbeat {
            node_id: self.config.node_id,
            status: NodeStatus::Online,
            yield_mode: if self.config.is_taylor {
                Some(activity.mode)
            } else {
                None
            },
            cpu_load: activity.signals.cpu_percent,
            memory_usage: activity.signals.memory_percent,
            gpu_usage: activity.signals.gpu_percent,
            active_tasks: active_tasks.len() as u32,
            models: models.clone(),
        }
    }

    /// Accept a task from the leader.
    pub async fn accept_task(&self, task: AgentTask) -> bool {
        let state = self.state.read().await;
        if *state != WorkerState::Active {
            warn!(state = ?*state, "cannot accept task — not active");
            return false;
        }

        let active = self.active_tasks.read().await;
        if active.len() >= self.config.max_concurrent_tasks as usize {
            warn!(
                current = active.len(),
                max = self.config.max_concurrent_tasks,
                "cannot accept task — at capacity"
            );
            return false;
        }
        drop(active);

        // Check Taylor yield mode.
        if self.config.is_taylor {
            let activity = self.activity_state.read().await;
            let max_jobs = activity.mode.max_concurrent_jobs();
            let current = self.active_tasks.read().await.len() as u32;
            if current >= max_jobs {
                debug!(
                    mode = %activity.mode,
                    max_jobs,
                    current,
                    "taylor node at yield capacity"
                );
                return false;
            }
        }

        let active_task = ActiveTask {
            task: task.clone(),
            started_at: Utc::now(),
            assigned_by: None,
        };

        self.active_tasks.write().await.push(active_task);
        info!(task_id = %task.id, kind = ?task.kind, "task accepted");
        true
    }

    /// Execute a task and return the result.
    pub async fn execute_task(&self, task: &AgentTask) -> TaskResult {
        let _start = Utc::now();
        let start_instant = tokio::time::Instant::now();

        let (success, output) = match &task.kind {
            AgentTaskKind::ShellCommand {
                command,
                timeout_secs,
            } => {
                info!(task_id = %task.id, command = %command, "executing shell command");
                let timeout = timeout_secs.unwrap_or(300);
                match tokio::time::timeout(
                    Duration::from_secs(timeout),
                    execute_shell_command(command),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => (false, format!("command timed out after {}s", timeout)),
                }
            }
            AgentTaskKind::ModelInference {
                model,
                prompt,
                max_tokens,
            } => {
                info!(
                    task_id = %task.id,
                    model = ?model,
                    prompt_len = prompt.len(),
                    "model inference requested"
                );
                let inference_base = self.inference_base_url();
                execute_model_inference(&inference_base, model.as_deref(), prompt, *max_tokens)
                    .await
            }
        };

        let duration_ms = start_instant.elapsed().as_millis() as u64;

        TaskResult {
            task_id: task.id,
            success,
            output,
            completed_at: Utc::now(),
            duration_ms,
        }
    }

    /// Complete a task — remove from active list and return the result.
    pub async fn complete_task(&self, task_id: Uuid) -> Option<ActiveTask> {
        let mut active = self.active_tasks.write().await;
        active
            .iter()
            .position(|t| t.task.id == task_id)
            .map(|pos| active.remove(pos))
    }

    /// Update the models list.
    pub async fn set_models(&self, new_models: Vec<String>) {
        *self.models.write().await = new_models;
    }

    /// Get current worker state.
    pub async fn state(&self) -> WorkerState {
        *self.state.read().await
    }

    /// Get active task count.
    pub async fn active_task_count(&self) -> usize {
        self.active_tasks.read().await.len()
    }

    /// Get the worker's node ID.
    pub fn node_id(&self) -> Uuid {
        self.config.node_id
    }

    /// Get the worker's name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Whether this is a Taylor node.
    pub fn is_taylor(&self) -> bool {
        self.config.is_taylor
    }

    /// Resolve the base URL used for model inference requests.
    fn inference_base_url(&self) -> String {
        match self.config.inference_base_url.as_deref() {
            Some(base) => normalize_http_base(base),
            None => normalize_http_base(&format!("{}:{}", self.config.host, self.config.port)),
        }
    }

    /// Signal shutdown — drain active tasks.
    pub async fn shutdown(&self) {
        *self.state.write().await = WorkerState::Draining;
        let _ = self.shutdown_tx.send(true);
        info!(name = %self.config.name, "worker shutting down");
    }

    /// Start the heartbeat loop. Returns a join handle.
    ///
    /// The loop sends heartbeats at the interval set by the leader.
    /// If a leader HTTP endpoint is available, this can POST heartbeats via reqwest.
    pub fn start_heartbeat_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let worker = Arc::clone(self);

        tokio::spawn(async move {
            let mut shutdown = worker.shutdown_rx.clone();

            loop {
                let interval = *worker.heartbeat_interval.read().await;
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        let state = worker.state().await;
                        if state == WorkerState::Active {
                            let hb = worker.build_heartbeat().await;
                            debug!(
                                node_id = %hb.node_id,
                                status = ?hb.status,
                                cpu = hb.cpu_load,
                                tasks = hb.active_tasks,
                                "heartbeat tick"
                            );
                            // In a full implementation: POST heartbeat to leader.
                            // let url = format!("http://{}/api/heartbeat", worker.config.leader_addr);
                            // let _ = reqwest::Client::new().post(&url).json(&hb).send().await;
                        }
                    }
                    _ = shutdown.changed() => {
                        info!("heartbeat loop shutting down");
                        break;
                    }
                }
            }
        })
    }
}

/// Execute a shell command and return (success, output).
async fn execute_shell_command(command: &str) -> (bool, String) {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let combined = if stderr.is_empty() {
                stdout
            } else {
                format!("{}\n--- stderr ---\n{}", stdout, stderr)
            };
            (output.status.success(), combined)
        }
        Err(e) => (false, format!("failed to execute command: {e}")),
    }
}

/// Execute model inference via ff-api/ff-runtime compatible endpoints.
async fn execute_model_inference(
    runtime_base_url: &str,
    model: Option<&str>,
    prompt: &str,
    max_tokens: Option<u32>,
) -> (bool, String) {
    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "model": model.unwrap_or("local-model"),
        "prompt": prompt,
        "max_tokens": max_tokens.unwrap_or(512),
    });

    let base = runtime_base_url.trim_end_matches('/');
    let endpoints = ["/v1/completions", "/inference"];
    let mut last_error = format!("inference endpoint unreachable at {base}");

    for endpoint in endpoints {
        let url = format!("{base}{endpoint}");
        match client.post(&url).json(&payload).send().await {
            Ok(response) if response.status().is_success() => {
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "<empty response>".to_string());
                return (true, body);
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if body.trim().is_empty() {
                    last_error = format!("runtime error {status} at {url}");
                } else {
                    last_error = format!("runtime error {status} at {url}: {body}");
                }
            }
            Err(err) => {
                last_error = format!("runtime request failed at {url}: {err}");
            }
        }
    }

    (false, last_error)
}

fn normalize_http_base(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.starts_with("http://") || base.starts_with("https://") {
        base.to_string()
    } else {
        format!("http://{base}")
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::activity::YieldMode;
    use ff_core::types::*;

    fn test_worker_config() -> WorkerConfig {
        WorkerConfig {
            node_id: Uuid::new_v4(),
            name: "james".into(),
            host: "192.168.5.101".into(),
            port: 51800,
            hardware: Hardware {
                os: OsType::Linux,
                cpu_model: "AMD Ryzen 7".into(),
                cpu_cores: 16,
                gpu: GpuType::None,
                gpu_model: None,
                memory_gib: 64,
                memory_type: MemoryType::Ddr5,
                interconnect: Interconnect::Ethernet10g,
                runtimes: vec![Runtime::LlamaCpp],
            },
            leader_addr: "192.168.5.100:51800".into(),
            inference_base_url: None,
            is_taylor: false,
            max_concurrent_tasks: 4,
        }
    }

    #[tokio::test]
    async fn test_worker_initial_state() {
        let worker = WorkerAgent::new(test_worker_config());
        assert_eq!(worker.state().await, WorkerState::Unregistered);
        assert_eq!(worker.active_task_count().await, 0);
    }

    #[tokio::test]
    async fn test_worker_registration() {
        let worker = WorkerAgent::new(test_worker_config());
        let reg = worker.build_registration().await;
        assert_eq!(reg.name, "james");
        assert!(!reg.is_taylor);

        let resp = RegistrationResponse {
            accepted: true,
            heartbeat_interval_secs: 10,
            reason: None,
        };
        assert!(worker.handle_registration_response(resp).await);
        assert_eq!(worker.state().await, WorkerState::Active);
    }

    #[tokio::test]
    async fn test_worker_accept_task() {
        let worker = WorkerAgent::new(test_worker_config());

        // Must be active to accept tasks.
        let task = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "echo hi".into(),
                timeout_secs: None,
            },
        };
        assert!(!worker.accept_task(task.clone()).await);

        // Register first.
        worker
            .handle_registration_response(RegistrationResponse {
                accepted: true,
                heartbeat_interval_secs: 10,
                reason: None,
            })
            .await;

        assert!(worker.accept_task(task).await);
        assert_eq!(worker.active_task_count().await, 1);
    }

    #[tokio::test]
    async fn test_worker_capacity_limit() {
        let mut config = test_worker_config();
        config.max_concurrent_tasks = 1;
        let worker = WorkerAgent::new(config);

        worker
            .handle_registration_response(RegistrationResponse {
                accepted: true,
                heartbeat_interval_secs: 10,
                reason: None,
            })
            .await;

        let task1 = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "sleep 1".into(),
                timeout_secs: None,
            },
        };
        let task2 = AgentTask {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "sleep 1".into(),
                timeout_secs: None,
            },
        };

        assert!(worker.accept_task(task1).await);
        assert!(!worker.accept_task(task2).await); // At capacity.
    }

    #[tokio::test]
    async fn test_worker_complete_task() {
        let worker = WorkerAgent::new(test_worker_config());

        worker
            .handle_registration_response(RegistrationResponse {
                accepted: true,
                heartbeat_interval_secs: 10,
                reason: None,
            })
            .await;

        let task_id = Uuid::new_v4();
        let task = AgentTask {
            id: task_id,
            created_at: Utc::now(),
            kind: AgentTaskKind::ShellCommand {
                command: "echo done".into(),
                timeout_secs: None,
            },
        };

        worker.accept_task(task).await;
        assert_eq!(worker.active_task_count().await, 1);

        let completed = worker.complete_task(task_id).await;
        assert!(completed.is_some());
        assert_eq!(worker.active_task_count().await, 0);
    }

    #[tokio::test]
    async fn test_heartbeat_includes_yield_mode() {
        let mut config = test_worker_config();
        config.is_taylor = true;
        let worker = WorkerAgent::new(config);

        worker
            .handle_registration_response(RegistrationResponse {
                accepted: true,
                heartbeat_interval_secs: 10,
                reason: None,
            })
            .await;

        let hb = worker.build_heartbeat().await;
        assert!(hb.yield_mode.is_some());
        assert_eq!(hb.yield_mode.unwrap(), YieldMode::Idle);
    }
}
