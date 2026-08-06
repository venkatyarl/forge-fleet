//! Parallel DAG executor.
//!
//! Walks the pipeline graph, executing steps whose dependencies are satisfied
//! concurrently (up to a parallelism limit). Handles retries, timeouts, and
//! cascading skips.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use ff_core::llm_completion_policy::{
    CompletionBudget, CompletionValidationError, LEGACY_DEFAULT_COMPLETION_TOKENS, WorkloadClass,
    apply_completion_policy, validate_completion_response,
};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};

use crate::error::PipelineError;
use crate::graph::PipelineGraph;
use crate::registry::RustFnRegistry;
use crate::step::{StepId, StepKind, StepResult, StepStatus};

// ─── Executor Config ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HttpClientTimeoutPolicy {
    connect: std::time::Duration,
    total: Option<std::time::Duration>,
}

const DEFAULT_HTTP_CLIENT_TIMEOUTS: HttpClientTimeoutPolicy = HttpClientTimeoutPolicy {
    connect: std::time::Duration::from_secs(10),
    // The enclosing Step timeout is the sole total request deadline.
    total: None,
};

fn build_http_client(policy: HttpClientTimeoutPolicy) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().connect_timeout(policy.connect);
    if let Some(total) = policy.total {
        builder = builder.timeout(total);
    }
    builder.build().expect("build reqwest client")
}

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
    /// Defaults to the local ForgeFleet gateway at `http://127.0.0.1:51002`
    /// (canonical 5-digit port, registered in `port_registry`). The gateway
    /// routes the request to the appropriate fleet backend internally.
    ///
    /// Examples:
    /// - `http://127.0.0.1:51002`
    /// - `http://127.0.0.1:51002/v1`
    /// - `http://127.0.0.1:51002/v1/chat/completions`
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
            http_client: build_http_client(DEFAULT_HTTP_CLIENT_TIMEOUTS),
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
    llm_base_url: String,
    llm_api_key: Option<String>,
    llm_model: Option<String>,
}

impl StepRuntime {
    fn from_config(config: &ExecutorConfig) -> Self {
        let llm_base_url = config
            .llm_base_url
            .clone()
            .or_else(|| std::env::var("FF_PIPELINE_LLM_BASE_URL").ok())
            // Canonical default = ForgeFleet gateway on the local host. 5-digit
            // port registered in port_registry to `service='forgefleetd'`. Prior
            // default `:4000` was LiteLLM's legacy port that nothing on the fleet
            // serves — it collided with Obsidian's local REST plugin in 2026-05.
            .unwrap_or_else(|| "http://127.0.0.1:51002".to_string());

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
            llm_base_url,
            llm_api_key,
            llm_model,
        }
    }
}

fn resolve_llm_chat_completions_url(
    step_endpoint: Option<&str>,
    global_endpoint: &str,
) -> Result<String, PipelineError> {
    // `Some` is an explicit authority choice. Even an empty or malformed value
    // must fail closed instead of silently sending the prompt elsewhere.
    normalize_llm_chat_completions_url(step_endpoint.unwrap_or(global_endpoint))
}

fn normalize_llm_chat_completions_url(endpoint: &str) -> Result<String, PipelineError> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(PipelineError::LlmRequest(
            "invalid LLM endpoint: endpoint is empty".to_string(),
        ));
    }

    let mut url = reqwest::Url::parse(endpoint).map_err(|_| {
        PipelineError::LlmRequest("invalid LLM endpoint: expected an absolute URL".to_string())
    })?;

    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(PipelineError::LlmRequest(
            "invalid LLM endpoint: only absolute http(s) URLs are allowed".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PipelineError::LlmRequest(
            "invalid LLM endpoint: credentials are not allowed in the URL".to_string(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(PipelineError::LlmRequest(
            "invalid LLM endpoint: query strings and fragments are not allowed".to_string(),
        ));
    }

    let path = url.path().trim_end_matches('/');
    let normalized_path = if path.ends_with("/v1/chat/completions") {
        path.to_string()
    } else if path.ends_with("/v1") {
        format!("{path}/chat/completions")
    } else {
        format!("{path}/v1/chat/completions")
    };
    url.set_path(&normalized_path);
    Ok(url.to_string())
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
    event_tx: Option<mpsc::Sender<PipelineEvent>>,
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

    // Channel for step completions (bounded to avoid unbounded growth).
    let done_cap = config.max_parallelism.saturating_mul(4).max(16);
    let (done_tx, mut done_rx) = mpsc::channel::<StepResult>(done_cap);

    let mut in_flight: usize = 0;

    loop {
        // 1. Mark skippable steps.
        let skippable = graph.skippable_steps(&statuses);
        for id in skippable {
            let reason = format!("dependency of '{id}' failed");
            statuses.insert(id.clone(), StepStatus::Skipped);
            let result = StepResult::skipped(id.clone(), reason.clone());
            results.insert(id.clone(), result);
            if let Some(tx) = &event_tx {
                let _ = tx
                    .send(PipelineEvent::StepSkipped {
                        step_id: id,
                        reason,
                    })
                    .await;
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
                        let _ = etx.try_send(PipelineEvent::StepStarted {
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
                drop(_permit);
                let _ = tx.send(result).await;
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
                let _ = tx
                    .send(PipelineEvent::StepCompleted {
                        result: result.clone(),
                    })
                    .await;
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
        let _ = tx
            .send(PipelineEvent::PipelineFinished {
                success,
                total_steps: graph.len(),
                succeeded,
                failed,
                skipped,
            })
            .await;
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
            endpoint,
        } => {
            let selected_model = model
                .clone()
                .or_else(|| runtime.llm_model.clone())
                .unwrap_or_else(|| "default".to_string());

            let endpoint =
                resolve_llm_chat_completions_url(endpoint.as_deref(), &runtime.llm_base_url)?;

            let completion_budget =
                CompletionBudget::new(max_tokens.unwrap_or(LEGACY_DEFAULT_COMPLETION_TOKENS))
                    .map_err(|e| PipelineError::LlmRequest(e.to_string()))?;

            let mut payload = json!({
                "model": &selected_model,
                "messages": [{"role": "user", "content": prompt}],
                "stream": false,
            });

            apply_completion_policy(&mut payload, WorkloadClass::CodeOneShot, completion_budget)
                .map_err(|e| PipelineError::LlmRequest(e.to_string()))?;

            let mut request = runtime.http_client.post(&endpoint).json(&payload);

            if let Some(api_key) = &runtime.llm_api_key {
                request = request.bearer_auth(api_key);
            }

            let response = request.send().await.map_err(classify_llm_http_error)?;
            let status = response.status();

            if !status.is_success() {
                return Err(PipelineError::LlmRequest(format!(
                    "LLM endpoint returned HTTP {}",
                    status.as_u16()
                )));
            }

            let response: Value = response.json().await.map_err(classify_llm_http_error)?;

            let reported_model = validate_reported_model(&response)?;
            let model_mismatch = reported_model
                .map(|reported| reported != selected_model)
                .unwrap_or(false);
            info!(
                llm_endpoint = %endpoint,
                requested_model = %selected_model,
                reported_model = ?reported_model,
                model_mismatch,
                "pipeline LLM route receipt"
            );
            if model_mismatch && selected_model != "default" {
                warn!(
                    llm_endpoint = %endpoint,
                    requested_model = %selected_model,
                    reported_model = ?reported_model,
                    "LLM endpoint reported a runtime model alias; preserving catalog request identity"
                );
            }

            validate_completion_response(&response)
                .map(|completion| completion.content)
                .map_err(|error| PipelineError::LlmResponse(safe_completion_error(&error)))
        }

        StepKind::Noop => Ok("noop".to_string()),
    }
}

fn validate_reported_model(response: &Value) -> Result<Option<&str>, PipelineError> {
    let Some(value) = response.get("model") else {
        return Ok(None);
    };
    let Some(model) = value
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return Err(PipelineError::LlmResponse(
            "LLM response model metadata is invalid".to_string(),
        ));
    };
    Ok(Some(model))
}

fn safe_completion_error(error: &CompletionValidationError) -> String {
    match error {
        CompletionValidationError::UnknownFinishReason { .. } => {
            "completion has an unsupported finish_reason".to_string()
        }
        _ => error.to_string(),
    }
}

fn classify_llm_http_error(error: reqwest::Error) -> PipelineError {
    // Keep provider URLs, query strings, and response bodies out of pipeline
    // receipts. Reqwest's predicates retain the useful failure taxonomy
    // without copying its potentially sensitive Display text.
    if error.is_timeout() {
        PipelineError::LlmRequest("LLM HTTP request timed out".to_string())
    } else if error.is_connect() {
        PipelineError::LlmRequest(
            "LLM endpoint is unreachable: connection establishment or DNS resolution failed"
                .to_string(),
        )
    } else if error.is_decode() {
        PipelineError::LlmResponse("LLM endpoint returned invalid JSON".to_string())
    } else if error.is_body() {
        PipelineError::LlmResponse("LLM response body transport failed".to_string())
    } else if error.is_redirect() {
        PipelineError::LlmRequest("LLM endpoint redirect was rejected".to_string())
    } else if error.is_request() {
        PipelineError::LlmRequest(
            "LLM request construction or HTTP protocol dispatch failed".to_string(),
        )
    } else {
        PipelineError::LlmRequest("LLM HTTP transport failed".to_string())
    }
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
        spawn_delayed_single_request_server(
            status_line,
            content_type,
            response_body,
            Duration::ZERO,
        )
        .await
    }

    async fn spawn_delayed_single_request_server(
        status_line: &str,
        content_type: &str,
        response_body: String,
        response_delay: Duration,
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
                tokio::time::sleep(response_delay).await;

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

    fn successful_llm_response(model: &str, content: &str) -> String {
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content
                },
                "finish_reason": "stop"
            }]
        })
        .to_string()
    }

    fn llm_pipeline(
        model: Option<&str>,
        max_tokens: Option<u32>,
        endpoint: Option<String>,
    ) -> PipelineGraph {
        llm_pipeline_with_timeout(model, max_tokens, endpoint, Duration::from_secs(300))
    }

    fn llm_pipeline_with_timeout(
        model: Option<&str>,
        max_tokens: Option<u32>,
        endpoint: Option<String>,
        timeout: Duration,
    ) -> PipelineGraph {
        let mut graph = PipelineGraph::new();
        graph
            .add_step(
                Step::new(
                    "llm",
                    "LLM Prompt",
                    StepKind::LlmPrompt {
                        prompt: "Say hello".to_string(),
                        model: model.map(str::to_string),
                        max_tokens,
                        endpoint,
                    },
                )
                .with_timeout(timeout),
            )
            .unwrap();
        graph
    }

    fn llm_failure(result: &PipelineRunResult) -> &str {
        assert!(!result.success);
        let step = &result.results[&StepId::new("llm")];
        assert_eq!(step.status, StepStatus::Failed);
        step.error.as_deref().expect("failed step has an error")
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

        let (tx, mut rx) = mpsc::channel(16);
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
        let llm_response = successful_llm_response("pipeline-model", "hello from llm");

        let (base_url, request_rx) =
            spawn_single_request_server("200 OK", "application/json", llm_response).await;

        let g = llm_pipeline(None, None, None);

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
        assert!(raw_request.contains("\"max_tokens\":2048"));
        assert!(raw_request.contains("\"enable_thinking\":false"));
    }

    #[test]
    fn default_http_client_has_bounded_connect_and_no_total_deadline() {
        assert_eq!(
            DEFAULT_HTTP_CLIENT_TIMEOUTS.connect,
            Duration::from_secs(10)
        );
        assert_eq!(DEFAULT_HTTP_CLIENT_TIMEOUTS.total, None);
    }

    #[tokio::test]
    async fn llm_step_timeout_outlives_scaled_legacy_client_deadline() {
        const RESPONSE_DELAY: Duration = Duration::from_millis(60);
        const SCALED_LEGACY_TOTAL_TIMEOUT: Duration = Duration::from_millis(20);
        const STEP_TIMEOUT: Duration = Duration::from_millis(500);

        let (legacy_url, _) = spawn_delayed_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("pipeline-model", "legacy client must time out"),
            RESPONSE_DELAY,
        )
        .await;
        let legacy_graph =
            llm_pipeline_with_timeout(Some("pipeline-model"), None, Some(legacy_url), STEP_TIMEOUT);
        let legacy_config = ExecutorConfig {
            http_client: build_http_client(HttpClientTimeoutPolicy {
                connect: Duration::from_millis(100),
                total: Some(SCALED_LEGACY_TOTAL_TIMEOUT),
            }),
            ..ExecutorConfig::default()
        };

        let legacy_result = execute(&legacy_graph, legacy_config, None).await.unwrap();
        let legacy_error = llm_failure(&legacy_result);
        assert!(legacy_error.contains("timed out"));
        assert!(!legacy_error.contains("unreachable"));

        let (default_url, _) = spawn_delayed_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("pipeline-model", "step deadline remained authoritative"),
            RESPONSE_DELAY,
        )
        .await;
        let default_graph = llm_pipeline_with_timeout(
            Some("pipeline-model"),
            None,
            Some(default_url),
            STEP_TIMEOUT,
        );

        let default_result = execute(&default_graph, ExecutorConfig::default(), None)
            .await
            .unwrap();
        assert!(default_result.success);
        assert_eq!(
            default_result.results[&StepId::new("llm")].output,
            "step deadline remained authoritative"
        );
    }

    #[tokio::test]
    async fn stalled_llm_response_is_bounded_by_step_deadline() {
        let (base_url, _) = spawn_delayed_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("pipeline-model", "must arrive too late"),
            Duration::from_millis(250),
        )
        .await;
        let graph = llm_pipeline_with_timeout(
            Some("pipeline-model"),
            None,
            Some(base_url),
            Duration::from_millis(30),
        );

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            execute(&graph, ExecutorConfig::default(), None),
        )
        .await
        .expect("the outer test deadline must not fire")
        .unwrap();

        assert!(!result.success);
        assert_eq!(
            result.results[&StepId::new("llm")].status,
            StepStatus::TimedOut
        );
    }

    #[test]
    fn llm_endpoint_normalization_accepts_supported_shapes() {
        let full = "https://models.example:55000/v1/chat/completions";
        assert_eq!(
            normalize_llm_chat_completions_url("https://models.example:55000").unwrap(),
            full
        );
        assert_eq!(
            normalize_llm_chat_completions_url("https://models.example:55000/v1/").unwrap(),
            full
        );
        assert_eq!(
            normalize_llm_chat_completions_url("https://models.example:55000/v1/chat/completions/")
                .unwrap(),
            full
        );
    }

    #[test]
    fn llm_endpoint_validation_rejects_non_http_and_ambiguous_urls() {
        for invalid in [
            "",
            "models.example:55000",
            "ftp://models.example/model",
            "http://user:secret@models.example",
            "https://models.example/v1?token=secret",
        ] {
            assert!(
                normalize_llm_chat_completions_url(invalid).is_err(),
                "endpoint should be rejected: {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn llm_step_endpoint_takes_precedence_over_global_endpoint() {
        let (step_url, step_request_rx) = spawn_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("step-model", "from step endpoint"),
        )
        .await;
        let (global_url, global_request_rx) = spawn_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("step-model", "from global endpoint"),
        )
        .await;

        let graph = llm_pipeline(Some("step-model"), Some(64), Some(step_url));
        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(global_url),
            None,
        )
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(
            result.results[&StepId::new("llm")].output,
            "from step endpoint"
        );
        assert!(step_request_rx.await.unwrap().contains("max_tokens\":64"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), global_request_rx)
                .await
                .is_err(),
            "global endpoint must not receive a request when the step endpoint is set"
        );
    }

    #[tokio::test]
    async fn invalid_explicit_llm_endpoint_does_not_fall_back() {
        let (global_url, global_request_rx) = spawn_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("pipeline-model", "must not be used"),
        )
        .await;
        let graph = llm_pipeline(
            Some("pipeline-model"),
            None,
            Some("ftp://explicit.invalid".to_string()),
        );

        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(global_url),
            None,
        )
        .await
        .unwrap();

        assert!(llm_failure(&result).contains("invalid LLM endpoint"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), global_request_rx)
                .await
                .is_err(),
            "invalid explicit endpoint must not fall back to the global endpoint"
        );
    }

    #[tokio::test]
    async fn unreachable_explicit_llm_endpoint_does_not_fall_back() {
        let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_url = format!("http://{}", unavailable.local_addr().unwrap());
        drop(unavailable);

        let (global_url, global_request_rx) = spawn_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("pipeline-model", "must not be used"),
        )
        .await;
        let graph = llm_pipeline(Some("pipeline-model"), None, Some(unavailable_url.clone()));

        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(global_url),
            None,
        )
        .await
        .unwrap();

        let error = llm_failure(&result);
        assert!(error.contains("unreachable"));
        assert!(error.contains("connection establishment or DNS resolution failed"));
        assert!(!error.contains(&unavailable_url));
        assert_ne!(error, "llm request failed: LLM endpoint is unreachable");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), global_request_rx)
                .await
                .is_err(),
            "unreachable explicit endpoint must not fall back to the global endpoint"
        );
    }

    #[tokio::test]
    async fn llm_response_model_alias_preserves_requested_identity() {
        let (base_url, _) = spawn_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("Lucy-Q4_K_M.gguf", "alias accepted safely"),
        )
        .await;
        let graph = llm_pipeline(Some("lucy-1-7b"), None, None);

        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(base_url),
            None,
        )
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(
            result.results[&StepId::new("llm")].output,
            "alias accepted safely"
        );
    }

    #[test]
    fn reported_model_metadata_is_optional_but_must_be_a_nonblank_string() {
        assert_eq!(validate_reported_model(&json!({})).unwrap(), None);
        assert_eq!(
            validate_reported_model(&json!({"model": " Lucy-Q4_K_M.gguf "})).unwrap(),
            Some("Lucy-Q4_K_M.gguf")
        );

        for response in [
            json!({"model": ""}),
            json!({"model": "  "}),
            json!({"model": 7}),
        ] {
            let error = validate_reported_model(&response).unwrap_err().to_string();
            assert_eq!(
                error,
                "llm response parse failed: LLM response model metadata is invalid"
            );
            assert!(!error.contains(&response.to_string()));
        }
    }

    #[tokio::test]
    async fn default_model_allows_provider_to_report_resolved_model() {
        let (base_url, _) = spawn_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("provider-resolved-model", "resolved safely"),
        )
        .await;
        let graph = llm_pipeline(None, None, None);

        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(base_url),
            None,
        )
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(
            result.results[&StepId::new("llm")].output,
            "resolved safely"
        );
    }

    #[tokio::test]
    async fn llm_truncation_fails_without_leaking_provider_body() {
        const SECRET: &str = "private-provider-reasoning-4f172";
        let response = json!({
            "model": "pipeline-model",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": SECRET,
                    "reasoning_content": SECRET
                },
                "finish_reason": "length"
            }]
        })
        .to_string();
        let (base_url, _) =
            spawn_single_request_server("200 OK", "application/json", response).await;
        let graph = llm_pipeline(Some("pipeline-model"), Some(32), None);

        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(base_url),
            None,
        )
        .await
        .unwrap();

        let error = llm_failure(&result);
        assert!(error.contains("truncated"));
        assert!(!error.contains(SECRET));
    }

    #[tokio::test]
    async fn llm_http_error_does_not_leak_provider_body() {
        const SECRET: &str = "provider-error-secret-c81ba";
        let (base_url, _) = spawn_single_request_server(
            "500 Internal Server Error",
            "application/json",
            format!(r#"{{"error":"{SECRET}"}}"#),
        )
        .await;
        let graph = llm_pipeline(None, None, None);

        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(base_url),
            None,
        )
        .await
        .unwrap();

        let error = llm_failure(&result);
        assert!(error.contains("HTTP 500"));
        assert!(!error.contains(SECRET));
    }

    #[tokio::test]
    async fn llm_completion_budget_is_bounded_before_dispatch() {
        let (base_url, request_rx) = spawn_single_request_server(
            "200 OK",
            "application/json",
            successful_llm_response("default", "must not be used"),
        )
        .await;
        let graph = llm_pipeline(None, Some(32_769), None);

        let result = execute(
            &graph,
            ExecutorConfig::default().with_llm_base_url(base_url),
            None,
        )
        .await
        .unwrap();

        assert!(llm_failure(&result).contains("exceeds hard cap"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), request_rx)
                .await
                .is_err(),
            "invalid budget must fail before dispatch"
        );
    }
}
