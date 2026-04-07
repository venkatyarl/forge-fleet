//! `ff-pipeline` — ForgeFleet DAG pipeline execution.
//!
//! Provides:
//! - Step and graph modeling
//! - Parallel dependency-aware execution
//! - Shell, Rust function, HTTP, and LLM step kinds
//! - Reusable pipeline templates

pub mod error;
pub mod executor;
pub mod graph;
pub mod registry;
pub mod step;
pub mod templates;

pub use error::{PipelineError, Result};
pub use executor::{ExecutorConfig, PipelineEvent, PipelineRunResult, execute};
pub use graph::PipelineGraph;
pub use registry::RustFnRegistry;
pub use step::{Step, StepConfig, StepId, StepKind, StepResult, StepStatus};
