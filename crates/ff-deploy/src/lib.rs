//! `ff-deploy` — deployment/release orchestration primitives for ForgeFleet.
//!
//! This crate provides:
//! - release domain models (`release`)
//! - rollout strategy + planning (`strategy`, `rollout`)
//! - health gate evaluation (`health_gate`)
//! - rollback decisioning and planning (`rollback`)
//! - deployment orchestration interfaces (`deployer`)

pub mod deployer;
pub mod health_gate;
pub mod release;
pub mod rollback;
pub mod rollout;
pub mod strategy;

pub use deployer::{DeploymentAdapter, DeploymentOrchestrator, DeploymentReport, StepOutcome};
pub use health_gate::{
    HealthGate, HealthGateConfig, HealthGateEvaluation, HealthGateStatus, HealthSnapshot,
};
pub use release::{ReleaseChannel, ReleaseManifest, ReleaseRecord, ReleaseState};
pub use rollback::{
    RollbackAction, RollbackCause, RollbackContext, RollbackDecider, RollbackDecision,
    RollbackPlan, RollbackPlanner, RollbackSeverity, RollbackStep,
};
pub use rollout::{RolloutError, RolloutPhase, RolloutPlan, RolloutPlanner, RolloutStep};
pub use strategy::{CanaryStrategy, FullStrategy, RolloutStrategy, StagedStrategy, StrategyError};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
