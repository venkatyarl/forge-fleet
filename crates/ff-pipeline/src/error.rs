//! Pipeline-specific error types.

use crate::step::StepId;

/// Errors that can occur during pipeline construction or execution.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("duplicate step: {0}")]
    DuplicateStep(StepId),

    #[error("step not found: {0}")]
    StepNotFound(StepId),

    #[error("cycle detected in pipeline graph")]
    CycleDetected,

    #[error("pipeline execution failed: {0}")]
    ExecutionFailed(String),

    #[error("step execution error: {0}")]
    StepExecution(String),

    #[error("rust function registry not configured")]
    RustFnRegistryMissing,

    #[error("rust function not found in registry: {0}")]
    RustFnNotFound(String),

    #[error("rust function execution failed: {0}")]
    RustFnExecution(String),

    #[error("http request failed: {0}")]
    HttpRequest(String),

    #[error("http request returned status {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("llm request failed: {0}")]
    LlmRequest(String),

    #[error("llm response parse failed: {0}")]
    LlmResponse(String),

    #[error("step timed out: {0}")]
    StepTimeout(StepId),

    #[error("empty pipeline — nothing to execute")]
    EmptyPipeline,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, PipelineError>;
