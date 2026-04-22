//! `ff-core` — ForgeFleet core primitives.
//!
//! This crate is the foundation of ForgeFleet, providing:
//! - **types** — Node, Model, Hardware, Tier, Runtime, Role, etc.
//! - **config** — Load/parse fleet.toml with hot-reload and env overrides
//! - **error** — Unified error type (`ForgeFleetError`)
//! - **db** — Postgres connection pool setup
//! - **hardware** — Detect OS, CPU, GPU, memory, interconnect at runtime
//! - **leader** — Leader election types and failover logic
//! - **activity** — Taylor yield modes and activity level tracking
//! - **task** — Agent task and result types

pub mod activity;
pub mod audit;
pub mod chaos;
pub mod circuit_breaker;
pub mod config;
pub mod db;
pub mod error;
pub mod hardware;
pub mod leader;
pub mod maintenance;
pub mod monitor;
pub mod node;
pub mod panic_hook;
pub mod notifications;
pub mod quarantine;
pub mod run_limits;
pub mod synthetic;
pub mod task;
pub mod types;
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
pub use error::{ForgeFleetError, Result};
pub use maintenance::{MaintenanceEntry, MaintenanceManager, MaintenancePhase, MaintenanceWindow};
pub use monitor::{
    AlertCondition, DiskMonitor, FleetMonitor, ModelMonitor, MonitorAlert, MonitorSettings,
    NodeMonitor,
};
pub use node::{ActivityLevel, AgentRegistrationAck, NodeRole};
pub use notifications::{NotificationLevel, NotificationSender, TelegramNotifier};
pub use quarantine::{NodeQuarantine, QuarantineEntry, QuarantinePolicy};
pub use run_limits::{RunLimits, should_escalate};
pub use synthetic::{
    BackupFreshnessProbe, DbWriteReadProbe, DiskSpaceProbe, HttpHealthProbe, LlmSmokeProbe,
    ProbeCategory, ProbeRegistry, ProbeResult, ProbeStatus, ReplicationLagProbe, SyntheticProbe,
};
pub use task::{AgentTask, AgentTaskKind, TaskResult};
pub use types::*;
pub use verifier::{
    CategoryScore, HealthScorecard, HealthVerifier, ScorecardResponse, ScorecardSnapshot,
    VerifierConfig,
};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
