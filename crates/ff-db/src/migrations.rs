//! Embedded migration runner.
//!
//! Migrations are SQL strings embedded in Rust, applied forward-only
//! with version tracking via a `_migrations` meta-table.

use sqlx::{Acquire, PgPool};
use tracing::{debug, info, warn};

use crate::error::{DbError, Result};
use crate::schema;

/// The highest migration version baked into the squashed fresh-DB bootstrap.
const BOOTSTRAP_BASELINE_VERSION: u32 = schema::PG_BASELINE_VERSION;

// ─── Postgres Migrations ─────────────────────────────────────────────────────

/// A single Postgres migration step.
struct PgMigration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// Postgres-only migrations. These run independently from the SQLite migrations
/// above and use their own version sequence.
static PG_MIGRATIONS: &[PgMigration] = &[
    PgMigration {
        version: 7,
        name: "fleet_config_tables",
        sql: schema::SCHEMA_V7_FLEET_POSTGRES,
    },
    PgMigration {
        version: 8,
        name: "task_provenance_schema",
        sql: schema::SCHEMA_V8_TASK_PROVENANCE,
    },
    PgMigration {
        version: 9,
        name: "fleet_secrets",
        sql: schema::SCHEMA_V9_FLEET_SECRETS,
    },
    PgMigration {
        version: 10,
        name: "deferred_tasks",
        sql: schema::SCHEMA_V10_DEFERRED_TASKS,
    },
    PgMigration {
        version: 11,
        name: "model_lifecycle",
        sql: schema::SCHEMA_V11_MODEL_LIFECYCLE,
    },
    PgMigration {
        version: 12,
        name: "onboarding_foundation",
        sql: schema::SCHEMA_V12_ONBOARDING,
    },
    PgMigration {
        version: 13,
        name: "virtual_brain",
        sql: schema::SCHEMA_V13_VIRTUAL_BRAIN,
    },
    PgMigration {
        version: 14,
        name: "computers_and_portfolio",
        sql: schema::SCHEMA_V14_COMPUTERS_AND_PORTFOLIO,
    },
    PgMigration {
        version: 15,
        name: "project_management",
        sql: schema::SCHEMA_V15_PROJECT_MANAGEMENT,
    },
    PgMigration {
        version: 16,
        name: "observability",
        sql: schema::SCHEMA_V16_OBSERVABILITY,
    },
    PgMigration {
        version: 17,
        name: "security_hardening",
        sql: schema::SCHEMA_V17_SECURITY_HARDENING,
    },
    PgMigration {
        version: 18,
        name: "network_scope",
        sql: schema::SCHEMA_V18_NETWORK_SCOPE,
    },
    PgMigration {
        version: 19,
        name: "storage_power_training",
        sql: schema::SCHEMA_V19_STORAGE_POWER_TRAINING,
    },
    PgMigration {
        version: 20,
        name: "port_registry",
        sql: schema::SCHEMA_V20_PORT_REGISTRY,
    },
    PgMigration {
        version: 21,
        name: "drop_deployment_model_fk",
        sql: schema::SCHEMA_V21_DROP_DEPLOYMENT_FK,
    },
    PgMigration {
        version: 22,
        name: "drop_model_presence_fk",
        sql: schema::SCHEMA_V22_DROP_MODEL_PRESENCE_FK,
    },
    PgMigration {
        version: 23,
        name: "sub_agents",
        sql: schema::SCHEMA_V23_SUB_AGENTS,
    },
    PgMigration {
        version: 24,
        name: "external_tools",
        sql: schema::SCHEMA_V24_EXTERNAL_TOOLS,
    },
    PgMigration {
        version: 25,
        name: "social_media_ingest",
        sql: schema::SCHEMA_V25_SOCIAL_MEDIA_INGEST,
    },
    PgMigration {
        version: 26,
        name: "cloud_llm_providers",
        sql: schema::SCHEMA_V26_CLOUD_LLM_PROVIDERS,
    },
    PgMigration {
        version: 27,
        name: "pool_aliases",
        sql: schema::SCHEMA_V27_POOL_ALIASES,
    },
    PgMigration {
        version: 28,
        name: "software_registry_seed",
        sql: schema::SCHEMA_V28_SOFTWARE_REGISTRY_SEED,
    },
    PgMigration {
        version: 29,
        name: "fix_ff_git_linux_playbook",
        sql: schema::SCHEMA_V29_FIX_FF_GIT_LINUX_PLAYBOOK,
    },
    PgMigration {
        version: 30,
        name: "playbook_self_heal_repo",
        sql: schema::SCHEMA_V30_PLAYBOOK_SELF_HEAL_REPO,
    },
    PgMigration {
        version: 31,
        name: "source_tree_path",
        sql: schema::SCHEMA_V31_SOURCE_TREE_PATH,
    },
    PgMigration {
        version: 32,
        name: "playbook_bugfixes",
        sql: schema::SCHEMA_V32_PLAYBOOK_BUGFIXES,
    },
    PgMigration {
        version: 33,
        name: "cli_aliases",
        sql: schema::SCHEMA_V33_CLI_ALIASES,
    },
    PgMigration {
        version: 34,
        name: "retire_alert_policies_toml",
        sql: schema::SCHEMA_V34_RETIRE_ALERT_POLICIES_TOML,
    },
    PgMigration {
        version: 35,
        name: "retire_cloud_llm_providers_toml",
        sql: schema::SCHEMA_V35_RETIRE_CLOUD_LLM_PROVIDERS_TOML,
    },
    PgMigration {
        version: 36,
        name: "retire_task_coverage_toml",
        sql: schema::SCHEMA_V36_RETIRE_TASK_COVERAGE_TOML,
    },
    PgMigration {
        version: 37,
        name: "retire_ports_toml",
        sql: schema::SCHEMA_V37_RETIRE_PORTS_TOML,
    },
    PgMigration {
        version: 38,
        name: "retire_external_tools_toml",
        sql: schema::SCHEMA_V38_RETIRE_EXTERNAL_TOOLS_TOML,
    },
    PgMigration {
        version: 39,
        name: "retire_model_catalog_toml",
        sql: schema::SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML,
    },
    PgMigration {
        version: 40,
        name: "agent_session_on_work_outputs",
        sql: schema::SCHEMA_V40_AGENT_SESSION_ON_WORK_OUTPUTS,
    },
    PgMigration {
        version: 41,
        name: "per_arch_build_leader",
        sql: schema::SCHEMA_V41_PER_ARCH_BUILD_LEADER,
    },
    PgMigration {
        version: 42,
        name: "research_subsystem",
        sql: schema::SCHEMA_V42_RESEARCH_SUBSYSTEM,
    },
    PgMigration {
        version: 43,
        name: "multi_host_and_self_heal",
        sql: schema::SCHEMA_V43_MULTI_HOST_AND_SELF_HEAL,
    },
    PgMigration {
        version: 44,
        name: "fleet_tasks",
        sql: schema::SCHEMA_V44_FLEET_TASKS,
    },
    PgMigration {
        version: 45,
        name: "beat_age_alerts",
        sql: schema::SCHEMA_V45_BEAT_AGE_ALERTS,
    },
    PgMigration {
        version: 46,
        name: "npm_cli_catalog",
        sql: schema::SCHEMA_V46_NPM_CLI_CATALOG,
    },
    PgMigration {
        version: 47,
        name: "fabric_measurements_and_docker",
        sql: schema::SCHEMA_V47_FABRIC_MEASUREMENTS_AND_DOCKER,
    },
    PgMigration {
        version: 48,
        name: "upgrade_playbook_restart_fix",
        sql: schema::SCHEMA_V48_UPGRADE_PLAYBOOK_RESTART_FIX,
    },
    PgMigration {
        version: 49,
        name: "connectivity_mode_and_eligibility",
        sql: schema::SCHEMA_V49_CONNECTIVITY_MODE_AND_ELIGIBILITY,
    },
    PgMigration {
        version: 50,
        name: "seed_canonical_ports",
        sql: schema::SCHEMA_V50_SEED_CANONICAL_PORTS,
    },
    PgMigration {
        version: 51,
        name: "idempotent_upgrade_playbook",
        sql: schema::SCHEMA_V51_IDEMPOTENT_UPGRADE_PLAYBOOK,
    },
    PgMigration {
        version: 52,
        name: "wait_for_siblings_barrier",
        sql: schema::SCHEMA_V52_WAIT_FOR_SIBLINGS_BARRIER,
    },
    PgMigration {
        version: 53,
        name: "oauth_subscription_providers",
        sql: schema::SCHEMA_V53_OAUTH_SUBSCRIPTION_PROVIDERS,
    },
    PgMigration {
        version: 54,
        name: "agent_orchestration",
        sql: schema::SCHEMA_V54_AGENT_ORCHESTRATION,
    },
    PgMigration {
        version: 55,
        name: "session_brain",
        sql: schema::SCHEMA_V55_SESSION_BRAIN,
    },
    PgMigration {
        version: 56,
        name: "retire_last_tomls_and_cli_build",
        sql: schema::SCHEMA_V56_RETIRE_LAST_TOMLS_AND_CLI_BUILD,
    },
    PgMigration {
        version: 57,
        name: "macos_ff_git_parity",
        sql: schema::SCHEMA_V57_MACOS_FF_GIT_PARITY,
    },
    PgMigration {
        version: 58,
        name: "kill_switch_ttl",
        sql: schema::SCHEMA_V58_KILL_SWITCH_TTL,
    },
    PgMigration {
        version: 59,
        name: "openclaw_macos_sudo",
        sql: schema::SCHEMA_V59_OPENCLAW_MACOS_SUDO,
    },
    PgMigration {
        version: 60,
        name: "auto_upgrade_memory",
        sql: schema::SCHEMA_V60_AUTO_UPGRADE_MEMORY,
    },
    PgMigration {
        version: 61,
        name: "peer_driven_upgrades",
        sql: schema::SCHEMA_V61_PEER_DRIVEN_UPGRADES,
    },
    PgMigration {
        version: 63,
        name: "drop_need_build_shortcut",
        sql: schema::SCHEMA_V63_DROP_NEED_BUILD_SHORTCUT,
    },
    PgMigration {
        version: 64,
        name: "register_ff_forgefleetd",
        sql: schema::SCHEMA_V64_REGISTER_FF_FORGEFLEETD,
    },
    PgMigration {
        version: 65,
        name: "register_open_design",
        sql: schema::SCHEMA_V65_REGISTER_OPEN_DESIGN,
    },
    PgMigration {
        version: 66,
        name: "data_driven_detection",
        sql: schema::SCHEMA_V66_DATA_DRIVEN_DETECTION,
    },
    PgMigration {
        version: 67,
        name: "auto_install_agent_hint",
        sql: schema::SCHEMA_V67_AUTO_INSTALL_AGENT_HINT,
    },
    PgMigration {
        version: 69,
        name: "skill_sources",
        sql: schema::SCHEMA_V69_SKILL_SOURCES,
    },
    PgMigration {
        version: 70,
        name: "fleet_model_catalog_qwen36",
        sql: schema::SCHEMA_V70_FLEET_MODEL_CATALOG_QWEN36,
    },
    PgMigration {
        version: 71,
        name: "backfill_fleet_model_catalog",
        sql: schema::SCHEMA_V71_BACKFILL_FLEET_MODEL_CATALOG,
    },
    PgMigration {
        version: 72,
        name: "sqlite_consolidation",
        sql: schema::SCHEMA_V72_SQLITE_CONSOLIDATION,
    },
    PgMigration {
        version: 73,
        name: "fleet_tool_registry",
        sql: schema::SCHEMA_V73_FLEET_TOOL_REGISTRY,
    },
    PgMigration {
        version: 74,
        name: "routing_mode",
        sql: schema::SCHEMA_V74_ROUTING_MODE,
    },
    PgMigration {
        version: 75,
        name: "work_items",
        sql: schema::SCHEMA_V75_WORK_ITEMS,
    },
    PgMigration {
        version: 76,
        name: "vault_sync",
        sql: schema::SCHEMA_V76_VAULT_SYNC,
    },
    PgMigration {
        version: 77,
        name: "fleet_task_notify",
        sql: schema::SCHEMA_V77_FLEET_TASK_NOTIFY,
    },
    PgMigration {
        version: 78,
        name: "pgvector_embeddings",
        sql: schema::SCHEMA_V78_PGVECTOR_EMBEDDINGS,
    },
    PgMigration {
        version: 79,
        name: "project_schedules",
        sql: schema::SCHEMA_V79_PROJECT_SCHEDULES,
    },
    PgMigration {
        version: 80,
        name: "agent_procedures",
        sql: schema::SCHEMA_V80_AGENT_PROCEDURES,
    },
    PgMigration {
        version: 81,
        name: "security_hardening",
        sql: schema::SCHEMA_V81_SECURITY_HARDENING,
    },
    PgMigration {
        version: 82,
        name: "rename_fleet_node_ssh_keys",
        sql: schema::SCHEMA_V82_RENAME_FLEET_NODE_SSH_KEYS,
    },
    PgMigration {
        version: 83,
        name: "rename_fleet_nodes",
        sql: schema::SCHEMA_V83_RENAME_FLEET_NODES,
    },
    PgMigration {
        version: 84,
        name: "rename_node_name_column",
        sql: schema::SCHEMA_V84_RENAME_NODE_NAME_COLUMN,
    },
    PgMigration {
        version: 85,
        name: "drop_compat_views",
        sql: schema::SCHEMA_V85_DROP_COMPAT_VIEWS,
    },
    PgMigration {
        version: 86,
        name: "drop_fleet_members",
        sql: schema::SCHEMA_V86_DROP_FLEET_MEMBERS,
    },
    PgMigration {
        version: 87,
        name: "rename_node_name_columns",
        sql: schema::SCHEMA_V87_RENAME_NODE_NAME_COLUMNS,
    },
    PgMigration {
        version: 88,
        name: "rename_fleet_node_runtime",
        sql: schema::SCHEMA_V88_RENAME_FLEET_NODE_RUNTIME,
    },
    PgMigration {
        version: 89,
        name: "github_ssh_aliases",
        sql: schema::SCHEMA_V89_GITHUB_SSH_ALIASES,
    },
    PgMigration {
        version: 90,
        name: "deployment_desired_state",
        sql: schema::SCHEMA_V90_DEPLOYMENT_DESIRED_STATE,
    },
    PgMigration {
        version: 91,
        name: "task_models_seed",
        sql: schema::SCHEMA_V91_TASK_MODELS,
    },
    PgMigration {
        version: 92,
        name: "ff_git_linux_parity",
        sql: schema::SCHEMA_V92_FF_GIT_LINUX_PARITY,
    },
    PgMigration {
        version: 93,
        name: "backfill_fleet_worker_runtime",
        sql: schema::SCHEMA_V93_BACKFILL_FLEET_WORKER_RUNTIME,
    },
    PgMigration {
        version: 94,
        name: "bge_quant_fix",
        sql: schema::SCHEMA_V94_BGE_QUANT_FIX,
    },
    PgMigration {
        version: 95,
        name: "bge_embedding_dim_1024",
        sql: schema::SCHEMA_V95_BGE_EMBEDDING_DIM,
    },
    PgMigration {
        version: 96,
        name: "register_pipeline_llm_alias",
        sql: schema::SCHEMA_V96_REGISTER_PIPELINE_LLM_ALIAS,
    },
    PgMigration {
        version: 97,
        name: "redis_nats_5digit_remap",
        sql: schema::SCHEMA_V97_REDIS_NATS_5DIGIT,
    },
    PgMigration {
        version: 98,
        name: "gemma4_repo_fix",
        sql: schema::SCHEMA_V98_GEMMA4_REPO_FIX,
    },
    PgMigration {
        version: 99,
        name: "default_pool_alias",
        sql: schema::SCHEMA_V99_DEFAULT_POOL_ALIAS,
    },
    PgMigration {
        version: 100,
        name: "retire_qwen25",
        sql: schema::SCHEMA_V100_RETIRE_QWEN25,
    },
    PgMigration {
        version: 101,
        name: "upgrade_playbook_refresh",
        sql: schema::SCHEMA_V101_UPGRADE_PLAYBOOK_REFRESH,
    },
    PgMigration {
        version: 102,
        name: "wave_self_kill_fix",
        sql: schema::SCHEMA_V102_WAVE_SELF_KILL_FIX,
    },
    PgMigration {
        version: 103,
        name: "retire_qwen2_vl",
        sql: schema::SCHEMA_V103_RETIRE_QWEN2_VL,
    },
    PgMigration {
        version: 104,
        name: "wave_disown_fix",
        sql: schema::SCHEMA_V104_WAVE_DISOWN_FIX,
    },
    PgMigration {
        version: 105,
        name: "skills_v1",
        sql: schema::SCHEMA_V105_SKILLS,
    },
    PgMigration {
        version: 106,
        name: "model_library_state",
        sql: schema::SCHEMA_V106_MODEL_LIBRARY_STATE,
    },
    PgMigration {
        version: 107,
        name: "dispatcher_foundation",
        sql: schema::SCHEMA_V107_DISPATCHER_FOUNDATION,
    },
    PgMigration {
        version: 108,
        name: "task_depends_on",
        sql: schema::SCHEMA_V108_TASK_DEPENDS_ON,
    },
    PgMigration {
        version: 109,
        name: "open_design_corepack_fix",
        sql: schema::SCHEMA_V109_OPEN_DESIGN_COREPACK_FIX,
    },
    PgMigration {
        version: 110,
        name: "amcheck_integrity",
        sql: schema::SCHEMA_V110_AMCHECK_INTEGRITY,
    },
    PgMigration {
        version: 111,
        name: "agent_swarm_data_plane",
        sql: schema::SCHEMA_V111_AGENT_SWARM_DATA_PLANE,
    },
    PgMigration {
        version: 112,
        name: "fleet_agents",
        sql: schema::SCHEMA_V112_FLEET_AGENTS,
    },
    PgMigration {
        version: 113,
        name: "coder_tool_calling",
        sql: schema::SCHEMA_V113_CODER_TOOL_CALLING,
    },
    PgMigration {
        version: 114,
        name: "node_reservation",
        sql: schema::SCHEMA_V114_NODE_RESERVATION,
    },
    PgMigration {
        version: 115,
        name: "agent_catalog",
        sql: schema::SCHEMA_V115_AGENT_CATALOG,
    },
    PgMigration {
        version: 116,
        name: "session_demand",
        sql: schema::SCHEMA_V116_SESSION_DEMAND,
    },
    PgMigration {
        version: 117,
        name: "brain_faceted_graph",
        sql: schema::SCHEMA_V117_BRAIN_FACETED_GRAPH,
    },
    PgMigration {
        version: 118,
        name: "disk_management",
        sql: schema::SCHEMA_V118_DISK_MANAGEMENT,
    },
    PgMigration {
        version: 119,
        name: "resource_arbiter",
        sql: schema::SCHEMA_V119_RESOURCE_ARBITER,
    },
    PgMigration {
        version: 120,
        name: "fleet_conformance",
        sql: schema::SCHEMA_V120_FLEET_CONFORMANCE,
    },
    PgMigration {
        // NOTE: V121 was already consumed by `cortex_code_graph` (applied to the
        // live DB during the overnight Cortex session) before this migration
        // merged. Because the runner only applies `version > current`, keeping
        // this at 121 meant it NEVER ran — `ff_interactions` was never created
        // and every interaction-log capture hook silently no-op'd. Renumbered to
        // 122 (the next free version) so it actually executes. Idempotent
        // (CREATE TABLE IF NOT EXISTS), so re-running anywhere is safe.
        version: 122,
        name: "interaction_log",
        sql: schema::SCHEMA_V122_INTERACTION_LOG,
    },
    PgMigration {
        version: 123,
        name: "cortex_file_index",
        sql: schema::SCHEMA_V123_CORTEX_FILE_INDEX,
    },
    PgMigration {
        version: 124,
        name: "cortex_symbol_lines",
        sql: schema::SCHEMA_V124_CORTEX_SYMBOL_LINES,
    },
    PgMigration {
        version: 125,
        name: "brain_community_registry",
        sql: schema::SCHEMA_V125_BRAIN_COMMUNITY_REGISTRY,
    },
    PgMigration {
        version: 126,
        name: "community_god_node_ondelete",
        sql: schema::SCHEMA_V126_COMMUNITY_GOD_NODE_ONDELETE,
    },
    PgMigration {
        version: 127,
        name: "cortex_code_communities",
        sql: schema::SCHEMA_V127_CORTEX_CODE_COMMUNITIES,
    },
    PgMigration {
        version: 128,
        name: "cortex_reexports",
        sql: schema::SCHEMA_V128_CORTEX_REEXPORTS,
    },
    PgMigration {
        version: 129,
        name: "docker_latest_tag",
        sql: schema::SCHEMA_V129_DOCKER_LATEST_TAG,
    },
    PgMigration {
        version: 130,
        name: "backup_restore_drill",
        sql: schema::SCHEMA_V130_BACKUP_RESTORE_DRILL,
    },
    PgMigration {
        version: 131,
        name: "fleet_integrity",
        sql: schema::SCHEMA_V131_FLEET_INTEGRITY,
    },
    PgMigration {
        version: 132,
        name: "evolution_backlog",
        sql: schema::SCHEMA_V132_EVOLUTION_BACKLOG,
    },
    PgMigration {
        version: 133,
        name: "leader_maintenance_lease",
        sql: schema::SCHEMA_V133_LEADER_MAINTENANCE_LEASE,
    },
    PgMigration {
        version: 134,
        name: "upgrade_rollouts",
        sql: schema::SCHEMA_V134_UPGRADE_ROLLOUTS,
    },
    PgMigration {
        version: 135,
        name: "integrity_active_repairs",
        sql: schema::SCHEMA_V135_INTEGRITY_ACTIVE_REPAIRS,
    },
    PgMigration {
        version: 136,
        name: "dsn_of_record",
        sql: schema::SCHEMA_V136_DSN_OF_RECORD,
    },
    PgMigration {
        version: 137,
        name: "gate_previous_value",
        sql: schema::SCHEMA_V137_GATE_PREVIOUS_VALUE,
    },
    PgMigration {
        version: 138,
        name: "interaction_worker_attribution",
        sql: schema::SCHEMA_V138_INTERACTION_WORKER_ATTRIBUTION,
    },
    PgMigration {
        version: 139,
        name: "agent_scratchpad",
        sql: schema::SCHEMA_V139_AGENT_SCRATCHPAD,
    },
    PgMigration {
        version: 140,
        name: "distributed_dev_workitems",
        sql: schema::SCHEMA_V140_DISTRIBUTED_DEV,
    },
    PgMigration {
        version: 141,
        name: "project_repos_folders",
        sql: schema::SCHEMA_V141_PROJECT_REPOS_FOLDERS,
    },
    PgMigration {
        version: 142,
        name: "cortex_universal_foundation",
        sql: schema::SCHEMA_V142_CORTEX_FOUNDATION,
    },
    PgMigration {
        version: 143,
        name: "project_git_policy",
        sql: schema::SCHEMA_V143_PROJECT_GIT_POLICY,
    },
    PgMigration {
        version: 144,
        name: "code_community_levels",
        sql: schema::SCHEMA_V144_CODE_COMMUNITY_LEVELS,
    },
    PgMigration {
        version: 145,
        name: "code_community_parent",
        sql: schema::SCHEMA_V145_CODE_COMMUNITY_PARENT,
    },
    PgMigration {
        version: 146,
        name: "disable_dead_computer_offline_alert",
        sql: schema::SCHEMA_V146_DISABLE_DEAD_COMPUTER_OFFLINE_ALERT,
    },
    PgMigration {
        version: 147,
        name: "telegram_sessions",
        sql: schema::SCHEMA_V147_TELEGRAM_SESSIONS,
    },
    PgMigration {
        version: 148,
        name: "computer_backends",
        sql: schema::SCHEMA_V148_COMPUTER_BACKENDS,
    },
    PgMigration {
        version: 149,
        name: "provider_routing",
        sql: schema::SCHEMA_V149_PROVIDER_ROUTING,
    },
    PgMigration {
        version: 150,
        name: "kimi_cli_external_tool",
        sql: schema::SCHEMA_V150_KIMI_CLI_EXTERNAL_TOOL,
    },
    PgMigration {
        version: 151,
        name: "computer_backends_path",
        sql: schema::SCHEMA_V151_COMPUTER_BACKENDS_PATH,
    },
    PgMigration {
        version: 152,
        name: "work_item_repo_binding",
        sql: schema::SCHEMA_V152_WORK_ITEM_REPO_BINDING,
    },
    PgMigration {
        version: 153,
        name: "retire_v75_work_stealing",
        sql: schema::SCHEMA_V153_RETIRE_V75_WORK_STEALING,
    },
    PgMigration {
        version: 154,
        name: "nested_subagent_workspace",
        sql: schema::SCHEMA_V154_NESTED_SUBAGENT_WORKSPACE,
    },
    PgMigration {
        version: 155,
        name: "drop_dead_bridge",
        sql: schema::SCHEMA_V155_DROP_DEAD_BRIDGE,
    },
    PgMigration {
        version: 156,
        name: "fleet_tasks_fold_columns",
        sql: schema::SCHEMA_V156_FLEET_TASKS_FOLD_COLUMNS,
    },
    PgMigration {
        version: 157,
        name: "fold_research_subtasks",
        sql: schema::SCHEMA_V157_FOLD_RESEARCH_SUBTASKS,
    },
    PgMigration {
        version: 158,
        name: "fold_self_heal_queue",
        sql: schema::SCHEMA_V158_FOLD_SELF_HEAL_QUEUE,
    },
    PgMigration {
        version: 159,
        name: "fold_deferred_tasks",
        sql: schema::SCHEMA_V159_FOLD_DEFERRED_TASKS,
    },
    PgMigration {
        version: 160,
        name: "notify_dedup",
        sql: schema::SCHEMA_V160_NOTIFY_DEDUP,
    },
    PgMigration {
        version: 161,
        name: "canonical_github_alias",
        sql: schema::SCHEMA_V161_CANONICAL_GITHUB_ALIAS,
    },
    PgMigration {
        version: 162,
        name: "drop_worktree_path_unique",
        sql: schema::SCHEMA_V162_DROP_WORKTREE_PATH_UNIQUE,
    },
    PgMigration {
        version: 163,
        name: "fleet_backup_config",
        sql: schema::SCHEMA_V163_FLEET_BACKUP_CONFIG,
    },
    // V164 is claimed by in-flight branch wi/a3ce533f6de1 — take 165.
    PgMigration {
        version: 165,
        name: "server_policy",
        sql: schema::SCHEMA_V165_SERVER_POLICY,
    },
    PgMigration {
        version: 166,
        name: "task_notification_outbox",
        sql: schema::SCHEMA_V166_TASK_NOTIFICATION_OUTBOX,
    },
    // V166 was claimed by task_notification_outbox on main first — telegram
    // reply routing takes 167 (collision caught by the versions-strictly-
    // increasing unit test).
    PgMigration {
        version: 167,
        name: "telegram_reply_routing",
        sql: schema::SCHEMA_V167_TELEGRAM_REPLY_ROUTING,
    },
    PgMigration {
        version: 168,
        name: "work_item_context",
        sql: schema::SCHEMA_V168_WORK_ITEM_CONTEXT,
    },
    PgMigration {
        version: 169,
        name: "peer_mount_inventory",
        sql: schema::SCHEMA_V169_PEER_MOUNT_INVENTORY,
    },
    PgMigration {
        version: 170,
        name: "work_queue",
        sql: schema::SCHEMA_V170_WORK_QUEUE,
    },
    PgMigration {
        version: 171,
        name: "artifact_index",
        sql: schema::SCHEMA_V171_ARTIFACT_INDEX,
    },
    // 172 was reserved by the metrics-schema branch, but 173–176 landed before
    // it did — the runner only applies versions ABOVE the current one, so a
    // late 172 would be silently skipped on any DB already at 173+. The
    // metrics schema landed as 177 instead; 172 stays a permanent gap (gaps
    // are fine, duplicates are not — see
    // migration_versions_are_strictly_increasing).
    PgMigration {
        version: 173,
        name: "computers_ip_ram_atomic",
        sql: schema::SCHEMA_V173_COMPUTERS_IP_RAM_ATOMIC,
    },
    PgMigration {
        version: 174,
        name: "dispatch_tick_at",
        sql: schema::SCHEMA_V174_DISPATCH_TICK_AT,
    },
    PgMigration {
        version: 175,
        name: "deployment_metrics_scrapes",
        sql: schema::SCHEMA_V175_DEPLOYMENT_METRICS_SCRAPES,
    },
    PgMigration {
        version: 176,
        name: "merge_trains",
        sql: schema::SCHEMA_V176_MERGE_TRAINS,
    },
    PgMigration {
        version: 177,
        name: "fleet_metrics",
        sql: schema::SCHEMA_V177_FLEET_METRICS,
    },
    PgMigration {
        version: 178,
        name: "error_events",
        sql: schema::SCHEMA_V178_ERROR_EVENTS,
    },
    PgMigration {
        version: 179,
        name: "work_item_events",
        sql: schema::SCHEMA_V179_WORK_ITEM_EVENTS,
    },
    PgMigration {
        version: 180,
        name: "model_capacity_view",
        sql: schema::SCHEMA_V180_MODEL_CAPACITY_VIEW,
    },
    PgMigration {
        version: 181,
        name: "fleet_velocity_views",
        sql: schema::SCHEMA_V181_FLEET_VELOCITY_VIEWS,
    },
    PgMigration {
        version: 182,
        name: "work_item_events_trigger",
        sql: schema::SCHEMA_V182_WORK_ITEM_EVENTS_TRIGGER,
    },
    PgMigration {
        version: 183,
        name: "artifact_cache_index",
        sql: schema::SCHEMA_V183_ARTIFACT_CACHE_INDEX,
    },
    PgMigration {
        version: 184,
        name: "postgres_replica_dead_alert",
        sql: schema::SCHEMA_V184_POSTGRES_REPLICA_DEAD_ALERT,
    },
    PgMigration {
        version: 185,
        name: "sub_agents_kind",
        sql: schema::SCHEMA_V185_SUB_AGENTS_KIND,
    },
    PgMigration {
        version: 186,
        name: "computer_metrics_rollups",
        sql: schema::SCHEMA_V186_COMPUTER_METRICS_ROLLUPS,
    },
    PgMigration {
        version: 187,
        name: "ssh_mesh_degraded_alert",
        sql: schema::SCHEMA_V187_SSH_MESH_DEGRADED_ALERT,
    },
    PgMigration {
        version: 188,
        name: "align_subagent_paths_to_nested_full_clone",
        sql: schema::SCHEMA_V188_ALIGN_SUBAGENT_PATHS,
    },
    PgMigration {
        version: 189,
        name: "fleet_capacity_registry",
        sql: schema::SCHEMA_V189_FLEET_CAPACITY_REGISTRY,
    },
    PgMigration {
        version: 190,
        name: "merge_queue_inplace_review",
        sql: schema::SCHEMA_V190_MERGE_QUEUE_INPLACE_REVIEW,
    },
    PgMigration {
        version: 191,
        name: "cloud_budget_buckets",
        sql: schema::SCHEMA_V191_CLOUD_BUDGET_BUCKETS,
    },
    PgMigration {
        version: 192,
        name: "postgres_wal_archiving_config",
        sql: schema::SCHEMA_V192_POSTGRES_WAL_ARCHIVING_CONFIG,
    },
    PgMigration {
        version: 193,
        name: "stale_local_backup_alert",
        sql: schema::SCHEMA_V193_STALE_LOCAL_BACKUP_ALERT,
    },
    PgMigration {
        version: 194,
        name: "merge_queue_review_fields",
        sql: schema::SCHEMA_V194_MERGE_QUEUE_REVIEW_FIELDS,
    },
    PgMigration {
        version: 195,
        name: "bootstrap_v161_v1_baseline",
        sql: schema::SCHEMA_V195_BOOTSTRAP_V161_V1_BASELINE,
    },
    PgMigration {
        version: 196,
        name: "computer_dispatch_tick",
        sql: schema::SCHEMA_V196_COMPUTER_DISPATCH_TICK,
    },
    PgMigration {
        version: 197,
        name: "operator_alert_dedup_counts",
        sql: schema::SCHEMA_V197_OPERATOR_ALERT_DEDUP_COUNTS,
    },
    PgMigration {
        version: 198,
        name: "auto_backlog_feeder",
        sql: schema::SCHEMA_V198_AUTO_BACKLOG_FEEDER,
    },
    PgMigration {
        version: 199,
        name: "continuous_rollout",
        sql: schema::SCHEMA_V199_CONTINUOUS_ROLLOUT,
    },
    PgMigration {
        version: 200,
        name: "review_ladder_mode",
        sql: schema::SCHEMA_V200_REVIEW_LADDER_MODE,
    },
    PgMigration {
        version: 201,
        name: "folder_owned_pr_review",
        sql: schema::SCHEMA_V201_FOLDER_OWNED_PR_REVIEW,
    },
    PgMigration {
        version: 202,
        name: "artifact_cache_holders",
        sql: schema::SCHEMA_V202_ARTIFACT_CACHE_HOLDERS,
    },
    PgMigration {
        version: 203,
        name: "work_item_provenance",
        sql: schema::SCHEMA_V203_WORK_ITEM_PROVENANCE,
    },
    PgMigration {
        version: 204,
        name: "work_item_velocity_instrumentation",
        sql: schema::SCHEMA_V204_WORK_ITEM_VELOCITY_INSTRUMENTATION,
    },
    PgMigration {
        version: 205,
        name: "mcp_bootstrap_generation",
        sql: schema::SCHEMA_V205_MCP_BOOTSTRAP_GENERATION,
    },
    PgMigration {
        version: 206,
        name: "model_endpoint_metrics",
        sql: schema::SCHEMA_V206_MODEL_ENDPOINT_METRICS,
    },
    PgMigration {
        version: 207,
        name: "merge_queue_review_tracking",
        sql: schema::SCHEMA_V207_MERGE_QUEUE_REVIEW_TRACKING,
    },
    PgMigration {
        version: 208,
        name: "work_items_parked",
        sql: schema::SCHEMA_V208_WORK_ITEMS_PARKED,
    },
    PgMigration {
        version: 209,
        name: "calendar_monitoring",
        sql: schema::SCHEMA_V209_CALENDAR_MONITORING,
    },
    PgMigration {
        version: 210,
        name: "fleet_capacity_registry_view",
        sql: schema::SCHEMA_V210_FLEET_CAPACITY_REGISTRY_VIEW,
    },
    PgMigration {
        version: 211,
        name: "decommission_taylor_github_identity",
        sql: schema::SCHEMA_V211_DECOMMISSION_TAYLOR_GITHUB_IDENTITY,
    },
    PgMigration {
        version: 212,
        name: "computer_metrics_retained_view",
        sql: schema::SCHEMA_V212_COMPUTER_METRICS_RETAINED_VIEW,
    },
    PgMigration {
        version: 213,
        name: "bootstrap_v161_baseline",
        sql: schema::SCHEMA_V213_BOOTSTRAP_V161_BASELINE,
    },
    PgMigration {
        version: 214,
        name: "self_heal_bug_history",
        sql: schema::SCHEMA_V214_SELF_HEAL_BUG_HISTORY,
    },
    PgMigration {
        version: 215,
        name: "sub_agent_capacity_boundary",
        sql: schema::SCHEMA_V215_SUB_AGENT_CAPACITY_BOUNDARY,
    },
    PgMigration {
        version: 216,
        name: "mesh_probe_diagnostics",
        sql: schema::SCHEMA_V216_MESH_PROBE_DIAGNOSTICS,
    },
    PgMigration {
        version: 217,
        name: "jira_monitoring",
        sql: schema::SCHEMA_V217_JIRA_MONITORING,
    },
    PgMigration {
        version: 218,
        name: "fabric_pair_model_columns",
        sql: schema::SCHEMA_V218_FABRIC_PAIR_MODEL_COLUMNS,
    },
    PgMigration {
        version: 219,
        name: "slm_health_monitor",
        sql: schema::SCHEMA_V219_SLM_HEALTH_MONITOR,
    },
    PgMigration {
        version: 220,
        name: "autonomous_work_item_loop",
        sql: schema::SCHEMA_V220_AUTONOMOUS_WORK_ITEM_LOOP,
    },
    PgMigration {
        version: 221,
        name: "service_connectivity_status",
        sql: schema::SCHEMA_V221_SERVICE_CONNECTIVITY_STATUS,
    },
    PgMigration {
        version: 222,
        name: "retire_code_review_graph",
        sql: schema::SCHEMA_V222_RETIRE_CODE_REVIEW_GRAPH,
    },
    PgMigration {
        version: 223,
        name: "real_sized_model_catalog",
        sql: schema::SCHEMA_V223_REAL_SIZED_MODEL_CATALOG,
    },
    PgMigration {
        version: 224,
        name: "cloud_budget_bucket_seeds",
        sql: schema::SCHEMA_V224_CLOUD_BUDGET_BUCKET_SEEDS,
    },
    PgMigration {
        version: 225,
        name: "movable_leader_lease",
        sql: schema::SCHEMA_V225_MOVABLE_LEADER_LEASE,
    },
    PgMigration {
        version: 226,
        name: "registry_hygiene",
        sql: schema::SCHEMA_V226_REGISTRY_HYGIENE,
    },
    PgMigration {
        version: 227,
        name: "computers_primary_ip_upsert_key",
        sql: schema::SCHEMA_V227_COMPUTERS_PRIMARY_IP_UPSERT_KEY,
    },
    PgMigration {
        version: 228,
        name: "model_server_metrics",
        sql: schema::SCHEMA_V228_MODEL_SERVER_METRICS,
    },
];

/// Postgres advisory-lock key guarding the migration runner.
///
/// Multiple processes call [`run_postgres_migrations`] concurrently —
/// forgefleetd's startup runner races any `ff` subcommand that opens the
/// pool at the same moment. Without serialization both read the same current
/// version, both compute the same `pending` list, both apply the next
/// migration's (idempotent) DDL, and then the second runner's
/// `INSERT INTO _migrations` violates `_migrations_pkey` and the process
/// aborts. On hosts under launchd/systemd KeepAlive the retry papers over it;
/// a host without auto-restart (or a bad-timing window) does NOT self-heal.
///
/// A session-level [`pg_advisory_lock`] serializes runners: the first holds
/// the lock for the whole run; the rest block, then wake to find the version
/// already advanced and nothing pending. The key is an arbitrary fixed
/// `i64` ("FFMIGRT8" in ASCII) — it only needs to be identical across every
/// binary that might run migrations against the same database, so it must
/// never change.
const MIGRATION_ADVISORY_LOCK_KEY: i64 = 0x46464D4947525438;

/// Ensure the Postgres `_migrations` tracking table exists.
async fn ensure_pg_migrations_table(conn: &mut sqlx::PgConnection) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version     INTEGER PRIMARY KEY,
            name        TEXT NOT NULL,
            applied_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Get the current Postgres schema version (0 if no migrations applied).
async fn pg_current_version(conn: &mut sqlx::PgConnection) -> Result<u32> {
    let row: (i32,) = sqlx::query_as("SELECT COALESCE(MAX(version), 0) FROM _migrations")
        .fetch_one(&mut *conn)
        .await?;
    Ok(row.0 as u32)
}

/// Run all pending Postgres migrations.
///
/// Idempotent — re-running on an up-to-date database is a no-op. Concurrent
/// callers are serialized via a session-level advisory lock
/// (see [`MIGRATION_ADVISORY_LOCK_KEY`]) so they can never collide on the
/// `_migrations` primary key.
pub async fn run_postgres_migrations(pool: &PgPool) -> Result<u32> {
    // Hold one connection for the whole run: the advisory lock is
    // session-scoped, so the lock and every migration query must share it.
    let mut conn = pool.acquire().await?;

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await?;

    let result = run_postgres_migrations_locked(&mut conn).await;

    // Always release before this connection returns to the pool — a pooled
    // connection handed back still holding the lock would leak it to the next
    // borrower and wedge every future migration run.
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_ADVISORY_LOCK_KEY)
        .execute(&mut *conn)
        .await
    {
        warn!(error = %e, "failed to release migration advisory lock");
    }

    result
}

/// Apply pending Postgres migrations on a connection that already holds the
/// migration advisory lock. Split out so the lock is acquired/released around
/// it exactly once in [`run_postgres_migrations`].
async fn run_postgres_migrations_locked(conn: &mut sqlx::PgConnection) -> Result<u32> {
    ensure_pg_migrations_table(&mut *conn).await?;
    let mut current = pg_current_version(&mut *conn).await?;

    // Fresh DB: apply the squashed v161 baseline instead of replaying the
    // legacy 7→161 migration chain, which has accumulated rename/renumber
    // drift and fails on a clean Postgres.
    if current == 0 {
        info!(
            baseline = BOOTSTRAP_BASELINE_VERSION,
            "fresh postgres database detected; applying bootstrap baseline"
        );

        let mut tx = conn.begin().await?;
        match sqlx::raw_sql(schema::BOOTSTRAP_V161_SQL)
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {
                tx.commit().await?;
                info!(
                    baseline = BOOTSTRAP_BASELINE_VERSION,
                    "postgres bootstrap baseline applied successfully"
                );
            }
            Err(e) => {
                return Err(DbError::Migration(format!(
                    "postgres bootstrap baseline (through v{BOOTSTRAP_BASELINE_VERSION}) failed: {e}"
                )));
            }
        }

        current = pg_current_version(&mut *conn).await?;
        if current < BOOTSTRAP_BASELINE_VERSION {
            return Err(DbError::Migration(format!(
                "postgres bootstrap baseline did not advance version to v{BOOTSTRAP_BASELINE_VERSION}; got v{current}"
            )));
        }
    }

    let pending: Vec<&PgMigration> = PG_MIGRATIONS
        .iter()
        .filter(|m| m.version > current)
        .collect();

    if pending.is_empty() {
        debug!(current_version = current, "postgres database is up to date");
        return Ok(current);
    }

    info!(
        current_version = current,
        pending = pending.len(),
        "running {} pending postgres migration(s)",
        pending.len()
    );

    for migration in &pending {
        info!(
            version = migration.version,
            name = migration.name,
            "applying postgres migration"
        );

        // Run DDL via raw_sql (supports multi-statement), then record version.
        let mut tx = conn.begin().await?;

        match sqlx::raw_sql(migration.sql).execute(&mut *tx).await {
            Ok(_) => {
                sqlx::query("INSERT INTO _migrations (version, name) VALUES ($1, $2)")
                    .bind(migration.version as i32)
                    .bind(migration.name)
                    .execute(&mut *tx)
                    .await?;

                tx.commit().await?;
                info!(
                    version = migration.version,
                    "postgres migration applied successfully"
                );
            }
            Err(e) => {
                // Transaction is dropped (rolled back) on error.
                warn!(version = migration.version, error = %e, "postgres migration failed");
                return Err(DbError::Migration(format!(
                    "postgres migration v{} '{}' failed: {e}",
                    migration.version, migration.name
                )));
            }
        }
    }

    let final_version = pg_current_version(&mut *conn).await?;
    info!(version = final_version, "all postgres migrations applied");
    Ok(final_version)
}

#[cfg(test)]
mod tests {
    use std::env;

    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[test]
    fn migration_advisory_lock_key_is_stable() {
        // The key must be identical across every binary version that runs
        // migrations against the same database, or concurrent runners on
        // mismatched binaries would not serialize. Pin it so a refactor can't
        // silently change it. (positive i64, fits pg's bigint advisory key.)
        assert_eq!(MIGRATION_ADVISORY_LOCK_KEY, 0x46464D4947525438);
        assert!(MIGRATION_ADVISORY_LOCK_KEY > 0);
    }

    #[test]
    fn migration_versions_are_strictly_increasing() {
        // Many builds land migrations concurrently; two branches claiming the
        // same version number both compile and both pass CI in isolation, so
        // the FIRST place a collision can be caught is here, at merge time,
        // when both entries are in the list. A duplicate (or out-of-order)
        // version would make the runner's applied-version bookkeeping skip or
        // double-apply SQL. Gaps are fine (versions get reserved by in-flight
        // branches); duplicates and regressions are not.
        for pair in PG_MIGRATIONS.windows(2) {
            assert!(
                pair[0].version < pair[1].version,
                "PG_MIGRATIONS out of order or duplicated: {} ({}) then {} ({})",
                pair[0].version,
                pair[0].name,
                pair[1].version,
                pair[1].name,
            );
        }
    }

    fn db_url() -> Option<String> {
        env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| env::var("FORGEFLEET_DATABASE_URL"))
            .ok()
    }

    async fn create_fresh_temp_db() -> Option<(PgPool, PgPool, String)> {
        let base_url = db_url()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_bootstrap_v161_{}", uuid::Uuid::new_v4().simple());
        let admin_url = format!("{prefix}/postgres");
        let db_url = format!("{prefix}/{db_name}");

        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .ok()?;

        // The bootstrap baseline requires pgcrypto, pgvector, and amcheck.
        // Skip the test if the server doesn't have them available.
        let extensions_ready: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pgcrypto')
                AND EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'vector')
                AND EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'amcheck')",
        )
        .fetch_one(&admin)
        .await
        .ok()?;
        if !extensions_ready {
            admin.close().await;
            return None;
        }

        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .ok()?;

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .ok()?;

        Some((admin, pool, db_name))
    }

    async fn drop_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .expect("terminate temp db sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
        admin.close().await;
    }

    #[tokio::test]
    async fn bootstrap_fresh_db_starts_at_v161() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        let final_version = run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let expected_version = PG_MIGRATIONS
            .last()
            .map(|m| m.version)
            .unwrap_or(BOOTSTRAP_BASELINE_VERSION);
        assert!(
            final_version >= BOOTSTRAP_BASELINE_VERSION,
            "expected at least v{BOOTSTRAP_BASELINE_VERSION}, got v{final_version}"
        );
        assert_eq!(
            final_version, expected_version,
            "expected final version v{expected_version}, got v{final_version}"
        );

        let row: (i32,) = sqlx::query_as("SELECT version FROM _migrations WHERE version = $1")
            .bind(BOOTSTRAP_BASELINE_VERSION as i32)
            .fetch_one(&pool)
            .await
            .expect("v161 bootstrap should be recorded in _migrations");
        assert_eq!(row.0 as u32, BOOTSTRAP_BASELINE_VERSION);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v173_rejects_partial_primary_ip_ram_updates() {
        // Needs Postgres — create_fresh_temp_db returns None (and we early-
        // return) when neither FORGEFLEET_POSTGRES_URL nor
        // FORGEFLEET_DATABASE_URL is set, so this never panics in CI.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO computers (name, primary_ip, os_family, ssh_user)
             VALUES ('v173-guard-test', '10.0.0.1', 'linux-ubuntu', 'ff')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert test computer");

        // The row has no RAM recorded yet: moving primary_ip alone would
        // leave a half-updated hardware identity.
        let err = sqlx::query("UPDATE computers SET primary_ip = '10.0.0.2' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect_err("primary_ip change with NULL total_ram_gb must be rejected");
        assert!(
            err.to_string().contains("partial update rejected"),
            "unexpected error: {err}"
        );

        // Both halves carried in one statement: allowed.
        sqlx::query(
            "UPDATE computers SET primary_ip = '10.0.0.2', total_ram_gb = 64 WHERE id = $1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .expect("paired primary_ip + total_ram_gb update should pass");

        // Moving the IP while wiping RAM in the same statement: rejected.
        let err = sqlx::query(
            "UPDATE computers SET primary_ip = '10.0.0.3', total_ram_gb = NULL WHERE id = $1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .expect_err("primary_ip change that wipes total_ram_gb must be rejected");
        assert!(
            err.to_string().contains("partial update rejected"),
            "unexpected error: {err}"
        );

        // Changing RAM while blanking the IP: rejected.
        let err =
            sqlx::query("UPDATE computers SET primary_ip = '', total_ram_gb = 128 WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await
                .expect_err("total_ram_gb change that blanks primary_ip must be rejected");
        assert!(
            err.to_string().contains("partial update rejected"),
            "unexpected error: {err}"
        );

        // Updates that touch neither column are unaffected by the guard.
        sqlx::query("UPDATE computers SET status = 'online' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("unrelated column update should pass");

        // The rejected statements rolled back: the last good pair survives.
        let (ip, ram): (String, Option<i32>) =
            sqlx::query_as("SELECT primary_ip, total_ram_gb FROM computers WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("re-read test computer");
        assert_eq!(ip, "10.0.0.2");
        assert_eq!(ram, Some(64));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v227_supports_atomic_computer_upsert_by_primary_ip() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let first_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO computers (name, primary_ip, total_ram_gb)
             VALUES ('v227-old-name', '10.0.0.227', 32)
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert initial computer");

        let upserted_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO computers (name, primary_ip, total_ram_gb)
             VALUES ('v227-new-name', '10.0.0.227', 64)
             ON CONFLICT (primary_ip) WHERE btrim(primary_ip) <> ''
             DO UPDATE SET name = EXCLUDED.name,
                           total_ram_gb = EXCLUDED.total_ram_gb
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("atomically upsert computer by primary_ip");

        assert_eq!(upserted_id, first_id);
        let row: (String, i32, i64) = sqlx::query_as(
            "SELECT MIN(name), MIN(total_ram_gb), COUNT(*)
               FROM computers
              WHERE primary_ip = '10.0.0.227'",
        )
        .fetch_one(&pool)
        .await
        .expect("read upserted computer");
        assert_eq!(row, ("v227-new-name".into(), 64, 1));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v178_error_events_round_trip() {
        // Needs Postgres — create_fresh_temp_db returns None (and we early-
        // return) when neither FORGEFLEET_POSTGRES_URL nor
        // FORGEFLEET_DATABASE_URL is set, so this never panics in CI.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let event = crate::ErrorEventInsert {
            worker_name: "v178-test-node".to_string(),
            deployment_id: None,
            library_id: None,
            catalog_id: Some("qwen3-coder-30b".to_string()),
            runtime: "llama.cpp".to_string(),
            error_kind: "load".to_string(),
            summary: "resolve gguf for /tmp/model: no .gguf files".to_string(),
            details: serde_json::json!({"port": 55000}),
            stderr_tail: Some("slot load_model: id 0 | new slot".to_string()),
        };
        let id = crate::pg_insert_error_event(&pool, &event)
            .await
            .expect("insert error event");
        assert!(id > 0);

        let rows = crate::pg_list_error_events(&pool, Some("v178-test-node"), None, 10)
            .await
            .expect("list error events");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].worker_name, "v178-test-node");
        assert_eq!(rows[0].error_kind, "load");
        assert_eq!(rows[0].runtime, "llama.cpp");

        let filtered = crate::pg_list_error_events(&pool, None, Some("load"), 10)
            .await
            .expect("list error events by kind");
        assert_eq!(filtered.len(), 1);

        let none = crate::pg_list_error_events(&pool, None, Some("oom"), 10)
            .await
            .expect("list error events by kind oom");
        assert!(none.is_empty());

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v179_work_item_events_round_trip() {
        // Needs Postgres — create_fresh_temp_db returns None (and we early-
        // return) when neither FORGEFLEET_POSTGRES_URL nor
        // FORGEFLEET_DATABASE_URL is set, so this never panics in CI.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        sqlx::query(
            "INSERT INTO projects (id, display_name) VALUES ('v179-test-proj', 'V179 Test')",
        )
        .execute(&pool)
        .await
        .expect("insert test project");
        let work_item_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO work_items (project_id, kind, title)
             VALUES ('v179-test-proj', 'task', 'v179 test item') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert test work item");

        sqlx::query(
            "INSERT INTO work_item_events
                 (work_item_id, from_status, to_status, computer, attempt, detail)
             VALUES ($1, 'idea', 'in_progress', 'v179-test-node', 1,
                     'test/local')",
        )
        .bind(work_item_id)
        .execute(&pool)
        .await
        .expect("insert work item event");

        let (from_status, to_status, computer, attempt): (
            Option<String>,
            String,
            Option<String>,
            Option<i32>,
        ) = sqlx::query_as(
            "SELECT from_status, to_status, computer, attempt
             FROM work_item_events WHERE work_item_id = $1",
        )
        .bind(work_item_id)
        .fetch_one(&pool)
        .await
        .expect("read back work item event");
        assert_eq!(from_status.as_deref(), Some("idea"));
        assert_eq!(to_status, "in_progress");
        assert_eq!(computer.as_deref(), Some("v179-test-node"));
        assert_eq!(attempt, Some(1));

        // Deleting the work item cascades to its events.
        sqlx::query("DELETE FROM work_items WHERE id = $1")
            .bind(work_item_id)
            .execute(&pool)
            .await
            .expect("delete test work item");
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_item_events WHERE work_item_id = $1")
                .bind(work_item_id)
                .fetch_one(&pool)
                .await
                .expect("count events after cascade");
        assert_eq!(remaining, 0);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v180_model_capacity_view_freshness_gate() {
        // Needs Postgres — create_fresh_temp_db returns None (and we early-
        // return) when neither FORGEFLEET_POSTGRES_URL nor
        // FORGEFLEET_DATABASE_URL is set, so this never panics in CI.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        sqlx::query("INSERT INTO fleet_workers (name, ip) VALUES ('v180-test-node', '10.0.0.1')")
            .execute(&pool)
            .await
            .expect("insert test worker");
        let deployment_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fleet_model_deployments
                 (worker_name, catalog_id, runtime, port, health_status)
             VALUES ('v180-test-node', 'qwen3-coder-30b', 'llama.cpp', 55000, 'healthy')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert test deployment");

        // No scrape samples yet → status is 'unknown' with NULL metrics.
        let (status, tokens_per_sec): (String, Option<f64>) = sqlx::query_as(
            "SELECT status, tokens_per_sec FROM model_capacity WHERE deployment_id = $1",
        )
        .bind(deployment_id)
        .fetch_one(&pool)
        .await
        .expect("read view without samples");
        assert_eq!(status, "unknown");
        assert!(tokens_per_sec.is_none());

        // A stale sample (older than the 90s freshness gate) → still 'unknown'.
        sqlx::query(
            "INSERT INTO deployment_metrics_scrapes
                 (deployment_id, worker_name, port, tokens_per_sec, queue_depth, scraped_at)
             VALUES ($1, 'v180-test-node', 55000, 5.0, 9, NOW() - INTERVAL '5 minutes')",
        )
        .bind(deployment_id)
        .execute(&pool)
        .await
        .expect("insert stale scrape");
        let status: String =
            sqlx::query_scalar("SELECT status FROM model_capacity WHERE deployment_id = $1")
                .bind(deployment_id)
                .fetch_one(&pool)
                .await
                .expect("read view with stale sample");
        assert_eq!(status, "unknown");

        // A fresh sample → status passes through health_status, and the view
        // reports the newest sample's metrics, not the stale one's.
        sqlx::query(
            "INSERT INTO deployment_metrics_scrapes
                 (deployment_id, worker_name, port, tokens_per_sec, queue_depth, scraped_at)
             VALUES ($1, 'v180-test-node', 55000, 42.5, 2, NOW())",
        )
        .bind(deployment_id)
        .execute(&pool)
        .await
        .expect("insert fresh scrape");
        let (computer, status, tokens_per_sec, queue_depth): (
            String,
            String,
            Option<f64>,
            Option<i32>,
        ) = sqlx::query_as(
            "SELECT computer, status, tokens_per_sec, queue_depth
             FROM model_capacity WHERE deployment_id = $1",
        )
        .bind(deployment_id)
        .fetch_one(&pool)
        .await
        .expect("read view with fresh sample");
        assert_eq!(computer, "v180-test-node");
        assert_eq!(status, "healthy");
        assert_eq!(tokens_per_sec, Some(42.5));
        assert_eq!(queue_depth, Some(2));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v204_fleet_velocity_views_use_authoritative_sources() {
        // Needs Postgres — create_fresh_temp_db returns None (and we early-
        // return) when neither FORGEFLEET_POSTGRES_URL nor
        // FORGEFLEET_DATABASE_URL is set, so this never panics in CI.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let detail_type: String = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'work_item_events'
               AND column_name = 'detail'",
        )
        .fetch_one(&pool)
        .await
        .expect("read event detail type");
        assert_eq!(detail_type, "text");

        let view_columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name || '.' || column_name
             FROM information_schema.columns
             WHERE table_schema = 'public'
               AND ((table_name = 'v_throughput_hourly' AND column_name = 'merge_count')
                 OR (table_name = 'v_lead_time_daily' AND column_name IN
                     ('avg_lead_time_seconds', 'p50_lead_time_seconds', 'p90_lead_time_seconds'))
                 OR (table_name = 'v_computer_builds_daily' AND column_name IN
                     ('build_count', 'avg_build_minutes'))
                 OR (table_name = 'v_first_pass_rate_daily' AND column_name = 'first_pass_rate'))
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .expect("read velocity view columns");
        assert_eq!(view_columns.len(), 7);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v184_rolls_up_and_prunes_computer_metrics_history() {
        // CI has no Postgres. The helper returns None unless one of the two
        // supported database URL variables is set, so this test never panics.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");
        let computer_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO computers (name, primary_ip, total_ram_gb)
             VALUES ('v184-metrics-node', '127.0.0.184', 64) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert computer");
        sqlx::query(
            "INSERT INTO computer_metrics_history
                (computer_id, recorded_at, cpu_pct, disk_free_gb,
                 llm_queue_depth, llm_active_requests)
             VALUES
                ($1, date_trunc('hour', NOW() - INTERVAL '8 days') + INTERVAL '1 minute', 20, 50, 1, 0),
                ($1, date_trunc('hour', NOW() - INTERVAL '8 days') + INTERVAL '2 minutes', 40, 45, 3, 2),
                ($1, NOW() - INTERVAL '1 day', 70, 40, 4, 1)",
        )
        .bind(computer_id)
        .execute(&pool)
        .await
        .expect("insert samples");

        crate::queries::pg_maintain_computer_metrics_history(&pool)
            .await
            .expect("maintain metrics");
        let (samples, cpu, disk): (i64, f64, f64) = sqlx::query_as(
            "SELECT sample_count, cpu_pct, disk_free_gb
               FROM computer_metrics_history_hourly WHERE computer_id = $1",
        )
        .bind(computer_id)
        .fetch_one(&pool)
        .await
        .expect("hourly rollup");
        assert_eq!(samples, 2);
        assert_eq!(cpu, 30.0);
        assert_eq!(disk, 45.0);
        let raw: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM computer_metrics_history WHERE computer_id = $1",
        )
        .bind(computer_id)
        .fetch_one(&pool)
        .await
        .expect("count retained raw");
        assert_eq!(raw, 1);
        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v212_exposes_all_retained_metrics_tiers() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let tiers: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT resolution
               FROM computer_metrics_history_retained
              ORDER BY resolution",
        )
        .fetch_all(&pool)
        .await
        .expect("read retained metrics view");
        // An empty history cannot prove UNION branches at runtime, so verify
        // the view definition also retains every tier name.
        let definition: String = sqlx::query_scalar(
            "SELECT pg_get_viewdef('computer_metrics_history_retained'::regclass, true)",
        )
        .fetch_one(&pool)
        .await
        .expect("read retained metrics view definition");
        assert!(tiers.is_empty());
        for tier in ["raw", "hourly", "daily"] {
            assert!(
                definition.contains(tier),
                "missing {tier} tier: {definition}"
            );
        }

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v197_adds_operator_alert_dedup_counts() {
        // CI has no Postgres. The helper returns None unless one of the two
        // supported database URL variables is set.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
             WHERE table_schema = 'public'
               AND table_name = 'operator_notify_dedup'
               AND column_name IN ('suppressed_count', 'send_count')
             ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .expect("read operator dedup columns");
        assert_eq!(columns, vec!["send_count", "suppressed_count"]);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v203_creates_work_item_provenance() {
        // CI has no Postgres. Keep this integration test optional on both
        // supported database URL variables.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'work_item_provenance'
             ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .expect("read provenance columns");
        assert!(columns.contains(&"builder_port".to_string()));
        assert!(columns.contains(&"reviewer_port".to_string()));
        assert!(columns.contains(&"cleanup_detail".to_string()));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v211_decommissions_taylor_github_identity() {
        // CI has no Postgres. Keep this integration test optional on both
        // supported database URL variables.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let aliases: Vec<(String, bool)> = sqlx::query_as(
            "SELECT alias_name, is_canonical
               FROM github_ssh_aliases
              WHERE alias_name IN ('github.com-venkat', 'github.com-taylor')
              ORDER BY alias_name",
        )
        .fetch_all(&pool)
        .await
        .expect("read GitHub identities");
        assert_eq!(aliases, vec![("github.com-venkat".to_string(), true)]);

        let legacy_secrets: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fleet_secrets
              WHERE key IN ('github_ssh_id_taylor_priv', 'github_ssh_id_taylor_pub')",
        )
        .fetch_one(&pool)
        .await
        .expect("count legacy GitHub secrets");
        assert_eq!(legacy_secrets, 0);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v215_disables_excess_slots_lazily() {
        // CI has no Postgres. Keep this integration test optional on both
        // supported database URL variables.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        sqlx::query(
            "INSERT INTO fleet_workers (name, ip, sub_agent_count)
             VALUES ('v215-test-node', '10.0.0.215', 2)",
        )
        .execute(&pool)
        .await
        .expect("insert test worker");
        let computer_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO computers (name, primary_ip, os_family)
             VALUES ('v215-test-node', '10.0.0.215', 'linux-ubuntu')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert test computer");

        sqlx::query(
            "INSERT INTO sub_agents (computer_id, slot, status, workspace_dir)
             VALUES ($1, 1, 'idle', '/tmp/slot-1'),
                    ($1, 2, 'busy', '/tmp/slot-2')",
        )
        .bind(computer_id)
        .execute(&pool)
        .await
        .expect("insert in-range and busy excess slots");

        let initial: Vec<(i32, String)> =
            sqlx::query_as("SELECT slot, status FROM sub_agents ORDER BY slot")
                .fetch_all(&pool)
                .await
                .expect("read initial statuses");
        assert_eq!(initial, vec![(1, "idle".into()), (2, "busy".into())]);

        sqlx::query("UPDATE sub_agents SET status = 'idle' WHERE slot = 2")
            .execute(&pool)
            .await
            .expect("release excess slot");
        let released_status: String =
            sqlx::query_scalar("SELECT status FROM sub_agents WHERE slot = 2")
                .fetch_one(&pool)
                .await
                .expect("read released status");
        assert_eq!(released_status, "disabled");

        sqlx::query("UPDATE fleet_workers SET sub_agent_count = 3 WHERE name = 'v215-test-node'")
            .execute(&pool)
            .await
            .expect("grow capacity");
        sqlx::query("UPDATE sub_agents SET status = 'idle' WHERE slot = 2")
            .execute(&pool)
            .await
            .expect("re-enable newly in-range slot");
        let grown_status: String =
            sqlx::query_scalar("SELECT status FROM sub_agents WHERE slot = 2")
                .fetch_one(&pool)
                .await
                .expect("read grown status");
        assert_eq!(grown_status, "idle");

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v217_creates_and_seeds_jira_monitoring() {
        // CI commonly has no Postgres; the helper checks both supported URL vars.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on a fresh database");

        let seeded: (String, i32, i32) = sqlx::query_as(
            "SELECT project_key,poll_interval_s,retag_after_s
               FROM jira_configs WHERE name='hireflow360'",
        )
        .fetch_one(&pool)
        .await
        .expect("read seeded Jira config");
        assert_eq!(seeded, ("HFPROD".into(), 300, 86_400));

        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables
              WHERE table_schema='public' AND table_name IN
                ('jira_configs','jira_rulesets','jira_monitor_leases',
                 'jira_watch_state','jira_issue_leases','jira_action_log')",
        )
        .fetch_one(&pool)
        .await
        .expect("count Jira monitor tables");
        assert_eq!(tables, 6);
        drop_temp_db(admin, pool, &db_name).await;
    }
}
