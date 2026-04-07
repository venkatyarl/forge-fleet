use crate::{leader::LeaderClient, state::SharedState};
use chrono::Utc;
use ff_core::{AgentTask, AgentTaskKind, TaskResult};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::{process::Command, sync::mpsc};
use tracing::{error, info, warn};

pub async fn run_task_executor(
    state: SharedState,
    mut task_rx: mpsc::Receiver<AgentTask>,
    leader: LeaderClient,
    runtime_url: String,
) {
    while let Some(task) = task_rx.recv().await {
        {
            let mut locked = state.write().await;
            locked.active_tasks.insert(task.id, task.clone());
        }

        let result = execute_task(&state, task.clone(), &runtime_url).await;

        {
            let mut locked = state.write().await;
            locked.active_tasks.remove(&task.id);
        }

        if let Err(err) = leader.report_task_result(&result).await {
            warn!(error = %err, task_id = %result.task_id, "failed to report task result to leader");
        }

        info!(task_id = %result.task_id, success = result.success, "task completed");
    }
}

pub async fn run_task_poller(
    task_tx: mpsc::Sender<AgentTask>,
    leader: LeaderClient,
    poll_interval_secs: u64,
) {
    let interval = Duration::from_secs(poll_interval_secs.max(2));

    loop {
        match leader.fetch_task().await {
            Ok(Some(task)) => {
                if let Err(err) = task_tx.send(task).await {
                    error!(error = %err, "task queue send failed");
                }
            }
            Ok(None) => {}
            Err(err) => warn!(error = %err, "task polling failed"),
        }

        tokio::time::sleep(interval).await;
    }
}

async fn execute_task(state: &SharedState, task: AgentTask, runtime_url: &str) -> TaskResult {
    let started = Instant::now();

    let (success, output) = match task.kind.clone() {
        AgentTaskKind::ShellCommand {
            command,
            timeout_secs,
        } => execute_shell_command(state, &command, timeout_secs).await,
        AgentTaskKind::ModelInference {
            model,
            prompt,
            max_tokens,
        } => execute_model_inference(state, runtime_url, model, prompt, max_tokens).await,
    };

    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;

    TaskResult {
        task_id: task.id,
        success,
        output,
        completed_at: Utc::now(),
        duration_ms,
    }
}

async fn execute_shell_command(
    state: &SharedState,
    command: &str,
    timeout_secs: Option<u64>,
) -> (bool, String) {
    let should_yield = {
        let locked = state.read().await;
        locked.yield_resources
    };

    if should_yield && looks_compute_heavy(command) {
        return (
            false,
            "task deferred: node is in user-interactive/protected mode".to_string(),
        );
    }

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(600));

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };

    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.arg("-lc").arg(command);
        c
    };

    cmd.kill_on_drop(true);

    let future = cmd.output();
    let output = tokio::time::timeout(timeout, future).await;

    match output {
        Ok(Ok(out)) => {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
            }

            (out.status.success(), combined.trim().to_string())
        }
        Ok(Err(err)) => (false, format!("shell execution failed: {err}")),
        Err(_) => (
            false,
            format!("shell execution timed out after {}s", timeout.as_secs()),
        ),
    }
}

async fn execute_model_inference(
    state: &SharedState,
    runtime_url: &str,
    model: Option<String>,
    prompt: String,
    max_tokens: Option<u32>,
) -> (bool, String) {
    let should_yield = {
        let locked = state.read().await;
        locked.yield_resources
    };

    if should_yield {
        return (
            false,
            "inference deferred: node is yielding resources to active user".to_string(),
        );
    }

    if let Some(model_name) = model.clone() {
        let mut locked = state.write().await;
        if !locked.running_models.iter().any(|m| m == &model_name) {
            locked.running_models.push(model_name);
        }
    }

    let client = reqwest::Client::new();
    let body = json!({
        "model": model.clone().unwrap_or_else(|| "local-model".to_string()),
        "prompt": prompt,
        "max_tokens": max_tokens.unwrap_or(512),
    });

    let endpoints = ["/v1/completions", "/inference"];

    let mut success = false;
    let mut output = "runtime unreachable".to_string();

    for ep in endpoints {
        let url = format!("{}{}", runtime_url, ep);
        match client.post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                let text = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "<empty response>".to_string());
                success = true;
                output = text;
                break;
            }
            Ok(resp) => {
                output = format!("runtime error {} at {}", resp.status(), url);
            }
            Err(err) => {
                output = format!("runtime request failed at {}: {}", url, err);
            }
        }
    }

    (success, output)
}

fn looks_compute_heavy(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    [
        "train",
        "finetune",
        "fine-tune",
        "benchmark",
        "docker build",
        "cargo build --release",
        "vllm",
        "llama-server",
    ]
    .iter()
    .any(|needle| c.contains(needle))
}
