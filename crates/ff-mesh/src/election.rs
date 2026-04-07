//! Leader election protocol — preferred leader (Taylor), election order,
//! heartbeat-based failure detection, automatic promotion, preferred-returns.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::info;

use ff_core::config::FleetConfig;
use ff_core::leader::{
    ElectionCandidate, ElectionResult, ElectionState, build_candidate_list, check_failover,
    elect_leader,
};

// ─── Election Event ──────────────────────────────────────────────────────────

/// An event in the election history log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectionEvent {
    /// When this event occurred.
    pub timestamp: DateTime<Utc>,
    /// The kind of event.
    pub kind: ElectionEventKind,
    /// The election result that triggered this event.
    pub result: ElectionResult,
}

/// Kinds of election events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElectionEventKind {
    /// Initial election on startup.
    Initial,
    /// Leader failed health check — failover triggered.
    LeaderFailure,
    /// Leader is yielding resources — voluntary step-down.
    LeaderYielding,
    /// Preferred leader returned — reclaiming leadership.
    PreferredReturn,
    /// Manual election triggered by admin.
    Manual,
}

// ─── Election Manager ────────────────────────────────────────────────────────

/// Manages leader election state, history, and preferred-returns.
pub struct ElectionManager {
    /// Fleet configuration (election order, preferred leader).
    config: Arc<FleetConfig>,
    /// This node's name.
    node_name: String,
    /// Current election state.
    state: RwLock<ElectionState>,
    /// Election history (most recent first).
    history: RwLock<Vec<ElectionEvent>>,
    /// Node health cache: name → (is_healthy, is_yielding).
    node_health: DashMap<String, (bool, bool)>,
}

impl ElectionManager {
    /// Create a new election manager.
    pub fn new(config: Arc<FleetConfig>, node_name: String) -> Self {
        Self {
            config,
            node_name,
            state: RwLock::new(ElectionState::Electing {
                triggered_at: Utc::now(),
            }),
            history: RwLock::new(Vec::new()),
            node_health: DashMap::new(),
        }
    }

    /// Get the current election state.
    pub async fn state(&self) -> ElectionState {
        self.state.read().await.clone()
    }

    /// Get the current leader name, if stable.
    pub async fn current_leader(&self) -> Option<String> {
        let state = self.state.read().await;
        state.leader().map(|s| s.to_string())
    }

    /// Check if this node is the current leader.
    pub async fn is_leader(&self) -> bool {
        let state = self.state.read().await;
        state.leader() == Some(self.node_name.as_str())
    }

    /// Update health information for a node.
    pub fn update_node_health(&self, name: String, is_healthy: bool, is_yielding: bool) {
        self.node_health.insert(name, (is_healthy, is_yielding));
    }

    /// Remove health info for a node.
    pub fn remove_node_health(&self, name: &str) {
        self.node_health.remove(name);
    }

    /// Get the ordered candidate list based on current health data.
    pub fn candidates(&self) -> Vec<ElectionCandidate> {
        let health: Vec<(String, bool, bool)> = self
            .node_health
            .iter()
            .map(|entry| {
                let (healthy, yielding) = *entry.value();
                (entry.key().clone(), healthy, yielding)
            })
            .collect();

        build_candidate_list(&self.config, &health)
    }

    /// Run a full election round.
    pub async fn run_election(&self) -> ElectionResult {
        let health = self.collect_health();
        let result = elect_leader(&self.config, &health);
        self.apply_result(&result, ElectionEventKind::Initial).await;
        result
    }

    /// Check if the current leader should be replaced (failover, yielding, preferred-return).
    pub async fn check_and_failover(&self) -> Option<ElectionResult> {
        let state = self.state.read().await;
        let current_leader = match state.leader() {
            Some(l) => l.to_string(),
            None => {
                drop(state);
                // No leader — run a fresh election.
                let result = self.run_election().await;
                return Some(result);
            }
        };
        drop(state);

        let health = self.collect_health();
        let failover = check_failover(&current_leader, &self.config, &health);

        if let Some(ref result) = failover {
            let kind = self.classify_failover(&current_leader, result);
            self.apply_result(result, kind).await;
        }

        // Check preferred-return: if Taylor is healthy & not yielding,
        // and current leader != Taylor, switch back.
        if failover.is_none() {
            let preferred = &self.config.leader.preferred;
            if current_leader != *preferred
                && let Some(entry) = self.node_health.get(preferred)
            {
                let (healthy, yielding) = *entry;
                if healthy && !yielding {
                    // Preferred leader is back — trigger election.
                    let result = elect_leader(&self.config, &health);
                    if result.elected.as_deref() == Some(preferred.as_str()) {
                        info!(
                            preferred = %preferred,
                            previous = %current_leader,
                            "preferred leader returned — reclaiming leadership"
                        );
                        self.apply_result(&result, ElectionEventKind::PreferredReturn)
                            .await;
                        return Some(result);
                    }
                }
            }
        }

        failover
    }

    /// Manually trigger an election.
    pub async fn manual_election(&self) -> ElectionResult {
        let health = self.collect_health();
        let result = elect_leader(&self.config, &health);
        self.apply_result(&result, ElectionEventKind::Manual).await;
        info!(
            elected = ?result.elected,
            reason = %result.reason,
            "manual election completed"
        );
        result
    }

    /// Get the election history.
    pub async fn history(&self) -> Vec<ElectionEvent> {
        self.history.read().await.clone()
    }

    /// Get the preferred leader name.
    pub fn preferred_leader(&self) -> &str {
        &self.config.leader.preferred
    }

    /// Get the fallback order.
    pub fn fallback_order(&self) -> &[String] {
        &self.config.leader.fallback_order
    }

    // ─── Internal helpers ────────────────────────────────────────────────

    fn collect_health(&self) -> Vec<(String, bool, bool)> {
        self.node_health
            .iter()
            .map(|entry| {
                let (healthy, yielding) = *entry.value();
                (entry.key().clone(), healthy, yielding)
            })
            .collect()
    }

    async fn apply_result(&self, result: &ElectionResult, kind: ElectionEventKind) {
        let mut state = self.state.write().await;

        *state = if let Some(ref leader) = result.elected {
            ElectionState::Stable {
                leader: leader.clone(),
                since: Utc::now(),
            }
        } else {
            ElectionState::NoLeader { since: Utc::now() }
        };

        let event = ElectionEvent {
            timestamp: Utc::now(),
            kind,
            result: result.clone(),
        };

        let mut hist = self.history.write().await;
        hist.insert(0, event); // Most recent first.

        // Keep last 100 events.
        hist.truncate(100);
    }

    fn classify_failover(
        &self,
        current_leader: &str,
        result: &ElectionResult,
    ) -> ElectionEventKind {
        // Check if the current leader is offline.
        if let Some(entry) = self.node_health.get(current_leader) {
            let (healthy, yielding) = *entry;
            if !healthy {
                return ElectionEventKind::LeaderFailure;
            }
            if yielding {
                return ElectionEventKind::LeaderYielding;
            }
        }

        // If the newly elected is the preferred leader, it's a preferred-return.
        if result.elected.as_deref() == Some(self.config.leader.preferred.as_str())
            && current_leader != self.config.leader.preferred
        {
            return ElectionEventKind::PreferredReturn;
        }

        ElectionEventKind::LeaderFailure
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::config::{FleetSettings, LeaderConfig};

    fn test_config() -> FleetConfig {
        FleetConfig {
            fleet: FleetSettings {
                name: "test".into(),
                heartbeat_interval_secs: 5,
                heartbeat_timeout_secs: 15,
                api_port: 51800,
                ..Default::default()
            },
            nodes: [
                (
                    "taylor".to_string(),
                    ff_core::config::NodeConfig {
                        ip: "192.168.5.100".into(),
                        role: ff_core::types::Role::Gateway,
                        election_priority: Some(1),
                        ..Default::default()
                    },
                ),
                (
                    "james".to_string(),
                    ff_core::config::NodeConfig {
                        ip: "192.168.5.101".into(),
                        role: ff_core::types::Role::Builder,
                        election_priority: Some(2),
                        ..Default::default()
                    },
                ),
                (
                    "marcus".to_string(),
                    ff_core::config::NodeConfig {
                        ip: "192.168.5.102".into(),
                        role: ff_core::types::Role::Builder,
                        election_priority: Some(50),
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            models: vec![],
            leader: LeaderConfig {
                preferred: "taylor".into(),
                fallback_order: vec!["james".into(), "marcus".into()],
                election_interval_secs: 10,
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_initial_election() {
        let config = Arc::new(test_config());
        let mgr = ElectionManager::new(config, "taylor".into());

        mgr.update_node_health("taylor".into(), true, false);
        mgr.update_node_health("james".into(), true, false);
        mgr.update_node_health("marcus".into(), true, false);

        let result = mgr.run_election().await;
        assert_eq!(result.elected, Some("taylor".into()));
        assert!(mgr.is_leader().await);
    }

    #[tokio::test]
    async fn test_failover_to_james() {
        let config = Arc::new(test_config());
        let mgr = ElectionManager::new(config, "taylor".into());

        // Taylor starts as leader.
        mgr.update_node_health("taylor".into(), true, false);
        mgr.update_node_health("james".into(), true, false);
        mgr.run_election().await;
        assert_eq!(mgr.current_leader().await, Some("taylor".into()));

        // Taylor goes offline.
        mgr.update_node_health("taylor".into(), false, false);
        let failover = mgr.check_and_failover().await;
        assert!(failover.is_some());
        assert_eq!(failover.unwrap().elected, Some("james".into()));
    }

    #[tokio::test]
    async fn test_preferred_return() {
        let config = Arc::new(test_config());
        let mgr = ElectionManager::new(config, "james".into());

        // James is leader because Taylor was down.
        mgr.update_node_health("taylor".into(), false, false);
        mgr.update_node_health("james".into(), true, false);
        mgr.update_node_health("marcus".into(), true, false);
        mgr.run_election().await;
        assert_eq!(mgr.current_leader().await, Some("james".into()));

        // Taylor comes back online.
        mgr.update_node_health("taylor".into(), true, false);
        let failover = mgr.check_and_failover().await;
        assert!(failover.is_some());
        assert_eq!(failover.unwrap().elected, Some("taylor".into()));
    }

    #[tokio::test]
    async fn test_taylor_yielding() {
        let config = Arc::new(test_config());
        let mgr = ElectionManager::new(config, "taylor".into());

        mgr.update_node_health("taylor".into(), true, true); // yielding
        mgr.update_node_health("james".into(), true, false);

        let result = mgr.run_election().await;
        // James should be elected when Taylor is yielding.
        assert_eq!(result.elected, Some("james".into()));
    }

    #[tokio::test]
    async fn test_election_history() {
        let config = Arc::new(test_config());
        let mgr = ElectionManager::new(config, "taylor".into());

        mgr.update_node_health("taylor".into(), true, false);
        mgr.update_node_health("james".into(), true, false);

        mgr.run_election().await;
        mgr.manual_election().await;

        let history = mgr.history().await;
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn test_no_leader_when_all_down() {
        let config = Arc::new(test_config());
        let mgr = ElectionManager::new(config, "taylor".into());

        mgr.update_node_health("taylor".into(), false, false);
        mgr.update_node_health("james".into(), false, false);
        mgr.update_node_health("marcus".into(), false, false);

        let result = mgr.run_election().await;
        assert_eq!(result.elected, None);

        let state = mgr.state().await;
        assert!(matches!(state, ElectionState::NoLeader { .. }));
    }
}
