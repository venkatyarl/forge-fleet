//! Connectivity-aware LLM routing for the Virtual Brain.
//!
//! Mirrors the Offline/Degraded gating in `ff_agent::coordinator::LocalLlmRouter`,
//! but selects from ff-brain's DB-routed fleet endpoints — the same
//! `ff_db::pg_route_deployments` scorer [`crate::embeddings`] and
//! [`crate::community_summary`] already call — so brain LLM calls (chat,
//! summarization, context) fall back to this node's own deployments only
//! while the fleet connection can't be trusted for cross-node dispatch,
//! instead of routing to another host.

use ff_agent::fleet_info::resolve_this_worker_name;
use ff_core::schema::state::ConnectionState;
use ff_db::{RouteCandidate, RouteFilter, pg_route_deployments};
use sqlx::PgPool;

/// Picks a fleet-routed LLM endpoint, constrained to this node's own
/// deployments while the fleet connection is offline/degraded.
#[derive(Debug, Clone, Copy)]
pub struct LocalLlmRouter {
    connection_state: ConnectionState,
}

impl LocalLlmRouter {
    pub fn new(connection_state: ConnectionState) -> Self {
        Self { connection_state }
    }

    /// True while the fleet connection can't be trusted for cross-node dispatch.
    pub fn is_local_mode(&self) -> bool {
        matches!(
            self.connection_state,
            ConnectionState::Offline | ConnectionState::Degraded
        )
    }

    /// Pick a scored endpoint honoring the current connection state: this
    /// node's own deployments only while offline/degraded, any healthy fleet
    /// endpoint (the normal `pg_route_deployments` scorer) while online.
    pub async fn pick_endpoint(
        &self,
        pool: &PgPool,
        workload: Option<&str>,
    ) -> Result<Option<RouteCandidate>, String> {
        let filter = RouteFilter {
            workload: workload.map(str::to_string),
            ..Default::default()
        };
        let candidates = pg_route_deployments(pool, &filter)
            .await
            .map_err(|e| format!("route an LLM endpoint: {e}"))?;

        if !self.is_local_mode() {
            return Ok(candidates.into_iter().next());
        }

        let this_node = resolve_this_worker_name().await;
        Ok(candidates
            .into_iter()
            .find(|c| c.worker_name.eq_ignore_ascii_case(&this_node)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_and_degraded_are_local_mode() {
        for state in [ConnectionState::Offline, ConnectionState::Degraded] {
            assert!(LocalLlmRouter::new(state).is_local_mode());
        }
    }

    #[test]
    fn online_is_not_local_mode() {
        assert!(!LocalLlmRouter::new(ConnectionState::Online).is_local_mode());
    }

    #[tokio::test]
    async fn pick_endpoint_in_local_mode_only_matches_this_node() {
        let Some(url) = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()
        else {
            return;
        };

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to test database");

        let router = LocalLlmRouter::new(ConnectionState::Offline);
        // No deployment on any real fleet will match this node name, so a
        // local-mode pick against live data must come back empty rather than
        // falling through to some other host's endpoint.
        let picked = router.pick_endpoint(&pool, None).await.unwrap();
        if let Some(candidate) = picked {
            let this_node = resolve_this_worker_name().await;
            assert!(candidate.worker_name.eq_ignore_ascii_case(&this_node));
        }
    }
}
