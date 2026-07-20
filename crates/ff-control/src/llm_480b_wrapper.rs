//! Concurrency-limited client for the local 480B code-generation endpoint.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

/// The 480B server is launched with `--parallel 2`.
pub const LLM_480B_PARALLELISM: usize = 2;

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

#[derive(Debug, thiserror::Error)]
pub enum Llm480bError {
    #[error("480B dispatch semaphore is closed")]
    SemaphoreClosed,
    #[error("480B endpoint request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("480B endpoint returned no completion")]
    EmptyResponse,
}

/// Wraps the local 480B endpoint and mirrors its two request slots.
#[derive(Debug, Clone)]
pub struct Llm480bWrapper {
    endpoint: Arc<str>,
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
}

impl Llm480bWrapper {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self::with_client(endpoint, reqwest::Client::new())
    }

    pub fn with_client(endpoint: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            endpoint: Arc::from(endpoint.into()),
            client,
            semaphore: Arc::new(Semaphore::new(LLM_480B_PARALLELISM)),
        }
    }

    /// Submit one code-generation request.
    ///
    /// The owned permit is intentionally kept in scope until the request and
    /// response body complete. Dropping it releases the slot on every exit
    /// path, including cancellation and errors.
    pub async fn generate(
        &self,
        request: &Llm480bRequest,
    ) -> Result<Llm480bResponse, Llm480bError> {
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

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wrapper_uses_two_shared_permits_and_releases_them() {
        let wrapper = Llm480bWrapper::new("http://127.0.0.1:1/v1/completions");
        assert_eq!(wrapper.available_permits(), 2);

        let first = wrapper.semaphore.clone().acquire_owned().await.unwrap();
        let second = wrapper.semaphore.clone().acquire_owned().await.unwrap();
        assert_eq!(wrapper.available_permits(), 0);
        assert!(wrapper.semaphore.clone().try_acquire_owned().is_err());

        drop(first);
        assert_eq!(wrapper.available_permits(), 1);
        drop(second);
        assert_eq!(wrapper.available_permits(), 2);
    }

    #[test]
    fn clones_share_the_same_dispatch_limit() {
        let wrapper = Llm480bWrapper::new("http://127.0.0.1:1/v1/completions");
        let clone = wrapper.clone();
        assert!(Arc::ptr_eq(&wrapper.semaphore, &clone.semaphore));
    }
}
