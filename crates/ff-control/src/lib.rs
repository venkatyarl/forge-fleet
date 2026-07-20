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
pub mod config;
pub mod control_plane;
pub mod dispatcher;
pub mod errors;
pub mod executor;
pub mod ha_coordinator;
pub mod health;
pub mod timeout;

#[cfg(test)]
mod tests;

pub use bootstrap::{
    BootstrapOptions, BootstrapPlan, BootstrapValidation, StartupSubsystem, build_bootstrap_plan,
    validate_fleet_config, validate_startup_order,
};
pub use commands::{
    ControlCommand, ControlCommandResult, DeployRequest, DeployResult, DeployStrategy,
    DiscoverMode, DiscoverRequest, DiscoverResult, RunTaskRequest, RunTaskResult, ScheduleRequest,
    ScheduleResult, StartAgentRequest, StartAgentResult,
};
pub use config::{AlertConfig, ControlConfig, DeduplicationConfig};
pub use control_plane::{
    ControlPlane, ControlPlaneHandles, DeploySubsystemHandle, DiscoverySubsystemHandle,
    OrchestratorSubsystemHandle, RuntimeSubsystemHandle, SchedulerSubsystemHandle, StartupEvent,
    StartupStepStatus,
};
pub use dispatcher::{DEFAULT_LEASE_DURATION, DEFAULT_MAX_BUILD_DURATION, WorkItemDispatch};
pub use errors::{ControlError, Result};
pub use executor::{clear_slot_edit_lock, slot_edit_lock};
pub use ha_coordinator::{
    DEFAULT_MAX_REPLICATION_LAG_BYTES, HaAction, HaClusterEvent, HaCoordinator,
    PatroniClusterMember, PatroniClusterState, PatroniMemberRole,
};
pub use health::{
    AggregateHealthStatus, ControlPlaneHealthSnapshot, DiscoveryHealthAggregate,
    RuntimeHealthAggregate, SchedulerHealthAggregate, aggregate_health_snapshot,
};
