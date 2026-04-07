//! `ff-db` — ForgeFleet persistence adapters.
//!
//! Embedded SQLite + Postgres persistence adapters for ForgeFleet.
//!
//! - **connection** — SQLite connection pool with WAL mode, pragma tuning
//! - **migrations** — Forward-only SQLite schema versioning with embedded SQL
//! - **schema** — SQLite table definitions (nodes, models, tasks, memories, etc.)
//! - **queries** — Typed SQLite helpers for common CRUD operations
//! - **runtime_registry** — SQLite/Postgres abstraction for runtime node + enrollment tables
//! - **operational_store** — SQLite/Postgres abstraction for live operational tables
//! - **replication** — Leader→follower WAL-based sync via SQLite backup API
//! - **backup** — Periodic backup to file and restore
//! - **sync** — High-level replication coordinator (leader/follower sync loops, backup scheduler)

pub mod backup;
pub mod connection;
pub mod migrations;
pub mod operational_store;
pub mod queries;
pub mod replication;
pub mod runtime_registry;
pub mod schema;
pub mod sync;

pub use connection::{DbPool, DbPoolConfig};
pub use migrations::run_migrations;
pub use operational_store::OperationalStore;
pub use runtime_registry::RuntimeRegistryStore;
pub use sync::{
    BackupScheduler, FollowerSync, LeaderSync, ReplicationBackupHelperAvailability, SyncConfig,
    SyncRole,
};

/// Convenience re-export of our error type.
pub use crate::error::DbError;

mod error {
    /// Database-specific error type for ff-db.
    #[derive(Debug, thiserror::Error)]
    pub enum DbError {
        #[error("SQLite error: {0}")]
        Sqlite(#[from] rusqlite::Error),

        #[error("migration error: {0}")]
        Migration(String),

        #[error("connection pool error: {0}")]
        Pool(String),

        #[error("Postgres error: {0}")]
        Postgres(#[from] sqlx::Error),

        #[error("replication error: {0}")]
        Replication(String),

        #[error("backup error: {0}")]
        Backup(String),

        #[error("serialization error: {0}")]
        Serialization(#[from] serde_json::Error),

        #[error("IO error: {0}")]
        Io(#[from] std::io::Error),

        #[error("not found: {0}")]
        NotFound(String),
    }

    pub type Result<T> = std::result::Result<T, DbError>;
}

pub use error::Result;
