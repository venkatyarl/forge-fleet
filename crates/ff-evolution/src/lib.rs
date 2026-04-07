//! `ff-evolution` — continuous improvement and autonomous maintenance for ForgeFleet.
//!
//! This crate closes the loop between observed failures and durable improvements:
//! observe → analyze → propose → apply → verify → learn.

pub mod analyzer;
pub mod backlog;
pub mod learning;
pub mod r#loop;
pub mod repair;
pub mod verification;

pub use analyzer::{
    AnalysisReport, FailureAnalyzer, FailureCategory, FailureObservation, FailureSource, RootCause,
    RootCauseCategory,
};
pub use backlog::{BacklogItem, BacklogPriority, BacklogService, BacklogStatus};
pub use learning::{LearningOutcome, LearningRecord, LearningStore, SuppressionPolicy};
pub use r#loop::{EvolutionEngine, EvolutionPhase, EvolutionRun, EvolutionState};
pub use repair::{RepairAction, RepairPlanner, RepairRisk, RepairStatus, RepairStrategy};
pub use verification::{
    VerificationInput, VerificationModel, VerificationOutcome, VerificationReport,
};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
