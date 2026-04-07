//! Runtime-specific error types for ff-runtime.

use std::path::PathBuf;

/// Errors specific to inference engine management.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("engine not running")]
    NotRunning,

    #[error("engine already running on port {port}")]
    AlreadyRunning { port: u16 },

    #[error("failed to start engine: {reason}")]
    StartFailed { reason: String },

    #[error("failed to stop engine: {reason}")]
    StopFailed { reason: String },

    #[error("health check failed: {reason}")]
    HealthCheckFailed { reason: String },

    #[error("model file not found: {}", path.display())]
    ModelNotFound { path: PathBuf },

    #[error("model download failed: {reason}")]
    DownloadFailed { reason: String },

    #[error("unsupported runtime {runtime} on {os}")]
    UnsupportedPlatform { runtime: String, os: String },

    #[error("binary not found on PATH: {name}")]
    BinaryNotFound { name: String },

    #[error("quantization failed: {reason}")]
    QuantizationFailed { reason: String },

    #[error("timeout waiting for engine to become healthy")]
    HealthTimeout,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

/// Convenience result type for ff-runtime operations.
pub type Result<T> = std::result::Result<T, RuntimeError>;
