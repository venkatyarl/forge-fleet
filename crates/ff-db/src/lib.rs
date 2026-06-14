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
pub mod leader_state;
pub mod migrations;
pub mod operational_store;
pub mod queries;
pub mod replication;
pub mod runtime_registry;
pub mod schema;
pub mod sync;

pub use leader_state::*;

pub use connection::{DbPool, DbPoolConfig};
pub use migrations::{run_migrations, run_postgres_migrations};
pub use operational_store::OperationalStore;
pub use queries::{
    AgentReadinessRow,
    // Resource arbiter (V119)
    ArbiterReservedHost,
    BrainCandidateRow,
    BrainCommunityRow,
    BrainMessageRow,
    BrainReminderRow,
    BrainThreadRow,
    // Virtual Brain (V13)
    BrainUserRow,
    BrainVaultEdgeRow,
    BrainVaultNodeRow,
    ComputerScheduleRow,
    // Cortex recall diagnostic (`ff cortex doctor`)
    CortexResolutionStats,
    CortexSuspiciousExtern,
    DeferredTaskRow,
    // Orchestrator P2 — per-session demand sensing (V116)
    DemandVector,
    // Fleet agents catalog (V112)
    FleetAgentRow,
    FleetModelRow,
    FleetNodeRow,
    FleetSecretRow,
    // Interaction log (V121 ff_interactions)
    InteractionRecord,
    MeshStatusRow,
    ModelCatalogRow,
    ModelDeploymentRow,
    ModelJobRow,
    ModelLibraryRow,
    // Orchestrator P3 — adaptive serving-mix autoscaler
    PlacementCandidate,
    ReprofileCandidate,
    RouteCandidate,
    RouteFilter,
    RoutingHop,
    ServingEndpoint,
    ServingSupply,
    SharedVolumeMountRow,
    // Phase 12 (V19) — shared volumes / power schedules / training jobs
    SharedVolumeRow,
    TrainingJobRow,
    WorkIntentRow,
    WorkerSshKeyRow,
    load_fleet_config_from_postgres,
    pg_active_deployment_counts,
    pg_advance_intent_cursor,
    pg_agent_readiness,
    pg_append_benchmark_result,
    pg_append_routing_log,
    pg_append_training_loss_sample,
    pg_arbiter_free_set,
    pg_arbiter_grant_set,
    pg_archive_brain_thread,
    pg_attach_thread,
    pg_attach_training_deferred_task,
    pg_bump_vault_node_hits,
    pg_cancel_deferred,
    pg_claim_deferred,
    pg_cortex_resolution_stats,
    pg_cortex_suspicious_externs,
    pg_count_brain_vault_nodes_current,
    pg_count_corpus_code_symbols,
    pg_create_brain_thread,
    pg_create_brain_user,
    pg_create_job,
    pg_create_schedule,
    pg_create_shared_volume,
    pg_create_training_job,
    // Orchestrator P2 — demand sensing (V116)
    pg_current_demand_vector,
    pg_delete_deployment,
    pg_delete_library,
    pg_delete_mesh_status_for_node,
    pg_delete_node_ssh_keys,
    pg_delete_schedule,
    pg_delete_secret,
    pg_delete_shared_volume_mount,
    pg_disable_safety_gate,
    pg_enqueue_deferred,
    pg_finish_deferred,
    pg_fire_brain_reminder,
    pg_force_cancel_deferred,
    pg_get_agent,
    pg_get_attached_thread,
    pg_get_benchmark_results,
    pg_get_brain_thread,
    pg_get_brain_thread_by_id,
    pg_get_brain_user,
    pg_get_brain_user_by_id,
    pg_get_brain_vault_node,
    pg_get_catalog,
    pg_get_deferred,
    pg_get_node,
    pg_get_secret,
    pg_get_setting,
    pg_get_shared_volume,
    pg_get_task_lineage,
    pg_get_training_job,
    pg_get_work_intent,
    pg_insert_brain_candidate,
    pg_insert_brain_message,
    pg_insert_brain_reminder,
    pg_insert_disk_policy_run,
    pg_insert_disk_usage,
    pg_insert_node_ssh_key,
    pg_insert_work_intent,
    // Interaction log (V121 ff_interactions)
    pg_interaction_channel_counts,
    pg_latest_demand_snapshot,
    pg_latest_disk_usage,
    pg_list_agents,
    pg_list_brain_candidates_pending,
    pg_list_brain_communities,
    pg_list_brain_messages,
    pg_list_brain_threads,
    pg_list_brain_vault_edges_for_node,
    pg_list_brain_vault_nodes_current,
    pg_list_catalog,
    pg_list_deferred,
    pg_list_deployments,
    pg_list_due_reminders,
    pg_list_interactions,
    pg_list_jobs,
    pg_list_library,
    pg_list_mesh_status,
    pg_list_models,
    pg_list_models_for_node,
    pg_list_node_ssh_keys,
    pg_list_nodes,
    pg_list_reserved_hosts,
    pg_list_schedules,
    pg_list_secrets,
    pg_list_shared_volume_mounts,
    pg_list_shared_volumes,
    pg_list_training_jobs,
    pg_list_work_intents,
    // Orchestrator P3 — adaptive serving-mix autoscaler
    pg_loadable_library_for_kind,
    pg_mark_schedule_fired,
    pg_node_free_disk,
    pg_open_disk_move,
    pg_pending_work_intents,
    pg_pick_agent_endpoint,
    pg_pick_agent_endpoint_soft,
    pg_pick_offload_endpoint,
    pg_placement_candidates,
    pg_promote_deferred,
    pg_read_safety_gate,
    pg_reap_expired_leases,
    pg_reap_stale_reservations,
    pg_reap_stale_running,
    pg_recent_demand_snapshots,
    pg_record_interaction,
    pg_reprofile_candidates,
    pg_reserve_host,
    pg_resolve_channel_user,
    pg_retired_catalog_ids,
    pg_retry_deferred,
    pg_route_deployments,
    pg_scheduler_pass,
    pg_search_brain_vault_nodes,
    pg_search_catalog,
    pg_set_agent_enabled,
    pg_set_library_pinned,
    pg_set_secret,
    pg_set_setting,
    pg_set_vault_node_community,
    pg_set_work_intent_state,
    pg_snooze_brain_reminder,
    pg_supersede_vault_node,
    pg_supplied_slots_by_kind,
    pg_touch_brain_thread,
    pg_unreserve_host,
    pg_update_brain_candidate_status,
    pg_update_disk_move,
    pg_update_job_progress,
    pg_update_training_job_status,
    pg_upsert_agent,
    pg_upsert_brain_community,
    pg_upsert_brain_vault_edge,
    pg_upsert_brain_vault_node,
    pg_upsert_catalog,
    pg_upsert_channel_identity,
    pg_upsert_deployment,
    pg_upsert_library,
    pg_upsert_mesh_status,
    pg_upsert_model,
    pg_upsert_node,
    pg_upsert_shared_volume_mount,
    // Orchestrator P2 — demand sensing emission (V116)
    record_session_work_signal,
    seed_from_fleet_toml,
};
pub use runtime_registry::RuntimeRegistryStore;
pub use sqlx::PgPool;
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
