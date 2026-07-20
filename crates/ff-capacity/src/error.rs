//! Error types for ff-capacity.

use thiserror::Error;

/// Errors returned by the capacity snapshot.
#[derive(Debug, Error)]
pub enum CapacityError {
    /// Postgres returned an error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Shorthand result type.
pub type Result<T> = std::result::Result<T, CapacityError>;
