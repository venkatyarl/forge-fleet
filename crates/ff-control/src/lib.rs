//! `ff-control` — compile-safe control-plane facade for ForgeFleet.
//!
//! This crate wires together major subsystem handles from:
//! - `ff-core` (fleet config + task types)
//! - `ff-discovery` (scanner/registry/health)
//! - `ff-runtime` (engine config/status)
//! - `ff-orchestrator` (task router)
//! - `ff-cron` (scheduler engine)
//! - `ff-deploy` (deployment subsystem marker)

pub mod bootstrap;
pub mod commands;
pub mod control_plane;
pub mod errors;
pub mod health;

pub use bootstrap::{
    BootstrapOptions, BootstrapPlan, BootstrapValidation, StartupSubsystem, build_bootstrap_plan,
    validate_fleet_config, validate_startup_order,
};
pub use commands::{
    ControlCommand, ControlCommandResult, DeployRequest, DeployResult, DeployStrategy,
    DiscoverMode, DiscoverRequest, DiscoverResult, RunTaskRequest, RunTaskResult, ScheduleRequest,
    ScheduleResult, StartAgentRequest, StartAgentResult,
};
pub use control_plane::{
    ControlPlane, ControlPlaneHandles, DeploySubsystemHandle, DiscoverySubsystemHandle,
    OrchestratorSubsystemHandle, RuntimeSubsystemHandle, SchedulerSubsystemHandle, StartupEvent,
    StartupStepStatus,
};
pub use errors::{ControlError, Result};
pub use health::{
    AggregateHealthStatus, ControlPlaneHealthSnapshot, DiscoveryHealthAggregate,
    RuntimeHealthAggregate, SchedulerHealthAggregate, aggregate_health_snapshot,
};
