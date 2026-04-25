//! Multi-agent orchestration — coordinate parallel agents across fleet nodes.
//!
//! Enables running N independent coding agents simultaneously, each on a
//! different fleet node, with coordination, event streaming, and result aggregation.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::agent_loop::{AgentEvent, AgentOutcome, AgentSession, AgentSessionConfig};

/// A task for parallel agent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    /// Unique task ID.
    pub id: String,
    /// Prompt for the agent.
    pub prompt: String,
    /// Which fleet endpoint to use.
    pub llm_base_url: String,
    /// Optional model override.
    pub model: Option<String>,
    /// Working directory.
    pub working_dir: PathBuf,
    /// Max turns for this task.
    pub max_turns: u32,
}

/// Result of a completed agent task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTaskResult {
    pub task_id: String,
    pub status: TaskStatus,
    pub output: String,
    pub events: Vec<AgentEvent>,
    pub duration_ms: u64,
    pub turn_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Completed,
    MaxTurns,
    Cancelled,
    Failed,
}

/// Orchestrate multiple agents working in parallel.
pub struct MultiAgentOrchestrator {
    cancel_token: CancellationToken,
}

impl MultiAgentOrchestrator {
    pub fn new() -> Self {
        Self {
            cancel_token: CancellationToken::new(),
        }
    }

    /// Run multiple agent tasks in parallel and collect results.
    pub async fn run_parallel(
        &self,
        tasks: Vec<AgentTask>,
        event_tx: Option<mpsc::UnboundedSender<OrchestratorEvent>>,
    ) -> Vec<AgentTaskResult> {
        let task_count = tasks.len();
        info!(count = task_count, "starting parallel agent execution");

        emit_orch(
            &event_tx,
            OrchestratorEvent::Started {
                task_count,
                task_ids: tasks.iter().map(|t| t.id.clone()).collect(),
            },
        );

        let mut handles = Vec::new();
        let cancel = self.cancel_token.clone();

        for task in tasks {
            let event_tx = event_tx.clone();
            let cancel = cancel.clone();

            let handle =
                tokio::spawn(async move { run_single_agent_task(task, event_tx, cancel).await });

            handles.push(handle);
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    results.push(AgentTaskResult {
                        task_id: "unknown".into(),
                        status: TaskStatus::Failed,
                        output: format!("Task panicked: {e}"),
                        events: Vec::new(),
                        duration_ms: 0,
                        turn_count: 0,
                    });
                }
            }
        }

        let completed = results
            .iter()
            .filter(|r| r.status == TaskStatus::Completed)
            .count();
        let failed = results
            .iter()
            .filter(|r| r.status == TaskStatus::Failed)
            .count();

        emit_orch(
            &event_tx,
            OrchestratorEvent::AllCompleted {
                total: results.len(),
                completed,
                failed,
            },
        );

        info!(
            total = results.len(),
            completed, failed, "parallel execution complete"
        );
        results
    }

    /// Cancel all running tasks.
    pub fn cancel_all(&self) {
        self.cancel_token.cancel();
    }
}

impl Default for MultiAgentOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_single_agent_task(
    task: AgentTask,
    orch_event_tx: Option<mpsc::UnboundedSender<OrchestratorEvent>>,
    cancel: CancellationToken,
) -> AgentTaskResult {
    let start = std::time::Instant::now();
    let task_id = task.id.clone();

    emit_orch(
        &orch_event_tx,
        OrchestratorEvent::TaskStarted {
            task_id: task_id.clone(),
            llm_endpoint: task.llm_base_url.clone(),
        },
    );

    let config = AgentSessionConfig {
        model: task.model.unwrap_or_else(|| "auto".into()),
        llm_base_url: task.llm_base_url,
        working_dir: task.working_dir,
        system_prompt: None,
        max_turns: task.max_turns,
        auto_save: false,
        ..Default::default()
    };

    let mut session = AgentSession::new(config);

    // Collect events
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let events_collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        events
    });

    let outcome = tokio::select! {
        _ = cancel.cancelled() => AgentOutcome::Cancelled,
        result = session.run(&task.prompt, Some(event_tx)) => result,
    };

    drop(session);
    let events = events_collector.await.unwrap_or_default();
    let duration_ms = start.elapsed().as_millis() as u64;

    let (status, output) = match outcome {
        AgentOutcome::EndTurn { final_message } => (TaskStatus::Completed, final_message),
        AgentOutcome::MaxTurns { partial_message } => (TaskStatus::MaxTurns, partial_message),
        AgentOutcome::Cancelled => (TaskStatus::Cancelled, "Cancelled".into()),
        AgentOutcome::Error(e) => (TaskStatus::Failed, e),
    };

    emit_orch(
        &orch_event_tx,
        OrchestratorEvent::TaskCompleted {
            task_id: task_id.clone(),
            status,
            duration_ms,
        },
    );

    AgentTaskResult {
        task_id,
        status,
        output,
        events,
        duration_ms,
        turn_count: 0,
    }
}

/// Events from the multi-agent orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "orchestrator_event")]
pub enum OrchestratorEvent {
    #[serde(rename = "started")]
    Started {
        task_count: usize,
        task_ids: Vec<String>,
    },
    #[serde(rename = "task_started")]
    TaskStarted {
        task_id: String,
        llm_endpoint: String,
    },
    #[serde(rename = "task_completed")]
    TaskCompleted {
        task_id: String,
        status: TaskStatus,
        duration_ms: u64,
    },
    #[serde(rename = "all_completed")]
    AllCompleted {
        total: usize,
        completed: usize,
        failed: usize,
    },
}

fn emit_orch(tx: &Option<mpsc::UnboundedSender<OrchestratorEvent>>, event: OrchestratorEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

/// Event stream — append-only log for agent replay and debugging.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventStream {
    pub events: Vec<TimestampedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedEvent {
    pub timestamp: DateTime<Utc>,
    pub session_id: String,
    pub event: AgentEvent,
}

impl EventStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&mut self, session_id: &str, event: AgentEvent) {
        self.events.push(TimestampedEvent {
            timestamp: Utc::now(),
            session_id: session_id.to_string(),
            event,
        });
    }

    pub fn events_for_session(&self, session_id: &str) -> Vec<&TimestampedEvent> {
        self.events
            .iter()
            .filter(|e| e.session_id == session_id)
            .collect()
    }

    pub fn replay_from(&self, index: usize) -> &[TimestampedEvent] {
        if index < self.events.len() {
            &self.events[index..]
        } else {
            &[]
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Test-driven verification pipeline — code on one node, test on another, verify on a third.
#[derive(Debug, Clone)]
pub struct VerificationPipeline {
    pub code_endpoint: String,
    pub test_endpoint: String,
    pub verify_endpoint: String,
}

impl VerificationPipeline {
    /// Run the full pipeline: code → test → verify.
    pub async fn run(&self, prompt: &str, working_dir: &PathBuf) -> VerificationResult {
        // Step 1: Generate code on the code endpoint
        let code_config = AgentSessionConfig {
            llm_base_url: self.code_endpoint.clone(),
            working_dir: working_dir.clone(),
            system_prompt: Some(
                "You are a coding agent. Write code to accomplish the task. Run tests after."
                    .into(),
            ),
            max_turns: 15,
            auto_save: false,
            ..Default::default()
        };

        let mut code_session = AgentSession::new(code_config);
        let code_outcome = code_session.run(prompt, None).await;

        let code_output = match &code_outcome {
            AgentOutcome::EndTurn { final_message } => final_message.clone(),
            AgentOutcome::MaxTurns { partial_message } => partial_message.clone(),
            AgentOutcome::Error(e) => {
                return VerificationResult {
                    passed: false,
                    code_output: e.clone(),
                    test_output: String::new(),
                    verify_output: "Skipped — code generation failed".into(),
                };
            }
            AgentOutcome::Cancelled => {
                return VerificationResult {
                    passed: false,
                    code_output: "Cancelled".into(),
                    test_output: String::new(),
                    verify_output: "Skipped".into(),
                };
            }
        };

        // Step 2: Run tests on the test endpoint
        let test_config = AgentSessionConfig {
            llm_base_url: self.test_endpoint.clone(),
            working_dir: working_dir.clone(),
            system_prompt: Some("You are a test runner. Run the project's test suite and report results. Use Bash to run tests.".into()),
            max_turns: 5,
            auto_save: false,
            ..Default::default()
        };

        let mut test_session = AgentSession::new(test_config);
        let test_outcome = test_session
            .run("Run the test suite and report pass/fail", None)
            .await;

        let test_output = match &test_outcome {
            AgentOutcome::EndTurn { final_message } => final_message.clone(),
            other => format!("{other:?}"),
        };

        // Step 3: Verify on the verify endpoint
        let verify_config = AgentSessionConfig {
            llm_base_url: self.verify_endpoint.clone(),
            working_dir: working_dir.clone(),
            system_prompt: Some("You are a code reviewer. Review the recent changes and test results. Report whether the implementation is correct.".into()),
            max_turns: 3,
            auto_save: false,
            ..Default::default()
        };

        let mut verify_session = AgentSession::new(verify_config);
        let verify_prompt = format!(
            "Code output:\n{}\n\nTest output:\n{}\n\nDoes the implementation look correct?",
            &code_output[..code_output.len().min(2000)],
            &test_output[..test_output.len().min(2000)]
        );
        let verify_outcome = verify_session.run(&verify_prompt, None).await;

        let verify_output = match &verify_outcome {
            AgentOutcome::EndTurn { final_message } => final_message.clone(),
            other => format!("{other:?}"),
        };

        let passed = verify_output.to_ascii_lowercase().contains("correct")
            || verify_output.to_ascii_lowercase().contains("pass")
            || verify_output.to_ascii_lowercase().contains("looks good");

        VerificationResult {
            passed,
            code_output,
            test_output,
            verify_output,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub passed: bool,
    pub code_output: String,
    pub test_output: String,
    pub verify_output: String,
}
