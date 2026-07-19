//! Patroni-backed HA coordination for the control plane.
//!
//! ForgeFleet's Postgres HA can be driven by [Patroni](https://patroni.readthedocs.io):
//! each database node runs a Patroni agent, and its REST API (`GET /cluster`)
//! is the source of truth for who the leader is and how healthy the replicas
//! are. This module is the control-plane side of that integration:
//!
//! - [`PatroniClusterState`] / [`PatroniClusterMember`] deserialize the
//!   `/cluster` payload (fetched by callers — this crate stays free of HTTP
//!   and DB dependencies, matching the rest of the facade).
//! - [`HaCoordinator::observe_cluster_state`] diffs consecutive snapshots and
//!   emits [`HaClusterEvent`]s (leader elected/changed/lost, members joining
//!   or leaving, state transitions, replication lag breaches).
//! - [`HaCoordinator::handle_failover_event`] maps failover-relevant events to
//!   [`HaAction`]s the caller executes (repoint the primary endpoint, suspend
//!   or resume writes, page the operator).
//!
//! The tick-driven promotion machinery for non-Patroni fleets lives in
//! `ff-agent::ha::pg_failover`; this coordinator does not overlap with it —
//! Patroni owns promotion, ForgeFleet only reacts.

use serde::{Deserialize, Deserializer, Serialize};

use crate::health::AggregateHealthStatus;

/// Patroni's default `maximum_lag_on_failover` (bytes). A replica further
/// behind than this is considered too stale to be a failover target, so we
/// surface it as [`HaClusterEvent::ReplicationLagExceeded`].
pub const DEFAULT_MAX_REPLICATION_LAG_BYTES: u64 = 1_048_576;

/// Role of a member as reported by Patroni's `/cluster` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatroniMemberRole {
    Leader,
    Replica,
    SyncStandby,
    StandbyLeader,
    #[serde(other)]
    Unknown,
}

impl PatroniMemberRole {
    /// Whether this member currently accepts writes for its cluster.
    pub fn is_leader(self) -> bool {
        matches!(self, Self::Leader | Self::StandbyLeader)
    }
}

/// One member of a Patroni cluster (an element of `/cluster` `members`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatroniClusterMember {
    pub name: String,
    pub role: PatroniMemberRole,
    /// Patroni state string, e.g. `running`, `streaming`, `stopped`,
    /// `start failed`, `creating replica`. Kept as-is because the set is
    /// open-ended across Patroni versions; see [`Self::is_operational`].
    pub state: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(default)]
    pub timeline: Option<u64>,
    /// Replication lag in bytes. Patroni reports the string `"unknown"`
    /// when it cannot compute lag; that deserializes to `None`.
    #[serde(default, deserialize_with = "deserialize_lag")]
    pub lag: Option<u64>,
}

impl PatroniClusterMember {
    /// Whether the member is in a state where it serves its role.
    pub fn is_operational(&self) -> bool {
        matches!(self.state.as_str(), "running" | "streaming")
    }
}

/// Snapshot of a Patroni cluster, matching `GET /cluster`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PatroniClusterState {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub members: Vec<PatroniClusterMember>,
    /// True when cluster management is paused (`patronictl pause`) —
    /// Patroni will not auto-failover while paused.
    #[serde(default)]
    pub pause: bool,
}

impl PatroniClusterState {
    /// The current write leader, if any member holds a leader role.
    pub fn leader(&self) -> Option<&PatroniClusterMember> {
        self.members.iter().find(|m| m.role.is_leader())
    }
}

fn deserialize_lag<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawLag {
        Bytes(u64),
        // Field is only matched, never read — it exists so `"unknown"` parses.
        #[allow(dead_code)]
        Text(String),
    }

    Ok(match Option::<RawLag>::deserialize(deserializer)? {
        Some(RawLag::Bytes(bytes)) => Some(bytes),
        Some(RawLag::Text(_)) | None => None,
    })
}

/// A change detected between two consecutive Patroni cluster snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HaClusterEvent {
    /// A leader appeared where the previous snapshot had none.
    LeaderElected {
        leader: String,
    },
    /// Leadership moved between members (failover or switchover).
    LeaderChanged {
        previous: String,
        current: String,
    },
    /// The previous leader is gone and no member has taken over.
    LeaderLost {
        previous: String,
    },
    MemberJoined {
        member: String,
        role: PatroniMemberRole,
    },
    MemberLeft {
        member: String,
    },
    MemberStateChanged {
        member: String,
        previous_state: String,
        current_state: String,
    },
    /// A replica's lag crossed the coordinator's configured maximum.
    ReplicationLagExceeded {
        member: String,
        lag_bytes: u64,
        max_lag_bytes: u64,
    },
}

impl HaClusterEvent {
    /// Whether this event affects who (if anyone) accepts writes.
    pub fn is_failover_event(&self) -> bool {
        matches!(
            self,
            Self::LeaderElected { .. } | Self::LeaderChanged { .. } | Self::LeaderLost { .. }
        )
    }
}

/// Control-plane reaction to a failover-relevant [`HaClusterEvent`].
/// Executing these (repointing pools, pausing schedulers, paging) is the
/// caller's job — this crate only decides *what* should happen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HaAction {
    /// Repoint fleet clients at the new primary.
    UpdatePrimaryEndpoint { host: String, port: u16 },
    /// Stop dispatching write workloads until a leader returns.
    SuspendWrites { reason: String },
    /// A leader is serving again; write workloads may resume.
    ResumeWrites,
    NotifyOperator {
        severity: AggregateHealthStatus,
        message: String,
    },
}

/// Stateful coordinator that tracks Patroni cluster snapshots and turns
/// them into events and control-plane actions.
#[derive(Debug, Clone, Default)]
pub struct HaCoordinator {
    last_state: Option<PatroniClusterState>,
    max_replication_lag_bytes: u64,
}

impl HaCoordinator {
    /// Coordinator with [`DEFAULT_MAX_REPLICATION_LAG_BYTES`].
    pub fn new() -> Self {
        Self {
            last_state: None,
            max_replication_lag_bytes: DEFAULT_MAX_REPLICATION_LAG_BYTES,
        }
    }

    /// Override the replication-lag threshold (bytes).
    pub fn with_max_replication_lag_bytes(mut self, max_lag_bytes: u64) -> Self {
        self.max_replication_lag_bytes = max_lag_bytes;
        self
    }

    /// The most recently observed snapshot, if any.
    pub fn last_state(&self) -> Option<&PatroniClusterState> {
        self.last_state.as_ref()
    }

    /// Ingest a fresh `/cluster` snapshot and return the changes since the
    /// previous one. The first snapshot reports every member as joined
    /// (plus `LeaderElected` when it already has a leader) so callers can
    /// bootstrap from a cold start.
    pub fn observe_cluster_state(&mut self, state: PatroniClusterState) -> Vec<HaClusterEvent> {
        let mut events = Vec::new();
        let previous = self.last_state.replace(state);
        let current = self.last_state.as_ref().expect("just replaced");

        // Leader transitions.
        let previous_leader = previous.as_ref().and_then(|s| s.leader()).map(|m| &m.name);
        let current_leader = current.leader().map(|m| &m.name);
        match (previous_leader, current_leader) {
            (None, Some(leader)) => events.push(HaClusterEvent::LeaderElected {
                leader: leader.clone(),
            }),
            (Some(previous), Some(current)) if previous != current => {
                events.push(HaClusterEvent::LeaderChanged {
                    previous: previous.clone(),
                    current: current.clone(),
                })
            }
            (Some(previous), None) => events.push(HaClusterEvent::LeaderLost {
                previous: previous.clone(),
            }),
            _ => {}
        }

        // Membership + per-member state transitions.
        let previous_members: Vec<&PatroniClusterMember> = previous
            .as_ref()
            .map(|s| s.members.iter().collect())
            .unwrap_or_default();
        for member in &current.members {
            match previous_members.iter().find(|m| m.name == member.name) {
                None => events.push(HaClusterEvent::MemberJoined {
                    member: member.name.clone(),
                    role: member.role,
                }),
                Some(before) if before.state != member.state => {
                    events.push(HaClusterEvent::MemberStateChanged {
                        member: member.name.clone(),
                        previous_state: before.state.clone(),
                        current_state: member.state.clone(),
                    })
                }
                Some(_) => {}
            }
        }
        for member in &previous_members {
            if !current.members.iter().any(|m| m.name == member.name) {
                events.push(HaClusterEvent::MemberLeft {
                    member: member.name.clone(),
                });
            }
        }

        // Replication lag breaches (replicas only — leaders report no lag).
        for member in &current.members {
            if member.role.is_leader() {
                continue;
            }
            if let Some(lag) = member.lag {
                if lag > self.max_replication_lag_bytes {
                    events.push(HaClusterEvent::ReplicationLagExceeded {
                        member: member.name.clone(),
                        lag_bytes: lag,
                        max_lag_bytes: self.max_replication_lag_bytes,
                    });
                }
            }
        }

        events
    }

    /// Map one event from [`Self::observe_cluster_state`] to the actions the
    /// control plane should take. Non-failover events produce at most an
    /// operator notification; leader transitions repoint the primary
    /// endpoint and gate write workloads.
    pub fn handle_failover_event(&self, event: &HaClusterEvent) -> Vec<HaAction> {
        match event {
            HaClusterEvent::LeaderElected { leader }
            | HaClusterEvent::LeaderChanged {
                current: leader, ..
            } => {
                let mut actions = Vec::new();
                if let Some(endpoint) = self.leader_endpoint(leader) {
                    actions.push(endpoint);
                }
                actions.push(HaAction::ResumeWrites);
                actions.push(HaAction::NotifyOperator {
                    severity: AggregateHealthStatus::Degraded,
                    message: format!("Patroni leadership moved to '{leader}'"),
                });
                actions
            }
            HaClusterEvent::LeaderLost { previous } => vec![
                HaAction::SuspendWrites {
                    reason: format!("Patroni leader '{previous}' lost with no successor"),
                },
                HaAction::NotifyOperator {
                    severity: AggregateHealthStatus::Unhealthy,
                    message: format!(
                        "Patroni cluster has no leader (previous: '{previous}'); writes suspended"
                    ),
                },
            ],
            HaClusterEvent::ReplicationLagExceeded {
                member,
                lag_bytes,
                max_lag_bytes,
            } => vec![HaAction::NotifyOperator {
                severity: AggregateHealthStatus::Degraded,
                message: format!(
                    "replica '{member}' lag {lag_bytes}B exceeds max {max_lag_bytes}B; \
                     it is not a viable failover target"
                ),
            }],
            HaClusterEvent::MemberLeft { member } => vec![HaAction::NotifyOperator {
                severity: AggregateHealthStatus::Degraded,
                message: format!("Patroni member '{member}' left the cluster"),
            }],
            HaClusterEvent::MemberJoined { .. } | HaClusterEvent::MemberStateChanged { .. } => {
                Vec::new()
            }
        }
    }

    /// Aggregate health of the last observed snapshot, using the same
    /// scale as the rest of the control plane ([`AggregateHealthStatus`]).
    /// No snapshot yet or no leader is `Unhealthy`; a paused cluster,
    /// non-operational member, or lag breach is `Degraded`.
    pub fn cluster_health(&self) -> AggregateHealthStatus {
        let Some(state) = &self.last_state else {
            return AggregateHealthStatus::Unhealthy;
        };
        let Some(leader) = state.leader() else {
            return AggregateHealthStatus::Unhealthy;
        };
        if !leader.is_operational() {
            return AggregateHealthStatus::Unhealthy;
        }

        let degraded = state.pause
            || state.members.iter().any(|m| {
                !m.is_operational()
                    || (!m.role.is_leader()
                        && m.lag
                            .is_some_and(|lag| lag > self.max_replication_lag_bytes))
            });
        if degraded {
            AggregateHealthStatus::Degraded
        } else {
            AggregateHealthStatus::Healthy
        }
    }

    fn leader_endpoint(&self, leader: &str) -> Option<HaAction> {
        let state = self.last_state.as_ref()?;
        let member = state.members.iter().find(|m| m.name == leader)?;
        Some(HaAction::UpdatePrimaryEndpoint {
            host: member.host.clone()?,
            port: member.port?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(
        name: &str,
        role: PatroniMemberRole,
        state: &str,
        lag: Option<u64>,
    ) -> PatroniClusterMember {
        PatroniClusterMember {
            name: name.to_string(),
            role,
            state: state.to_string(),
            host: Some(format!("10.0.0.{}", name.len())),
            port: Some(55432),
            api_url: Some(format!("http://{name}:8008/patroni")),
            timeline: Some(1),
            lag,
        }
    }

    fn cluster(members: Vec<PatroniClusterMember>) -> PatroniClusterState {
        PatroniClusterState {
            scope: Some("forgefleet".to_string()),
            members,
            pause: false,
        }
    }

    #[test]
    fn deserializes_patroni_cluster_payload_with_unknown_lag() {
        let json = r#"{
            "scope": "forgefleet",
            "members": [
                {"name": "pg0", "role": "leader", "state": "running",
                 "host": "10.0.0.1", "port": 5432, "timeline": 2},
                {"name": "pg1", "role": "replica", "state": "streaming",
                 "host": "10.0.0.2", "port": 5432, "timeline": 2, "lag": 0},
                {"name": "pg2", "role": "sync_standby", "state": "streaming",
                 "host": "10.0.0.3", "port": 5432, "timeline": 2, "lag": "unknown"}
            ]
        }"#;

        let state: PatroniClusterState = serde_json::from_str(json).unwrap();
        assert_eq!(state.members.len(), 3);
        assert_eq!(state.leader().unwrap().name, "pg0");
        assert_eq!(state.members[1].lag, Some(0));
        assert_eq!(state.members[2].lag, None);
        assert_eq!(state.members[2].role, PatroniMemberRole::SyncStandby);
        assert!(!state.pause);
    }

    #[test]
    fn unknown_role_string_maps_to_unknown_variant() {
        let json = r#"{"name": "pg9", "role": "demoted", "state": "stopped"}"#;
        let member: PatroniClusterMember = serde_json::from_str(json).unwrap();
        assert_eq!(member.role, PatroniMemberRole::Unknown);
    }

    #[test]
    fn first_snapshot_reports_joins_and_leader_election() {
        let mut coordinator = HaCoordinator::new();
        let events = coordinator.observe_cluster_state(cluster(vec![
            member("pg0", PatroniMemberRole::Leader, "running", None),
            member("pg1", PatroniMemberRole::Replica, "streaming", Some(0)),
        ]));

        assert!(events.contains(&HaClusterEvent::LeaderElected {
            leader: "pg0".to_string()
        }));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, HaClusterEvent::MemberJoined { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn failover_emits_leader_changed_and_repoints_primary() {
        let mut coordinator = HaCoordinator::new();
        coordinator.observe_cluster_state(cluster(vec![
            member("pg0", PatroniMemberRole::Leader, "running", None),
            member("pg1", PatroniMemberRole::Replica, "streaming", Some(0)),
        ]));

        let events = coordinator.observe_cluster_state(cluster(vec![
            member("pg0", PatroniMemberRole::Replica, "stopped", None),
            member("pg1", PatroniMemberRole::Leader, "running", None),
        ]));

        let changed = HaClusterEvent::LeaderChanged {
            previous: "pg0".to_string(),
            current: "pg1".to_string(),
        };
        assert!(events.contains(&changed));
        assert!(changed.is_failover_event());

        let actions = coordinator.handle_failover_event(&changed);
        assert!(actions.iter().any(|a| matches!(
            a,
            HaAction::UpdatePrimaryEndpoint { host, port: 55432 } if host == "10.0.0.3"
        )));
        assert!(actions.contains(&HaAction::ResumeWrites));
    }

    #[test]
    fn leader_lost_suspends_writes_and_pages_operator() {
        let mut coordinator = HaCoordinator::new();
        coordinator.observe_cluster_state(cluster(vec![member(
            "pg0",
            PatroniMemberRole::Leader,
            "running",
            None,
        )]));

        let events = coordinator.observe_cluster_state(cluster(vec![member(
            "pg0",
            PatroniMemberRole::Replica,
            "start failed",
            None,
        )]));

        let lost = HaClusterEvent::LeaderLost {
            previous: "pg0".to_string(),
        };
        assert!(events.contains(&lost));

        let actions = coordinator.handle_failover_event(&lost);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, HaAction::SuspendWrites { .. }))
        );
        assert!(actions.iter().any(|a| matches!(
            a,
            HaAction::NotifyOperator {
                severity: AggregateHealthStatus::Unhealthy,
                ..
            }
        )));
        assert_eq!(
            coordinator.cluster_health(),
            AggregateHealthStatus::Unhealthy
        );
    }

    #[test]
    fn detects_member_state_changes_departures_and_lag_breaches() {
        let mut coordinator = HaCoordinator::new().with_max_replication_lag_bytes(1024);
        coordinator.observe_cluster_state(cluster(vec![
            member("pg0", PatroniMemberRole::Leader, "running", None),
            member("pg1", PatroniMemberRole::Replica, "streaming", Some(0)),
            member("pg2", PatroniMemberRole::Replica, "streaming", Some(0)),
        ]));

        let events = coordinator.observe_cluster_state(cluster(vec![
            member("pg0", PatroniMemberRole::Leader, "running", None),
            member(
                "pg1",
                PatroniMemberRole::Replica,
                "creating replica",
                Some(4096),
            ),
        ]));

        assert!(events.contains(&HaClusterEvent::MemberStateChanged {
            member: "pg1".to_string(),
            previous_state: "streaming".to_string(),
            current_state: "creating replica".to_string(),
        }));
        assert!(events.contains(&HaClusterEvent::MemberLeft {
            member: "pg2".to_string()
        }));
        assert!(events.contains(&HaClusterEvent::ReplicationLagExceeded {
            member: "pg1".to_string(),
            lag_bytes: 4096,
            max_lag_bytes: 1024,
        }));
        assert_eq!(
            coordinator.cluster_health(),
            AggregateHealthStatus::Degraded
        );
    }

    #[test]
    fn steady_state_produces_no_events_and_healthy_cluster() {
        let mut coordinator = HaCoordinator::new();
        let snapshot = cluster(vec![
            member("pg0", PatroniMemberRole::Leader, "running", None),
            member("pg1", PatroniMemberRole::Replica, "streaming", Some(128)),
        ]);
        coordinator.observe_cluster_state(snapshot.clone());
        let events = coordinator.observe_cluster_state(snapshot);
        assert!(events.is_empty());
        assert_eq!(coordinator.cluster_health(), AggregateHealthStatus::Healthy);
    }

    #[test]
    fn paused_cluster_reports_degraded() {
        let mut coordinator = HaCoordinator::new();
        let mut snapshot = cluster(vec![member(
            "pg0",
            PatroniMemberRole::Leader,
            "running",
            None,
        )]);
        snapshot.pause = true;
        coordinator.observe_cluster_state(snapshot);
        assert_eq!(
            coordinator.cluster_health(),
            AggregateHealthStatus::Degraded
        );
    }
}
