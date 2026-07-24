//! Connection-aware routing for local LLM inference.

use std::sync::Arc;

use ff_agent::inference_router::InferenceRouter;
use ff_core::schema::state::ConnectionState;

/// Selects an inference endpoint appropriate for the current connection state.
///
/// Offline and degraded operation is restricted to an endpoint on this
/// computer. Online operation uses the normal local-first fleet routing.
#[derive(Clone, Debug)]
pub struct LocalLlmRouter {
    inner: Arc<InferenceRouter>,
}

impl LocalLlmRouter {
    pub fn new(inner: Arc<InferenceRouter>) -> Self {
        Self { inner }
    }

    pub async fn select(&self, state: ConnectionState) -> Option<String> {
        match state {
            ConnectionState::Offline | ConnectionState::Degraded => {
                self.inner.active_local_url().await
            }
            ConnectionState::Online => self.inner.active_url().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_agent::inference_router::RouterEndpoint;

    fn endpoint(url: &str, is_local: bool) -> RouterEndpoint {
        RouterEndpoint {
            url: url.into(),
            model_id: "test".into(),
            label: url.into(),
            supports_tools: true,
            tier: if is_local { 1 } else { 4 },
            is_local,
            n_ctx: Some(32_768),
        }
    }

    fn router() -> LocalLlmRouter {
        LocalLlmRouter::new(Arc::new(InferenceRouter::new(vec![
            endpoint("http://remote", false),
            endpoint("http://local", true),
        ])))
    }

    #[tokio::test]
    async fn offline_and_degraded_use_only_local_llm() {
        for state in [ConnectionState::Offline, ConnectionState::Degraded] {
            assert_eq!(
                router().select(state).await.as_deref(),
                Some("http://local")
            );
        }
    }

    #[tokio::test]
    async fn online_uses_normal_router_selection() {
        let router = LocalLlmRouter::new(Arc::new(InferenceRouter::new(vec![
            endpoint("http://remote", false),
            endpoint("http://local", true),
        ])));

        assert_eq!(
            router.select(ConnectionState::Online).await.as_deref(),
            Some("http://remote")
        );
    }
}
