//! Tests for [`crate::subsystem_watchdog::SubsystemWatchdog`] — the
//! leader-gated watchdog that trips a restart/notify decision after a
//! subsystem stays unhealthy for enough consecutive ticks.

use ff_core::config::FleetConfig;
use ff_runtime::EngineStatus;

use crate::bootstrap::{BootstrapOptions, StartupSubsystem};
use crate::control_plane::ControlPlane;
use crate::health::AggregateHealthStatus;
use crate::subsystem_watchdog::{
    RestartOutcome, SubsystemObservation, SubsystemRestarter, WatchdogSubsystem,
};
use crate::subsystem_watchdog::{SubsystemWatchdog, WatchdogAction};

#[derive(Default)]
struct RecordingRestarter {
    restarted: Vec<WatchdogSubsystem>,
    fail: bool,
}

impl SubsystemRestarter for RecordingRestarter {
    fn restart(&mut self, subsystem: WatchdogSubsystem) -> Result<(), String> {
        self.restarted.push(subsystem);
        if self.fail {
            Err("supervisor unavailable".to_string())
        } else {
            Ok(())
        }
    }
}

/// A control plane with no nodes/models configured — enough to exercise
/// health aggregation without a live fleet.
fn bare_control_plane() -> ControlPlane {
    let options = BootstrapOptions {
        require_nodes: false,
        ..BootstrapOptions::default()
    };
    ControlPlane::bootstrap(FleetConfig::default(), options)
        .expect("bare control plane should bootstrap")
}

fn mark_runtime_healthy(cp: &mut ControlPlane) {
    cp.handles.runtime.last_status = Some(EngineStatus {
        running: true,
        healthy: true,
        pid: Some(1234),
        model_id: Some("qwen3-32b".to_string()),
        endpoint: Some("http://127.0.0.1:51800".to_string()),
        uptime_secs: Some(10),
    });
}

// --- normal operation --------------------------------------------------

#[test]
fn healthy_subsystems_never_trip_across_many_ticks() {
    let mut cp = bare_control_plane();
    mark_runtime_healthy(&mut cp);

    let mut watchdog = SubsystemWatchdog::new();
    for _ in 0..5 {
        let actions = watchdog.tick(&cp, true);
        assert!(actions.is_empty(), "healthy tick should produce no actions");
    }
    assert!(
        watchdog.events().is_empty(),
        "no unhealthy observation should ever be recorded"
    );
}

#[test]
fn non_leader_tick_is_always_a_noop_even_when_unhealthy() {
    // Runtime defaults to `running: false` (no status observed yet), which
    // the watchdog classifies as unhealthy — but a non-leader must never
    // act on it.
    let cp = bare_control_plane();

    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(1);
    for _ in 0..5 {
        let actions = watchdog.tick(&cp, false);
        assert!(
            actions.is_empty(),
            "follower must never take watchdog actions"
        );
    }
    assert!(
        watchdog.events().is_empty(),
        "follower must not track health state either"
    );
}

// --- failure scenarios ---------------------------------------------------

#[test]
fn unhealthy_runtime_trips_after_threshold_consecutive_ticks() {
    // Runtime defaults to `running: false` => Unhealthy every tick.
    let cp = bare_control_plane();

    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(3);

    for tick in 1..=2 {
        let actions = watchdog.tick(&cp, true);
        assert!(
            actions.is_empty(),
            "tick {tick} is below the trip threshold and should not act"
        );
    }

    let actions = watchdog.tick(&cp, true);
    assert!(
        actions.iter().any(|a| matches!(
            a,
            WatchdogAction::RestartSubsystem {
                subsystem: StartupSubsystem::Runtime,
                ..
            }
        )),
        "third consecutive unhealthy tick should restart the runtime subsystem: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            WatchdogAction::NotifyOperator {
                subsystem: StartupSubsystem::Runtime,
                ..
            }
        )),
        "tripping should also page the operator: {actions:?}"
    );

    assert_eq!(
        watchdog.events().len(),
        3,
        "one unhealthy observation should be recorded per tick"
    );
}

#[test]
fn watchdog_only_trips_once_per_streak_not_every_tick_after() {
    let cp = bare_control_plane();
    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(2);

    watchdog.tick(&cp, true); // consecutive = 1, no trip
    let trip_actions = watchdog.tick(&cp, true); // consecutive = 2, trips
    assert!(!trip_actions.is_empty());

    let followup_actions = watchdog.tick(&cp, true); // consecutive = 3, already tripped
    assert!(
        followup_actions.is_empty(),
        "watchdog should only fire the restart decision on the tick that crosses the threshold"
    );
}

#[test]
fn recovering_subsystem_resets_the_consecutive_counter() {
    let mut cp = bare_control_plane();
    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(3);

    // Two unhealthy ticks build up a streak, but don't trip yet.
    watchdog.tick(&cp, true);
    let actions = watchdog.tick(&cp, true);
    assert!(actions.is_empty());

    // Recovery: runtime reports healthy, streak resets.
    mark_runtime_healthy(&mut cp);
    let actions = watchdog.tick(&cp, true);
    assert!(
        actions.is_empty(),
        "a healthy tick must never trip the watchdog"
    );

    // Regression: fall unhealthy again. If the counter hadn't reset, this
    // single tick would already be at/above the old streak and could
    // mistakenly trip immediately.
    cp.handles.runtime.last_status = None;
    let actions = watchdog.tick(&cp, true);
    assert!(
        actions.is_empty(),
        "counter must have reset on recovery, so one more unhealthy tick shouldn't trip it"
    );
}

#[test]
fn managed_subsystem_restarts_after_dead_threshold() {
    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(2);
    let mut restarter = RecordingRestarter::default();

    let first = watchdog.restart_dead_subsystems(
        [SubsystemObservation::dead(
            WatchdogSubsystem::MergeDrain,
            "join handle finished",
        )],
        true,
        &mut restarter,
    );
    assert!(first[0].restart.is_none());

    let second = watchdog.restart_dead_subsystems(
        [SubsystemObservation::dead(
            WatchdogSubsystem::MergeDrain,
            "join handle finished",
        )],
        true,
        &mut restarter,
    );

    assert_eq!(restarter.restarted, vec![WatchdogSubsystem::MergeDrain]);
    assert!(matches!(second[0].restart, Some(RestartOutcome::Restarted)));
    assert_eq!(watchdog.subsystem_events().len(), 2);
}

#[test]
fn managed_subsystem_non_leader_does_not_restart() {
    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(1);
    let mut restarter = RecordingRestarter::default();

    let events = watchdog.restart_dead_subsystems(
        [SubsystemObservation::dead(
            WatchdogSubsystem::SelfHeal,
            "stale heartbeat",
        )],
        false,
        &mut restarter,
    );

    assert!(events.is_empty());
    assert!(restarter.restarted.is_empty());
    assert!(watchdog.subsystem_events().is_empty());
}

#[test]
fn managed_subsystem_recovery_resets_dead_counter() {
    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(2);
    let mut restarter = RecordingRestarter::default();

    watchdog.restart_dead_subsystems(
        [SubsystemObservation::dead(
            WatchdogSubsystem::Reaper,
            "missed heartbeat",
        )],
        true,
        &mut restarter,
    );
    watchdog.restart_dead_subsystems(
        [SubsystemObservation::alive(WatchdogSubsystem::Reaper)],
        true,
        &mut restarter,
    );
    let events = watchdog.restart_dead_subsystems(
        [SubsystemObservation::dead(
            WatchdogSubsystem::Reaper,
            "missed heartbeat",
        )],
        true,
        &mut restarter,
    );

    assert_eq!(events[0].consecutive_unhealthy, 1);
    assert!(events[0].restart.is_none());
    assert!(restarter.restarted.is_empty());
}

#[test]
fn managed_subsystem_records_restart_failure() {
    let mut watchdog = SubsystemWatchdog::new().with_trip_threshold(1);
    let mut restarter = RecordingRestarter {
        restarted: Vec::new(),
        fail: true,
    };

    let events = watchdog.restart_dead_subsystems(
        [SubsystemObservation {
            subsystem: WatchdogSubsystem::Scheduler,
            status: AggregateHealthStatus::Unhealthy,
            reason: Some("task exited".to_string()),
            observed_at: chrono::Utc::now(),
        }],
        true,
        &mut restarter,
    );

    assert!(matches!(
        events[0].restart,
        Some(RestartOutcome::Failed { .. })
    ));
    assert_eq!(restarter.restarted, vec![WatchdogSubsystem::Scheduler]);
}
