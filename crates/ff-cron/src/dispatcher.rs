use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, warn};
use uuid::Uuid;

use ff_core::task::{AgentTask, AgentTaskKind};

use crate::job::JobTask;

pub type BoxFutureResult<T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'static>>;

pub type LocalExecutor =
    Arc<dyn Fn(String, Option<u64>) -> BoxFutureResult<String> + Send + Sync + 'static>;

pub type RemoteSubmitter =
    Arc<dyn Fn(AgentTask, Option<String>) -> BoxFutureResult<Uuid> + Send + Sync + 'static>;

#[derive(Debug, Clone)]
pub struct DispatchRequest {
    pub job_id: Uuid,
    pub run_id: Uuid,
    pub task: JobTask,
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    LocalCompleted {
        output: String,
        duration_ms: u64,
    },
    RemoteQueued {
        task_id: Uuid,
        worker_hint: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct RemoteDispatchRecord {
    pub task_id: Uuid,
    pub job_id: Uuid,
    pub run_id: Uuid,
    pub worker_hint: Option<String>,
    pub queued_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("local executor failed: {0}")]
    LocalExecution(String),

    #[error("remote dispatcher failed: {0}")]
    RemoteDispatch(String),

    #[error("remote submitter not configured")]
    MissingRemoteSubmitter,

    #[error("unsupported fleet task kind: {0}")]
    UnsupportedFleetTask(String),
}

/// Dispatches scheduled jobs either locally or to ff-mesh workers.
#[derive(Clone)]
pub struct Dispatcher {
    local_executor: LocalExecutor,
    remote_submitter: Option<RemoteSubmitter>,
    remote_in_flight: Arc<DashMap<Uuid, RemoteDispatchRecord>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            local_executor: default_local_executor(),
            remote_submitter: None,
            remote_in_flight: Arc::new(DashMap::new()),
        }
    }

    pub fn with_local_executor(mut self, executor: LocalExecutor) -> Self {
        self.local_executor = executor;
        self
    }

    pub fn with_remote_submitter(mut self, submitter: RemoteSubmitter) -> Self {
        self.remote_submitter = Some(submitter);
        self
    }

    pub async fn dispatch(
        &self,
        request: DispatchRequest,
    ) -> Result<DispatchOutcome, DispatchError> {
        match request.task {
            JobTask::LocalCommand {
                command,
                timeout_secs,
            } => {
                let started = Instant::now();
                let output = (self.local_executor)(command, timeout_secs)
                    .await
                    .map_err(|e| DispatchError::LocalExecution(e.to_string()))?;

                Ok(DispatchOutcome::LocalCompleted {
                    output,
                    duration_ms: started.elapsed().as_millis() as u64,
                })
            }
            JobTask::FleetTask {
                kind,
                payload,
                worker_hint,
            } => {
                let Some(remote_submitter) = &self.remote_submitter else {
                    return Err(DispatchError::MissingRemoteSubmitter);
                };

                let agent_task = fleet_task_to_agent_task(&kind, payload)
                    .map_err(|e| DispatchError::UnsupportedFleetTask(e.to_string()))?;

                let task_id = remote_submitter(agent_task, worker_hint.clone())
                    .await
                    .map_err(|e| DispatchError::RemoteDispatch(e.to_string()))?;

                self.remote_in_flight.insert(
                    task_id,
                    RemoteDispatchRecord {
                        task_id,
                        job_id: request.job_id,
                        run_id: request.run_id,
                        worker_hint: worker_hint.clone(),
                        queued_at: Utc::now(),
                    },
                );

                debug!(
                    task_id = %task_id,
                    job_id = %request.job_id,
                    run_id = %request.run_id,
                    attempt = request.attempt,
                    worker_hint = ?worker_hint,
                    "queued remote task"
                );

                Ok(DispatchOutcome::RemoteQueued {
                    task_id,
                    worker_hint,
                })
            }
        }
    }

    pub fn complete_remote_dispatch(&self, task_id: Uuid) -> Option<RemoteDispatchRecord> {
        self.remote_in_flight
            .remove(&task_id)
            .map(|(_, record)| record)
    }

    pub fn list_remote_in_flight(&self) -> Vec<RemoteDispatchRecord> {
        self.remote_in_flight
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

fn fleet_task_to_agent_task(kind: &str, payload: serde_json::Value) -> anyhow::Result<AgentTask> {
    let task_kind = match kind.trim().to_ascii_lowercase().as_str() {
        "shell_command" => {
            let command = payload
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("payload.command is required for shell_command"))?
                .to_string();
            let timeout_secs = payload.get("timeout_secs").and_then(|v| v.as_u64());

            AgentTaskKind::ShellCommand {
                command,
                timeout_secs,
            }
        }
        "model_inference" => {
            let prompt = payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("payload.prompt is required for model_inference"))?
                .to_string();
            let model = payload
                .get("model")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let max_tokens = payload
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            AgentTaskKind::ModelInference {
                model,
                prompt,
                max_tokens,
            }
        }
        other => {
            return Err(anyhow!(
                "unsupported fleet task kind '{}'; expected shell_command or model_inference",
                other
            ));
        }
    };

    Ok(AgentTask {
        id: Uuid::new_v4(),
        created_at: Utc::now(),
        kind: task_kind,
    })
}

fn default_local_executor() -> LocalExecutor {
    Arc::new(|command: String, timeout_secs: Option<u64>| {
        Box::pin(async move {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&command);

            let child = cmd.output();
            let output = if let Some(timeout) = timeout_secs {
                tokio::time::timeout(std::time::Duration::from_secs(timeout), child)
                    .await
                    .map_err(|_| anyhow!("command timed out after {}s", timeout))??
            } else {
                child.await?
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!("{}{}", stdout, stderr).trim().to_string();

            if output.status.success() {
                Ok(combined)
            } else {
                warn!(
                    status = ?output.status.code(),
                    command = %command,
                    "local job command exited with error"
                );
                Err(anyhow!(
                    "command failed with status {:?}: {}",
                    output.status.code(),
                    combined
                ))
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_dispatch_works() {
        let dispatcher = Dispatcher::new();
        let outcome = dispatcher
            .dispatch(DispatchRequest {
                job_id: Uuid::new_v4(),
                run_id: Uuid::new_v4(),
                task: JobTask::LocalCommand {
                    command: "echo hello".into(),
                    timeout_secs: Some(5),
                },
                attempt: 1,
            })
            .await
            .unwrap();

        match outcome {
            DispatchOutcome::LocalCompleted { output, .. } => {
                assert!(output.contains("hello"));
            }
            _ => panic!("expected local completion"),
        }
    }

    #[tokio::test]
    async fn fleet_dispatch_requires_submitter() {
        let dispatcher = Dispatcher::new();
        let err = dispatcher
            .dispatch(DispatchRequest {
                job_id: Uuid::new_v4(),
                run_id: Uuid::new_v4(),
                task: JobTask::FleetTask {
                    kind: "shell_command".into(),
                    payload: serde_json::json!({ "command": "echo hi" }),
                    worker_hint: None,
                },
                attempt: 1,
            })
            .await
            .unwrap_err();

        assert!(matches!(err, DispatchError::MissingRemoteSubmitter));
    }
}
