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
pub use sqlx::PgPool;
pub use migrations::{run_migrations, run_postgres_migrations};
pub use operational_store::OperationalStore;
pub use queries::{
    DeferredTaskRow, FleetModelRow, FleetNodeRow, FleetSecretRow, MeshStatusRow,
    ModelCatalogRow, ModelDeploymentRow, ModelJobRow, ModelLibraryRow, NodeSshKeyRow,
    pg_append_routing_log, pg_cancel_deferred, pg_claim_deferred, pg_create_job,
    pg_delete_deployment, pg_delete_library, pg_delete_mesh_status_for_node,
    pg_delete_node_ssh_keys, pg_delete_secret, pg_enqueue_deferred, pg_finish_deferred,
    pg_get_catalog, pg_get_deferred, pg_get_node, pg_get_secret, pg_get_setting,
    pg_get_task_lineage, pg_insert_disk_usage, pg_insert_node_ssh_key,
    pg_latest_disk_usage, pg_list_catalog, pg_list_deferred, pg_list_deployments,
    pg_list_jobs, pg_list_library, pg_list_mesh_status, pg_list_models,
    pg_list_models_for_node, pg_list_node_ssh_keys, pg_list_nodes, pg_list_secrets,
    pg_promote_deferred, pg_retry_deferred, pg_scheduler_pass, pg_search_catalog, pg_set_secret,
    pg_set_setting, pg_update_job_progress, pg_upsert_catalog, pg_upsert_deployment,
    pg_upsert_library, pg_upsert_mesh_status, pg_upsert_model, pg_upsert_node,
    seed_from_fleet_toml, RoutingHop,
};
pub use runtime_registry::RuntimeRegistryStore;
pub use sync::{
    BackupScheduler, FollowerSync, LeaderSync, ReplicationBackupHelperAvailability, SyncConfig,
    SyncRole,
};

/// Convenience re-export of our error type.
pub use crate::error::DbError;

pub mod error {
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
