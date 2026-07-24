//! Local-only LLM routing for the Brain layer.
//!
//! [`crate::community_summary`]'s endpoint selection is DB-first
//! (`ff_db::pg_pick_offload_endpoint`) — it dials Postgres and picks a warm
//! fleet-wide endpoint. That's wrong to rely on during the Offline/Degraded
//! window: the DB call itself may be what's unreachable. This module reuses
//! ff-agent's local-only inference router (`InferenceRouter::active_local_url`,
//! the same primitive backing `ff_agent::coordinator::LocalLlmRouter`) so
//! Brain's own LLM calls stay confined to this computer until connectivity
//! returns, instead of hanging on a fleet round-trip.

use std::sync::Arc;

use ff_agent::inference_router::InferenceRouter;
use ff_core::schema::state::ConnectionState;

/// Selects a local-only LLM endpoint for Brain when the fleet is unreachable
/// (`Offline`/`Degraded` connection state).
#[derive(Clone, Debug)]
pub struct LocalLlmRouter {
    inner: Arc<InferenceRouter>,
}

impl LocalLlmRouter {
    pub fn new(inner: Arc<InferenceRouter>) -> Self {
        Self { inner }
    }

    /// True when `state` should confine Brain's LLM calls to this node.
    pub fn applies(state: ConnectionState) -> bool {
        matches!(state, ConnectionState::Offline | ConnectionState::Degraded)
    }

    /// Resolve a local-only endpoint URL for the given connection state.
    ///
    /// Returns `None` in `Online` state — callers should fall back to the
    /// normal DB-routed fleet endpoint (e.g.
    /// [`crate::community_summary::summarize_communities`]'s
    /// `pg_pick_offload_endpoint` path) — or when no local endpoint is
    /// currently healthy.
    pub async fn resolve_endpoint(&self, state: ConnectionState) -> Option<String> {
        if !Self::applies(state) {
            return None;
        }
        self.inner.active_local_url().await
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
            tier: 1,
            is_local,
            n_ctx: None,
        }
    }

    fn router() -> LocalLlmRouter {
        LocalLlmRouter::new(Arc::new(InferenceRouter::new(vec![
            endpoint("http://remote", false),
            endpoint("http://local", true),
        ])))
    }

    #[tokio::test]
    async fn offline_and_degraded_resolve_to_local_endpoint() {
        let router = router();
        for state in [ConnectionState::Offline, ConnectionState::Degraded] {
            assert_eq!(
                router.resolve_endpoint(state).await.as_deref(),
                Some("http://local")
            );
        }
    }

    #[tokio::test]
    async fn online_state_defers_to_fleet_routing() {
        let router = router();
        assert_eq!(router.resolve_endpoint(ConnectionState::Online).await, None);
    }

    #[tokio::test]
    async fn no_local_endpoint_returns_none() {
        let router = LocalLlmRouter::new(Arc::new(InferenceRouter::new(vec![endpoint(
            "http://remote",
            false,
        )])));
        assert_eq!(
            router.resolve_endpoint(ConnectionState::Offline).await,
            None
        );
    }
}
