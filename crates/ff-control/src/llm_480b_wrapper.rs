//! Concurrency-limited adapters for the local 480B code-generation service.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time;
use tracing::{info, warn};

use crate::errors::{ControlError, Result};
use crate::escalation_logger::{EscalationReason, log_escalation};

const DEFAULT_PARALLELISM: usize = 2;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodegenResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// CLI adapter introduced by the codegen task-processing feature.
#[derive(Debug)]
pub struct Llm480bWrapper {
    binary: String,
    semaphore: Semaphore,
    timeout: Duration,
}

impl Llm480bWrapper {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            semaphore: Semaphore::new(DEFAULT_PARALLELISM),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_parallel(mut self, parallel: usize) -> Self {
        self.semaphore = Semaphore::new(parallel.max(1));
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub async fn generate(&self, task: &str, repo: &Path) -> Result<CodegenResult> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| ControlError::Llm480b(format!("semaphore acquire failed: {e}")))?;

        info!(binary = %self.binary, task, repo = %repo.display(), "480B codegen invocation start");
        let output = time::timeout(
            self.timeout,
            Command::new(&self.binary)
                .arg("--parallel")
                .arg("2")
                .arg("--task")
                .arg(task)
                .arg("--repo")
                .arg(repo)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| ControlError::Llm480b("480B codegen timed out".to_string()))?
        .map_err(|e| ControlError::Llm480b(format!("failed to spawn 480B codegen: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);
        if output.status.success() {
            info!(exit_code, "480B codegen invocation succeeded");
        } else {
            warn!(exit_code, stderr = %stderr, "480B codegen invocation failed");
        }
        Ok(CodegenResult {
            stdout,
            stderr,
            exit_code,
        })
    }
}

/// Request accepted by the HTTP adapter.
#[derive(Debug, Clone, Serialize)]
pub struct Llm480bRequest {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Llm480bResponse {
    pub content: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Llm480bError {
    #[error("480B dispatch semaphore is closed")]
    SemaphoreClosed,
    #[error("480B endpoint request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("480B endpoint returned no completion")]
    EmptyResponse,
}

/// HTTP adapter retained for callers that submit OpenAI-compatible requests.
#[derive(Debug, Clone)]
pub struct Llm480bHttpWrapper {
    endpoint: Arc<str>,
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'static str,
    messages: [ChatMessage<'a>; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: Llm480bResponse,
}

impl Llm480bHttpWrapper {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self::with_client(endpoint, reqwest::Client::new())
    }

    pub fn with_client(endpoint: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            endpoint: Arc::from(endpoint.into()),
            client,
            semaphore: Arc::new(Semaphore::new(DEFAULT_PARALLELISM)),
        }
    }

    pub async fn generate(
        &self,
        request: &Llm480bRequest,
    ) -> std::result::Result<Llm480bResponse, Llm480bError> {
        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Llm480bError::SemaphoreClosed)?;
        let response: ChatResponse = self
            .client
            .post(ff_core::url::normalize_chat_completions_url(
                self.endpoint.as_ref(),
            ))
            .json(&ChatRequest {
                model: "qwen3-coder-480b",
                messages: [ChatMessage {
                    role: "user",
                    content: &request.prompt,
                }],
                max_tokens: request.max_tokens,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or(Llm480bError::EmptyResponse)
    }

    /// Dispatch a Lane-1.5 escalation and capture its input/output pair for
    /// future training. Interaction logging is best-effort.
    pub async fn generate_escalated(
        &self,
        pool: &ff_db::PgPool,
        request: &Llm480bRequest,
        reason: EscalationReason,
    ) -> std::result::Result<Llm480bResponse, Llm480bError> {
        let started = Instant::now();
        info!(
            reason = reason.as_str(),
            model = "qwen3-coder-480b",
            "routing task to Lane-1.5"
        );
        let response = self.generate(request).await?;
        log_escalation(
            pool,
            reason,
            &request.prompt,
            &response.content,
            started.elapsed(),
        )
        .await;
        Ok(response)
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parallelism_allows_two_concurrent_slots() {
        let wrapper = Llm480bWrapper::new("dummy");
        let _p1 = wrapper.semaphore.try_acquire().expect("first permit");
        let _p2 = wrapper.semaphore.try_acquire().expect("second permit");
        assert!(wrapper.semaphore.try_acquire().is_err());
    }

    #[test]
    fn with_parallel_changes_capacity_and_clamps_to_one() {
        let wrapper = Llm480bWrapper::new("dummy").with_parallel(4);
        let permits: Vec<_> = (0..4)
            .map(|_| wrapper.semaphore.try_acquire().expect("acquire permit"))
            .collect();
        assert!(wrapper.semaphore.try_acquire().is_err());
        drop(permits);

        let wrapper = Llm480bWrapper::new("dummy").with_parallel(0);
        let _permit = wrapper.semaphore.try_acquire().expect("single permit");
        assert!(wrapper.semaphore.try_acquire().is_err());
    }

    #[test]
    fn http_clones_share_the_dispatch_limit() {
        let wrapper = Llm480bHttpWrapper::new("http://127.0.0.1:1");
        let clone = wrapper.clone();
        assert!(Arc::ptr_eq(&wrapper.semaphore, &clone.semaphore));
        assert_eq!(wrapper.available_permits(), 2);
    }
}
