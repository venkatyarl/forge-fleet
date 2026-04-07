//! Parallel DAG executor.
//!
//! Walks the pipeline graph, executing steps whose dependencies are satisfied
//! concurrently (up to a parallelism limit). Handles retries, timeouts, and
//! cascading skips.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};

use crate::error::PipelineError;
use crate::graph::PipelineGraph;
use crate::registry::RustFnRegistry;
use crate::step::{StepId, StepKind, StepResult, StepStatus};

// ─── Executor Config ─────────────────────────────────────────────────────────

/// Configuration for the pipeline executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Maximum number of steps to run in parallel.
    pub max_parallelism: usize,
    /// Optional registry used by `StepKind::RustFn`.
    pub rust_fn_registry: Option<Arc<RustFnRegistry>>,
    /// Shared HTTP client for HTTP and LLM steps.
    pub http_client: reqwest::Client,
    /// Base URL for OpenAI-compatible chat completions endpoint.
    ///
    /// Examples:
    /// - `http://127.0.0.1:4000`
    /// - `http://127.0.0.1:4000/v1`
    /// - `http://127.0.0.1:4000/v1/chat/completions`
    pub llm_base_url: Option<String>,
    /// Optional bearer token for the LLM endpoint.
    pub llm_api_key: Option<String>,
    /// Default model for `StepKind::LlmPrompt` when the step does not specify one.
    pub llm_model: Option<String>,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_parallelism: 4,
            rust_fn_registry: None,
            http_client: reqwest::Client::new(),
            llm_base_url: None,
            llm_api_key: None,
            llm_model: None,
        }
    }
}

impl ExecutorConfig {
    /// Attach a Rust function registry.
    pub fn with_rust_fn_registry(mut self, registry: Arc<RustFnRegistry>) -> Self {
        self.rust_fn_registry = Some(registry);
        self
    }

    /// Set the LLM base URL.
    pub fn with_llm_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.llm_base_url = Some(base_url.into());
        self
    }

    /// Set the default LLM model.
    pub fn with_llm_model(mut self, model: impl Into<String>) -> Self {
        self.llm_model = Some(model.into());
        self
    }

    /// Set bearer API key used for LLM requests.
    pub fn with_llm_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.llm_api_key = Some(api_key.into());
        self
    }
}

#[derive(Clone)]
struct StepRuntime {
    rust_fn_registry: Option<Arc<RustFnRegistry>>,
    http_client: reqwest::Client,
    llm_chat_completions_url: String,
    llm_api_key: Option<String>,
    llm_model: Option<String>,
}

impl StepRuntime {
    fn from_config(config: &ExecutorConfig) -> Self {
        let llm_base_url = config
            .llm_base_url
            .clone()
            .or_else(|| std::env::var("FF_PIPELINE_LLM_BASE_URL").ok())
            .unwrap_or_else(|| "http://127.0.0.1:4000".to_string());

        let llm_api_key = config
            .llm_api_key
            .clone()
            .or_else(|| std::env::var("FF_PIPELINE_LLM_API_KEY").ok());

        let llm_model = config
            .llm_model
            .clone()
            .or_else(|| std::env::var("FF_PIPELINE_LLM_MODEL").ok());

        Self {
            rust_fn_registry: config.rust_fn_registry.clone(),
            http_client: config.http_client.clone(),
            llm_chat_completions_url: normalize_chat_completions_url(&llm_base_url),
            llm_api_key,
            llm_model,
        }
    }
}

fn normalize_chat_completions_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

// ─── Progress Callback ──────────────────────────────────────────────────────

/// Events emitted by the executor for progress tracking.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// A step started executing.
    StepStarted { step_id: StepId, attempt: u32 },
    /// A step completed (successfully or not).
    StepCompleted { result: StepResult },
    /// A step was skipped due to dependency failure.
    StepSkipped { step_id: StepId, reason: String },
    /// The entire pipeline finished.
    PipelineFinished {
        success: bool,
        total_steps: usize,
        succeeded: usize,
        failed: usize,
        skipped: usize,
    },
}

// ─── Pipeline Run Result ─────────────────────────────────────────────────────

/// Summary of a complete pipeline execution.
#[derive(Debug, Clone)]
pub struct PipelineRunResult {
    pub success: bool,
    pub results: HashMap<StepId, StepResult>,
    pub total_duration_ms: u64,
}

// ─── Executor ────────────────────────────────────────────────────────────────

/// Execute a pipeline graph respecting dependencies and parallelism limits.
pub async fn execute(
    graph: &PipelineGraph,
    config: ExecutorConfig,
    event_tx: Option<mpsc::UnboundedSender<PipelineEvent>>,
) -> Result<PipelineRunResult, PipelineError> {
    if graph.is_empty() {
        return Err(PipelineError::EmptyPipeline);
    }

    // Validate the graph is a DAG.
    let _topo = graph.topological_sort()?;

    let start = Instant::now();
    let semaphore = Arc::new(Semaphore::new(config.max_parallelism));
    let runtime = Arc::new(StepRuntime::from_config(&config));

    let mut statuses: HashMap<StepId, StepStatus> = HashMap::new();
    let mut results: HashMap<StepId, StepResult> = HashMap::new();

    // Channel for step completions.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<StepResult>();

    let mut in_flight: usize = 0;

    loop {
        // 1. Mark skippable steps.
        let skippable = graph.skippable_steps(&statuses);
        for id in skippable {
            let reason = format!("dependency of '{}' failed", id);
            statuses.insert(id.clone(), StepStatus::Skipped);
            let result = StepResult::skipped(id.clone(), reason.clone());
            results.insert(id.clone(), result);
            if let Some(tx) = &event_tx {
                let _ = tx.send(PipelineEvent::StepSkipped {
                    step_id: id,
                    reason,
                });
            }
        }

        // 2. Find and launch ready steps.
        let ready = graph.ready_steps(&statuses);
        for id in ready {
            statuses.insert(id.clone(), StepStatus::Running);
            in_flight += 1;

            let step = graph.get_step(&id).expect("ready step must exist").clone();
            let sem = semaphore.clone();
            let tx = done_tx.clone();
            let evt_tx = event_tx.clone();
            let runtime = runtime.clone();

            tokio::spawn(async move {
                // Acquire semaphore permit (limits parallelism).
                let _permit = sem.acquire().await.expect("semaphore closed");

                let max_attempts = step.config.retries + 1;
                let mut last_result = None;

                for attempt in 1..=max_attempts {
                    if let Some(etx) = &evt_tx {
                        let _ = etx.send(PipelineEvent::StepStarted {
                            step_id: step.id.clone(),
                            attempt,
                        });
                    }

                    let step_start = Instant::now();

                    let outcome = tokio::time::timeout(
                        step.config.timeout,
                        execute_step_kind(&step.kind, runtime.as_ref()),
                    )
                    .await;

                    let elapsed_ms = step_start.elapsed().as_millis() as u64;

                    match outcome {
                        Ok(Ok(output)) => {
                            let r =
                                StepResult::success(step.id.clone(), output, attempt, elapsed_ms);
                            last_result = Some(r);
                            break;
                        }
                        Ok(Err(err)) => {
                            warn!(step = %step.id, attempt, error = %err, "step failed");
                            last_result = Some(StepResult::failure(
                                step.id.clone(),
                                err.to_string(),
                                String::new(),
                                attempt,
                                elapsed_ms,
                            ));
                            if attempt < max_attempts {
                                tokio::time::sleep(step.config.retry_delay).await;
                            }
                        }
                        Err(_elapsed) => {
                            warn!(step = %step.id, attempt, "step timed out");
                            last_result =
                                Some(StepResult::timed_out(step.id.clone(), attempt, elapsed_ms));
                            // Don't retry on timeout.
                            break;
                        }
                    }
                }

                let result = last_result.expect("at least one execution attempt");
                let _ = tx.send(result);
            });
        }

        // 3. If nothing in flight and nothing ready, we're done.
        if in_flight == 0 {
            break;
        }

        // 4. Wait for a step to complete.
        if let Some(result) = done_rx.recv().await {
            in_flight -= 1;
            let final_status = result.status;
            statuses.insert(result.step_id.clone(), final_status);

            if let Some(tx) = &event_tx {
                let _ = tx.send(PipelineEvent::StepCompleted {
                    result: result.clone(),
                });
            }

            debug!(
                step = %result.step_id,
                status = ?final_status,
                attempts = result.attempts,
                "step finished"
            );

            results.insert(result.step_id.clone(), result);
        }
    }

    // Build summary.
    let succeeded = statuses
        .values()
        .filter(|s| **s == StepStatus::Succeeded)
        .count();
    let failed = statuses
        .values()
        .filter(|s| matches!(s, StepStatus::Failed | StepStatus::TimedOut))
        .count();
    let skipped = statuses
        .values()
        .filter(|s| **s == StepStatus::Skipped)
        .count();
    let success = failed == 0;

    if let Some(tx) = &event_tx {
        let _ = tx.send(PipelineEvent::PipelineFinished {
            success,
            total_steps: graph.len(),
            succeeded,
            failed,
            skipped,
        });
    }

    info!(
        success,
        succeeded,
        failed,
        skipped,
        duration_ms = start.elapsed().as_millis() as u64,
        "pipeline finished"
    );

    Ok(PipelineRunResult {
        success,
        results,
        total_duration_ms: start.elapsed().as_millis() as u64,
    })
}

// ─── Step Kind Execution ─────────────────────────────────────────────────────

/// Execute a single step kind and return its output.
async fn execute_step_kind(
    kind: &StepKind,
    runtime: &StepRuntime,
) -> Result<String, PipelineError> {
    match kind {
        StepKind::Shell { command, cwd, env } => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let output = cmd.output().await.map_err(PipelineError::Io)?;

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if output.status.success() {
                Ok(stdout)
            } else {
                let msg = if stderr.is_empty() {
                    format!("exit code: {:?}", output.status.code())
                } else {
                    stderr
                };
                Err(PipelineError::StepExecution(msg))
            }
        }

        StepKind::RustFn { name, args } => {
            let registry = runtime
                .rust_fn_registry
                .as_ref()
                .ok_or(PipelineError::RustFnRegistryMissing)?;

            registry.call(name, args.clone()).await
        }

        StepKind::HttpCall {
            method,
            url,
            headers,
            body,
        } => {
            let parsed_method: reqwest::Method = method.to_uppercase().parse().map_err(|e| {
                PipelineError::StepExecution(format!("invalid HTTP method '{method}': {e}"))
            })?;

            let mut request = runtime.http_client.request(parsed_method, url);

            if let Some(headers) = headers {
                for (name, value) in headers {
                    let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                        PipelineError::StepExecution(format!("invalid header name '{name}': {e}"))
                    })?;
                    let header_value = HeaderValue::from_str(value).map_err(|e| {
                        PipelineError::StepExecution(format!(
                            "invalid header value for '{name}': {e}"
                        ))
                    })?;
                    request = request.header(header_name, header_value);
                }
            }

            if let Some(body) = body {
                request = request.body(body.clone());
            }

            let response = request
                .send()
                .await
                .map_err(|e| PipelineError::HttpRequest(e.to_string()))?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| PipelineError::HttpRequest(e.to_string()))?;

            if status.is_success() {
                Ok(text)
            } else {
                Err(PipelineError::HttpStatus {
                    status: status.as_u16(),
                    body: text,
                })
            }
        }

        StepKind::LlmPrompt {
            prompt,
            model,
            max_tokens,
        } => {
            let selected_model = model
                .clone()
                .or_else(|| runtime.llm_model.clone())
                .unwrap_or_else(|| "default".to_string());

            let mut payload = json!({
                "model": selected_model,
                "messages": [{"role": "user", "content": prompt}],
                "stream": false,
            });

            if let Some(max_tokens) = max_tokens {
                payload["max_tokens"] = json!(max_tokens);
            }

            let mut request = runtime
                .http_client
                .post(&runtime.llm_chat_completions_url)
                .json(&payload);

            if let Some(api_key) = &runtime.llm_api_key {
                request = request.bearer_auth(api_key);
            }

            let response = request
                .send()
                .await
                .map_err(|e| PipelineError::LlmRequest(e.to_string()))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|e| PipelineError::LlmRequest(e.to_string()))?;

            if !status.is_success() {
                return Err(PipelineError::LlmRequest(format!(
                    "status {}: {}",
                    status.as_u16(),
                    body
                )));
            }

            let json: Value = serde_json::from_str(&body).map_err(|e| {
                PipelineError::LlmResponse(format!("invalid JSON response: {e}; body: {body}"))
            })?;

            extract_llm_text(&json).ok_or_else(|| {
                PipelineError::LlmResponse(format!("missing assistant content in response: {body}"))
            })
        }

        StepKind::Noop => Ok("noop".to_string()),
    }
}

fn extract_llm_text(response: &Value) -> Option<String> {
    if let Some(content) = response.pointer("/choices/0/message/content") {
        return extract_content_value(content);
    }

    response
        .pointer("/choices/0/text")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn extract_content_value(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }

    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }

    if let Some(items) = content.as_array() {
        let mut joined = String::new();
        for item in items {
            if let Some(s) = item.as_str() {
                joined.push_str(s);
                continue;
            }
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                joined.push_str(text);
                continue;
            }
            if let Some(text) = item.get("content").and_then(Value::as_str) {
                joined.push_str(text);
            }
        }

        if !joined.is_empty() {
            return Some(joined);
        }
    }

    None
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::PipelineGraph;
    use crate::step::Step;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    fn build_linear_pipeline() -> PipelineGraph {
        let mut g = PipelineGraph::new();
        g.add_step(Step::shell("check", "Cargo check", "echo check_ok"))
            .unwrap();
        g.add_step(Step::shell("build", "Cargo build", "echo build_ok"))
            .unwrap();
        g.add_step(Step::shell("test", "Cargo test", "echo test_ok"))
            .unwrap();
        g.add_dependency(&"build".into(), &"check".into()).unwrap();
        g.add_dependency(&"test".into(), &"build".into()).unwrap();
        g
    }

    async fn spawn_single_request_server(
        status_line: &str,
        content_type: &str,
        response_body: String,
    ) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();

        let status_line = status_line.to_string();
        let content_type = content_type.to_string();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0_u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = tx.send(req);

                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn execute_linear_pipeline() {
        let graph = build_linear_pipeline();
        let result = execute(&graph, ExecutorConfig::default(), None)
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.results.len(), 3);
        for r in result.results.values() {
            assert_eq!(r.status, StepStatus::Succeeded);
        }
    }

    #[tokio::test]
    async fn execute_parallel_diamond() {
        let mut g = PipelineGraph::new();
        g.add_step(Step::shell("a", "Start", "echo a")).unwrap();
        g.add_step(Step::shell("b", "Left", "echo b")).unwrap();
        g.add_step(Step::shell("c", "Right", "echo c")).unwrap();
        g.add_step(Step::shell("d", "Join", "echo d")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();
        g.add_dependency(&"c".into(), &"a".into()).unwrap();
        g.add_dependency(&"d".into(), &"b".into()).unwrap();
        g.add_dependency(&"d".into(), &"c".into()).unwrap();

        let result = execute(
            &g,
            ExecutorConfig {
                max_parallelism: 2,
                ..ExecutorConfig::default()
            },
            None,
        )
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(result.results.len(), 4);
    }

    #[tokio::test]
    async fn execute_with_failure_skips_dependents() {
        let mut g = PipelineGraph::new();
        g.add_step(Step::shell("a", "Fail", "exit 1")).unwrap();
        g.add_step(Step::shell("b", "Dependent", "echo b")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();

        let result = execute(&g, ExecutorConfig::default(), None).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.results[&StepId::new("a")].status, StepStatus::Failed);
        assert_eq!(
            result.results[&StepId::new("b")].status,
            StepStatus::Skipped
        );
    }

    #[tokio::test]
    async fn execute_with_retries() {
        let mut g = PipelineGraph::new();
        // This will fail — but we can verify retries are attempted.
        let step =
            Step::shell("flaky", "Flaky step", "exit 1").with_retries(2, Duration::from_millis(10));
        g.add_step(step).unwrap();

        let result = execute(&g, ExecutorConfig::default(), None).await.unwrap();

        assert!(!result.success);
        let r = &result.results[&StepId::new("flaky")];
        assert_eq!(r.status, StepStatus::Failed);
        assert_eq!(r.attempts, 3); // 1 initial + 2 retries
    }

    #[tokio::test]
    async fn execute_noop_pipeline() {
        let mut g = PipelineGraph::new();
        g.add_step(Step::noop("barrier", "Barrier")).unwrap();

        let result = execute(&g, ExecutorConfig::default(), None).await.unwrap();

        assert!(result.success);
        assert_eq!(result.results[&StepId::new("barrier")].output, "noop");
    }

    #[tokio::test]
    async fn execute_empty_pipeline_errors() {
        let g = PipelineGraph::new();
        let err = execute(&g, ExecutorConfig::default(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, PipelineError::EmptyPipeline));
    }

    #[tokio::test]
    async fn execute_events_emitted() {
        let mut g = PipelineGraph::new();
        g.add_step(Step::shell("a", "Echo", "echo hello")).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _result = execute(&g, ExecutorConfig::default(), Some(tx))
            .await
            .unwrap();

        let mut saw_started = false;
        let mut saw_completed = false;
        let mut saw_finished = false;

        while let Ok(event) = rx.try_recv() {
            match event {
                PipelineEvent::StepStarted { .. } => saw_started = true,
                PipelineEvent::StepCompleted { .. } => saw_completed = true,
                PipelineEvent::PipelineFinished { .. } => saw_finished = true,
                _ => {}
            }
        }

        assert!(saw_started);
        assert!(saw_completed);
        assert!(saw_finished);
    }

    #[tokio::test]
    async fn execute_allow_failure_continues() {
        let mut g = PipelineGraph::new();
        let step_a = Step::shell("a", "Allowed fail", "exit 1").allow_failure();
        g.add_step(step_a).unwrap();
        g.add_step(Step::shell("b", "After", "echo ok")).unwrap();
        g.add_dependency(&"b".into(), &"a".into()).unwrap();

        let result = execute(&g, ExecutorConfig::default(), None).await.unwrap();

        // "a" failed but allowed, so "b" should still run.
        assert_eq!(result.results[&StepId::new("a")].status, StepStatus::Failed);
        assert_eq!(
            result.results[&StepId::new("b")].status,
            StepStatus::Succeeded
        );
        // Pipeline is still "failed" because a step failed.
        assert!(!result.success);
    }

    #[tokio::test]
    async fn execute_rust_fn_step_via_registry() {
        let registry = Arc::new(RustFnRegistry::new());
        registry
            .register("echo_json", |args| async move {
                Ok(format!("fn_output:{}", args.unwrap_or_default()))
            })
            .await;

        let mut g = PipelineGraph::new();
        g.add_step(Step::new(
            "fn",
            "Rust Function",
            StepKind::RustFn {
                name: "echo_json".to_string(),
                args: Some("{\"hello\":\"world\"}".to_string()),
            },
        ))
        .unwrap();

        let config = ExecutorConfig::default().with_rust_fn_registry(registry);
        let result = execute(&g, config, None).await.unwrap();

        assert!(result.success);
        assert_eq!(
            result.results[&StepId::new("fn")].output,
            "fn_output:{\"hello\":\"world\"}"
        );
    }

    #[tokio::test]
    async fn execute_http_call_step_real_request() {
        let (base_url, request_rx) =
            spawn_single_request_server("200 OK", "text/plain", "pong".to_string()).await;

        let mut g = PipelineGraph::new();
        g.add_step(Step::new(
            "http",
            "HTTP Call",
            StepKind::HttpCall {
                method: "POST".to_string(),
                url: format!("{base_url}/echo"),
                headers: Some(vec![
                    ("Content-Type".to_string(), "text/plain".to_string()),
                    ("X-Test".to_string(), "1".to_string()),
                ]),
                body: Some("ping".to_string()),
            },
        ))
        .unwrap();

        let result = execute(&g, ExecutorConfig::default(), None).await.unwrap();

        assert!(result.success);
        assert_eq!(result.results[&StepId::new("http")].output, "pong");

        let raw_request = request_rx.await.unwrap();
        assert!(raw_request.contains("POST /echo HTTP/1.1"));

        let raw_request_lower = raw_request.to_ascii_lowercase();
        assert!(raw_request_lower.contains("x-test: 1"));
        assert!(raw_request.contains("ping"));
    }

    #[tokio::test]
    async fn execute_llm_prompt_step_openai_compatible() {
        let llm_response = json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": "pipeline-model",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "hello from llm"
                    },
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let (base_url, request_rx) =
            spawn_single_request_server("200 OK", "application/json", llm_response).await;

        let mut g = PipelineGraph::new();
        g.add_step(Step::new(
            "llm",
            "LLM Prompt",
            StepKind::LlmPrompt {
                prompt: "Say hello".to_string(),
                model: None,
                max_tokens: Some(32),
            },
        ))
        .unwrap();

        let config = ExecutorConfig::default()
            .with_llm_base_url(base_url)
            .with_llm_model("pipeline-model");

        let result = execute(&g, config, None).await.unwrap();

        assert!(result.success);
        assert_eq!(result.results[&StepId::new("llm")].output, "hello from llm");

        let raw_request = request_rx.await.unwrap();
        assert!(raw_request.contains("POST /v1/chat/completions HTTP/1.1"));
        assert!(raw_request.contains("\"model\":\"pipeline-model\""));
        assert!(raw_request.contains("\"Say hello\""));
    }
}
