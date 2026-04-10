//! Fleet Pulse error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PulseError {
    #[error("Redis connection failed: {0}")]
    Connection(#[from] redis::RedisError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("No metrics found for node: {0}")]
    NodeNotFound(String),

    #[error("Pulse subscriber error: {0}")]
    Subscriber(String),
}

pub type Result<T> = std::result::Result<T, PulseError>;
