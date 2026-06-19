//! ForgeFleet core primitives.
//!
//! This crate provides the shared building blocks used across ForgeFleet:
//!
//! - Core types for nodes, models, hardware, tiers, runtimes, and roles.
//! - Fleet configuration loading, hot reload, and environment overrides.
//! - The unified [`ForgeFleetError`] error type.
//! - Postgres connection pool setup.
//! - Runtime hardware detection for OS, CPU, GPU, memory, and interconnects.
//! - Leader election types and failover logic.
//! - Activity level tracking and yield modes.
//! - Agent task and result types.

pub mod activity;
pub mod audit;
pub mod build_version;
pub mod chaos;
pub mod circuit_breaker;
pub mod computer;
pub mod config;
pub mod db;
pub mod db_health;
pub mod error;
pub mod fleet_resolver;
pub mod hardware;
pub mod leader;
pub mod maintenance;
pub mod model_id;
pub mod monitor;
pub mod notifications;
pub mod panic_hook;
pub mod quarantine;
pub mod run_limits;
pub mod synthetic;
pub mod task;
pub mod task_error;
pub mod types;
pub mod url;
pub mod verifier;

// Re-export the most commonly used items at crate root.
pub use activity::{ActivitySignals, ActivityState, YieldMode};
pub use chaos::{
    ChaosConfig, ChaosEngine, ChaosHooks, Simulation, SimulationId, SimulationState, SimulationType,
};
pub use circuit_breaker::{
    BackendId, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerRegistry,
    CircuitBreakerSnapshot, CircuitState,
};
pub use computer::{ActivityLevel, AgentRegistrationAck, WorkerRole};
pub use error::{ForgeFleetError, Result};
pub use fleet_resolver::{
    FleetNodeInfo, FleetResolveError, FleetResolver, resolve_fleet_nodes, resolve_fleet_nodes_sync,
};
pub use maintenance::{MaintenanceEntry, MaintenanceManager, MaintenancePhase, MaintenanceWindow};
pub use monitor::{
    AlertCondition, DiskMonitor, FleetMonitor, ModelMonitor, MonitorAlert, MonitorSettings,
    NodeMonitor,
};
pub use notifications::{NotificationLevel, NotificationSender, TelegramNotifier};
pub use quarantine::{NodeQuarantine, QuarantineEntry, QuarantinePolicy};
pub use run_limits::{RunLimits, should_escalate};
pub use synthetic::{
    BackupFreshnessProbe, DbWriteReadProbe, DiskSpaceProbe, HttpHealthProbe, LlmSmokeProbe,
    ProbeCategory, ProbeRegistry, ProbeResult, ProbeStatus, ReplicationLagProbe, SyntheticProbe,
};
pub use task::{AgentTask, AgentTaskKind, TaskResult};
pub use task_error::{TaskErrorClass, classify_task_error};
pub use types::*;
pub use verifier::{
    CategoryScore, HealthScorecard, HealthVerifier, ScorecardResponse, ScorecardSnapshot,
    VerifierConfig,
};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
