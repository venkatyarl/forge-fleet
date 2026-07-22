//! Leader-gated watchdog over control-plane subsystem health.
//!
//! Wraps [`crate::health::aggregate_health_snapshot`] with consecutive-tick
//! tracking per [`StartupSubsystem`], so a single bad health check doesn't
//! trigger a restart — only a subsystem that stays unhealthy for
//! [`SubsystemWatchdog::trip_threshold`] consecutive ticks trips it.
//!
//! Like [`crate::ha_coordinator::HaCoordinator`], this module only decides
//! what should happen ([`WatchdogAction`]); executing a restart or paging an
//! operator is the caller's job. [`SubsystemWatchdog::tick`] is a no-op
//! unless `is_leader` is true, so followers never race the leader to "fix"
//! the same subsystem.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bootstrap::StartupSubsystem;
use crate::control_plane::ControlPlane;
use crate::health::{AggregateHealthStatus, ControlPlaneHealthSnapshot, aggregate_health_snapshot};

/// Consecutive unhealthy observations required before a subsystem trips the
/// watchdog.
pub const DEFAULT_TRIP_THRESHOLD: u32 = 3;

/// One subsystem's health as observed on a single watchdog tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogEvent {
    pub subsystem: StartupSubsystem,
    pub status: AggregateHealthStatus,
    pub consecutive_unhealthy: u32,
    pub observed_at: DateTime<Utc>,
}

/// Control-plane reaction to a subsystem tripping the watchdog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WatchdogAction {
    RestartSubsystem {
        subsystem: StartupSubsystem,
        reason: String,
    },
    NotifyOperator {
        subsystem: StartupSubsystem,
        message: String,
    },
}

/// Tracks consecutive-unhealthy streaks per subsystem across ticks.
#[derive(Debug, Clone)]
pub struct SubsystemWatchdog {
    trip_threshold: u32,
    consecutive_unhealthy: HashMap<StartupSubsystem, u32>,
    events: Vec<WatchdogEvent>,
}

impl Default for SubsystemWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl SubsystemWatchdog {
    /// Watchdog with [`DEFAULT_TRIP_THRESHOLD`].
    pub fn new() -> Self {
        Self {
            trip_threshold: DEFAULT_TRIP_THRESHOLD,
            consecutive_unhealthy: HashMap::new(),
            events: Vec::new(),
        }
    }

    /// Override the consecutive-tick threshold before a subsystem trips.
    pub fn with_trip_threshold(mut self, trip_threshold: u32) -> Self {
        self.trip_threshold = trip_threshold;
        self
    }

    /// Every unhealthy observation recorded so far, oldest first.
    pub fn events(&self) -> &[WatchdogEvent] {
        &self.events
    }

    /// One watchdog pass. Returns no-op (and records nothing) unless
    /// `is_leader` — only the elected leader restarts subsystems.
    pub fn tick(&mut self, control_plane: &ControlPlane, is_leader: bool) -> Vec<WatchdogAction> {
        if !is_leader {
            return Vec::new();
        }

        let snapshot = aggregate_health_snapshot(control_plane);
        let mut actions = Vec::new();

        for (subsystem, status) in [
            (StartupSubsystem::Discovery, discovery_status(&snapshot)),
            (StartupSubsystem::Runtime, runtime_status(&snapshot)),
            (StartupSubsystem::Scheduler, scheduler_status(&snapshot)),
        ] {
            let counter = self.consecutive_unhealthy.entry(subsystem).or_insert(0);
            if status == AggregateHealthStatus::Unhealthy {
                *counter += 1;
            } else {
                *counter = 0;
            }
            let consecutive_unhealthy = *counter;

            if consecutive_unhealthy == 0 {
                continue;
            }

            self.events.push(WatchdogEvent {
                subsystem,
                status,
                consecutive_unhealthy,
                observed_at: Utc::now(),
            });

            if consecutive_unhealthy == self.trip_threshold {
                actions.push(WatchdogAction::RestartSubsystem {
                    subsystem,
                    reason: format!(
                        "{subsystem:?} unhealthy for {consecutive_unhealthy} consecutive ticks"
                    ),
                });
                actions.push(WatchdogAction::NotifyOperator {
                    subsystem,
                    message: format!(
                        "watchdog restarted {subsystem:?} after {consecutive_unhealthy} \
                         consecutive unhealthy ticks"
                    ),
                });
            }
        }

        actions
    }
}

fn discovery_status(snapshot: &ControlPlaneHealthSnapshot) -> AggregateHealthStatus {
    if snapshot.discovery.unreachable_nodes > 0 {
        AggregateHealthStatus::Unhealthy
    } else if snapshot.discovery.degraded_nodes > 0 {
        AggregateHealthStatus::Degraded
    } else {
        AggregateHealthStatus::Healthy
    }
}

fn runtime_status(snapshot: &ControlPlaneHealthSnapshot) -> AggregateHealthStatus {
    if !snapshot.runtime.running {
        AggregateHealthStatus::Unhealthy
    } else if !snapshot.runtime.healthy {
        AggregateHealthStatus::Degraded
    } else {
        AggregateHealthStatus::Healthy
    }
}

fn scheduler_status(snapshot: &ControlPlaneHealthSnapshot) -> AggregateHealthStatus {
    if snapshot.scheduler.failed_runs > 0 {
        AggregateHealthStatus::Unhealthy
    } else {
        AggregateHealthStatus::Healthy
    }
}
