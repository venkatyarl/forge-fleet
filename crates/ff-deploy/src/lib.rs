//! `ff-deploy` — deployment/release orchestration primitives for ForgeFleet.
//!
//! This crate provides:
//! - release domain models (`release`)
//! - deploy target resolution with retry (`resolution`)
//! - rollout strategy + planning (`strategy`, `rollout`)
//! - health gate evaluation (`health_gate`)
//! - rollback decisioning and planning (`rollback`)
//! - deployment orchestration interfaces (`deployer`)

pub mod config;
pub mod daemon;
pub mod deploy;
pub mod deployer;
pub mod git_utils;
pub mod health_gate;
pub mod node;
pub mod release;
pub mod resolution;
pub mod rollback;
pub mod rollout;
pub mod strategy;

pub use config::DeployConfig;
pub use daemon::{ActiveLease, RestartReport, restart_with_lease_drain};
pub use deploy::{
    LeaseSource, RestartCoordinator, git_fetch_and_reset_hard, git_stash_dirty_tree,
    git_tree_is_dirty,
};
pub use deployer::{DeploymentAdapter, DeploymentOrchestrator, DeploymentReport, StepOutcome};

pub use health_gate::{
    HealthGate, HealthGateConfig, HealthGateEvaluation, HealthGateStatus, HealthSnapshot,
};
pub use node::{
    drain_active_work_item_leases, forgefleetd_restart_command, restart_forgefleetd_local,
    restart_forgefleetd_local_with_drain, restart_forgefleetd_with_drain,
};
pub use release::{ReleaseChannel, ReleaseManifest, ReleaseRecord, ReleaseState};
pub use resolution::{ResolutionError, ResolutionRetryPolicy, ResolvedTarget, resolve_with_retry};
pub use rollback::{
    RollbackAction, RollbackCause, RollbackContext, RollbackDecider, RollbackDecision,
    RollbackPlan, RollbackPlanner, RollbackSeverity, RollbackStep,
};
pub use rollout::{RolloutError, RolloutPhase, RolloutPlan, RolloutPlanner, RolloutStep};
pub use strategy::{CanaryStrategy, FullStrategy, RolloutStrategy, StagedStrategy, StrategyError};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
