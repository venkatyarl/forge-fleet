//! Leader election types and failover logic.
//!
//! ForgeFleet uses a priority-based leader election model:
//! - Taylor is the **preferred leader** (priority 1)
//! - Fallback order is configured in fleet.toml
//! - Election runs periodically or on leader failure
//! - Leader can yield when Venkat is actively using the machine

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::FleetConfig;

// ─── Election State ──────────────────────────────────────────────────────────

/// Current state of leader election.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElectionState {
    /// A leader is active and healthy.
    Stable {
        leader: String,
        since: DateTime<Utc>,
    },
    /// Election is in progress — no confirmed leader yet.
    Electing { triggered_at: DateTime<Utc> },
    /// No viable leader candidates — degraded mode.
    NoLeader { since: DateTime<Utc> },
}

impl ElectionState {
    /// Returns the current leader name, if stable.
    pub fn leader(&self) -> Option<&str> {
        match self {
            Self::Stable { leader, .. } => Some(leader),
            _ => None,
        }
    }

    /// Returns `true` if election is stable with a leader.
    pub fn is_stable(&self) -> bool {
        matches!(self, Self::Stable { .. })
    }
}

// ─── Election Candidate ──────────────────────────────────────────────────────

/// A node that is eligible to become leader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectionCandidate {
    /// Node name.
    pub name: String,
    /// Election priority (lower = more preferred).
    pub priority: u32,
    /// Whether this node is online and healthy.
    pub is_healthy: bool,
    /// Whether this node is currently yielding resources
    /// (Taylor in Interactive/Protected mode).
    pub is_yielding: bool,
    /// Last known heartbeat.
    pub last_heartbeat: Option<DateTime<Utc>>,
}

impl ElectionCandidate {
    /// A candidate is eligible if healthy and not yielding.
    pub fn is_eligible(&self) -> bool {
        self.is_healthy && !self.is_yielding
    }
}

// ─── Election Result ─────────────────────────────────────────────────────────

/// Result of running a leader election round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectionResult {
    /// The newly elected leader (if any).
    pub elected: Option<String>,
    /// All candidates considered, in priority order.
    pub candidates: Vec<ElectionCandidate>,
    /// When the election ran.
    pub timestamp: DateTime<Utc>,
    /// Reason for the election outcome.
    pub reason: String,
}

// ─── Election Logic ──────────────────────────────────────────────────────────

/// Build the ordered candidate list from fleet config and live node data.
///
/// Candidates are returned in election priority order (lowest priority number first).
/// The preferred leader is always first if present.
pub fn build_candidate_list(
    config: &FleetConfig,
    node_health: &[(String, bool, bool)], // (name, is_healthy, is_yielding)
) -> Vec<ElectionCandidate> {
    // Build a lookup map for health data.
    let health_map: std::collections::HashMap<&str, (bool, bool)> = node_health
        .iter()
        .map(|(name, healthy, yielding)| (name.as_str(), (*healthy, *yielding)))
        .collect();

    let mut candidates: Vec<ElectionCandidate> = config
        .nodes
        .iter()
        .map(|(name, node)| {
            let (is_healthy, is_yielding) = health_map
                .get(name.as_str())
                .copied()
                .unwrap_or((false, false));

            ElectionCandidate {
                name: name.clone(),
                priority: node.priority(),
                is_healthy,
                is_yielding,
                last_heartbeat: None,
            }
        })
        .collect();

    // Sort by election priority (lower first).
    candidates.sort_by_key(|c| c.priority);
    candidates
}

/// Run a leader election round.
///
/// Returns the best eligible candidate based on:
/// 1. Must be healthy
/// 2. Must not be yielding (unless no alternatives)
/// 3. Lower priority number wins
pub fn elect_leader(config: &FleetConfig, node_health: &[(String, bool, bool)]) -> ElectionResult {
    let candidates = build_candidate_list(config, node_health);
    let now = Utc::now();

    // First pass: find best eligible (healthy + not yielding).
    let eligible = candidates
        .iter()
        .find(|c| c.is_eligible())
        .map(|c| (c.name.clone(), c.priority));
    if let Some((name, priority)) = eligible {
        return ElectionResult {
            elected: Some(name.clone()),
            candidates,
            timestamp: now,
            reason: format!(
                "{} elected (priority {}, healthy, not yielding)",
                name, priority
            ),
        };
    }

    // Second pass: allow yielding nodes if that's all we have.
    let healthy = candidates
        .iter()
        .find(|c| c.is_healthy)
        .map(|c| (c.name.clone(), c.priority));
    if let Some((name, priority)) = healthy {
        return ElectionResult {
            elected: Some(name.clone()),
            candidates,
            timestamp: now,
            reason: format!(
                "{} elected (priority {}, healthy but yielding — no better option)",
                name, priority
            ),
        };
    }

    // No healthy candidates at all.
    ElectionResult {
        elected: None,
        candidates,
        timestamp: now,
        reason: "no healthy candidates available".into(),
    }
}

/// Check if the current leader should be replaced.
///
/// Returns `Some(new_leader)` if a failover is warranted.
pub fn check_failover(
    current_leader: &str,
    config: &FleetConfig,
    node_health: &[(String, bool, bool)],
) -> Option<ElectionResult> {
    let health_map: std::collections::HashMap<&str, (bool, bool)> = node_health
        .iter()
        .map(|(name, healthy, yielding)| (name.as_str(), (*healthy, *yielding)))
        .collect();

    let (leader_healthy, leader_yielding) = health_map
        .get(current_leader)
        .copied()
        .unwrap_or((false, false));

    // Case 1: Leader is offline → immediate failover.
    if !leader_healthy {
        let result = elect_leader(config, node_health);
        if result.elected.as_deref() != Some(current_leader) {
            return Some(result);
        }
    }

    // Case 2: Leader is yielding and a non-yielding candidate exists.
    if leader_yielding {
        let result = elect_leader(config, node_health);
        if let Some(ref elected) = result.elected
            && elected != current_leader
        {
            return Some(result);
        }
    }

    // Case 3: A higher-priority node has come online
    // (e.g., Taylor rebooted and preferred leader is back).
    let candidates = build_candidate_list(config, node_health);
    if let Some(best) = candidates.iter().find(|c| c.is_eligible())
        && best.name != current_leader
    {
        // Check if the better candidate has strictly higher priority.
        let current_priority = candidates
            .iter()
            .find(|c| c.name == current_leader)
            .map(|c| c.priority)
            .unwrap_or(u32::MAX);

        if best.priority < current_priority {
            let result = elect_leader(config, node_health);
            return Some(result);
        }
    }

    None // No failover needed.
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FleetConfig;

    fn test_config() -> FleetConfig {
        toml::from_str(
            r#"
[general]
name = "Test"

[leader]
preferred = "taylor"
fallback_order = ["james", "marcus"]

[database]

[nodes.taylor]
ip = "192.168.5.100"
role = "gateway"
election_priority = 1

[nodes.james]
ip = "192.168.5.101"
role = "builder"
election_priority = 2

[nodes.marcus]
ip = "192.168.5.102"
role = "builder"
election_priority = 50
"#,
        )
        .unwrap()
    }

    #[test]
    fn test_elect_preferred_leader() {
        let config = test_config();
        let health = vec![
            ("taylor".into(), true, false),
            ("james".into(), true, false),
            ("marcus".into(), true, false),
        ];
        let result = elect_leader(&config, &health);
        assert_eq!(result.elected, Some("taylor".into()));
        assert!(result.reason.contains("taylor"));
    }

    #[test]
    fn test_elect_fallback_when_preferred_down() {
        let config = test_config();
        let health = vec![
            ("taylor".into(), false, false), // offline
            ("james".into(), true, false),
            ("marcus".into(), true, false),
        ];
        let result = elect_leader(&config, &health);
        assert_eq!(result.elected, Some("james".into()));
    }

    #[test]
    fn test_elect_skip_yielding() {
        let config = test_config();
        let health = vec![
            ("taylor".into(), true, true), // yielding
            ("james".into(), true, false),
            ("marcus".into(), true, false),
        ];
        let result = elect_leader(&config, &health);
        assert_eq!(result.elected, Some("james".into()));
    }

    #[test]
    fn test_elect_yielding_fallback() {
        let config = test_config();
        let health = vec![
            ("taylor".into(), true, true), // yielding
            ("james".into(), true, true),  // yielding
            ("marcus".into(), true, true), // yielding
        ];
        let result = elect_leader(&config, &health);
        // All yielding → pick best priority anyway.
        assert_eq!(result.elected, Some("taylor".into()));
        assert!(result.reason.contains("yielding"));
    }

    #[test]
    fn test_elect_no_healthy() {
        let config = test_config();
        let health = vec![
            ("taylor".into(), false, false),
            ("james".into(), false, false),
            ("marcus".into(), false, false),
        ];
        let result = elect_leader(&config, &health);
        assert_eq!(result.elected, None);
        assert!(result.reason.contains("no healthy"));
    }

    #[test]
    fn test_failover_leader_offline() {
        let config = test_config();
        let health = vec![
            ("taylor".into(), false, false),
            ("james".into(), true, false),
            ("marcus".into(), true, false),
        ];
        let result = check_failover("taylor", &config, &health);
        assert!(result.is_some());
        assert_eq!(result.unwrap().elected, Some("james".into()));
    }

    #[test]
    fn test_no_failover_when_stable() {
        let config = test_config();
        let health = vec![
            ("taylor".into(), true, false),
            ("james".into(), true, false),
            ("marcus".into(), true, false),
        ];
        let result = check_failover("taylor", &config, &health);
        assert!(result.is_none());
    }

    #[test]
    fn test_failover_preferred_returns() {
        let config = test_config();
        // James is current leader, but Taylor just came back online.
        let health = vec![
            ("taylor".into(), true, false),
            ("james".into(), true, false),
            ("marcus".into(), true, false),
        ];
        let result = check_failover("james", &config, &health);
        assert!(result.is_some());
        assert_eq!(result.unwrap().elected, Some("taylor".into()));
    }

    #[test]
    fn test_election_state_methods() {
        let stable = ElectionState::Stable {
            leader: "taylor".into(),
            since: Utc::now(),
        };
        assert!(stable.is_stable());
        assert_eq!(stable.leader(), Some("taylor"));

        let electing = ElectionState::Electing {
            triggered_at: Utc::now(),
        };
        assert!(!electing.is_stable());
        assert_eq!(electing.leader(), None);
    }
}
