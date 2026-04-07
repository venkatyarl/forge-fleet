use chrono::{DateTime, Utc};
use ff_cron::RunStatus;
use ff_discovery::HealthStatus;
use serde::{Deserialize, Serialize};

use crate::control_plane::ControlPlane;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateHealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoveryHealthAggregate {
    pub total_nodes: usize,
    pub healthy_nodes: usize,
    pub degraded_nodes: usize,
    pub unreachable_nodes: usize,
    pub unknown_nodes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeHealthAggregate {
    pub running: bool,
    pub healthy: bool,
    pub model_id: Option<String>,
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchedulerHealthAggregate {
    pub total_jobs: usize,
    pub pending_runs: usize,
    pub running_runs: usize,
    pub dispatched_runs: usize,
    pub failed_runs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPlaneHealthSnapshot {
    pub timestamp: DateTime<Utc>,
    pub overall: AggregateHealthStatus,
    pub discovery: DiscoveryHealthAggregate,
    pub runtime: RuntimeHealthAggregate,
    pub scheduler: SchedulerHealthAggregate,
    pub startup_steps_completed: usize,
    pub startup_steps_total: usize,
}

/// Aggregate a point-in-time health snapshot from all subsystem handles.
pub fn aggregate_health_snapshot(control_plane: &ControlPlane) -> ControlPlaneHealthSnapshot {
    let discovery = aggregate_discovery(control_plane);
    let runtime = aggregate_runtime(control_plane);
    let scheduler = aggregate_scheduler(control_plane);

    let overall = if discovery.unreachable_nodes > 0 || scheduler.failed_runs > 0 {
        AggregateHealthStatus::Unhealthy
    } else if discovery.degraded_nodes > 0 || !runtime.healthy {
        AggregateHealthStatus::Degraded
    } else {
        AggregateHealthStatus::Healthy
    };

    ControlPlaneHealthSnapshot {
        timestamp: Utc::now(),
        overall,
        discovery,
        runtime,
        scheduler,
        startup_steps_completed: control_plane.startup_events.len(),
        startup_steps_total: control_plane.startup_plan.order.len(),
    }
}

fn aggregate_discovery(control_plane: &ControlPlane) -> DiscoveryHealthAggregate {
    let nodes = control_plane.handles.discovery.registry.list_nodes();

    let mut agg = DiscoveryHealthAggregate {
        total_nodes: nodes.len(),
        ..DiscoveryHealthAggregate::default()
    };

    for node in nodes {
        match node.health.as_ref().map(|h| &h.status) {
            Some(HealthStatus::Healthy) => agg.healthy_nodes += 1,
            Some(HealthStatus::Degraded) => agg.degraded_nodes += 1,
            Some(HealthStatus::Unreachable) => agg.unreachable_nodes += 1,
            None => agg.unknown_nodes += 1,
        }
    }

    agg
}

fn aggregate_runtime(control_plane: &ControlPlane) -> RuntimeHealthAggregate {
    match &control_plane.handles.runtime.last_status {
        Some(status) => RuntimeHealthAggregate {
            running: status.running,
            healthy: status.healthy,
            model_id: status.model_id.clone(),
            endpoint: status.endpoint.clone(),
        },
        None => RuntimeHealthAggregate::default(),
    }
}

fn aggregate_scheduler(control_plane: &ControlPlane) -> SchedulerHealthAggregate {
    let runs = control_plane.handles.scheduler.engine.list_runs();

    let mut agg = SchedulerHealthAggregate {
        total_jobs: control_plane.handles.scheduler.engine.list_jobs().len(),
        ..SchedulerHealthAggregate::default()
    };

    for run in runs {
        match run.status {
            RunStatus::Pending => agg.pending_runs += 1,
            RunStatus::Running => agg.running_runs += 1,
            RunStatus::Dispatched => agg.dispatched_runs += 1,
            RunStatus::Failed => agg.failed_runs += 1,
            RunStatus::Succeeded | RunStatus::Skipped => {}
        }
    }

    agg
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use ff_core::Role;
    use ff_core::config::{FleetConfig, FleetSettings, LeaderConfig, NodeConfig};
    use ff_discovery::{DiscoveredNode, HealthCheckResult};
    use ff_runtime::EngineStatus;

    use crate::bootstrap::BootstrapOptions;

    use super::*;

    fn sample_config() -> FleetConfig {
        FleetConfig {
            fleet: FleetSettings::default(),
            nodes: [(
                "taylor".to_string(),
                NodeConfig {
                    ip: "127.0.0.1".to_string(),
                    role: Role::Gateway,
                    election_priority: Some(1),
                    ram_gb: Some(64),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            models: vec![],
            leader: LeaderConfig::default(),
            ..Default::default()
        }
    }

    #[test]
    fn aggregates_healthy_when_runtime_reports_healthy() {
        let mut cp = ControlPlane::bootstrap(sample_config(), BootstrapOptions::default()).unwrap();

        cp.handles.runtime.last_status = Some(EngineStatus {
            running: true,
            healthy: true,
            pid: Some(12345),
            model_id: Some("qwen3-32b".to_string()),
            endpoint: Some("http://127.0.0.1:51800".to_string()),
            uptime_secs: Some(42),
        });

        let snapshot = aggregate_health_snapshot(&cp);
        assert_eq!(snapshot.overall, AggregateHealthStatus::Healthy);
        assert!(snapshot.runtime.running);
        assert!(snapshot.runtime.healthy);
    }

    #[tokio::test]
    async fn aggregates_discovery_and_scheduler_health() {
        let cp = ControlPlane::bootstrap(sample_config(), BootstrapOptions::default()).unwrap();

        let ip1 = IpAddr::V4(Ipv4Addr::new(192, 168, 5, 100));
        let ip2 = IpAddr::V4(Ipv4Addr::new(192, 168, 5, 101));

        cp.handles
            .discovery
            .registry
            .upsert_discovered_node(DiscoveredNode {
                ip: ip1,
                open_ports: vec![51800],
                discovered_at: Utc::now(),
            });
        cp.handles
            .discovery
            .registry
            .upsert_discovered_node(DiscoveredNode {
                ip: ip2,
                open_ports: vec![51800],
                discovered_at: Utc::now(),
            });

        let healthy = HealthCheckResult {
            name: "node-1".to_string(),
            host: ip1.to_string(),
            port: 51800,
            checked_at: Utc::now(),
            latency_ms: 10,
            tcp_ok: true,
            http_ok: Some(true),
            http_status: Some(200),
            status: HealthStatus::Healthy,
            error: None,
        };

        let unreachable = HealthCheckResult {
            name: "node-2".to_string(),
            host: ip2.to_string(),
            port: 51800,
            checked_at: Utc::now(),
            latency_ms: 500,
            tcp_ok: false,
            http_ok: Some(false),
            http_status: None,
            status: HealthStatus::Unreachable,
            error: Some("timeout".to_string()),
        };

        assert!(
            cp.handles
                .discovery
                .registry
                .update_health_by_ip(ip1, healthy)
        );
        assert!(
            cp.handles
                .discovery
                .registry
                .update_health_by_ip(ip2, unreachable)
        );

        let schedule = cp
            .schedule(crate::commands::ScheduleRequest {
                name: "test-job".to_string(),
                cron_expression: "* * * * *".to_string(),
                task: ff_cron::JobTask::LocalCommand {
                    command: "echo hi".to_string(),
                    timeout_secs: Some(1),
                },
                priority: ff_cron::JobPriority::Normal,
                dry_run: false,
            })
            .await
            .unwrap();

        assert!(schedule.persisted);

        let snapshot = aggregate_health_snapshot(&cp);
        assert_eq!(snapshot.discovery.total_nodes, 2);
        assert_eq!(snapshot.discovery.healthy_nodes, 1);
        assert_eq!(snapshot.discovery.unreachable_nodes, 1);
        assert_eq!(snapshot.scheduler.total_jobs, 1);
        assert_eq!(snapshot.overall, AggregateHealthStatus::Unhealthy);
    }
}
