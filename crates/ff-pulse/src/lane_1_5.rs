//! Lane 1.5 health gate for LLM-backed review.
//!
//! The dispatcher is injected because `ff-agent` owns `fleet_oneshot` and
//! already depends on this crate.  This keeps the dependency graph acyclic
//! while letting the review lane use the canonical fleet router.

use std::future::Future;

/// Result of probing an LLM before trusting it as a reviewer.
#[derive(Debug, PartialEq, Eq)]
pub enum LlmHealthGate<T> {
    Healthy(T),
    Unhealthy(String),
}

impl<T> LlmHealthGate<T> {
    pub fn into_healthy(self) -> Option<T> {
        match self {
            Self::Healthy(response) => Some(response),
            Self::Unhealthy(_) => None,
        }
    }
}

/// Run the Lane 1.5 LLM health check through the caller's `fleet_oneshot`.
///
/// A successful but empty completion is unhealthy: it cannot produce a review
/// verdict. Router errors are also contained as an unhealthy result so review
/// can continue to its next fallback instead of failing the whole pipeline.
pub async fn check_llm_health<T, E, F, Fut>(fleet_oneshot: F) -> LlmHealthGate<T>
where
    E: std::fmt::Display,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(T, String), E>>,
{
    match fleet_oneshot().await {
        Ok((response, text)) if !text.trim().is_empty() => LlmHealthGate::Healthy(response),
        Ok(_) => LlmHealthGate::Unhealthy("fleet_oneshot returned an empty completion".into()),
        Err(error) => LlmHealthGate::Unhealthy(format!("fleet_oneshot failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_non_empty_completion() {
        let result =
            check_llm_health(|| async { Ok::<_, &'static str>((42, "APPROVE".to_string())) }).await;
        assert_eq!(result, LlmHealthGate::Healthy(42));
    }

    #[tokio::test]
    async fn rejects_empty_completion() {
        let result =
            check_llm_health(|| async { Ok::<_, &'static str>((42, " \n".to_string())) }).await;
        assert_eq!(
            result,
            LlmHealthGate::Unhealthy("fleet_oneshot returned an empty completion".into())
        );
    }

    #[tokio::test]
    async fn contains_router_errors() {
        let result =
            check_llm_health(|| async { Err::<((), String), _>("no healthy fleet deployment") })
                .await;
        assert_eq!(
            result,
            LlmHealthGate::Unhealthy("fleet_oneshot failed: no healthy fleet deployment".into())
        );
    }
}
