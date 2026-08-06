//! Embedded migration runner.
//!
//! Migrations are SQL strings embedded in Rust, applied forward-only
//! with version tracking via a `_migrations` meta-table.

use sqlx::{Acquire, PgPool};
use std::collections::{BTreeMap, BTreeSet};
use tracing::{debug, error, info, warn};

use crate::error::{DbError, Result};
use crate::schema;

/// The highest migration version baked into the squashed fresh-DB bootstrap.
const BOOTSTRAP_BASELINE_VERSION: u32 = schema::PG_BASELINE_VERSION;

/// Transaction-scoped lock shared by enrollment issuance and consumption.
/// The value is the stable ASCII tag `FF_ENROL` encoded as a signed i64.
pub const SECURE_ENROLLMENT_XACT_LOCK_KEY: i64 = 0x4646_5f45_4e52_4f4c;

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
    PgMigration {
        version: 229,
        name: "model_metric_boot_staleness",
        sql: schema::SCHEMA_V229_MODEL_METRIC_BOOT_STALENESS,
    },
    PgMigration {
        version: 230,
        name: "model_error_classes",
        sql: schema::SCHEMA_V230_MODEL_ERROR_CLASSES,
    },
    PgMigration {
        version: 231,
        name: "fabric_pair_model_invariants",
        sql: schema::SCHEMA_V231_FABRIC_PAIR_MODEL_INVARIANTS,
    },
    PgMigration {
        version: 232,
        name: "fabric_pair_empty_ip",
        sql: schema::SCHEMA_V232_FABRIC_PAIR_EMPTY_IP,
    },
    PgMigration {
        version: 233,
        name: "fabric_link_dead_alert",
        sql: schema::SCHEMA_V233_FABRIC_LINK_DEAD_ALERT,
    },
    PgMigration {
        version: 235,
        name: "code_rulesets",
        sql: schema::SCHEMA_V235_CODE_RULESETS,
    },
    PgMigration {
        version: 238,
        name: "work_item_context_and_cortex_subgraph",
        sql: schema::SCHEMA_V238_WORK_ITEM_CONTEXT_AND_CORTEX_SUBGRAPH,
    },
    PgMigration {
        version: 239,
        name: "work_item_lease_project_tracking",
        sql: schema::SCHEMA_V239_WORK_ITEM_LEASE_PROJECT_TRACKING,
    },
    PgMigration {
        version: 243,
        name: "fleet_model_catalog_rich_fields",
        sql: schema::SCHEMA_V243_FLEET_MODEL_CATALOG_RICH_FIELDS,
    },
    PgMigration {
        version: 244,
        name: "sub_agents_capabilities",
        sql: schema::SCHEMA_V244_SUB_AGENTS_CAPABILITIES,
    },
    PgMigration {
        version: 245,
        name: "notifications",
        sql: schema::SCHEMA_V245_NOTIFICATIONS,
    },
    // 246, 248-249 are reserved by in-flight branches (Autopilot /
    // mesh-repair); gaps are fine, collisions are not.
    PgMigration {
        version: 247,
        name: "error_miner_tables",
        sql: schema::SCHEMA_V247_ERROR_MINER_TABLES,
    },
    PgMigration {
        version: 250,
        name: "ff_interactions_episodic_tagging",
        sql: schema::SCHEMA_V250_FF_INTERACTIONS_EPISODIC_TAGGING,
    },
    // 251-257 are reserved by in-flight branches; gaps are fine, collisions
    // are not.
    PgMigration {
        version: 258,
        name: "model_catalog_view",
        sql: schema::SCHEMA_V258_MODEL_CATALOG_VIEW,
    },
    PgMigration {
        version: 271,
        name: "local_failure_diagnoses",
        sql: schema::SCHEMA_V271_LOCAL_FAILURE_DIAGNOSES,
    },
    PgMigration {
        version: 272,
        name: "work_item_retry_count",
        sql: schema::SCHEMA_V272_WORK_ITEM_RETRY_COUNT,
    },
    PgMigration {
        version: 273,
        name: "cloud_backends",
        sql: schema::SCHEMA_V273_CLOUD_BACKENDS,
    },
    PgMigration {
        version: 274,
        name: "project_repo_scan_metadata",
        sql: schema::SCHEMA_V274_PROJECT_REPO_SCAN_METADATA,
    },
    PgMigration {
        version: 275,
        name: "ff_capabilities",
        sql: schema::SCHEMA_V275_FF_CAPABILITIES,
    },
    PgMigration {
        version: 276,
        name: "glm_45_air_ab_scoreboard",
        sql: schema::SCHEMA_V276_GLM_45_AIR_AB_SCOREBOARD,
    },
    PgMigration {
        version: 277,
        name: "node_health",
        sql: schema::SCHEMA_V277_NODE_HEALTH,
    },
    PgMigration {
        version: 278,
        name: "durable_project_workstreams",
        sql: schema::SCHEMA_V278_DURABLE_PROJECT_WORKSTREAMS,
    },
    PgMigration {
        version: 279,
        name: "fleet_logs",
        sql: schema::SCHEMA_V279_FLEET_LOGS,
    },
    // V280 was quarantined manually on the live fleet. Its destructive table
    // merge is intentionally not runnable; the exact historical ledger row is
    // recognized by REVIEWED_LEGACY_LEDGER_ROWS below.
    PgMigration {
        version: 281,
        name: "work_item_acceptance_criteria",
        sql: schema::SCHEMA_V281_WORK_ITEM_ACCEPTANCE_CRITERIA,
    },
    PgMigration {
        version: 282,
        name: "oplog_replay",
        sql: schema::SCHEMA_V282_OPLOG_REPLAY,
    },
    PgMigration {
        version: 283,
        name: "project_digest_attempts",
        sql: schema::SCHEMA_V283_PROJECT_DIGEST_ATTEMPTS,
    },
    PgMigration {
        version: 284,
        name: "model_load_reservations",
        sql: schema::SCHEMA_V284_MODEL_LOAD_RESERVATIONS,
    },
    PgMigration {
        version: 285,
        name: "ring_safe_fabric",
        sql: schema::SCHEMA_V285_RING_SAFE_FABRIC,
    },
    PgMigration {
        version: 286,
        name: "oauth_distribution_gate",
        sql: schema::SCHEMA_V286_OAUTH_DISTRIBUTION_GATE,
    },
    PgMigration {
        version: 287,
        name: "ssh_mesh_auto_repair_gate",
        sql: schema::SCHEMA_V287_SSH_MESH_AUTO_REPAIR_GATE,
    },
    PgMigration {
        version: 288,
        name: "ubuntu_2604_software_projection",
        sql: schema::SCHEMA_V288_UBUNTU_2604_SOFTWARE_PROJECTION,
    },
    PgMigration {
        version: 289,
        name: "secure_enrollment_tokens",
        sql: schema::SCHEMA_V289_SECURE_ENROLLMENT_TOKENS,
    },
    PgMigration {
        version: 290,
        name: "secure_enrollment_hardening",
        sql: schema::SCHEMA_V290_SECURE_ENROLLMENT_HARDENING,
    },
    PgMigration {
        version: 291,
        name: "release_artifact_custody",
        sql: schema::SCHEMA_V291_RELEASE_ARTIFACT_CUSTODY,
    },
    PgMigration {
        version: 292,
        name: "james_qwen_served_model_alias",
        sql: schema::SCHEMA_V292_JAMES_QWEN_SERVED_MODEL_ALIAS,
    },
    PgMigration {
        version: 293,
        name: "smolvlm_exact_variant_authority",
        sql: schema::SCHEMA_V293_SMOLVLM_EXACT_VARIANT_AUTHORITY,
    },
    PgMigration {
        version: 294,
        name: "ace_gemma4_mlx_exact_authority",
        sql: schema::SCHEMA_V294_ACE_GEMMA4_MLX_EXACT_AUTHORITY,
    },
    PgMigration {
        version: 296,
        name: "devstral_code_capability_authority",
        sql: schema::SCHEMA_V296_DEVSTRAL_CODE_CAPABILITY_AUTHORITY,
    },
    PgMigration {
        version: 297,
        name: "release_rollout_post_success_rollback",
        sql: schema::SCHEMA_V297_RELEASE_ROLLOUT_POST_SUCCESS_ROLLBACK,
    },
];

/// Explicit-only migrations are structurally unreachable from
/// [`run_postgres_migrations`], which is called by daemon/startup paths.
/// Applying one requires the bounded, source-attested operator API below.
static EXPLICIT_PG_MIGRATIONS: &[PgMigration] = &[PgMigration {
    version: 295,
    name: "release_rollout_authority",
    sql: schema::SCHEMA_V295_RELEASE_ROLLOUT_AUTHORITY,
}];

/// Exact evidence from the one reviewed legacy V290 fleet ledger. These rows
/// are deliberately separate from runnable migrations: their source branches
/// were either superseded before merge or, for V280, explicitly quarantined.
/// They authorize status/reconciliation only at the recorded microsecond and
/// with no retroactive provenance; they must never be inserted by startup.
struct ReviewedLegacyLedgerRow {
    version: u32,
    name: &'static str,
    applied_at: &'static str,
}

static REVIEWED_LEGACY_LEDGER_ROWS: &[ReviewedLegacyLedgerRow] = &[
    ReviewedLegacyLedgerRow {
        version: 211,
        name: "decommission_taylor_github_identity",
        applied_at: "2026-07-20T16:39:14.109001Z",
    },
    ReviewedLegacyLedgerRow {
        version: 234,
        name: "work_item_cortex_subgraph_id",
        applied_at: "2026-07-22T04:50:37.318577Z",
    },
    ReviewedLegacyLedgerRow {
        version: 236,
        name: "work_item_context_and_cortex_subgraph",
        applied_at: "2026-07-22T15:13:32.818634Z",
    },
    ReviewedLegacyLedgerRow {
        version: 246,
        name: "glm_45_air_ab_catalog",
        applied_at: "2026-07-24T03:03:02.342428Z",
    },
    ReviewedLegacyLedgerRow {
        version: 260,
        name: "model_utilization_view",
        applied_at: "2026-07-24T20:49:10.028987Z",
    },
    ReviewedLegacyLedgerRow {
        version: 270,
        name: "workstream_session_fields",
        applied_at: "2026-07-25T19:04:23.380858Z",
    },
    ReviewedLegacyLedgerRow {
        version: 274,
        name: "project_repo_scan_metadata",
        applied_at: "2026-07-26T05:54:24.449824Z",
    },
    ReviewedLegacyLedgerRow {
        version: 277,
        name: "node_health",
        applied_at: "2026-07-26T18:23:36.239244Z",
    },
    ReviewedLegacyLedgerRow {
        version: 280,
        name: "merge_fleet_tables__QUARANTINED_MANUAL",
        applied_at: "2026-07-27T06:28:42.086663Z",
    },
];

const REVIEWED_LEDGER_ONLY_VERSIONS: &[u32] = &[234, 236, 246, 260, 270, 280];

const RELEASE_ROLLOUT_AUTHORITY_MIGRATION: u32 = 295;
const DEVSTRAL_CODE_CAPABILITY_MIGRATION: u32 = 296;
const RELEASE_ROLLOUT_POST_SUCCESS_ROLLBACK_MIGRATION: u32 = 297;

pub const LATEST_AUTOMATIC_POSTGRES_MIGRATION: u32 =
    RELEASE_ROLLOUT_POST_SUCCESS_ROLLBACK_MIGRATION;
pub const LATEST_EXPLICIT_POSTGRES_MIGRATION: u32 = 295;

fn automatic_migration_prerequisites_satisfied(
    migration_version: u32,
    applied_versions: &BTreeSet<u32>,
) -> bool {
    !matches!(
        migration_version,
        DEVSTRAL_CODE_CAPABILITY_MIGRATION | RELEASE_ROLLOUT_POST_SUCCESS_ROLLBACK_MIGRATION
    ) || applied_versions.contains(&RELEASE_ROLLOUT_AUTHORITY_MIGRATION)
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct PostgresMigrationDescriptor {
    pub version: u32,
    pub name: String,
    pub explicit_only: bool,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct AppliedPostgresMigration {
    pub version: u32,
    pub name: String,
    pub source_commit: Option<String>,
    pub applied_by: Option<String>,
    pub applied_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct PostgresMigrationStatus {
    pub current_version: u32,
    pub automatic_ceiling: u32,
    pub explicit_ceiling: u32,
    pub pending_automatic: Vec<PostgresMigrationDescriptor>,
    pub pending_explicit: Vec<PostgresMigrationDescriptor>,
    pub applied: Vec<AppliedPostgresMigration>,
    pub drift: Vec<String>,
    pub rollout_schema_valid: Option<bool>,
    /// True only for the exact reviewed V290 ledger + physical V247 pre-state.
    pub reviewed_v247_repair_pending: bool,
    /// Exact forward-repair postcondition once V295 has been recorded.
    pub reconciliation_schema_valid: Option<bool>,
}

type AppliedMigrationQueryRow = (
    i32,
    String,
    Option<String>,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
);

/// Fail closed unless the enrollment authority table is exactly the reviewed
/// v290 shape. This is intentionally validation-only: callers such as
/// `ff onboard` and the TLS supervisor must never repair or create authority
/// schema as an ordinary request side effect. Only the forward migration runner
/// owns that lifecycle.
pub async fn validate_secure_enrollment_schema(pool: &PgPool) -> Result<()> {
    let valid: bool = sqlx::query_scalar(
        r#"
        WITH relation AS (
            SELECT to_regclass('public.fleet_enrollment_tokens') AS oid
        ),
        expected_columns(attnum, attname, type_name, required, default_expr) AS (
            VALUES
                (1, 'token_hash', 'bytea', true, NULL::text),
                (2, 'node_name', 'text', true, NULL::text),
                (3, 'intended_ip', 'inet', true, NULL::text),
                (4, 'ssh_user', 'text', true, NULL::text),
                (5, 'role', 'text', true, NULL::text),
                (6, 'runtime', 'text', true, NULL::text),
                (7, 'purpose', 'text', true, '''node-enrollment''::text'),
                (8, 'leader_name', 'text', true, NULL::text),
                (9, 'leader_epoch', 'bigint', true, NULL::text),
                (10, 'expires_at', 'timestamp with time zone', true, NULL::text),
                (11, 'consumed_at', 'timestamp with time zone', false, NULL::text),
                (12, 'consumed_peer_ip', 'inet', false, NULL::text),
                (13, 'created_at', 'timestamp with time zone', true, 'clock_timestamp()'),
                (14, 'created_by', 'text', false, NULL::text),
                (15, 'revoked_at', 'timestamp with time zone', false, NULL::text)
        ),
        actual_columns AS (
            SELECT a.attnum::integer AS attnum,
                   a.attname,
                   format_type(a.atttypid, a.atttypmod) AS type_name,
                   a.attnotnull AS required,
                   pg_get_expr(d.adbin, d.adrelid) AS default_expr
              FROM relation r
              JOIN pg_attribute a ON a.attrelid = r.oid
              LEFT JOIN pg_attrdef d
                ON d.adrelid = a.attrelid AND d.adnum = a.attnum
             WHERE a.attnum > 0 AND NOT a.attisdropped
        ),
        expected_constraints(conname, condef) AS (
            VALUES
                ('fleet_enrollment_tokens_canonical_leader', 'CHECK (leader_name ~ ''^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$''::text)'),
                ('fleet_enrollment_tokens_canonical_name', 'CHECK (node_name ~ ''^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$''::text)'),
                ('fleet_enrollment_tokens_canonical_ssh_user', 'CHECK (ssh_user ~ ''^[a-z_][a-z0-9_-]{0,63}$''::text)'),
                ('fleet_enrollment_tokens_consumption', 'CHECK (consumed_at IS NULL AND consumed_peer_ip IS NULL OR consumed_at IS NOT NULL AND consumed_peer_ip IS NOT NULL)'),
                ('fleet_enrollment_tokens_epoch', 'CHECK (leader_epoch >= 0)'),
                ('fleet_enrollment_tokens_expiry', 'CHECK (expires_at > created_at AND expires_at <= (created_at + ''00:15:00''::interval))'),
                ('fleet_enrollment_tokens_hash_length', 'CHECK (octet_length(token_hash) = 32)'),
                ('fleet_enrollment_tokens_pkey', 'PRIMARY KEY (token_hash)'),
                ('fleet_enrollment_tokens_purpose', 'CHECK (purpose = ''node-enrollment''::text)'),
                ('fleet_enrollment_tokens_revocation', 'CHECK (revoked_at IS NULL OR consumed_at IS NULL AND revoked_at >= created_at)'),
                ('fleet_enrollment_tokens_role', 'CHECK (role = ANY (ARRAY[''builder''::text, ''gateway''::text, ''testbed''::text]))'),
                ('fleet_enrollment_tokens_runtime', 'CHECK (runtime ~ ''^[a-z0-9][a-z0-9._-]{0,31}$''::text)')
        ),
        actual_constraints AS (
            SELECT c.conname, pg_get_constraintdef(c.oid, true) AS condef,
                   c.convalidated
              FROM relation r
              JOIN pg_constraint c ON c.conrelid = r.oid
        ),
        roster_layout(layout) AS (
            SELECT 'legacy'::text
             WHERE (SELECT c.relkind = 'r'
                      FROM pg_class c
                     WHERE c.oid = 'public.computers'::regclass)
               AND (SELECT c.relkind = 'r'
                      FROM pg_class c
                     WHERE c.oid = 'public.fleet_workers'::regclass)
            UNION ALL
            SELECT 'unified'::text
             WHERE to_regclass('public.fleet_nodes') IS NOT NULL
               AND (SELECT c.relkind = 'r'
                      FROM pg_class c
                     WHERE c.oid = to_regclass('public.fleet_nodes'))
               AND (SELECT c.relkind = 'v'
                      FROM pg_class c
                     WHERE c.oid = 'public.computers'::regclass)
               AND (SELECT c.relkind = 'v'
                      FROM pg_class c
                     WHERE c.oid = 'public.fleet_workers'::regclass)
               AND btrim(regexp_replace(
                       pg_get_viewdef('public.computers'::regclass, true),
                       E'\\s+', ' ', 'g'
                   )) = 'SELECT id, name, primary_ip, all_ips, hostname, mac_addresses, os_family, os_distribution, os_version, os_version_latest, os_upgrade_available, os_version_checked_at, cpu_cores, total_ram_gb, total_disk_gb, has_gpu, gpu_kind, gpu_count, gpu_model, gpu_vram_gb, gpu_total_vram_gb, cuda_version, metal_version, rocm_version, gpu_driver_version, ssh_user, ssh_port, ssh_public_key, enrolled_at, last_seen_at, offline_since, status_changed_at, status, metadata, network_scope, source_tree_path, build_archs, connectivity_mode, election_eligibility, reservation_state, reserved_reason, reserved_at, reservation_owner, reservation_expires_at, dispatch_tick_at FROM fleet_nodes;'
               AND btrim(regexp_replace(
                       pg_get_viewdef('public.fleet_workers'::regclass, true),
                       E'\\s+', ' ', 'g'
                   )) = 'SELECT name, ip, ssh_user, ram_gb, worker_cpu_cores AS cpu_cores, os, role, election_priority, hardware, alt_ips, capabilities, preferences, resources, worker_status AS status, registered_at, updated_at, runtime, models_dir, disk_quota_pct, sub_agent_count, gh_account, tooling FROM fleet_nodes;'
        ),
        token_expected_indexes(indexname, indexdef) AS (
            VALUES
                ('fleet_enrollment_tokens_pkey', 'CREATE UNIQUE INDEX fleet_enrollment_tokens_pkey ON public.fleet_enrollment_tokens USING btree (token_hash)'),
                ('idx_fleet_enrollment_tokens_expiry', 'CREATE INDEX idx_fleet_enrollment_tokens_expiry ON public.fleet_enrollment_tokens USING btree (expires_at) WHERE ((consumed_at IS NULL) AND (revoked_at IS NULL))'),
                ('idx_fleet_enrollment_tokens_node', 'CREATE INDEX idx_fleet_enrollment_tokens_node ON public.fleet_enrollment_tokens USING btree (node_name, created_at DESC)'),
                ('idx_fleet_enrollment_tokens_pending_ip', 'CREATE UNIQUE INDEX idx_fleet_enrollment_tokens_pending_ip ON public.fleet_enrollment_tokens USING btree (intended_ip) WHERE ((consumed_at IS NULL) AND (revoked_at IS NULL))'),
                ('idx_fleet_enrollment_tokens_pending_name', 'CREATE UNIQUE INDEX idx_fleet_enrollment_tokens_pending_name ON public.fleet_enrollment_tokens USING btree (lower(node_name)) WHERE ((consumed_at IS NULL) AND (revoked_at IS NULL))')
        ),
        legacy_expected_indexes(indexname, indexdef) AS (
            VALUES
                ('idx_computers_enrollment_canonical_name', 'CREATE UNIQUE INDEX idx_computers_enrollment_canonical_name ON public.computers USING btree (lower(name))'),
                ('idx_computers_enrollment_primary_ip', 'CREATE UNIQUE INDEX idx_computers_enrollment_primary_ip ON public.computers USING btree (primary_ip) WHERE (NULLIF(primary_ip, ''''::text) IS NOT NULL)'),
                ('idx_fleet_workers_enrollment_canonical_name', 'CREATE UNIQUE INDEX idx_fleet_workers_enrollment_canonical_name ON public.fleet_workers USING btree (lower(name))'),
                ('idx_fleet_workers_enrollment_ip', 'CREATE UNIQUE INDEX idx_fleet_workers_enrollment_ip ON public.fleet_workers USING btree (ip) WHERE (NULLIF(ip, ''''::text) IS NOT NULL)')
        ),
        unified_expected_indexes(indexname, indexdef) AS (
            VALUES
                ('idx_fleet_nodes_enrollment_canonical_name', 'CREATE UNIQUE INDEX idx_fleet_nodes_enrollment_canonical_name ON public.fleet_nodes USING btree (lower(name))'),
                ('idx_fleet_nodes_enrollment_primary_ip', 'CREATE UNIQUE INDEX idx_fleet_nodes_enrollment_primary_ip ON public.fleet_nodes USING btree (primary_ip) WHERE (NULLIF(primary_ip, ''''::text) IS NOT NULL)'),
                ('idx_fleet_nodes_enrollment_worker_ip', 'CREATE UNIQUE INDEX idx_fleet_nodes_enrollment_worker_ip ON public.fleet_nodes USING btree (ip) WHERE (NULLIF(ip, ''''::text) IS NOT NULL)')
        ),
        expected_indexes AS (
            SELECT * FROM token_expected_indexes
            UNION ALL
            SELECT l.*
              FROM legacy_expected_indexes l
             WHERE (SELECT layout FROM roster_layout) = 'legacy'
            UNION ALL
            SELECT u.*
              FROM unified_expected_indexes u
             WHERE (SELECT layout FROM roster_layout) = 'unified'
        ),
        actual_indexes AS (
            SELECT i.relname AS indexname, x.indisvalid, x.indisready,
                   pg_get_indexdef(x.indexrelid) AS indexdef
              FROM pg_index x
              JOIN pg_class i ON i.oid = x.indexrelid
             WHERE x.indrelid = (SELECT oid FROM relation)
                OR (x.indrelid = 'public.computers'::regclass
                    AND i.relname IN (
                        'idx_computers_enrollment_canonical_name',
                        'idx_computers_enrollment_primary_ip'
                    ))
                OR (x.indrelid = 'public.fleet_workers'::regclass
                    AND i.relname IN (
                        'idx_fleet_workers_enrollment_canonical_name',
                        'idx_fleet_workers_enrollment_ip'
                    ))
                OR (x.indrelid = to_regclass('public.fleet_nodes')
                    AND i.relname IN (
                        'idx_fleet_nodes_enrollment_canonical_name',
                        'idx_fleet_nodes_enrollment_primary_ip',
                        'idx_fleet_nodes_enrollment_worker_ip'
                    ))
        ),
        token_index_count AS (
            SELECT count(*) AS count
              FROM relation r
              JOIN pg_index x ON x.indrelid = r.oid
        )
        SELECT
            (SELECT oid IS NOT NULL FROM relation)
            AND (SELECT count(*) FROM actual_columns) = 15
            AND NOT EXISTS (
                SELECT attnum, attname, type_name, required, default_expr FROM expected_columns
                EXCEPT
                SELECT attnum, attname, type_name, required, default_expr FROM actual_columns
            )
            AND NOT EXISTS (
                SELECT attnum, attname, type_name, required, default_expr FROM actual_columns
                EXCEPT
                SELECT attnum, attname, type_name, required, default_expr FROM expected_columns
            )
            AND (SELECT count(*) FROM actual_constraints) = 12
            AND NOT EXISTS (
                SELECT conname, condef FROM expected_constraints
                EXCEPT SELECT conname, condef FROM actual_constraints
            )
            AND NOT EXISTS (
                SELECT conname, condef FROM actual_constraints
                EXCEPT SELECT conname, condef FROM expected_constraints
            )
            AND NOT EXISTS (SELECT 1 FROM actual_constraints WHERE NOT convalidated)
            AND (SELECT count(*) FROM roster_layout) = 1
            AND (SELECT count FROM token_index_count) = 5
            AND (SELECT count(*) FROM actual_indexes) =
                (SELECT count(*) FROM expected_indexes)
            AND NOT EXISTS (
                SELECT indexname, indexdef FROM expected_indexes
                EXCEPT SELECT indexname, indexdef FROM actual_indexes
            )
            AND NOT EXISTS (
                SELECT indexname, indexdef FROM actual_indexes
                EXCEPT SELECT indexname, indexdef FROM expected_indexes
            )
            AND NOT EXISTS (
                SELECT 1 FROM actual_indexes WHERE NOT indisvalid OR NOT indisready
            )
            AND obj_description(
                    (SELECT oid FROM relation), 'pg_class'
                ) = 'forgefleet secure enrollment authority schema v290; forward-only migrations only'
        "#,
    )
    .fetch_one(pool)
    .await?;

    if valid {
        Ok(())
    } else {
        Err(DbError::Migration(
            "fleet_enrollment_tokens is missing or not the exact reviewed v290 shape; run the controlled forward migration before enrollment".to_string(),
        ))
    }
}

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

    // A missing migration below the current maximum is never an automatic
    // backfill. In particular, the reviewed live fleet is at V290 with an
    // exact empty partial V247; advancing V291-V294 first would destroy the
    // bounded repair profile. Hold that exact state for the explicit V295
    // transaction, and reject every unreviewed missing-V247 lineage.
    if current >= 247 {
        let applied = read_applied_postgres_migrations_conn(&mut *conn).await?;
        if !applied.iter().any(|row| row.version == 247) {
            let reviewed_hold = current == 290
                && reviewed_live_v290_ledger_matches(&applied)
                && reviewed_historical_schema_is_exact(&mut *conn).await?
                && forward_compatibility_state(&mut *conn, "projects").await?
                    == ForwardCompatibilityState::Absent
                && forward_compatibility_state(&mut *conn, "work_item_leases").await?
                    == ForwardCompatibilityState::Exact
                && v247_schema_state(&mut *conn).await? == V247SchemaState::ReviewedEmptyPartial;
            if reviewed_hold {
                warn!(
                    current_version = current,
                    "holding reviewed legacy V290 for explicit V247/V295 reconciliation"
                );
                return Ok(current);
            }
            return Err(DbError::Migration(format!(
                "v247 is missing below current v{current}; automatic migration advance is blocked"
            )));
        }
    }

    let applied_versions = read_applied_postgres_migrations_conn(&mut *conn)
        .await?
        .into_iter()
        .map(|row| row.version)
        .collect::<BTreeSet<_>>();
    let v296_held_for_v295 = current < DEVSTRAL_CODE_CAPABILITY_MIGRATION
        && !automatic_migration_prerequisites_satisfied(
            DEVSTRAL_CODE_CAPABILITY_MIGRATION,
            &applied_versions,
        );
    let pending: Vec<&PgMigration> = PG_MIGRATIONS
        .iter()
        .filter(|m| m.version > current)
        .filter(|m| automatic_migration_prerequisites_satisfied(m.version, &applied_versions))
        .collect();

    if pending.is_empty() {
        if v296_held_for_v295 {
            warn!(
                current_version = current,
                required_version = RELEASE_ROLLOUT_AUTHORITY_MIGRATION,
                held_version = DEVSTRAL_CODE_CAPABILITY_MIGRATION,
                "holding automatic Devstral capability migration until explicit rollout authority is recorded"
            );
        } else {
            debug!(current_version = current, "postgres database is up to date");
        }
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
                // Drop the failed tx (rolls back) so we can reuse `conn` below.
                drop(tx);
                //
                // NON-FATAL QUARANTINE (operator 2026-07-27, after the 3rd
                // migration crash-loop: v276 view-rename, v278 idempotency, v280
                // merge-ordering). Previously ONE failed migration aborted the
                // WHOLE daemon startup — before the self-heal/reporting loops even
                // start — so a single bad migration took down all 18 nodes AND
                // left no running orchestrator to detect or fix it (self-heal
                // can't recover a failure in its own startup path). That turned a
                // one-feature bug into a fleet-wide outage requiring a human every
                // time.
                //
                // Instead: log LOUD, record the migration as FAILED (quarantined)
                // so it isn't retried on every boot (no crash-loop), and CONTINUE
                // — the daemon starts, the rest of the fleet keeps running, and
                // self-heal operates. A broken migration now degrades ONE feature
                // (whatever needed that schema) instead of the whole fleet; the
                // systemic-error doctor + this quarantine surface it for a real
                // source fix. `_migration_failures` is created best-effort.
                error!(
                    version = migration.version,
                    name = migration.name,
                    error = %e,
                    "postgres migration FAILED — quarantining (non-fatal) and CONTINUING startup; \
                     schema for this feature is degraded until the migration is fixed"
                );
                // Record the failure OUTSIDE the rolled-back tx so it persists.
                let _ = sqlx::query(
                    "CREATE TABLE IF NOT EXISTS _migration_failures ( \
                        version int PRIMARY KEY, name text, error text, \
                        first_failed_at timestamptz NOT NULL DEFAULT now(), \
                        last_failed_at  timestamptz NOT NULL DEFAULT now(), \
                        attempts int NOT NULL DEFAULT 1 )",
                )
                .execute(&mut *conn)
                .await;
                let _ = sqlx::query(
                    "INSERT INTO _migration_failures (version, name, error) VALUES ($1,$2,$3) \
                     ON CONFLICT (version) DO UPDATE SET \
                        last_failed_at = now(), attempts = _migration_failures.attempts + 1, \
                        error = EXCLUDED.error",
                )
                .bind(migration.version as i32)
                .bind(migration.name)
                .bind(e.to_string())
                .execute(&mut *conn)
                .await;
                // Mark it applied=quarantined in _migrations so the runner does
                // NOT re-attempt it every boot (that's the crash-loop). A later
                // build with a corrected migration body must use a NEW version
                // number (forward-only), so skipping this one is safe.
                let _ = sqlx::query(
                    "INSERT INTO _migrations (version, name) VALUES ($1, $2) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(migration.version as i32)
                .bind(format!("{}__QUARANTINED", migration.name))
                .execute(&mut *conn)
                .await;
                continue;
            }
        }
    }

    let final_version = pg_current_version(&mut *conn).await?;
    info!(version = final_version, "all postgres migrations applied");
    Ok(final_version)
}

fn migration_descriptor(
    migration: &PgMigration,
    explicit_only: bool,
) -> PostgresMigrationDescriptor {
    PostgresMigrationDescriptor {
        version: migration.version,
        name: migration.name.to_string(),
        explicit_only,
    }
}

fn validate_explicit_apply_identity(
    expected_source_commit: &str,
    running_source_commit: &str,
    running_git_state: &str,
    applied_by: &str,
) -> Result<()> {
    let full_lower_sha = |value: &str| {
        value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if !full_lower_sha(expected_source_commit) || !full_lower_sha(running_source_commit) {
        return Err(DbError::Migration(
            "explicit migration requires full lowercase 40-character source commits".to_string(),
        ));
    }
    if expected_source_commit != running_source_commit {
        return Err(DbError::Migration(format!(
            "explicit migration source mismatch: requested {expected_source_commit}, running {running_source_commit}"
        )));
    }
    if !matches!(running_git_state, "pushed" | "unpushed") {
        return Err(DbError::Migration(format!(
            "explicit migration refuses build state {running_git_state:?}; use a clean exact-source binary"
        )));
    }
    if applied_by.is_empty()
        || applied_by.len() > 128
        || !applied_by
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b'-'))
    {
        return Err(DbError::Migration(
            "explicit migration applied_by identity is not canonical".to_string(),
        ));
    }
    Ok(())
}

async fn postgres_migration_table_exists(pool: &PgPool) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT to_regclass('public._migrations') IS NOT NULL")
            .fetch_one(pool)
            .await?,
    )
}

async fn read_applied_postgres_migrations(pool: &PgPool) -> Result<Vec<AppliedPostgresMigration>> {
    if !postgres_migration_table_exists(pool).await? {
        return Ok(Vec::new());
    }
    let has_provenance: bool = sqlx::query_scalar(
        "SELECT count(*) = 2
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = '_migrations'
            AND column_name IN ('source_commit', 'applied_by')",
    )
    .fetch_one(pool)
    .await?;
    let rows: Vec<AppliedMigrationQueryRow> = if has_provenance {
        sqlx::query_as(
            "SELECT version, name, source_commit, applied_by, applied_at
                   FROM _migrations ORDER BY version",
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
            "SELECT version, name, NULL::text, NULL::text, applied_at
                   FROM _migrations ORDER BY version",
        )
        .fetch_all(pool)
        .await?
    };
    rows.into_iter()
        .map(|(version, name, source_commit, applied_by, applied_at)| {
            let version = u32::try_from(version).map_err(|_| {
                DbError::Migration("_migrations contains a negative version".to_string())
            })?;
            Ok(AppliedPostgresMigration {
                version,
                name,
                source_commit,
                applied_by,
                applied_at,
            })
        })
        .collect()
}

async fn read_applied_postgres_migrations_conn(
    conn: &mut sqlx::PgConnection,
) -> Result<Vec<AppliedPostgresMigration>> {
    let has_provenance: bool = sqlx::query_scalar(
        "SELECT count(*) = 2
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = '_migrations'
            AND column_name IN ('source_commit', 'applied_by')",
    )
    .fetch_one(&mut *conn)
    .await?;
    let rows: Vec<AppliedMigrationQueryRow> = if has_provenance {
        sqlx::query_as(
            "SELECT version, name, source_commit, applied_by, applied_at
               FROM _migrations ORDER BY version",
        )
        .fetch_all(&mut *conn)
        .await?
    } else {
        sqlx::query_as(
            "SELECT version, name, NULL::text, NULL::text, applied_at
               FROM _migrations ORDER BY version",
        )
        .fetch_all(&mut *conn)
        .await?
    };
    rows.into_iter()
        .map(|(version, name, source_commit, applied_by, applied_at)| {
            Ok(AppliedPostgresMigration {
                version: u32::try_from(version).map_err(|_| {
                    DbError::Migration("_migrations contains a negative version".to_string())
                })?,
                name,
                source_commit,
                applied_by,
                applied_at,
            })
        })
        .collect()
}

fn reviewed_legacy_row_matches(
    row: &AppliedPostgresMigration,
    reviewed: &ReviewedLegacyLedgerRow,
) -> bool {
    row.version == reviewed.version
        && row.name == reviewed.name
        && row.source_commit.is_none()
        && row.applied_by.is_none()
        && row
            .applied_at
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
            == reviewed.applied_at
}

fn reviewed_live_v290_ledger_matches(applied: &[AppliedPostgresMigration]) -> bool {
    applied.last().is_some_and(|row| row.version == 290)
        && REVIEWED_LEGACY_LEDGER_ROWS.iter().all(|reviewed| {
            applied
                .iter()
                .find(|row| row.version == reviewed.version)
                .is_some_and(|row| reviewed_legacy_row_matches(row, reviewed))
        })
        && !applied.iter().any(|row| row.version == 247)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum V247SchemaState {
    Exact,
    ReviewedEmptyPartial,
    Drift,
}

/// Classify V247 by its complete column/constraint/index shape. The partial
/// branch is intentionally narrower than `CREATE TABLE IF NOT EXISTS`: it is
/// the one observed empty fleet table and cannot absorb arbitrary drift.
async fn v247_schema_state(conn: &mut sqlx::PgConnection) -> Result<V247SchemaState> {
    let state: String = sqlx::query_scalar(
        r#"
        WITH error_columns AS (
            SELECT COALESCE(
                jsonb_agg(
                    jsonb_build_array(column_name, data_type, is_nullable, column_default)
                    ORDER BY ordinal_position
                ), '[]'::jsonb
            ) AS shape
              FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'error_signatures'
        ), error_constraints AS (
            SELECT count(*) AS total,
                   count(*) FILTER (
                       WHERE conname = 'error_signatures_pkey'
                         AND contype = 'p' AND conkey = ARRAY[1]::smallint[]
                   ) AS exact_pk,
                   count(*) FILTER (
                       WHERE conname = 'error_signatures_state_check'
                         AND contype = 'c' AND conkey = ARRAY[9]::smallint[]
                         AND pg_get_constraintdef(oid, true) =
                             'CHECK (state = ANY (ARRAY[''new''::text, ''filed''::text, ''fix_merged''::text, ''verifying''::text, ''resolved''::text, ''regressed''::text]))'
                   ) AS exact_state
              FROM pg_constraint
             WHERE conrelid = to_regclass('public.error_signatures')
        ), error_indexes AS (
            SELECT count(*) AS total,
                   count(*) FILTER (
                       WHERE indexname = 'error_signatures_pkey'
                         AND indexdef = 'CREATE UNIQUE INDEX error_signatures_pkey ON public.error_signatures USING btree (signature)'
                   ) AS exact_pk
              FROM pg_indexes
             WHERE schemaname = 'public' AND tablename = 'error_signatures'
        ), digest_columns AS (
            SELECT COALESCE(
                jsonb_agg(
                    jsonb_build_array(column_name, data_type, is_nullable, column_default)
                    ORDER BY ordinal_position
                ), '[]'::jsonb
            ) AS shape
              FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'fleet_log_digest'
        ), digest_constraints AS (
            SELECT count(*) AS total,
                   count(*) FILTER (
                       WHERE conname = 'fleet_log_digest_pkey'
                         AND contype = 'p' AND conkey = ARRAY[1]::smallint[]
                   ) AS exact_pk,
                   count(*) FILTER (
                       WHERE conname = 'fleet_log_digest_node_day_level_line_class_key'
                         AND contype = 'u' AND conkey = ARRAY[2,3,4,5]::smallint[]
                   ) AS exact_unique
              FROM pg_constraint
             WHERE conrelid = to_regclass('public.fleet_log_digest')
        ), digest_indexes AS (
            SELECT count(*) AS total,
                   count(*) FILTER (
                       WHERE indexname = 'fleet_log_digest_pkey'
                         AND indexdef = 'CREATE UNIQUE INDEX fleet_log_digest_pkey ON public.fleet_log_digest USING btree (id)'
                   ) AS exact_pk,
                   count(*) FILTER (
                       WHERE indexname = 'fleet_log_digest_node_day_level_line_class_key'
                         AND indexdef = 'CREATE UNIQUE INDEX fleet_log_digest_node_day_level_line_class_key ON public.fleet_log_digest USING btree (node, day, level, line_class)'
                   ) AS exact_unique
              FROM pg_indexes
             WHERE schemaname = 'public' AND tablename = 'fleet_log_digest'
        )
        SELECT CASE
            WHEN error_columns.shape =
                '[["signature","text","NO",null],["error_class","text","YES",null],["first_seen","timestamp with time zone","NO","now()"],["last_seen","timestamp with time zone","NO","now()"],["count_24h","integer","NO","0"],["count_total","integer","NO","0"],["sample_text","text","YES",null],["affected_nodes","jsonb","YES",null],["state","text","NO","''new''::text"],["work_item_id","uuid","YES",null],["fix_commit_sha","text","YES",null],["resolved_at","timestamp with time zone","YES",null]]'::jsonb
              AND error_constraints.total = 2
              AND error_constraints.exact_pk = 1
              AND error_constraints.exact_state = 1
              AND error_indexes.total = 1 AND error_indexes.exact_pk = 1
              AND digest_columns.shape =
                '[["id","uuid","NO","gen_random_uuid()"],["node","text","NO",null],["day","date","NO",null],["level","text","NO",null],["line_class","text","NO",null],["count","integer","NO","0"],["sample","text","YES",null]]'::jsonb
              AND digest_constraints.total = 2
              AND digest_constraints.exact_pk = 1
              AND digest_constraints.exact_unique = 1
              AND digest_indexes.total = 2
              AND digest_indexes.exact_pk = 1
              AND digest_indexes.exact_unique = 1
            THEN 'exact'
            WHEN error_columns.shape =
                '[["signature","text","NO",null],["error_class","text","YES",null],["first_seen","timestamp with time zone","YES","now()"],["last_seen","timestamp with time zone","YES","now()"],["count_24h","integer","YES","0"],["count_total","integer","YES","0"],["sample_text","text","YES",null],["affected_nodes","jsonb","YES",null],["state","text","YES","''new''::text"],["work_item_id","uuid","YES",null],["fix_commit_sha","text","YES",null],["resolved_at","timestamp with time zone","YES",null]]'::jsonb
              AND error_constraints.total = 1
              AND error_constraints.exact_pk = 1
              AND error_constraints.exact_state = 0
              AND error_indexes.total = 1 AND error_indexes.exact_pk = 1
              AND to_regclass('public.fleet_log_digest') IS NULL
            THEN 'reviewed_empty_partial_candidate'
            ELSE 'drift'
        END
          FROM error_columns, error_constraints, error_indexes,
               digest_columns, digest_constraints, digest_indexes
        "#,
    )
    .fetch_one(&mut *conn)
    .await?;
    if state == "reviewed_empty_partial_candidate" {
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM error_signatures")
            .fetch_one(&mut *conn)
            .await?;
        return Ok(if rows == 0 {
            V247SchemaState::ReviewedEmptyPartial
        } else {
            V247SchemaState::Drift
        });
    }
    Ok(match state.as_str() {
        "exact" => V247SchemaState::Exact,
        _ => V247SchemaState::Drift,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardCompatibilityState {
    Absent,
    Exact,
    Drift,
}

/// Validate the historical effects whose migration numbers were repurposed,
/// plus the one deliberately unexecuted V280 roster layout.
async fn reviewed_historical_schema_is_exact(conn: &mut sqlx::PgConnection) -> Result<bool> {
    Ok(sqlx::query_scalar(
        r#"
        WITH scan_columns AS (
            SELECT count(*) = 2
                   AND bool_and(data_type = 'text' AND is_nullable = 'YES' AND column_default IS NULL)
                   AS exact
              FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'project_repos'
               AND column_name IN ('tech_stack', 'local_path')
        ), health_columns AS (
            SELECT COALESCE(
                jsonb_agg(
                    jsonb_build_array(column_name, data_type, is_nullable, column_default)
                    ORDER BY ordinal_position
                ), '[]'::jsonb
            ) = '[["worker_name","text","NO",null],["sampled_at","timestamp with time zone","NO","now()"],["mem_total_kb","bigint","NO",null],["mem_available_kb","bigint","NO",null],["mem_available_gb","double precision","NO",null],["swap_total_kb","bigint","NO","0"],["swap_free_kb","bigint","NO","0"],["load_avg_1m","double precision","YES",null],["load_avg_5m","double precision","YES",null],["load_avg_15m","double precision","YES",null],["service_rss_json","jsonb","NO","''[]''::jsonb"],["oom_kills_json","jsonb","NO","''[]''::jsonb"],["dmesg_cursor","text","YES",null],["pressure_state","text","NO","''healthy''::text"],["build_reserve_gb","double precision","NO","4.0"]]'::jsonb AS exact
              FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'node_health'
        ), health_constraints AS (
            SELECT count(*) = 3
               AND count(*) FILTER (WHERE conname = 'node_health_pkey' AND contype = 'p' AND conkey = ARRAY[1,2]::smallint[]) = 1
               AND count(*) FILTER (WHERE conname = 'node_health_worker_name_fkey' AND contype = 'f' AND conkey = ARRAY[1]::smallint[]) = 1
               AND count(*) FILTER (
                    WHERE conname = 'node_health_pressure_state_check' AND contype = 'c'
                      AND conkey = ARRAY[14]::smallint[]
                      AND pg_get_constraintdef(oid, true) = 'CHECK (pressure_state = ANY (ARRAY[''healthy''::text, ''build_paused''::text, ''critical''::text]))'
               ) = 1 AS exact
              FROM pg_constraint
             WHERE conrelid = to_regclass('public.node_health')
        ), health_indexes AS (
            SELECT count(*) = 3
               AND count(*) FILTER (WHERE indexname = 'node_health_pkey' AND indexdef = 'CREATE UNIQUE INDEX node_health_pkey ON public.node_health USING btree (worker_name, sampled_at)') = 1
               AND count(*) FILTER (WHERE indexname = 'idx_node_health_latest' AND indexdef = 'CREATE INDEX idx_node_health_latest ON public.node_health USING btree (worker_name, sampled_at DESC)') = 1
               AND count(*) FILTER (WHERE indexname = 'idx_node_health_pressure' AND indexdef = 'CREATE INDEX idx_node_health_pressure ON public.node_health USING btree (pressure_state, sampled_at DESC)') = 1 AS exact
              FROM pg_indexes
             WHERE schemaname = 'public' AND tablename = 'node_health'
        ), roster AS (
            SELECT count(*) = 2
               AND count(*) FILTER (WHERE relname = 'computers' AND relkind = 'r') = 1
               AND count(*) FILTER (WHERE relname = 'fleet_workers' AND relkind = 'r') = 1 AS exact
              FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = 'public'
               AND relname IN ('computers', 'fleet_workers', 'fleet_nodes', 'fleet_workers_legacy')
        )
        SELECT scan_columns.exact AND health_columns.exact
           AND health_constraints.exact AND health_indexes.exact AND roster.exact
          FROM scan_columns, health_columns, health_constraints, health_indexes, roster
        "#,
    )
    .fetch_one(&mut *conn)
    .await?)
}

async fn forward_compatibility_state(
    conn: &mut sqlx::PgConnection,
    table_name: &str,
) -> Result<ForwardCompatibilityState> {
    let state: String = match table_name {
        "projects" => sqlx::query_scalar(
            r#"
            WITH columns AS (
                SELECT count(*) AS total,
                       count(*) FILTER (WHERE column_name = 'workstream_id' AND data_type = 'text' AND is_nullable = 'YES' AND column_default IS NULL) AS workstream,
                       count(*) FILTER (WHERE column_name = 'digest_template_id' AND data_type = 'jsonb' AND is_nullable = 'YES' AND column_default IS NULL) AS digest,
                       count(*) FILTER (WHERE column_name = 'logo_url' AND data_type = 'text' AND is_nullable = 'YES' AND column_default IS NULL) AS logo
                  FROM information_schema.columns
                 WHERE table_schema = 'public' AND table_name = 'projects'
                   AND column_name IN ('workstream_id', 'digest_template_id', 'logo_url')
            ), indexes AS (
                SELECT count(*) AS total,
                       count(*) FILTER (
                           WHERE indexname = 'idx_projects_workstream_id'
                             AND indexdef = 'CREATE INDEX idx_projects_workstream_id ON public.projects USING btree (workstream_id) WHERE (workstream_id IS NOT NULL)'
                       ) AS exact
                  FROM pg_indexes
                 WHERE schemaname = 'public' AND tablename = 'projects'
                   AND indexname = 'idx_projects_workstream_id'
            )
            SELECT CASE
                     WHEN columns.total = 0 AND indexes.total = 0 THEN 'absent'
                     WHEN columns.total = 3 AND columns.workstream = 1
                          AND columns.digest = 1 AND columns.logo = 1
                          AND indexes.total = 1 AND indexes.exact = 1 THEN 'exact'
                     ELSE 'drift'
                   END
              FROM columns, indexes
            "#,
        )
        .fetch_one(&mut *conn)
        .await?,
        "work_item_leases" => sqlx::query_scalar(
            r#"
            SELECT CASE
                     WHEN count(*) = 0 THEN 'absent'
                     WHEN count(*) = 1
                      AND bool_and(data_type = 'timestamp with time zone'
                                   AND is_nullable = 'YES' AND column_default IS NULL)
                       THEN 'exact'
                     ELSE 'drift'
                   END
              FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'work_item_leases'
               AND column_name = 'build_started_at'
            "#,
        )
        .fetch_one(&mut *conn)
        .await?,
        _ => unreachable!("fixed internal compatibility target"),
    };
    Ok(match state.as_str() {
        "absent" => ForwardCompatibilityState::Absent,
        "exact" => ForwardCompatibilityState::Exact,
        _ => ForwardCompatibilityState::Drift,
    })
}

/// Read migration state without creating tables, taking locks, or applying DDL.
pub async fn postgres_migration_status(pool: &PgPool) -> Result<PostgresMigrationStatus> {
    let applied = read_applied_postgres_migrations(pool).await?;
    let applied_versions = applied
        .iter()
        .map(|migration| migration.version)
        .collect::<BTreeSet<_>>();
    let current_version = applied_versions.iter().next_back().copied().unwrap_or(0);
    let mut schema_conn = pool.acquire().await?;
    let v247_state = v247_schema_state(&mut schema_conn).await?;
    let historical_schema_exact = reviewed_historical_schema_is_exact(&mut schema_conn).await?;
    let project_forward_state = forward_compatibility_state(&mut schema_conn, "projects").await?;
    let lease_forward_state =
        forward_compatibility_state(&mut schema_conn, "work_item_leases").await?;
    drop(schema_conn);
    let embedded = PG_MIGRATIONS
        .iter()
        .map(|migration| (migration.version, (migration, false)))
        .chain(
            EXPLICIT_PG_MIGRATIONS
                .iter()
                .map(|migration| (migration.version, (migration, true))),
        )
        .collect::<BTreeMap<_, _>>();
    let mut drift = Vec::new();
    for row in &applied {
        if let Some((expected, explicit)) = embedded.get(&row.version) {
            if row.name != expected.name {
                drift.push(format!(
                    "v{} name drift: stored {:?}, embedded {:?}",
                    row.version, row.name, expected.name
                ));
            }
            if *explicit
                && (row.source_commit.as_deref().is_none_or(|source| {
                    source.len() != 40
                        || !source
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                }) || row.applied_by.as_deref().is_none_or(str::is_empty))
            {
                drift.push(format!(
                    "v{} explicit provenance is missing or invalid",
                    row.version
                ));
            }
        } else if row.version > BOOTSTRAP_BASELINE_VERSION {
            let reviewed = REVIEWED_LEGACY_LEDGER_ROWS
                .iter()
                .find(|reviewed| reviewed.version == row.version);
            if !REVIEWED_LEDGER_ONLY_VERSIONS.contains(&row.version)
                || reviewed.is_none_or(|reviewed| !reviewed_legacy_row_matches(row, reviewed))
            {
                drift.push(format!(
                    "v{} is applied but absent from this binary or differs from the exact reviewed legacy row",
                    row.version
                ));
            }
        }
    }

    // Once any ledger-only version is present, this is the reviewed legacy
    // lineage. Bind every reviewed row to its exact microsecond and preserve
    // its original NULL provenance; a matching name alone is insufficient.
    let legacy_lineage = applied
        .iter()
        .any(|row| REVIEWED_LEDGER_ONLY_VERSIONS.contains(&row.version));
    if legacy_lineage {
        for reviewed in REVIEWED_LEGACY_LEDGER_ROWS {
            match applied.iter().find(|row| row.version == reviewed.version) {
                Some(row) if reviewed_legacy_row_matches(row, reviewed) => {}
                Some(_) => drift.push(format!(
                    "v{} reviewed legacy timestamp/name/provenance drift",
                    reviewed.version
                )),
                None => drift.push(format!(
                    "v{} reviewed legacy ledger row is missing",
                    reviewed.version
                )),
            }
        }
    }

    let reviewed_v247_repair_pending = reviewed_live_v290_ledger_matches(&applied)
        && historical_schema_exact
        && project_forward_state == ForwardCompatibilityState::Absent
        && lease_forward_state == ForwardCompatibilityState::Exact
        && v247_state == V247SchemaState::ReviewedEmptyPartial;
    for (version, (expected, _)) in &embedded {
        if *version > BOOTSTRAP_BASELINE_VERSION
            && *version <= current_version
            && !applied_versions.contains(version)
            && (*version != 247 || !reviewed_v247_repair_pending)
        {
            drift.push(format!(
                "v{version} ({}) is missing below current v{current_version}",
                expected.name
            ));
        }
    }

    let pending_automatic = PG_MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current_version && !reviewed_v247_repair_pending)
        .map(|migration| migration_descriptor(migration, false))
        .collect();
    let pending_explicit = EXPLICIT_PG_MIGRATIONS
        .iter()
        .filter(|migration| !applied_versions.contains(&migration.version))
        .map(|migration| migration_descriptor(migration, true))
        .collect();
    let rollout_schema_valid = if applied_versions.contains(&295) {
        Some(crate::rollout_authority::release_rollout_schema_is_exact(pool).await?)
    } else {
        None
    };
    if rollout_schema_valid == Some(false) {
        drift.push("v295 rollout authority schema or committed data drifted".to_string());
    }
    let reconciliation_schema_valid = if applied_versions.contains(&295) {
        Some(
            v247_state == V247SchemaState::Exact
                && historical_schema_exact
                && project_forward_state == ForwardCompatibilityState::Exact
                && lease_forward_state == ForwardCompatibilityState::Exact,
        )
    } else {
        None
    };
    if reconciliation_schema_valid == Some(false) {
        drift.push("v295 historical reconciliation schema drifted".to_string());
    }

    if let Some(v247) = applied.iter().find(|row| row.version == 247) {
        if v247.source_commit.is_some() || v247.applied_by.is_some() {
            let bound = applied
                .iter()
                .find(|row| row.version == 295)
                .is_some_and(|v295| {
                    v247.source_commit == v295.source_commit
                        && v247.applied_by == v295.applied_by
                        && v247.applied_at == v295.applied_at
                });
            if !bound {
                drift.push(
                    "late-applied v247 provenance/timestamp is not bound exactly to v295"
                        .to_string(),
                );
            }
        }
    }

    Ok(PostgresMigrationStatus {
        current_version,
        automatic_ceiling: LATEST_AUTOMATIC_POSTGRES_MIGRATION,
        explicit_ceiling: LATEST_EXPLICIT_POSTGRES_MIGRATION,
        pending_automatic,
        pending_explicit,
        applied,
        drift,
        rollout_schema_valid,
        reviewed_v247_repair_pending,
        reconciliation_schema_valid,
    })
}

async fn explicit_migration_failures_exist(
    conn: &mut sqlx::PgConnection,
    target: u32,
) -> Result<bool> {
    let failures_table: bool =
        sqlx::query_scalar("SELECT to_regclass('public._migration_failures') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !failures_table {
        return Ok(false);
    }
    Ok(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM _migration_failures WHERE version <= $1",
        )
        .bind(
            i32::try_from(target).map_err(|_| {
                DbError::Migration("explicit migration target exceeds i32".to_string())
            })?,
        )
        .fetch_one(&mut *conn)
        .await?
            > 0,
    )
}

/// Apply through the one explicit migration ceiling under exact source
/// provenance. Unlike the startup runner, every failure is fatal and rolls the
/// current migration back; nothing is quarantined or marked applied on error.
#[allow(clippy::too_many_arguments)]
pub async fn apply_explicit_postgres_migrations(
    pool: &PgPool,
    target: u32,
    expected_source_commit: &str,
    running_source_commit: &str,
    running_git_state: &str,
    applied_by: &str,
) -> Result<PostgresMigrationStatus> {
    if target != LATEST_EXPLICIT_POSTGRES_MIGRATION {
        return Err(DbError::Migration(format!(
            "explicit migration target must be exactly v{LATEST_EXPLICIT_POSTGRES_MIGRATION}; got v{target}"
        )));
    }
    validate_explicit_apply_identity(
        expected_source_commit,
        running_source_commit,
        running_git_state,
        applied_by,
    )?;

    let before = postgres_migration_status(pool).await?;
    if !before.drift.is_empty() {
        return Err(DbError::Migration(format!(
            "migration authority drift: {}",
            before.drift.join("; ")
        )));
    }
    if before.current_version < 290 {
        return Err(DbError::Migration(format!(
            "explicit v{target} requires the reviewed v290-or-newer base; database is v{}",
            before.current_version
        )));
    }

    let mut conn = pool.acquire().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(MIGRATION_ADVISORY_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?;
    if !acquired {
        return Err(DbError::Migration(
            "another migration runner holds the migration lock".to_string(),
        ));
    }

    let result = async {
        ensure_pg_migrations_table(&mut conn).await?;
        if explicit_migration_failures_exist(&mut conn, target).await? {
            return Err(DbError::Migration(
                "explicit migration refuses a database with quarantined migration failures"
                    .to_string(),
            ));
        }

        let mut tx = conn.begin().await?;
        sqlx::query("LOCK TABLE _migrations IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        let locked_applied = read_applied_postgres_migrations_conn(&mut tx).await?;
        if locked_applied != before.applied {
            return Err(DbError::Migration(
                "migration ledger changed while acquiring the explicit lock; retry".to_string(),
            ));
        }
        let locked_current = locked_applied
            .last()
            .map(|row| row.version)
            .unwrap_or(0);
        if locked_current != before.current_version {
            return Err(DbError::Migration(format!(
                "migration version changed from v{} to v{} while acquiring the explicit lock; retry",
                before.current_version, locked_current
            )));
        }
        if let Some(applied) = before.applied.iter().find(|row| row.version == target) {
            if applied.source_commit.as_deref() != Some(expected_source_commit) {
                return Err(DbError::Migration(format!(
                    "v{target} provenance drift: stored {:?}, requested {expected_source_commit}",
                    applied.source_commit
                )));
            }
            tx.rollback().await?;
            return Ok(());
        }
        if locked_current > target {
            return Err(DbError::Migration(format!(
                "database v{locked_current} is newer than bounded target v{target}, \
                 but the exact source-attested v{target} ledger row is missing"
            )));
        }

        if !reviewed_historical_schema_is_exact(&mut tx).await? {
            return Err(DbError::Migration(
                "reviewed V274/V277/V280 historical schema profile is not exact".to_string(),
            ));
        }
        let project_state = forward_compatibility_state(&mut tx, "projects").await?;
        let lease_state = forward_compatibility_state(&mut tx, "work_item_leases").await?;
        if project_state == ForwardCompatibilityState::Drift
            || lease_state == ForwardCompatibilityState::Drift
        {
            return Err(DbError::Migration(
                "V295 forward-compatibility pre-state drifted".to_string(),
            ));
        }

        let repair_v247 = before.reviewed_v247_repair_pending;
        let locked_v247_state = v247_schema_state(&mut tx).await?;
        if repair_v247 {
            if !reviewed_live_v290_ledger_matches(&locked_applied)
                || locked_v247_state != V247SchemaState::ReviewedEmptyPartial
                || project_state != ForwardCompatibilityState::Absent
                || lease_state != ForwardCompatibilityState::Exact
            {
                return Err(DbError::Migration(
                    "reviewed V247 forward-repair pre-state changed under lock".to_string(),
                ));
            }
            sqlx::raw_sql(schema::SCHEMA_V295_REPAIR_REVIEWED_EMPTY_V247)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    DbError::Migration(format!("explicit V247 forward repair failed: {error}"))
                })?;
        } else if locked_v247_state != V247SchemaState::Exact
            || !locked_applied.iter().any(|row| row.version == 247)
        {
            return Err(DbError::Migration(
                "V247 must be exact and recorded, or match the one reviewed repair profile"
                    .to_string(),
            ));
        }

        sqlx::raw_sql(schema::SCHEMA_V295_FORWARD_COMPATIBILITY_REPAIR)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                DbError::Migration(format!("explicit V295 compatibility repair failed: {error}"))
            })?;
        if v247_schema_state(&mut tx).await? != V247SchemaState::Exact
            || forward_compatibility_state(&mut tx, "projects").await?
                != ForwardCompatibilityState::Exact
            || forward_compatibility_state(&mut tx, "work_item_leases").await?
                != ForwardCompatibilityState::Exact
        {
            return Err(DbError::Migration(
                "V295 compatibility repair postcondition is not exact".to_string(),
            ));
        }

        let mut current = before.current_version;
        let ordered = PG_MIGRATIONS
            .iter()
            .chain(EXPLICIT_PG_MIGRATIONS.iter())
            .filter(|migration| migration.version > current && migration.version <= target)
            .collect::<Vec<_>>();
        for migration in ordered {
            sqlx::raw_sql(migration.sql)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    DbError::Migration(format!(
                        "explicit v{} ({}) failed: {error}",
                        migration.version, migration.name
                    ))
                })?;
            if migration.version == LATEST_EXPLICIT_POSTGRES_MIGRATION {
                let applied_at: chrono::DateTime<chrono::Utc> =
                    sqlx::query_scalar("SELECT clock_timestamp()")
                        .fetch_one(&mut *tx)
                        .await?;
                if repair_v247 {
                    sqlx::query(
                        "INSERT INTO _migrations
                            (version, name, applied_at, source_commit, applied_by)
                         VALUES (247, 'error_miner_tables', $1, $2, $3)",
                    )
                    .bind(applied_at)
                    .bind(expected_source_commit)
                    .bind(applied_by)
                    .execute(&mut *tx)
                    .await?;
                }
                sqlx::query(
                    "INSERT INTO _migrations
                        (version, name, applied_at, source_commit, applied_by)
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(i32::try_from(migration.version).expect("migration version fits i32"))
                .bind(migration.name)
                .bind(applied_at)
                .bind(expected_source_commit)
                .bind(applied_by)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query("INSERT INTO _migrations (version, name) VALUES ($1, $2)")
                    .bind(i32::try_from(migration.version).expect("migration version fits i32"))
                    .bind(migration.name)
                    .execute(&mut *tx)
                    .await?;
            }
            current = migration.version;
        }
        if current != target {
            return Err(DbError::Migration(format!(
                "bounded explicit migration stopped at v{current}, expected v{target}"
            )));
        }
        tx.commit().await?;
        Ok(())
    }
    .await;

    let unlocked = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_ADVISORY_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await;
    if !matches!(unlocked, Ok(true)) {
        conn.close().await.ok();
        return Err(DbError::Migration(format!(
            "explicit migration lock release failed: {unlocked:?}"
        )));
    }
    result?;

    let after = postgres_migration_status(pool).await?;
    let target_provenance_is_exact = after.applied.iter().any(|row| {
        row.version == target && row.source_commit.as_deref() == Some(expected_source_commit)
    });
    if after.current_version < target || !target_provenance_is_exact || !after.drift.is_empty() {
        return Err(DbError::Migration(format!(
            "explicit migration postcondition failed at v{} (target provenance exact={}): {}",
            after.current_version,
            target_provenance_is_exact,
            after.drift.join("; ")
        )));
    }
    Ok(after)
}

#[cfg(test)]
mod tests {
    use std::env;

    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

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

    #[test]
    fn v295_remains_explicit_only_and_v296_v297_wait_for_it() {
        assert_eq!(LATEST_AUTOMATIC_POSTGRES_MIGRATION, 297);
        assert_eq!(LATEST_EXPLICIT_POSTGRES_MIGRATION, 295);
        assert_eq!(PG_MIGRATIONS.last().unwrap().version, 297);
        assert!(
            PG_MIGRATIONS
                .iter()
                .all(|migration| migration.version != 295)
        );
        assert_eq!(EXPLICIT_PG_MIGRATIONS.len(), 1);
        assert_eq!(EXPLICIT_PG_MIGRATIONS[0].version, 295);
        assert_eq!(EXPLICIT_PG_MIGRATIONS[0].name, "release_rollout_authority");
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 296)
            .expect("V296 must be registered");
        assert_eq!(migration.name, "devstral_code_capability_authority");
        assert!(migration.sql.contains("INSERT INTO fleet_model_catalog"));
        assert!(!migration.sql.contains("INSERT INTO model_catalog"));
        assert!(!migration.sql.contains("UPDATE model_catalog"));
        let rollback_migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 297)
            .expect("V297 must be registered");
        assert_eq!(
            rollback_migration.name,
            "release_rollout_post_success_rollback"
        );
        assert!(rollback_migration.sql.contains("OLD.state = 'succeeded'"));
        assert!(
            rollback_migration
                .sql
                .contains("NEW.state = 'rolling_back'")
        );

        assert!(!automatic_migration_prerequisites_satisfied(
            296,
            &BTreeSet::new()
        ));
        assert!(!automatic_migration_prerequisites_satisfied(
            297,
            &BTreeSet::new()
        ));
        assert!(automatic_migration_prerequisites_satisfied(
            296,
            &BTreeSet::from([295])
        ));
        assert!(automatic_migration_prerequisites_satisfied(
            297,
            &BTreeSet::from([295])
        ));
        assert!(automatic_migration_prerequisites_satisfied(
            294,
            &BTreeSet::new()
        ));
    }

    #[test]
    fn explicit_apply_identity_requires_exact_clean_source() {
        const SOURCE: &str = "39b017341b7536df64b61f42672ab33fb62343f8";
        validate_explicit_apply_identity(SOURCE, SOURCE, "unpushed", "adele@host").unwrap();
        assert!(validate_explicit_apply_identity(SOURCE, SOURCE, "pushed", "adele@host").is_ok());
        assert!(
            validate_explicit_apply_identity(SOURCE, &"a".repeat(40), "unpushed", "adele@host")
                .is_err()
        );
        assert!(validate_explicit_apply_identity(SOURCE, SOURCE, "dirty", "adele@host").is_err());
        assert!(
            validate_explicit_apply_identity("short", SOURCE, "unpushed", "adele@host").is_err()
        );
        assert!(
            validate_explicit_apply_identity(SOURCE, SOURCE, "unpushed", "bad operator").is_err()
        );
    }

    #[tokio::test]
    async fn v295_explicit_fresh_replay_drift_lock_and_rollout_authority() {
        use crate::rollout_authority::{
            ReleaseRolloutAuthoritySpec, RolloutArtifactAuthority,
            RolloutAuthorityRegistrationOutcome, RolloutTargetAuthority,
            RolloutTransactionBeginOutcome, pg_begin_release_rollout,
            pg_cas_release_rollout_target_state, pg_cas_release_rollout_transaction_state,
            pg_claim_succeeded_release_rollout_rollback, pg_register_release_rollout_authority,
        };

        const SOURCE: &str = "39b017341b7536df64b61f42672ab33fb62343f8";
        const TARGET_ID: Uuid = Uuid::from_u128(0xb5f0a59f_a46d_4e88_8113_847fa275f782);
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        let blank = postgres_migration_status(&pool).await.unwrap();
        assert_eq!(blank.current_version, 0);
        assert!(!postgres_migration_table_exists(&pool).await.unwrap());
        assert!(
            apply_explicit_postgres_migrations(
                &pool,
                295,
                SOURCE,
                SOURCE,
                "unpushed",
                "test@v295",
            )
            .await
            .expect_err("explicit rollout migration needs the reviewed v290 base")
            .to_string()
            .contains("v290-or-newer")
        );
        assert!(
            !postgres_migration_table_exists(&pool).await.unwrap(),
            "rejected fresh apply must not create migration state"
        );

        sqlx::raw_sql(schema::BOOTSTRAP_V161_SQL)
            .execute(&pool)
            .await
            .expect("fresh baseline");
        for migration in PG_MIGRATIONS.iter().filter(|migration| {
            migration.version > BOOTSTRAP_BASELINE_VERSION && migration.version <= 290
        }) {
            let mut tx = pool.begin().await.unwrap();
            sqlx::raw_sql(migration.sql)
                .execute(&mut *tx)
                .await
                .unwrap_or_else(|error| {
                    panic!("strict fresh v{} failed: {error}", migration.version)
                });
            sqlx::query("INSERT INTO _migrations (version, name) VALUES ($1, $2)")
                .bind(migration.version as i32)
                .bind(migration.name)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        // V280 is intentionally non-runnable; fresh replay retains the exact
        // table-backed roster layout required by V291 and the reviewed fleet.
        assert_eq!(
            postgres_migration_status(&pool)
                .await
                .unwrap()
                .current_version,
            290
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT to_regclass('public.release_rollout_authorities')::text",
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            None,
            "V290 must not contain v295 objects",
        );

        let mut lock_holder = pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(MIGRATION_ADVISORY_LOCK_KEY)
            .execute(&mut *lock_holder)
            .await
            .unwrap();
        let lock_error =
            apply_explicit_postgres_migrations(&pool, 295, SOURCE, SOURCE, "unpushed", "test@v295")
                .await
                .expect_err("concurrent explicit migration must fail fast");
        assert!(
            lock_error.to_string().contains("migration lock"),
            "unexpected lock failure: {lock_error}"
        );
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(MIGRATION_ADVISORY_LOCK_KEY)
            .execute(&mut *lock_holder)
            .await
            .unwrap();
        drop(lock_holder);

        let applied =
            apply_explicit_postgres_migrations(&pool, 295, SOURCE, SOURCE, "unpushed", "test@v295")
                .await
                .expect("apply exact v295");
        assert_eq!(applied.current_version, 295);
        assert_eq!(applied.rollout_schema_valid, Some(true));
        assert_eq!(applied.reconciliation_schema_valid, Some(true));
        assert!(applied.drift.is_empty());

        let replay =
            apply_explicit_postgres_migrations(&pool, 295, SOURCE, SOURCE, "unpushed", "test@v295")
                .await
                .expect("exact replay must be idempotent");
        assert_eq!(replay.current_version, 295);

        let advanced = run_postgres_migrations(&pool)
            .await
            .expect("automatic V296/V297 must follow recorded explicit V295");
        assert_eq!(advanced, 297);
        let replay_after_v296 =
            apply_explicit_postgres_migrations(&pool, 295, SOURCE, SOURCE, "unpushed", "test@v295")
                .await
                .expect("exact V295 replay must remain idempotent after V296");
        assert_eq!(replay_after_v296.current_version, 297);
        assert!(
            apply_explicit_postgres_migrations(
                &pool,
                295,
                &"a".repeat(40),
                &"a".repeat(40),
                "unpushed",
                "test@v295",
            )
            .await
            .expect_err("stored provenance mismatch must fail")
            .to_string()
            .contains("provenance drift")
        );

        let mut partial = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO release_rollout_authorities
                (source_commit, expected_target_count, expected_artifact_count, created_by)
             VALUES ($1, 1, 2, 'test@v295')",
        )
        .bind("3".repeat(40))
        .execute(&mut *partial)
        .await
        .unwrap();
        assert!(
            partial.commit().await.is_err(),
            "deferred authority validator must reject an unsealed partial authority"
        );

        sqlx::query(
            "INSERT INTO computers (id, name, primary_ip, os_family, ssh_user)
             VALUES ($1, 'vinny', '192.0.2.9', 'macos', 'test')",
        )
        .bind(crate::rollout_authority::FORBIDDEN_VINNY_ID)
        .execute(&pool)
        .await
        .unwrap();
        let mut forbidden = pool.begin().await.unwrap();
        let forbidden_authority: Uuid = sqlx::query_scalar(
            "INSERT INTO release_rollout_authorities
                (source_commit, expected_target_count, expected_artifact_count, created_by)
             VALUES ($1, 1, 2, 'test@v295') RETURNING id",
        )
        .bind("4".repeat(40))
        .fetch_one(&mut *forbidden)
        .await
        .unwrap();
        assert!(
            sqlx::query(
                "INSERT INTO release_rollout_authority_targets
                    (authority_id, target_ordinal, computer_id, computer_name,
                     target_triple, artifact_version)
                 VALUES ($1, 0, $2, 'vinny', 'aarch64-unknown-linux-gnu', 'forbidden')",
            )
            .bind(forbidden_authority)
            .bind(crate::rollout_authority::FORBIDDEN_VINNY_ID)
            .execute(&mut *forbidden)
            .await
            .is_err(),
            "database constraints must reject Vinny by exact name and UUID"
        );
        forbidden.rollback().await.unwrap();

        sqlx::query(
            "INSERT INTO computers
                (id, name, primary_ip, os_family, ssh_user)
             VALUES ($1, 'beyonce', '192.0.2.10', 'linux-ubuntu', 'test')",
        )
        .bind(TARGET_ID)
        .execute(&pool)
        .await
        .unwrap();
        let artifact_version = format!("recovery.{SOURCE}.ubuntu24-arm64");
        let ff_id: Uuid = sqlx::query_scalar(
            "INSERT INTO release_artifacts
                (artifact_name, artifact_version, source_commit, target_triple, sha256, size_bytes)
             VALUES ('ff', $1, $2, 'aarch64-unknown-linux-gnu', $3, 10)
             RETURNING id",
        )
        .bind(&artifact_version)
        .bind(SOURCE)
        .bind("1".repeat(64))
        .fetch_one(&pool)
        .await
        .unwrap();
        let daemon_id: Uuid = sqlx::query_scalar(
            "INSERT INTO release_artifacts
                (artifact_name, artifact_version, source_commit, target_triple, sha256, size_bytes)
             VALUES ('forgefleetd', $1, $2, 'aarch64-unknown-linux-gnu', $3, 20)
             RETURNING id",
        )
        .bind(&artifact_version)
        .bind(SOURCE)
        .bind("2".repeat(64))
        .fetch_one(&pool)
        .await
        .unwrap();
        let spec = ReleaseRolloutAuthoritySpec {
            source_commit: SOURCE.to_string(),
            created_by: "test@v295".to_string(),
            targets: vec![RolloutTargetAuthority {
                target_ordinal: 0,
                computer_id: TARGET_ID,
                computer_name: "beyonce".to_string(),
                target_triple: "aarch64-unknown-linux-gnu".to_string(),
                artifact_version,
                artifacts: vec![
                    RolloutArtifactAuthority {
                        artifact_name: "ff".to_string(),
                        artifact_id: ff_id,
                    },
                    RolloutArtifactAuthority {
                        artifact_name: "forgefleetd".to_string(),
                        artifact_id: daemon_id,
                    },
                ],
            }],
        };
        let authority = pg_register_release_rollout_authority(&pool, &spec)
            .await
            .expect("seal complete exact authority");
        assert_eq!(
            authority.outcome,
            RolloutAuthorityRegistrationOutcome::Inserted
        );
        assert_eq!(
            pg_register_release_rollout_authority(&pool, &spec)
                .await
                .unwrap()
                .outcome,
            RolloutAuthorityRegistrationOutcome::ExactExisting,
        );
        let mut drifted = spec.clone();
        drifted.created_by = "other@test".to_string();
        assert!(
            pg_register_release_rollout_authority(&pool, &drifted)
                .await
                .is_err()
        );

        let request_id = Uuid::new_v4();
        let begun =
            pg_begin_release_rollout(&pool, request_id, authority.authority.id, "test@v295", 30)
                .await
                .expect("begin complete leased transaction");
        assert_eq!(begun.outcome, RolloutTransactionBeginOutcome::Inserted);
        assert_eq!(
            pg_begin_release_rollout(&pool, request_id, authority.authority.id, "test@v295", 30,)
                .await
                .unwrap()
                .outcome,
            RolloutTransactionBeginOutcome::ExactExisting,
        );
        assert!(
            pg_begin_release_rollout(
                &pool,
                Uuid::new_v4(),
                authority.authority.id,
                "test@v295",
                30,
            )
            .await
            .is_err(),
            "one-active constraint must reject a second transaction"
        );
        let target = pg_cas_release_rollout_target_state(
            &pool,
            begun.transaction.id,
            TARGET_ID,
            begun.transaction.lease_token,
            0,
            "pending",
            "installing",
            None,
        )
        .await
        .unwrap()
        .expect("exact target CAS");
        assert_eq!(target.cas_revision, 1);
        assert!(
            pg_cas_release_rollout_target_state(
                &pool,
                begun.transaction.id,
                TARGET_ID,
                begun.transaction.lease_token,
                0,
                "pending",
                "installing",
                None,
            )
            .await
            .unwrap()
            .is_none(),
            "stale target CAS must be rejected"
        );
        let running = pg_cas_release_rollout_transaction_state(
            &pool,
            begun.transaction.id,
            begun.transaction.lease_token,
            begun.transaction.cas_revision,
            "planned",
            "running",
        )
        .await
        .unwrap()
        .expect("exact parent CAS");
        assert_eq!(running.cas_revision, 1);

        let target = pg_cas_release_rollout_target_state(
            &pool,
            begun.transaction.id,
            TARGET_ID,
            running.lease_token,
            target.cas_revision,
            "installing",
            "verifying",
            Some("{\"phase\":\"rollback_proof\"}"),
        )
        .await
        .unwrap()
        .expect("target verifying CAS");
        let target = pg_cas_release_rollout_target_state(
            &pool,
            begun.transaction.id,
            TARGET_ID,
            running.lease_token,
            target.cas_revision,
            "verifying",
            "succeeded",
            Some("{\"phase\":\"succeeded\"}"),
        )
        .await
        .unwrap()
        .expect("target succeeded CAS");
        let succeeded = pg_cas_release_rollout_transaction_state(
            &pool,
            begun.transaction.id,
            running.lease_token,
            running.cas_revision,
            "running",
            "succeeded",
        )
        .await
        .unwrap()
        .expect("parent succeeded CAS");

        let unrotated_token = sqlx::query(
            "UPDATE release_rollout_transactions
                SET state = 'rolling_back', completed_at = NULL,
                    lease_owner = 'rollback@test-malformed',
                    lease_expires_at = clock_timestamp() + interval '30 seconds',
                    cas_revision = cas_revision + 1
              WHERE id = $1",
        )
        .bind(succeeded.id)
        .execute(&pool)
        .await;
        assert!(
            unrotated_token.is_err(),
            "V297 must reject a succeeded rollback claim that does not rotate its lease token"
        );
        let retained_completion = sqlx::query(
            "UPDATE release_rollout_transactions
                SET state = 'rolling_back', lease_token = gen_random_uuid(),
                    lease_owner = 'rollback@test-malformed',
                    lease_expires_at = clock_timestamp() + interval '30 seconds',
                    cas_revision = cas_revision + 1
              WHERE id = $1",
        )
        .bind(succeeded.id)
        .execute(&pool)
        .await;
        assert!(
            retained_completion.is_err(),
            "V297 must reject a succeeded rollback claim that retains completed_at"
        );
        let unchanged_after_malformed_claims: (String, i64, uuid::Uuid, bool) = sqlx::query_as(
            "SELECT state, cas_revision, lease_token, completed_at IS NOT NULL
                   FROM release_rollout_transactions
                  WHERE id = $1",
        )
        .bind(succeeded.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            unchanged_after_malformed_claims,
            (
                "succeeded".to_string(),
                succeeded.cas_revision,
                succeeded.lease_token,
                true,
            ),
            "malformed trigger attempts must make zero writes"
        );

        let claim_a = pg_claim_succeeded_release_rollout_rollback(
            &pool,
            succeeded.id,
            succeeded.cas_revision,
            "rollback@test-a",
        );
        let claim_b = pg_claim_succeeded_release_rollout_rollback(
            &pool,
            succeeded.id,
            succeeded.cas_revision,
            "rollback@test-b",
        );
        let (claim_a, claim_b) = tokio::join!(claim_a, claim_b);
        let (claim_a, claim_b) = (claim_a.unwrap(), claim_b.unwrap());
        let claimed = match (claim_a, claim_b) {
            (Some(claimed), None) | (None, Some(claimed)) => claimed,
            other => panic!("exactly one concurrent rollback claim must win: {other:?}"),
        };
        assert_eq!(claimed.state, "rolling_back");
        assert_eq!(claimed.cas_revision, succeeded.cas_revision + 1);
        assert_ne!(claimed.lease_token, succeeded.lease_token);
        assert!(claimed.lease_expires_at > chrono::Utc::now());
        let completed_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT completed_at FROM release_rollout_transactions WHERE id = $1",
        )
        .bind(claimed.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(completed_at.is_none());

        let target = pg_cas_release_rollout_target_state(
            &pool,
            claimed.id,
            TARGET_ID,
            claimed.lease_token,
            target.cas_revision,
            "succeeded",
            "rolling_back",
            Some("{\"phase\":\"rolling_back\"}"),
        )
        .await
        .unwrap()
        .expect("target rollback claim CAS");
        let _target = pg_cas_release_rollout_target_state(
            &pool,
            claimed.id,
            TARGET_ID,
            claimed.lease_token,
            target.cas_revision,
            "rolling_back",
            "rolled_back",
            Some("{\"phase\":\"rolled_back\"}"),
        )
        .await
        .unwrap()
        .expect("target rolled-back CAS");
        let rolled_back = pg_cas_release_rollout_transaction_state(
            &pool,
            claimed.id,
            claimed.lease_token,
            claimed.cas_revision,
            "rolling_back",
            "rolled_back",
        )
        .await
        .unwrap()
        .expect("parent rolled-back CAS");
        assert!(
            pg_claim_succeeded_release_rollout_rollback(
                &pool,
                rolled_back.id,
                rolled_back.cas_revision,
                "rollback@test",
            )
            .await
            .unwrap()
            .is_none(),
            "a completed rollback must refuse a second claim"
        );

        let cancelled = pg_begin_release_rollout(
            &pool,
            Uuid::new_v4(),
            authority.authority.id,
            "test@v295",
            30,
        )
        .await
        .unwrap()
        .transaction;
        let cancelled = pg_cas_release_rollout_transaction_state(
            &pool,
            cancelled.id,
            cancelled.lease_token,
            cancelled.cas_revision,
            "planned",
            "cancelled",
        )
        .await
        .unwrap()
        .expect("cancel fixture");
        assert!(
            pg_claim_succeeded_release_rollout_rollback(
                &pool,
                cancelled.id,
                cancelled.cas_revision,
                "rollback@test",
            )
            .await
            .unwrap()
            .is_none()
        );

        let failed = pg_begin_release_rollout(
            &pool,
            Uuid::new_v4(),
            authority.authority.id,
            "test@v295",
            30,
        )
        .await
        .unwrap()
        .transaction;
        let failed = pg_cas_release_rollout_transaction_state(
            &pool,
            failed.id,
            failed.lease_token,
            failed.cas_revision,
            "planned",
            "running",
        )
        .await
        .unwrap()
        .expect("failed fixture running CAS");
        let failed = pg_cas_release_rollout_transaction_state(
            &pool,
            failed.id,
            failed.lease_token,
            failed.cas_revision,
            "running",
            "failed",
        )
        .await
        .unwrap()
        .expect("failed fixture terminal CAS");
        assert!(
            pg_claim_succeeded_release_rollout_rollback(
                &pool,
                failed.id,
                failed.cas_revision,
                "rollback@test",
            )
            .await
            .unwrap()
            .is_none()
        );

        sqlx::query("DROP INDEX release_rollout_one_active_transaction")
            .execute(&pool)
            .await
            .unwrap();
        let drift = postgres_migration_status(&pool).await.unwrap();
        assert_eq!(drift.rollout_schema_valid, Some(false));
        assert!(!drift.drift.is_empty());
        assert!(apply_explicit_postgres_migrations(
            &pool, 295, SOURCE, SOURCE, "unpushed", "test@v295",
        )
        .await
        .expect_err("replay over schema drift must fail closed")
        .to_string()
        .contains("migration authority drift"));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v295_reconciles_only_the_exact_reviewed_legacy_v247_profile() {
        const SOURCE: &str = "39b017341b7536df64b61f42672ab33fb62343f8";
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_reviewed_legacy_v290(&pool).await;

        let ready = postgres_migration_status(&pool).await.unwrap();
        assert_eq!(ready.current_version, 290);
        assert!(
            ready.drift.is_empty(),
            "unexpected drift: {:?}",
            ready.drift
        );
        assert!(ready.reviewed_v247_repair_pending);
        assert!(ready.pending_automatic.is_empty());
        assert_eq!(
            run_postgres_migrations(&pool)
                .await
                .expect("startup must hold the reviewed repair profile"),
            290
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT to_regclass('public.release_artifacts')::text",
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            None,
            "automatic V291-V294 must not advance ahead of V247 repair",
        );

        sqlx::query(
            "UPDATE _migrations
                SET applied_at = applied_at + interval '1 microsecond'
              WHERE version = 260",
        )
        .execute(&pool)
        .await
        .unwrap();
        let timestamp_drift = postgres_migration_status(&pool).await.unwrap();
        assert!(!timestamp_drift.reviewed_v247_repair_pending);
        assert!(
            timestamp_drift
                .drift
                .iter()
                .any(|item| item.contains("v260"))
        );
        assert!(
            apply_explicit_postgres_migrations(
                &pool,
                295,
                SOURCE,
                SOURCE,
                "unpushed",
                "test@legacy",
            )
            .await
            .expect_err("one-microsecond legacy drift must block")
            .to_string()
            .contains("migration authority drift")
        );
        sqlx::query(
            "UPDATE _migrations
                SET applied_at = '2026-07-24T20:49:10.028987Z'
              WHERE version = 260",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO error_signatures (signature, state)
             VALUES ('must-not-coerce', 'not-a-reviewed-state')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let nonempty = postgres_migration_status(&pool).await.unwrap();
        assert!(!nonempty.reviewed_v247_repair_pending);
        assert!(!nonempty.drift.is_empty());
        sqlx::query("DELETE FROM error_signatures")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("ALTER TABLE error_signatures ALTER COLUMN count_24h SET DEFAULT 1")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !postgres_migration_status(&pool)
                .await
                .unwrap()
                .reviewed_v247_repair_pending
        );
        sqlx::query("ALTER TABLE error_signatures ALTER COLUMN count_24h SET DEFAULT 0")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("CREATE INDEX unreviewed_error_state_idx ON error_signatures(state)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !postgres_migration_status(&pool)
                .await
                .unwrap()
                .reviewed_v247_repair_pending
        );
        sqlx::query("DROP INDEX unreviewed_error_state_idx")
            .execute(&pool)
            .await
            .unwrap();

        // Force a late V295 failure and prove V247 + V291-V295 + compatibility
        // repairs share one rollback boundary.
        sqlx::query("CREATE TABLE release_rollout_authorities (wrong INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let failed = apply_explicit_postgres_migrations(
            &pool,
            295,
            SOURCE,
            SOURCE,
            "unpushed",
            "test@legacy",
        )
        .await
        .expect_err("conflicting V295 relation must roll the whole repair back");
        assert!(failed.to_string().contains("explicit v295"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _migrations WHERE version = 247")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM information_schema.columns
                  WHERE table_schema='public' AND table_name='projects'
                    AND column_name IN ('workstream_id','digest_template_id','logo_url')",
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT to_regclass('public.release_artifacts')::text",
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            None
        );
        let mut rolled_back = pool.acquire().await.unwrap();
        assert_eq!(
            v247_schema_state(&mut rolled_back).await.unwrap(),
            V247SchemaState::ReviewedEmptyPartial
        );
        drop(rolled_back);
        sqlx::query("DROP TABLE release_rollout_authorities")
            .execute(&pool)
            .await
            .unwrap();

        let applied = apply_explicit_postgres_migrations(
            &pool,
            295,
            SOURCE,
            SOURCE,
            "unpushed",
            "test@legacy",
        )
        .await
        .expect("exact reviewed legacy repair");
        assert_eq!(applied.current_version, 295);
        assert!(!applied.reviewed_v247_repair_pending);
        assert_eq!(applied.reconciliation_schema_valid, Some(true));
        assert!(applied.drift.is_empty());
        for reviewed in REVIEWED_LEGACY_LEDGER_ROWS {
            let row = applied
                .applied
                .iter()
                .find(|row| row.version == reviewed.version)
                .unwrap();
            assert!(reviewed_legacy_row_matches(row, reviewed));
        }
        let repaired = applied
            .applied
            .iter()
            .find(|row| row.version == 247)
            .unwrap();
        let authority = applied
            .applied
            .iter()
            .find(|row| row.version == 295)
            .unwrap();
        assert_eq!(repaired.name, "error_miner_tables");
        assert_eq!(repaired.source_commit.as_deref(), Some(SOURCE));
        assert_eq!(repaired.applied_by.as_deref(), Some("test@legacy"));
        assert_eq!(repaired.applied_at, authority.applied_at);
        assert_eq!(repaired.source_commit, authority.source_commit);
        assert_eq!(repaired.applied_by, authority.applied_by);

        let replay = apply_explicit_postgres_migrations(
            &pool,
            295,
            SOURCE,
            SOURCE,
            "unpushed",
            "test@legacy",
        )
        .await
        .expect("exact replay is idempotent");
        assert_eq!(replay.current_version, 295);

        sqlx::query(
            "UPDATE _migrations
                SET applied_at = applied_at + interval '1 microsecond'
              WHERE version = 247",
        )
        .execute(&pool)
        .await
        .unwrap();
        let bound_drift = postgres_migration_status(&pool).await.unwrap();
        assert!(
            bound_drift
                .drift
                .iter()
                .any(|item| item.contains("not bound"))
        );
        assert!(
            apply_explicit_postgres_migrations(
                &pool,
                295,
                SOURCE,
                SOURCE,
                "unpushed",
                "test@legacy",
            )
            .await
            .is_err()
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[test]
    fn v291_is_immutable_release_artifact_authority() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 291)
            .expect("v291 must be appended to PG_MIGRATIONS");
        assert_eq!(migration.name, "release_artifact_custody");
        for required in [
            "CREATE TABLE release_artifacts",
            "CREATE TABLE release_artifact_custody",
            "UNIQUE (artifact_name, artifact_version, source_commit, target_triple)",
            "CHECK (source_commit ~ '^[0-9a-f]{40}$')",
            "CHECK (sha256 ~ '^[0-9a-f]{64}$')",
            "CHECK (size_bytes > 0)",
            "REFERENCES computers(id) ON DELETE RESTRICT",
            "PRIMARY KEY (artifact_id, computer_id)",
            "UNIQUE (computer_id, relative_path)",
            "CREATE TRIGGER release_artifacts_immutable",
            "CREATE TRIGGER release_artifact_custody_refresh_only",
            "NEW.last_verified_at < OLD.last_verified_at",
            "position(E'\\\\' in relative_path) = 0",
        ] {
            assert!(migration.sql.contains(required), "v291 missing {required}");
        }
        for forbidden in ["ON DELETE CASCADE", "ON DELETE SET NULL"] {
            assert!(
                !migration.sql.contains(forbidden),
                "v291 custody evidence must not be erasable through {forbidden}"
            );
        }
        assert!(
            !migration.sql.contains("IF NOT EXISTS"),
            "a preexisting authority table must fail the migration, not be reused"
        );
    }

    #[test]
    fn v292_is_exact_fail_closed_james_alias_authority() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 292)
            .expect("v292 must be appended after v291");
        assert_eq!(migration.name, "james_qwen_served_model_alias");
        for required in [
            "FOR UPDATE",
            "jsonb_typeof(original_variants) IS DISTINCT FROM 'array'",
            "matching_variant_count <> 1",
            "alias_field_count = 0",
            "alias_field_count = 1 AND matching_aliases = exact_aliases",
            "WITH ORDINALITY",
            "ORDER BY item.ordinality",
            "variants = original_variants",
            "final_variants IS DISTINCT FROM expected_variants",
            "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf",
        ] {
            assert!(migration.sql.contains(required), "v292 missing {required}");
        }
        for forbidden in ["ILIKE", "LOWER(", "regexp_replace", "translate("] {
            assert!(
                !migration.sql.contains(forbidden),
                "v292 must not use fuzzy identity matching through {forbidden}"
            );
        }
        let v292_position = PG_MIGRATIONS
            .iter()
            .position(|migration| migration.version == 292)
            .expect("v292 position");
        let v293_position = PG_MIGRATIONS
            .iter()
            .position(|migration| migration.version == 293)
            .expect("v293 position");
        assert_eq!(v293_position, v292_position + 1, "v293 must follow v292");
    }

    #[test]
    fn v293_is_exact_fail_closed_smolvlm_variant_authority() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 293)
            .expect("v293 must be appended after v292");
        assert_eq!(migration.name, "smolvlm_exact_variant_authority");
        for required in [
            "FOR UPDATE",
            "catalog_row_count > 1",
            "catalog_row_count = 0",
            "INSERT INTO fleet_model_catalog",
            "exact_nonvariant_state",
            "nonvariant_state IS DISTINCT FROM exact_nonvariant_state",
            "jsonb_typeof(original_variants) IS DISTINCT FROM 'array'",
            "original_variants = '[]'::jsonb",
            "original_variants IS DISTINCT FROM exact_variants",
            "variants = original_variants",
            "final_variants IS DISTINCT FROM exact_variants",
            "ggml-org/SmolVLM2-500M-Video-Instruct-GGUF",
            "ccd7aae53bcb1997355c2f094959e72b3642ce17",
            "6f67b8036b2469fcd71728702720c6b51aebd759b78137a8120733b4d66438bc",
            "921dc7e259f308e5b027111fa185efcbf33db13f6e35749ddf7f5cdb60ef520b",
            "SmolVLM2-500M-Video-Instruct-Q8_0.gguf",
        ] {
            assert!(migration.sql.contains(required), "v293 missing {required}");
        }
        for forbidden in [
            "ILIKE",
            "LOWER(",
            "regexp_replace",
            "translate(",
            "ON CONFLICT",
            "DO UPDATE",
        ] {
            assert!(
                !migration.sql.contains(forbidden),
                "v293 must not repair or fuzz authority through {forbidden}"
            );
        }
        let v293_position = PG_MIGRATIONS
            .iter()
            .position(|migration| migration.version == 293)
            .expect("v293 position");
        let v294_position = PG_MIGRATIONS
            .iter()
            .position(|migration| migration.version == 294)
            .expect("v294 position");
        assert_eq!(v294_position, v293_position + 1, "v294 must follow v293");
    }

    #[test]
    fn v294_is_exact_fail_closed_ace_gemma4_mlx_authority() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 294)
            .expect("v294 must be appended after v293");
        assert_eq!(migration.name, "ace_gemma4_mlx_exact_authority");
        for required in [
            "catalog_row_count = 0",
            "catalog_row_count = 1",
            "FOR UPDATE",
            "actual_row IS DISTINCT FROM exact_row",
            "found duplicate gemma4-e4b-it catalog rows",
            "mlx-community/gemma-4-e4b-it-4bit",
            "475b9088d29754a3379866cf5aeb6b41acd313c2",
            "fee6332c1abaafb77f6f9624236c63aa2f1d0187",
            "aarch64-apple-darwin",
            "5179241512",
            "932b8271fc3fe65adcc78b96c10c6268bbfb13e8f67d1358727c0d6ee97e1eff",
            "cc8d3a0ce36466ccc1278bf987df5f71db1719b9ca6b4118264f45cb627bfe0f",
        ] {
            assert!(migration.sql.contains(required), "v294 missing {required}");
        }
        for forbidden in [
            "ILIKE",
            "LOWER(",
            "ON CONFLICT",
            "DO UPDATE",
            "original_variants = '[]'",
        ] {
            assert!(
                !migration.sql.contains(forbidden),
                "v294 must not repair or fuzz authority through {forbidden}"
            );
        }
        assert_eq!(
            PG_MIGRATIONS
                .iter()
                .position(|migration| migration.version == 294),
            PG_MIGRATIONS.len().checked_sub(1),
            "v294 must remain the final forward migration"
        );
    }

    #[test]
    fn v271_has_cloud_fixes_local_acceptance_artifact() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 271)
            .expect("V271 must be registered");
        assert_eq!(migration.name, "local_failure_diagnoses");
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS local_failure_diagnoses")
        );
        assert!(migration.sql.contains("local_retest_passed"));
        assert!(migration.sql.contains("dreamer_context_pack"));
        assert!(migration.sql.contains("fine_tune_model_ab"));
    }

    #[test]
    fn v272_adds_work_item_retry_count() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 272)
            .expect("V272 must be registered");
        assert_eq!(migration.name, "work_item_retry_count");
        assert!(
            migration
                .sql
                .contains("ADD COLUMN IF NOT EXISTS retry_count")
        );
        assert!(migration.sql.contains("NOT NULL DEFAULT 0"));
    }

    #[test]
    fn v273_defines_single_refresher_cloud_backends() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 273)
            .expect("V273 must be registered");
        assert_eq!(migration.name, "cloud_backends");
        assert!(migration.sql.contains("backend        TEXT PRIMARY KEY"));
        assert!(migration.sql.contains("refresher_node TEXT NOT NULL"));
        for backend in ["claude", "codex", "kimi"] {
            assert!(migration.sql.contains(&format!("('{backend}',")));
        }
    }

    #[test]
    fn v274_preserves_project_repo_scan_history() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 274)
            .expect("V274 must be registered");
        assert_eq!(migration.name, "project_repo_scan_metadata");
        assert!(migration.sql.contains("ALTER TABLE project_repos"));
        assert!(migration.sql.contains("tech_stack TEXT"));
        assert!(migration.sql.contains("local_path TEXT"));
        assert!(!migration.sql.contains("workstream_id"));
    }

    #[test]
    fn v275_defines_capability_store_and_skill_backfill() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 275)
            .expect("V275 must be registered");
        assert_eq!(migration.name, "ff_capabilities");
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS ff_capabilities")
        );
        assert!(migration.sql.contains("FROM skills"));
        assert!(
            migration
                .sql
                .contains("ON CONFLICT (kind, source, source_id) DO UPDATE")
        );
    }

    #[test]
    fn v276_defines_glm_devstral_ab_scoreboard() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 276)
            .expect("V276 must be registered");
        assert_eq!(migration.name, "glm_45_air_ab_scoreboard");
        assert!(migration.sql.contains("'glm-4.5-air'"));
        assert!(migration.sql.contains("'devstral-small-2-24b'"));
        assert!(
            migration
                .sql
                .contains("CREATE OR REPLACE VIEW v_builder_stats")
        );
        assert!(migration.sql.contains("parallel_slots = 4"));
    }

    #[test]
    fn v278_defines_durable_project_workstreams() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 278)
            .expect("V278 must be registered");
        assert_eq!(migration.name, "durable_project_workstreams");
        for artifact in [
            "ff_workstreams",
            "project_id",
            "aliases",
            "session_attachments",
            "last_seen_at",
            "workstream_notes",
            "workstream_threads",
            "workstream_one_thread_per_item",
            "leader_generation",
            "next_seq",
            "owner_identity",
        ] {
            assert!(migration.sql.contains(artifact), "missing {artifact}");
        }
    }

    #[tokio::test]
    async fn v278_applies_when_postgres_is_available() {
        let Some(database_url) = db_url() else {
            return;
        };
        let pool = PgPool::connect(&database_url).await.unwrap();
        sqlx::raw_sql(schema::SCHEMA_V278_DURABLE_PROJECT_WORKSTREAMS)
            .execute(&pool)
            .await
            .unwrap();
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

    async fn prepare_reviewed_legacy_v290(pool: &PgPool) {
        sqlx::raw_sql(schema::BOOTSTRAP_V161_SQL)
            .execute(pool)
            .await
            .expect("fresh baseline");
        for migration in PG_MIGRATIONS.iter().filter(|migration| {
            migration.version > BOOTSTRAP_BASELINE_VERSION && migration.version <= 290
        }) {
            let mut tx = pool.begin().await.unwrap();
            sqlx::raw_sql(migration.sql)
                .execute(&mut *tx)
                .await
                .unwrap_or_else(|error| {
                    panic!("strict fresh v{} failed: {error}", migration.version)
                });
            sqlx::query("INSERT INTO _migrations (version, name) VALUES ($1, $2)")
                .bind(migration.version as i32)
                .bind(migration.name)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }

        // Reproduce the exact reviewed fleet evidence without touching the live
        // database. This fixture intentionally uses DDL only in its disposable
        // database so the production path can remain validation-first.
        sqlx::raw_sql(
            r#"
            DELETE FROM _migrations WHERE version = 247;
            DROP TABLE fleet_log_digest;
            ALTER TABLE error_signatures
                ALTER COLUMN first_seen DROP NOT NULL,
                ALTER COLUMN last_seen DROP NOT NULL,
                ALTER COLUMN count_24h DROP NOT NULL,
                ALTER COLUMN count_total DROP NOT NULL,
                ALTER COLUMN state DROP NOT NULL,
                DROP CONSTRAINT error_signatures_state_check;
            ALTER TABLE work_item_leases
                ADD COLUMN build_started_at TIMESTAMPTZ;

            UPDATE _migrations SET applied_at = '2026-07-20T16:39:14.109001Z'
             WHERE version = 211;
            UPDATE _migrations SET applied_at = '2026-07-26T05:54:24.449824Z'
             WHERE version = 274;
            UPDATE _migrations SET applied_at = '2026-07-26T18:23:36.239244Z'
             WHERE version = 277;

            INSERT INTO _migrations (version, name, applied_at) VALUES
              (234, 'work_item_cortex_subgraph_id', '2026-07-22T04:50:37.318577Z'),
              (236, 'work_item_context_and_cortex_subgraph', '2026-07-22T15:13:32.818634Z'),
              (246, 'glm_45_air_ab_catalog', '2026-07-24T03:03:02.342428Z'),
              (260, 'model_utilization_view', '2026-07-24T20:49:10.028987Z'),
              (270, 'workstream_session_fields', '2026-07-25T19:04:23.380858Z'),
              (280, 'merge_fleet_tables__QUARANTINED_MANUAL', '2026-07-27T06:28:42.086663Z');
            "#,
        )
        .execute(pool)
        .await
        .expect("construct exact reviewed legacy V290 fixture");
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

    async fn prepare_v292_catalog_table(pool: &PgPool) {
        let server_version_num: String = sqlx::query_scalar("SHOW server_version_num")
            .fetch_one(pool)
            .await
            .expect("read disposable postgres version");
        let server_major = server_version_num
            .parse::<u32>()
            .expect("server_version_num must be numeric")
            / 10_000;
        assert_eq!(server_major, 16, "V292 must be proven on PostgreSQL 16");
        sqlx::raw_sql(
            "CREATE TABLE fleet_model_catalog (\
                 id TEXT PRIMARY KEY,\
                 variants JSONB\
             )",
        )
        .execute(pool)
        .await
        .expect("create minimal V292 catalog authority");
    }

    async fn seed_v292_catalog(pool: &PgPool, variants: &serde_json::Value) {
        sqlx::query("DELETE FROM fleet_model_catalog")
            .execute(pool)
            .await
            .expect("reset V292 catalog fixture");
        sqlx::query("INSERT INTO fleet_model_catalog (id, variants) VALUES ($1, $2)")
            .bind("qwen3-vl-30b-a3b")
            .bind(variants)
            .execute(pool)
            .await
            .expect("seed V292 catalog fixture");
    }

    async fn v292_catalog_variants(pool: &PgPool) -> serde_json::Value {
        sqlx::query_scalar("SELECT variants FROM fleet_model_catalog WHERE id = 'qwen3-vl-30b-a3b'")
            .fetch_one(pool)
            .await
            .expect("read V292 catalog fixture")
    }

    async fn v292_rejection(pool: &PgPool) -> String {
        sqlx::raw_sql(schema::SCHEMA_V292_JAMES_QWEN_SERVED_MODEL_ALIAS)
            .execute(pool)
            .await
            .expect_err("V292 fixture drift must fail closed")
            .to_string()
    }

    #[tokio::test]
    async fn v292_adds_exact_alias_preserves_variants_and_is_idempotent_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v292_catalog_table(&pool).await;
        let original = serde_json::json!([
            {
                "runtime": "mlx",
                "quant": "4bit",
                "hf_repo": "mlx-community/Qwen3-VL-30B-A3B-Instruct-4bit",
                "size_gb": 18.0,
                "metadata": {"keep": [1, 2, 3]}
            },
            {
                "runtime": "llama.cpp",
                "quant": "Q4_K_M",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
                "size_gb": 18.0,
                "context_window": 131072
            },
            {
                "runtime": "vllm",
                "quant": "fp16",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct",
                "size_gb": 60.0
            }
        ]);
        seed_v292_catalog(&pool, &original).await;

        sqlx::raw_sql(schema::SCHEMA_V292_JAMES_QWEN_SERVED_MODEL_ALIAS)
            .execute(&pool)
            .await
            .expect("V292 must add the reviewed exact alias");
        let after_add = v292_catalog_variants(&pool).await;
        assert_eq!(after_add[0], original[0], "first variant must be unchanged");
        assert_eq!(after_add[2], original[2], "last variant must be unchanged");
        let mut expected_target = original[1].clone();
        expected_target["served_model_aliases"] =
            serde_json::json!(["Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf"]);
        assert_eq!(
            after_add[1], expected_target,
            "target must retain all prior keys and gain only the exact alias"
        );
        assert_eq!(after_add.as_array().unwrap().len(), 3);

        sqlx::raw_sql(schema::SCHEMA_V292_JAMES_QWEN_SERVED_MODEL_ALIAS)
            .execute(&pool)
            .await
            .expect("exact reviewed V292 end state must be an idempotent no-op");
        assert_eq!(v292_catalog_variants(&pool).await, after_add);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v292_fresh_v161_bootstrap_converges_to_exact_alias_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        let server_version_num: String = sqlx::query_scalar("SHOW server_version_num")
            .fetch_one(&pool)
            .await
            .expect("read disposable postgres version");
        assert_eq!(
            server_version_num.parse::<u32>().unwrap() / 10_000,
            16,
            "V292 must be proven on PostgreSQL 16"
        );

        let final_version = run_postgres_migrations(&pool)
            .await
            .expect("fresh V161 bootstrap plus forward migrations must converge");
        assert_eq!(final_version, 294);
        let migration_name: String =
            sqlx::query_scalar("SELECT name FROM _migrations WHERE version = 292")
                .fetch_one(&pool)
                .await
                .expect("V292 must be durably recorded");
        assert_eq!(migration_name, "james_qwen_served_model_alias");
        let exact_alias_variants: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*)
              FROM fleet_model_catalog catalog,
                   jsonb_array_elements(catalog.variants) variant
             WHERE catalog.id = 'qwen3-vl-30b-a3b'
               AND variant->>'runtime' = 'llama.cpp'
               AND variant->>'hf_repo' = 'Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF'
               AND variant->>'quant' = 'Q4_K_M'
               AND variant->'served_model_aliases'
                   = '["Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf"]'::jsonb
            "#,
        )
        .fetch_one(&pool)
        .await
        .expect("read exact V292 bootstrap authority");
        assert_eq!(exact_alias_variants, 1);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v292_rejects_missing_catalog_row_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v292_catalog_table(&pool).await;

        let error = v292_rejection(&pool).await;
        assert!(
            error.contains("expected exactly one qwen3-vl-30b-a3b catalog row"),
            "unexpected missing-row error: {error}"
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM fleet_model_catalog")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v292_rejects_malformed_or_wrong_variant_selector_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v292_catalog_table(&pool).await;
        let cases = [
            serde_json::json!({"not": "an array"}),
            serde_json::json!([42]),
            serde_json::json!([{
                "runtime": "vllm",
                "quant": "Q4_K_M",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF"
            }]),
            serde_json::json!([{
                "runtime": "llama.cpp",
                "quant": "Q4_K_M",
                "hf_repo": "other/Qwen3-VL-30B-A3B-Instruct-GGUF"
            }]),
            serde_json::json!([{
                "runtime": "llama.cpp",
                "quant": "Q5_K_M",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF"
            }]),
        ];
        for variants in cases {
            seed_v292_catalog(&pool, &variants).await;
            let error = v292_rejection(&pool).await;
            assert!(
                error.contains("variants must")
                    || error.contains("expected exactly one reviewed qwen3-vl llama.cpp variant"),
                "unexpected wrong-variant error: {error}"
            );
            assert_eq!(v292_catalog_variants(&pool).await, variants);
        }

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v292_rejects_duplicate_exact_variant_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v292_catalog_table(&pool).await;
        let exact = serde_json::json!({
            "runtime": "llama.cpp",
            "quant": "Q4_K_M",
            "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
            "size_gb": 18.0
        });
        let variants = serde_json::json!([exact, exact]);
        seed_v292_catalog(&pool, &variants).await;

        let error = v292_rejection(&pool).await;
        assert!(
            error.contains("expected exactly one reviewed qwen3-vl llama.cpp variant"),
            "unexpected duplicate error: {error}"
        );
        assert_eq!(v292_catalog_variants(&pool).await, variants);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v292_rejects_every_preexisting_alias_drift_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v292_catalog_table(&pool).await;
        let exact = serde_json::json!({
            "runtime": "llama.cpp",
            "quant": "Q4_K_M",
            "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
            "size_gb": 18.0
        });
        let other = serde_json::json!({
            "runtime": "mlx",
            "quant": "4bit",
            "hf_repo": "mlx-community/Qwen3-VL-30B-A3B-Instruct-4bit"
        });
        let drift_cases = [
            serde_json::json!([{
                "runtime": "llama.cpp",
                "quant": "Q4_K_M",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
                "served_model_aliases": []
            }]),
            serde_json::json!([{
                "runtime": "llama.cpp",
                "quant": "Q4_K_M",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
                "served_model_aliases": [
                    "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf",
                    "extra.gguf"
                ]
            }]),
            serde_json::json!([
                exact,
                {
                    "runtime": "mlx",
                    "quant": "4bit",
                    "hf_repo": "mlx-community/Qwen3-VL-30B-A3B-Instruct-4bit",
                    "served_model_aliases": ["unreviewed-mlx-alias"]
                }
            ]),
            serde_json::json!([
                {
                    "runtime": "llama.cpp",
                    "quant": "Q4_K_M",
                    "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
                    "served_model_aliases": [
                        "Qwen3VL-30B-A3B-Instruct-Q4_K_M.gguf"
                    ]
                },
                {
                    "runtime": "mlx",
                    "quant": "4bit",
                    "hf_repo": "mlx-community/Qwen3-VL-30B-A3B-Instruct-4bit",
                    "served_model_aliases": ["unreviewed-mlx-alias"]
                }
            ]),
            serde_json::json!([other, {
                "runtime": "llama.cpp",
                "quant": "Q4_K_M",
                "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF",
                "served_model_aliases": null
            }]),
        ];
        for variants in drift_cases {
            seed_v292_catalog(&pool, &variants).await;
            let error = v292_rejection(&pool).await;
            assert!(
                error.contains("unreviewed served-model alias state"),
                "unexpected alias-drift error: {error}"
            );
            assert_eq!(v292_catalog_variants(&pool).await, variants);
        }

        drop_temp_db(admin, pool, &db_name).await;
    }

    fn v293_expected_variants() -> serde_json::Value {
        serde_json::json!([{
            "runtime": "llama.cpp",
            "quant": "Q8_0",
            "hf_repo": "ggml-org/SmolVLM2-500M-Video-Instruct-GGUF",
            "source_revision": "ccd7aae53bcb1997355c2f094959e72b3642ce17",
            "size_gb": 0.545593888,
            "model_file": "SmolVLM2-500M-Video-Instruct-Q8_0.gguf",
            "model_size_bytes": 436808704_i64,
            "model_sha256": "6f67b8036b2469fcd71728702720c6b51aebd759b78137a8120733b4d66438bc",
            "mmproj_file": "mmproj-SmolVLM2-500M-Video-Instruct-Q8_0.gguf",
            "mmproj_size_bytes": 108785184_i64,
            "mmproj_sha256": "921dc7e259f308e5b027111fa185efcbf33db13f6e35749ddf7f5cdb60ef520b",
            "served_model_aliases": ["SmolVLM2-500M-Video-Instruct-Q8_0.gguf"]
        }])
    }

    fn v293_expected_row() -> serde_json::Value {
        serde_json::json!({
            "id": "smolvlm2-500m-video",
            "name": "SmolVLM2 500M Video",
            "family": "smolvlm",
            "parameters": "500M",
            "tier": 1,
            "description": "SmolVLM2 500M - tiny video/vision understanding SLM (research lane)",
            "gated": false,
            "preferred_workloads": ["vision", "video", "multimodal", "slm"],
            "variants": v293_expected_variants(),
            "tool_calling": false,
            "display_name": null,
            "tasks": null,
            "modalities": null,
            "benchmarks": null,
            "license": null,
            "lifecycle": null
        })
    }

    async fn prepare_v293_catalog_table(pool: &PgPool) {
        let server_version_num: String = sqlx::query_scalar("SHOW server_version_num")
            .fetch_one(pool)
            .await
            .expect("read disposable postgres version");
        let server_major = server_version_num
            .parse::<u32>()
            .expect("server_version_num must be numeric")
            / 10_000;
        assert_eq!(server_major, 16, "V293 must be proven on PostgreSQL 16");
        sqlx::raw_sql(
            "CREATE TABLE fleet_model_catalog (\
                 id TEXT NOT NULL,\
                 name TEXT NOT NULL,\
                 family TEXT NOT NULL,\
                 parameters TEXT NOT NULL,\
                 tier INTEGER NOT NULL,\
                 description TEXT,\
                 gated BOOLEAN NOT NULL,\
                 preferred_workloads JSONB NOT NULL,\
                 variants JSONB,\
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),\
                 tool_calling BOOLEAN NOT NULL,\
                 display_name TEXT,\
                 tasks JSONB,\
                 modalities JSONB,\
                 benchmarks JSONB,\
                 license TEXT,\
                 lifecycle TEXT,\
                 sentinel JSONB NOT NULL DEFAULT '{}'::jsonb\
             )",
        )
        .execute(pool)
        .await
        .expect("create minimal V293 catalog authority");
    }

    async fn seed_v293_catalog_row(
        pool: &PgPool,
        variants: &serde_json::Value,
        sentinel: &serde_json::Value,
    ) {
        sqlx::query(
            r#"
            INSERT INTO fleet_model_catalog (
                id, name, family, parameters, tier, description, gated,
                preferred_workloads, variants, tool_calling, display_name,
                tasks, modalities, benchmarks, license, lifecycle, sentinel
            ) VALUES (
                'smolvlm2-500m-video',
                'SmolVLM2 500M Video',
                'smolvlm',
                '500M',
                1,
                'SmolVLM2 500M - tiny video/vision understanding SLM (research lane)',
                false,
                '["vision", "video", "multimodal", "slm"]',
                $1,
                false,
                NULL, NULL, NULL, NULL, NULL, NULL,
                $2
            )
            "#,
        )
        .bind(variants)
        .bind(sentinel)
        .execute(pool)
        .await
        .expect("seed exact V293 catalog metadata");
    }

    async fn v293_rejection(pool: &PgPool) -> String {
        sqlx::raw_sql(schema::SCHEMA_V293_SMOLVLM_EXACT_VARIANT_AUTHORITY)
            .execute(pool)
            .await
            .expect_err("V293 fixture drift must fail closed")
            .to_string()
    }

    #[tokio::test]
    async fn v293_adds_exact_authority_is_idempotent_and_preserves_other_data_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v293_catalog_table(&pool).await;
        seed_v293_catalog_row(
            &pool,
            &serde_json::json!([]),
            &serde_json::json!({"keep": "target"}),
        )
        .await;
        sqlx::raw_sql(
            r#"
            INSERT INTO fleet_model_catalog (
                id, name, family, parameters, tier, description, gated,
                preferred_workloads, variants, tool_calling, sentinel
            ) VALUES (
                'unrelated', 'Unrelated', 'test', '1B', 1, NULL, false,
                '[]', '[{"leave":true}]', false, '{"keep":"other"}'
            );
            "#,
        )
        .execute(&pool)
        .await
        .expect("seed unrelated V293 row");
        let target_before: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'variants' FROM fleet_model_catalog catalog
              WHERE id = 'smolvlm2-500m-video'",
        )
        .fetch_one(&pool)
        .await
        .expect("snapshot target non-variant columns");
        let unrelated_before: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog
              WHERE id = 'unrelated'",
        )
        .fetch_one(&pool)
        .await
        .expect("snapshot unrelated row");

        sqlx::raw_sql(schema::SCHEMA_V293_SMOLVLM_EXACT_VARIANT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("V293 must add exact reviewed authority");
        let after_first: serde_json::Value = sqlx::query_scalar(
            "SELECT variants FROM fleet_model_catalog WHERE id = 'smolvlm2-500m-video'",
        )
        .fetch_one(&pool)
        .await
        .expect("read V293 authority");
        assert_eq!(after_first, v293_expected_variants());

        sqlx::raw_sql(schema::SCHEMA_V293_SMOLVLM_EXACT_VARIANT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("exact reviewed V293 state must be idempotent");
        let target_after: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'variants' FROM fleet_model_catalog catalog
              WHERE id = 'smolvlm2-500m-video'",
        )
        .fetch_one(&pool)
        .await
        .expect("read target non-variant columns");
        let unrelated_after: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog
              WHERE id = 'unrelated'",
        )
        .fetch_one(&pool)
        .await
        .expect("read unrelated row");
        assert_eq!(target_after, target_before);
        assert_eq!(unrelated_after, unrelated_before);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v293_fresh_v161_bootstrap_converges_to_exact_authority_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        let final_version = run_postgres_migrations(&pool)
            .await
            .expect("fresh V161 bootstrap plus forward migrations must converge");
        assert_eq!(final_version, 294);
        let migration_name: String =
            sqlx::query_scalar("SELECT name FROM _migrations WHERE version = 293")
                .fetch_one(&pool)
                .await
                .expect("V293 must be durably recorded");
        assert_eq!(migration_name, "smolvlm_exact_variant_authority");
        let row: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'updated_at' FROM fleet_model_catalog catalog
              WHERE id = 'smolvlm2-500m-video'",
        )
        .fetch_one(&pool)
        .await
        .expect("read bootstrapped V293 authority");
        assert_eq!(row, v293_expected_row());

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v293_seeds_missing_row_and_rejects_duplicate_catalog_rows_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v293_catalog_table(&pool).await;

        sqlx::raw_sql(schema::SCHEMA_V293_SMOLVLM_EXACT_VARIANT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("V293 must seed the exact source-controlled row on fresh bootstrap");
        let seeded: serde_json::Value = sqlx::query_scalar(
            "SELECT variants FROM fleet_model_catalog WHERE id = 'smolvlm2-500m-video'",
        )
        .fetch_one(&pool)
        .await
        .expect("read freshly seeded V293 row");
        assert_eq!(seeded, v293_expected_variants());

        sqlx::query("DELETE FROM fleet_model_catalog")
            .execute(&pool)
            .await
            .expect("reset V293 duplicate fixture");
        seed_v293_catalog_row(&pool, &serde_json::json!([]), &serde_json::json!({})).await;
        seed_v293_catalog_row(&pool, &serde_json::json!([]), &serde_json::json!({})).await;
        let duplicate_error = v293_rejection(&pool).await;
        assert!(
            duplicate_error.contains("found duplicate smolvlm2-500m-video catalog rows"),
            "unexpected duplicate-row error: {duplicate_error}"
        );
        let variants: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT variants FROM fleet_model_catalog
              WHERE id = 'smolvlm2-500m-video'",
        )
        .fetch_all(&pool)
        .await
        .expect("read duplicate V293 rows");
        assert_eq!(variants, vec![serde_json::json!([]), serde_json::json!([])]);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v293_rejects_malformed_duplicate_or_drifted_variants_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v293_catalog_table(&pool).await;
        let exact = v293_expected_variants()[0].clone();
        let drift_cases = [
            serde_json::json!(null),
            serde_json::json!({"not": "an array"}),
            serde_json::json!([42]),
            serde_json::json!([exact.clone(), exact]),
            serde_json::json!([{
                "runtime": "llama.cpp",
                "quant": "Q8_0",
                "hf_repo": "ggml-org/SmolVLM2-500M-Video-Instruct-GGUF",
                "source_revision": "wrong",
                "served_model_aliases": ["SmolVLM2-500M-Video-Instruct-Q8_0.gguf"]
            }]),
        ];
        for variants in drift_cases {
            sqlx::query("DELETE FROM fleet_model_catalog")
                .execute(&pool)
                .await
                .expect("reset V293 drift fixture");
            seed_v293_catalog_row(&pool, &variants, &serde_json::json!({"keep": true})).await;
            let error = v293_rejection(&pool).await;
            assert!(
                error.contains("variants must be a JSON array")
                    || error.contains("unreviewed SmolVLM variant authority"),
                "unexpected V293 drift error: {error}"
            );
            let after: serde_json::Value = sqlx::query_scalar(
                "SELECT variants FROM fleet_model_catalog WHERE id = 'smolvlm2-500m-video'",
            )
            .fetch_one(&pool)
            .await
            .expect("read rejected V293 fixture");
            assert_eq!(after, variants);
        }

        sqlx::query("DELETE FROM fleet_model_catalog")
            .execute(&pool)
            .await
            .expect("reset V293 metadata drift fixture");
        seed_v293_catalog_row(&pool, &serde_json::json!([]), &serde_json::json!({})).await;
        sqlx::query(
            "UPDATE fleet_model_catalog SET name = 'Drifted SmolVLM' \
              WHERE id = 'smolvlm2-500m-video'",
        )
        .execute(&pool)
        .await
        .expect("drift V293 metadata fixture");
        let metadata_error = v293_rejection(&pool).await;
        assert!(
            metadata_error.contains("found unreviewed SmolVLM catalog metadata"),
            "unexpected V293 metadata error: {metadata_error}"
        );
        let after_metadata_rejection: (String, serde_json::Value) = sqlx::query_as(
            "SELECT name, variants FROM fleet_model_catalog \
              WHERE id = 'smolvlm2-500m-video'",
        )
        .fetch_one(&pool)
        .await
        .expect("read rejected V293 metadata fixture");
        assert_eq!(after_metadata_rejection.0, "Drifted SmolVLM");
        assert_eq!(after_metadata_rejection.1, serde_json::json!([]));

        drop_temp_db(admin, pool, &db_name).await;
    }

    async fn v294_rejection(pool: &PgPool) -> String {
        sqlx::raw_sql(schema::SCHEMA_V294_ACE_GEMMA4_MLX_EXACT_AUTHORITY)
            .execute(pool)
            .await
            .expect_err("V294 authority drift must fail closed")
            .to_string()
    }

    #[tokio::test]
    async fn v294_absent_to_exact_is_idempotent_and_preserves_unrelated_rows_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v293_catalog_table(&pool).await;
        sqlx::raw_sql(
            r#"
            INSERT INTO fleet_model_catalog (
                id, name, family, parameters, tier, description, gated,
                preferred_workloads, variants, tool_calling, sentinel
            ) VALUES (
                'unrelated-v294', 'Unrelated', 'test', '1B', 1, NULL, false,
                '[]', '[{"keep":true}]', false, '{"keep":"other"}'
            );
            "#,
        )
        .execute(&pool)
        .await
        .expect("seed unrelated V294 row");
        let unrelated_before: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog
              WHERE id = 'unrelated-v294'",
        )
        .fetch_one(&pool)
        .await
        .expect("snapshot unrelated V294 row");

        sqlx::raw_sql(schema::SCHEMA_V294_ACE_GEMMA4_MLX_EXACT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("V294 must insert an absent exact authority row");
        let first: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'updated_at' - 'sentinel'
               FROM fleet_model_catalog catalog WHERE id = 'gemma4-e4b-it'",
        )
        .fetch_one(&pool)
        .await
        .expect("read exact V294 row");
        assert_eq!(first["id"], "gemma4-e4b-it");
        assert_eq!(first["family"], "gemma");
        assert_eq!(first["parameters"], "E4B");
        assert_eq!(first["tool_calling"], false);
        assert_eq!(first["variants"].as_array().unwrap().len(), 1);
        let variant = &first["variants"][0];
        assert_eq!(variant["runtime"], "mlx");
        assert_eq!(variant["quant"], "4bit");
        assert_eq!(
            variant["source_revision"],
            "475b9088d29754a3379866cf5aeb6b41acd313c2"
        );
        assert_eq!(variant["artifact_size_bytes"], 5_179_241_512_i64);
        assert_eq!(variant["files"].as_array().unwrap().len(), 10);

        sqlx::raw_sql(schema::SCHEMA_V294_ACE_GEMMA4_MLX_EXACT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("exact final V294 state must be idempotent");
        let after_replay: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'updated_at' - 'sentinel'
               FROM fleet_model_catalog catalog WHERE id = 'gemma4-e4b-it'",
        )
        .fetch_one(&pool)
        .await
        .expect("read replayed V294 row");
        let unrelated_after: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog
              WHERE id = 'unrelated-v294'",
        )
        .fetch_one(&pool)
        .await
        .expect("read unrelated V294 row");
        assert_eq!(after_replay, first);
        assert_eq!(unrelated_after, unrelated_before);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v294_rejects_empty_partial_metadata_and_duplicate_states_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        prepare_v293_catalog_table(&pool).await;
        sqlx::raw_sql(schema::SCHEMA_V294_ACE_GEMMA4_MLX_EXACT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("seed exact V294 authority");

        sqlx::query("UPDATE fleet_model_catalog SET variants = '[]' WHERE id = 'gemma4-e4b-it'")
            .execute(&pool)
            .await
            .expect("create empty V294 variant drift");
        let empty_error = v294_rejection(&pool).await;
        assert!(
            empty_error.contains("found unreviewed Gemma 4 E4B authority state"),
            "unexpected empty-state error: {empty_error}"
        );
        let variants_after: serde_json::Value = sqlx::query_scalar(
            "SELECT variants FROM fleet_model_catalog WHERE id = 'gemma4-e4b-it'",
        )
        .fetch_one(&pool)
        .await
        .expect("read rejected empty V294 state");
        assert_eq!(variants_after, serde_json::json!([]));

        sqlx::query("DELETE FROM fleet_model_catalog WHERE id = 'gemma4-e4b-it'")
            .execute(&pool)
            .await
            .expect("reset V294 drift fixture");
        sqlx::raw_sql(schema::SCHEMA_V294_ACE_GEMMA4_MLX_EXACT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("restore exact V294 authority");
        sqlx::query(
            "UPDATE fleet_model_catalog SET name = 'Drifted Gemma' WHERE id = 'gemma4-e4b-it'",
        )
        .execute(&pool)
        .await
        .expect("create V294 metadata drift");
        let metadata_error = v294_rejection(&pool).await;
        assert!(metadata_error.contains("found unreviewed Gemma 4 E4B authority state"));
        let name_after: String =
            sqlx::query_scalar("SELECT name FROM fleet_model_catalog WHERE id = 'gemma4-e4b-it'")
                .fetch_one(&pool)
                .await
                .expect("read rejected V294 metadata drift");
        assert_eq!(name_after, "Drifted Gemma");

        sqlx::query("DELETE FROM fleet_model_catalog WHERE id = 'gemma4-e4b-it'")
            .execute(&pool)
            .await
            .expect("reset V294 duplicate fixture");
        sqlx::raw_sql(schema::SCHEMA_V294_ACE_GEMMA4_MLX_EXACT_AUTHORITY)
            .execute(&pool)
            .await
            .expect("restore exact V294 row for duplicate fixture");
        sqlx::query(
            "INSERT INTO fleet_model_catalog
             SELECT * FROM fleet_model_catalog WHERE id = 'gemma4-e4b-it'",
        )
        .execute(&pool)
        .await
        .expect("create duplicate exact V294 row");
        let duplicate_error = v294_rejection(&pool).await;
        assert!(
            duplicate_error.contains("found duplicate gemma4-e4b-it catalog rows"),
            "unexpected duplicate-state error: {duplicate_error}"
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v294_fresh_v161_bootstrap_converges_to_exact_authority_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        let final_version = run_postgres_migrations(&pool)
            .await
            .expect("fresh V161 bootstrap plus V294 must converge");
        assert_eq!(final_version, 294);
        let migration_name: String =
            sqlx::query_scalar("SELECT name FROM _migrations WHERE version = 294")
                .fetch_one(&pool)
                .await
                .expect("V294 must be durably recorded");
        assert_eq!(migration_name, "ace_gemma4_mlx_exact_authority");
        let exact_files: i32 = sqlx::query_scalar(
            "SELECT jsonb_array_length(variants->0->'files')
               FROM fleet_model_catalog WHERE id = 'gemma4-e4b-it'",
        )
        .fetch_one(&pool)
        .await
        .expect("read bootstrapped V294 authority");
        assert_eq!(exact_files, 10);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v296_v297_wait_for_v295_then_apply_in_order_on_pg16() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        let held_version = run_postgres_migrations(&pool)
            .await
            .expect("fresh automatic migrations must stop before gated V296");
        assert_eq!(held_version, 294);
        let absent_before_v295: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM fleet_model_catalog \
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("count fresh Devstral rows before V295");
        assert_eq!(absent_before_v295, 0);
        let v296_before_v295: i64 =
            sqlx::query_scalar("SELECT count(*) FROM _migrations WHERE version = 296")
                .fetch_one(&pool)
                .await
                .expect("read held V296 ledger state");
        assert_eq!(v296_before_v295, 0);

        sqlx::query(
            "INSERT INTO _migrations (version, name) \
             VALUES (295, 'release_rollout_authority')",
        )
        .execute(&pool)
        .await
        .expect("record reviewed V295 prerequisite fixture");
        let final_version = run_postgres_migrations(&pool)
            .await
            .expect("V296/V297 must run once explicit V295 is recorded");
        assert_eq!(final_version, 297);
        let migration_name: String =
            sqlx::query_scalar("SELECT name FROM _migrations WHERE version = 296")
                .fetch_one(&pool)
                .await
                .expect("V296 must be durably recorded");
        assert_eq!(migration_name, "devstral_code_capability_authority");
        let rollback_migration_name: String =
            sqlx::query_scalar("SELECT name FROM _migrations WHERE version = 297")
                .fetch_one(&pool)
                .await
                .expect("V297 must be durably recorded");
        assert_eq!(
            rollback_migration_name,
            "release_rollout_post_success_rollback"
        );

        let seeded: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'updated_at' \
               FROM fleet_model_catalog catalog \
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("read seeded Devstral authority");
        assert_eq!(
            seeded,
            serde_json::json!({
                "id": "devstral-small-2-24b",
                "name": "Devstral Small 2 24B",
                "family": "devstral",
                "parameters": "24B",
                "tier": 2,
                "description": "Mistral dense 24B - multi-file coding/agentic specialist",
                "gated": false,
                "preferred_workloads": ["reasoning", "tool_calling", "code"],
                "variants": [{
                    "runtime": "llama.cpp",
                    "quant": "UD-Q4_K_XL",
                    "hf_repo": "unsloth/Devstral-Small-2-24B-Instruct-2512-GGUF",
                    "size_gb": 14
                }],
                "tool_calling": true,
                "display_name": null,
                "tasks": null,
                "modalities": null,
                "benchmarks": null,
                "license": null,
                "lifecycle": null
            })
        );

        sqlx::raw_sql(schema::SCHEMA_V296_DEVSTRAL_CODE_CAPABILITY_AUTHORITY)
            .execute(&pool)
            .await
            .expect("exact seeded V296 authority must replay idempotently");
        let seeded_after_replay: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'updated_at' \
               FROM fleet_model_catalog catalog \
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("read replayed seeded authority");
        assert_eq!(seeded_after_replay, seeded);

        sqlx::raw_sql(
            r#"
            INSERT INTO fleet_model_catalog (id, name, family, parameters, tier)
            VALUES ('v296-unrelated', 'Unrelated', 'test', '1B', 4);

            UPDATE fleet_model_catalog
               SET name = 'Operator Devstral',
                   family = 'operator-family',
                   parameters = '99B',
                   tier = 4,
                   description = 'operator description',
                   gated = true,
                   preferred_workloads = '["reasoning", "tool_calling"]',
                   variants = '[{"operator":true}]',
                   updated_at = '2026-01-02T03:04:05Z',
                   tool_calling = false,
                   display_name = 'Operator Display',
                   tasks = '["operator-task"]',
                   modalities = '["operator-modality"]',
                   benchmarks = '{"operator":99}',
                   license = 'operator-license',
                   lifecycle = 'candidate'
             WHERE id = 'devstral-small-2-24b';
            "#,
        )
        .execute(&pool)
        .await
        .expect("install upgraded-database preservation fixture");
        let nonworkload_before: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) - 'preferred_workloads' \
               FROM fleet_model_catalog catalog \
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("snapshot every non-workload column");
        let unrelated_before: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog \
              WHERE id = 'v296-unrelated'",
        )
        .fetch_one(&pool)
        .await
        .expect("snapshot unrelated row");

        sqlx::raw_sql(schema::SCHEMA_V296_DEVSTRAL_CODE_CAPABILITY_AUTHORITY)
            .execute(&pool)
            .await
            .expect("V296 must append the missing code capability");
        let repaired: (serde_json::Value, serde_json::Value) = sqlx::query_as(
            "SELECT preferred_workloads, to_jsonb(catalog) - 'preferred_workloads' \
               FROM fleet_model_catalog catalog \
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("read repaired upgraded row");
        assert_eq!(
            repaired.0,
            serde_json::json!(["reasoning", "tool_calling", "code"])
        );
        assert_eq!(repaired.1, nonworkload_before);

        sqlx::raw_sql(schema::SCHEMA_V296_DEVSTRAL_CODE_CAPABILITY_AUTHORITY)
            .execute(&pool)
            .await
            .expect("repaired upgraded state must replay idempotently");
        let repaired_after_replay: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog \
              WHERE id = 'devstral-small-2-24b'",
        )
        .fetch_one(&pool)
        .await
        .expect("read replayed repaired row");
        assert_eq!(
            repaired_after_replay["preferred_workloads"],
            serde_json::json!(["reasoning", "tool_calling", "code"])
        );

        for protected_workloads in [
            serde_json::json!(["reasoning", "CoDe"]),
            serde_json::json!("code"),
            serde_json::json!({"code": true}),
        ] {
            sqlx::query(
                "UPDATE fleet_model_catalog SET preferred_workloads = $1 \
                  WHERE id = 'devstral-small-2-24b'",
            )
            .bind(&protected_workloads)
            .execute(&pool)
            .await
            .expect("install protected workload fixture");
            let before: serde_json::Value = sqlx::query_scalar(
                "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog \
                  WHERE id = 'devstral-small-2-24b'",
            )
            .fetch_one(&pool)
            .await
            .expect("snapshot protected workload fixture");
            sqlx::raw_sql(schema::SCHEMA_V296_DEVSTRAL_CODE_CAPABILITY_AUTHORITY)
                .execute(&pool)
                .await
                .expect("V296 protected workload replay");
            let after: serde_json::Value = sqlx::query_scalar(
                "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog \
                  WHERE id = 'devstral-small-2-24b'",
            )
            .fetch_one(&pool)
            .await
            .expect("read protected workload fixture");
            assert_eq!(after, before);
        }

        let unrelated_after: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(catalog) FROM fleet_model_catalog catalog \
              WHERE id = 'v296-unrelated'",
        )
        .fetch_one(&pool)
        .await
        .expect("read unrelated row after V296 replays");
        assert_eq!(unrelated_after, unrelated_before);

        drop_temp_db(admin, pool, &db_name).await;
    }

    async fn reset_enrollment_authority_to_original_v289(pool: &PgPool) {
        sqlx::raw_sql(
            r#"
            DROP TABLE public.fleet_enrollment_tokens CASCADE;
            DROP INDEX IF EXISTS public.idx_computers_enrollment_canonical_name;
            DROP INDEX IF EXISTS public.idx_computers_enrollment_primary_ip;
            DROP INDEX IF EXISTS public.idx_fleet_workers_enrollment_canonical_name;
            DROP INDEX IF EXISTS public.idx_fleet_workers_enrollment_ip;
            DROP INDEX IF EXISTS public.idx_fleet_nodes_enrollment_canonical_name;
            DROP INDEX IF EXISTS public.idx_fleet_nodes_enrollment_primary_ip;
            DROP INDEX IF EXISTS public.idx_fleet_nodes_enrollment_worker_ip;
            "#,
        )
        .execute(pool)
        .await
        .expect("remove hardened enrollment authority");
        sqlx::raw_sql(schema::SCHEMA_V289_SECURE_ENROLLMENT_TOKENS)
            .execute(pool)
            .await
            .expect("recreate immutable V289 enrollment authority");
        sqlx::query("DELETE FROM _migrations WHERE version >= 290")
            .execute(pool)
            .await
            .expect("rewind V290 and every later test marker");
    }

    async fn switch_test_roster_to_legacy_tables(pool: &PgPool) {
        let already_legacy: bool = sqlx::query_scalar(
            "SELECT count(*) = 2
               FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'public' AND c.relkind = 'r'
                AND c.relname IN ('computers', 'fleet_workers')",
        )
        .fetch_one(pool)
        .await
        .expect("inspect test roster relations");
        if already_legacy {
            return;
        }
        sqlx::raw_sql(
            r#"
            DROP INDEX IF EXISTS public.idx_fleet_nodes_enrollment_canonical_name;
            DROP INDEX IF EXISTS public.idx_fleet_nodes_enrollment_primary_ip;
            DROP INDEX IF EXISTS public.idx_fleet_nodes_enrollment_worker_ip;
            ALTER VIEW public.computers RENAME TO computers_unified_test;
            ALTER VIEW public.fleet_workers RENAME TO fleet_workers_unified_test;
            CREATE TABLE public.computers (
                name TEXT NOT NULL,
                primary_ip TEXT
            );
            CREATE TABLE public.fleet_workers (
                name TEXT NOT NULL,
                ip TEXT
            );
            "#,
        )
        .execute(pool)
        .await
        .expect("replace unified test projections with legacy roster tables");
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
    async fn v273_assigns_one_non_null_refresher_per_cloud_backend() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT backend, refresher_node
               FROM cloud_backends
              ORDER BY backend",
        )
        .fetch_all(&pool)
        .await
        .expect("cloud backend ownership should be queryable");
        assert_eq!(
            rows,
            vec![
                ("claude".to_string(), "ace".to_string()),
                ("codex".to_string(), "ace".to_string()),
                ("kimi".to_string(), "sarah".to_string()),
            ]
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v275_backfills_skills_idempotently() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on fresh DB");

        let skill_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO skills
                (id, name, source, version, description, tools, body_md, body_sha256)
             VALUES ($1, 'v275-test', 'test', '1.0.0', 'test skill',
                     '[\"shell\"]', '# test', 'test-sha')",
        )
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("insert test skill");

        for _ in 0..2 {
            sqlx::raw_sql(schema::SCHEMA_V275_FF_CAPABILITIES)
                .execute(&pool)
                .await
                .expect("capability backfill is rerunnable");
        }

        let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "SELECT status, spec
               FROM ff_capabilities
              WHERE kind = 'skill' AND source = 'test' AND source_id = $1",
        )
        .bind(skill_id)
        .fetch_all(&pool)
        .await
        .expect("read backfilled capability");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "active");
        assert_eq!(rows[0].1["body_md"], "# test");
        assert_eq!(rows[0].1["tools"], serde_json::json!(["shell"]));

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
            "INSERT INTO computers
                (name, primary_ip, total_ram_gb, os_family, ssh_user)
             VALUES ('v227-old-name', '10.0.0.227', 32, 'linux-ubuntu', 'test')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert initial computer");

        let upserted_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO computers
                (name, primary_ip, total_ram_gb, os_family, ssh_user)
             VALUES ('v227-new-name', '10.0.0.227', 64, 'linux-ubuntu', 'test')
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
            "INSERT INTO computers
                (name, primary_ip, total_ram_gb, os_family, ssh_user)
             VALUES ('v184-metrics-node', '127.0.0.184', 64, 'linux-ubuntu', 'test')
             RETURNING id",
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
    async fn v211_preserves_taylor_github_identity_history() {
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
            "INSERT INTO computers (name, primary_ip, os_family, ssh_user)
             VALUES ('v215-test-node', '10.0.0.215', 'linux-ubuntu', 'test')
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

        // Slot synchronization expands the stored capacity while rows are
        // inserted. Reassert the simulated daemon-computed capacity before
        // exercising V215's release boundary.
        sqlx::query("UPDATE fleet_workers SET sub_agent_count = 2 WHERE name = 'v215-test-node'")
            .execute(&pool)
            .await
            .expect("restore bounded test capacity");
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

    #[tokio::test]
    async fn v243_adds_fleet_model_catalog_rich_fields() {
        // CI commonly has no Postgres; the helper checks both supported URL vars.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on a fresh database");

        let row: crate::models::FleetModelCatalog = sqlx::query_as(
            "INSERT INTO fleet_model_catalog
                (id, name, family, parameters, tier, gated, preferred_workloads, variants,
                 tool_calling, display_name, tasks, modalities, benchmarks, license, lifecycle)
             VALUES
                ('v243-test', 'V243 Test', 'test', '1B', 1, false, '[]', '[]', false,
                 'V243 Test Model', '[\"chat\"]', '[\"text\"]', '{\"mmlu\": 80.0}',
                 'apache-2.0', 'preview')
             RETURNING id, name, family, parameters, tier, description, gated,
                       preferred_workloads, variants, tool_calling, updated_at,
                       display_name, tasks, modalities, benchmarks, license, lifecycle",
        )
        .fetch_one(&pool)
        .await
        .expect("insert + read row with rich fields");

        assert_eq!(row.display_name.as_deref(), Some("V243 Test Model"));
        assert_eq!(row.license.as_deref(), Some("apache-2.0"));
        assert_eq!(row.lifecycle.as_deref(), Some("preview"));
        assert!(row.tasks.is_some());
        assert!(row.modalities.is_some());
        assert!(row.benchmarks.is_some());

        let existing_row_defaults: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT display_name, lifecycle FROM fleet_model_catalog
              WHERE id = 'qwen3-4b-instruct-2507'",
        )
        .fetch_one(&pool)
        .await
        .expect("read pre-existing seeded row");
        assert_eq!(existing_row_defaults, (None, None));

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v244_adds_sub_agents_capability_columns() {
        // CI commonly has no Postgres; the helper checks both supported URL vars.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on a fresh database");

        let computer_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO computers (id, name, primary_ip, os_family, ssh_user)
             VALUES ($1, 'v244-testbox', '127.0.0.244', 'linux-ubuntu', 'test')",
        )
        .bind(computer_id)
        .execute(&pool)
        .await
        .expect("insert computers row");

        let row: crate::models::Slot = sqlx::query_as(
            "INSERT INTO sub_agents
                (id, computer_id, slot, workspace_dir, capabilities, skill, ram_gb)
             VALUES
                ($1, $2, 0, '/tmp/v244-test', '[\"gpu\"]', '[\"rust\"]', 64)
             RETURNING id, computer_id, slot, status, current_work_item_id, started_at,
                       workspace_dir, model_preference, last_heartbeat_at, metadata, kind,
                       capabilities, skill, ram_gb",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(computer_id)
        .fetch_one(&pool)
        .await
        .expect("insert + read row with capability columns");

        assert_eq!(row.capabilities, serde_json::json!(["gpu"]));
        assert_eq!(row.skill, serde_json::json!(["rust"]));
        assert_eq!(row.ram_gb, Some(64));

        // Pre-existing rows (created before V244) get the tag columns'
        // defaults and a NULL ram_gb rather than failing to read.
        sqlx::query(
            "INSERT INTO sub_agents (id, computer_id, slot, workspace_dir)
             VALUES ($1, $2, 1, '/tmp/v244-test-defaults')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(computer_id)
        .execute(&pool)
        .await
        .expect("insert row without capability columns set");

        let defaults: (serde_json::Value, serde_json::Value, Option<i32>) = sqlx::query_as(
            "SELECT capabilities, skill, ram_gb FROM sub_agents WHERE computer_id = $1 AND slot = 1",
        )
        .bind(computer_id)
        .fetch_one(&pool)
        .await
        .expect("read back defaulted row");
        assert_eq!(defaults.0, serde_json::json!([]));
        assert_eq!(defaults.1, serde_json::json!([]));
        assert_eq!(defaults.2, None);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v247_creates_error_miner_tables() {
        // CI commonly has no Postgres; the helper checks both supported URL vars.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply on a fresh database");

        let signature: crate::models::ErrorSignature = sqlx::query_as(
            "INSERT INTO error_signatures
                (signature, error_class, count_24h, count_total, sample_text, affected_nodes)
             VALUES ('v247-test-sig', 'ssh:timeout', 3, 3, 'sample', '[\"node-a\"]')
             RETURNING signature, error_class, first_seen, last_seen, count_24h, count_total,
                       sample_text, affected_nodes, state, work_item_id, fix_commit_sha, resolved_at",
        )
        .fetch_one(&pool)
        .await
        .expect("insert + read error_signatures row");

        assert_eq!(signature.state, "new");
        assert_eq!(signature.count_24h, 3);
        assert!(signature.work_item_id.is_none());

        let digest: crate::models::FleetLogDigest = sqlx::query_as(
            "INSERT INTO fleet_log_digest (node, day, level, line_class, count, sample)
             VALUES ('node-a', CURRENT_DATE, 'warning', 'connection refused', 5, 'sample line')
             RETURNING id, node, day, level, line_class, count, sample",
        )
        .fetch_one(&pool)
        .await
        .expect("insert + read fleet_log_digest row");

        assert_eq!(digest.count, 5);

        let conflict: crate::models::FleetLogDigest = sqlx::query_as(
            "INSERT INTO fleet_log_digest (node, day, level, line_class, count, sample)
             VALUES ('node-a', CURRENT_DATE, 'warning', 'connection refused', 2, 'sample line 2')
             ON CONFLICT (node, day, level, line_class) DO UPDATE SET count = EXCLUDED.count
             RETURNING id, node, day, level, line_class, count, sample",
        )
        .fetch_one(&pool)
        .await
        .expect("upsert on unique (node, day, level, line_class)");

        assert_eq!(conflict.id, digest.id);
        assert_eq!(conflict.count, 2);

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[test]
    fn v279_creates_fleet_logs_table() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 279)
            .expect("V279 must be registered");
        assert_eq!(migration.name, "fleet_logs");
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS fleet_logs")
        );
        assert!(migration.sql.contains("node_id"));
        assert!(migration.sql.contains("log_level"));
        assert!(migration.sql.contains("message"));
    }

    #[test]
    fn v277_is_node_health_and_v280_is_not_runnable() {
        let v277 = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 277)
            .expect("historical V277 must be registered");
        assert_eq!(v277.name, "node_health");
        assert!(v277.sql.contains("CREATE TABLE IF NOT EXISTS node_health"));
        assert!(!v277.sql.contains("build_started_at"));
        assert!(
            PG_MIGRATIONS
                .iter()
                .all(|migration| migration.version != 280)
        );
        assert!(schema::SCHEMA_V280_MERGE_FLEET_TABLES.contains("RENAME TO fleet_nodes"));
        assert!(REVIEWED_LEGACY_LEDGER_ROWS.iter().any(|row| {
            row.version == 280 && row.name == "merge_fleet_tables__QUARANTINED_MANUAL"
        }));
    }

    #[test]
    fn v282_creates_oplog_replay_tables() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 282)
            .expect("V282 must be registered");
        assert_eq!(migration.name, "oplog_replay");
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS isolated_node_oplog")
        );
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS oplog_shared_state")
        );
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS oplog_replay_checkpoints")
        );
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS oplog_replay_applied")
        );
    }

    #[test]
    fn v284_creates_token_fenced_model_load_reservations() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 284)
            .expect("V284 must be registered");
        assert_eq!(migration.name, "model_load_reservations");
        assert!(migration.sql.contains("model_load_reservations"));
        assert!(migration.sql.contains("owner_token UUID NOT NULL"));
        assert!(migration.sql.contains("expires_at TIMESTAMPTZ NOT NULL"));
        assert!(
            migration
                .sql
                .contains("ADD COLUMN IF NOT EXISTS process_start_marker TEXT")
        );
        assert!(
            migration
                .sql
                .contains("agent_profile_verified_at TIMESTAMPTZ")
        );
    }

    #[test]
    fn v285_marks_operator_owned_fabric_endpoints() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 285)
            .expect("v285 migration registered");
        assert!(migration.sql.contains("endpoints_explicit"));
        assert!(migration.sql.contains("DEFAULT FALSE"));
    }

    #[tokio::test]
    async fn v284_reservation_races_expiry_and_matching_cleanup() {
        // CI has no database; this helper checks both supported URL variables.
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };
        run_postgres_migrations(&pool)
            .await
            .expect("migrations should apply");

        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        assert!(
            crate::queries::pg_reserve_model_load(&pool, "node-a", 55000, "lib-a", first, 60)
                .await
                .unwrap()
        );

        let same_port =
            crate::queries::pg_reserve_model_load(&pool, "node-a", 55000, "lib-b", second, 60);
        let same_library =
            crate::queries::pg_reserve_model_load(&pool, "node-a", 55001, "lib-a", second, 60);
        let (same_port, same_library) = tokio::join!(same_port, same_library);
        assert!(!same_port.unwrap());
        assert!(!same_library.unwrap());

        assert_eq!(
            crate::queries::pg_release_model_load_reservation(
                &pool, "node-a", 55000, "lib-a", second,
            )
            .await
            .unwrap(),
            0
        );
        sqlx::query("UPDATE model_load_reservations SET expires_at = NOW() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            crate::queries::pg_reserve_model_load(&pool, "node-a", 55000, "lib-a", second, 60)
                .await
                .unwrap()
        );
        assert_eq!(
            crate::queries::pg_release_model_load_reservation(
                &pool, "node-a", 55000, "lib-a", first,
            )
            .await
            .unwrap(),
            0
        );
        let owners: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT owner_token FROM model_load_reservations ORDER BY resource_kind",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(owners, vec![second, second]);

        sqlx::query(
            "INSERT INTO fleet_workers (name, ip, ssh_user)
             VALUES ('activation-node', '127.0.0.1', 'tester')
             ON CONFLICT (name) DO NOTHING",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO fleet_model_catalog
                    (id, name, family, parameters, tier, preferred_workloads, tool_calling)
             VALUES ('model-a', 'Model A', 'test', '1B', 1, '[\"code-gen\"]', TRUE)
             ON CONFLICT (id) DO UPDATE
                 SET preferred_workloads = EXCLUDED.preferred_workloads,
                     tool_calling = TRUE",
        )
        .execute(&pool)
        .await
        .unwrap();
        let library_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fleet_model_library
                    (worker_name, catalog_id, runtime, file_path)
             VALUES ('activation-node', 'model-a', 'llama.cpp', '/models/a.gguf')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let library_id = library_id.to_string();
        let first_activation = crate::queries::pg_activate_deployment_if_vacant(
            &pool,
            "activation-node",
            &library_id,
            "model-a",
            "llama.cpp",
            55000,
            101,
            "start-101",
            "healthy",
            32768,
            1,
            true,
        );
        let second_activation = crate::queries::pg_activate_deployment_if_vacant(
            &pool,
            "activation-node",
            &library_id,
            "model-a",
            "llama.cpp",
            55000,
            202,
            "start-202",
            "healthy",
            32768,
            1,
            true,
        );
        let (first_activation, second_activation) =
            tokio::join!(first_activation, second_activation);
        let activations = [first_activation.unwrap(), second_activation.unwrap()];
        assert_eq!(
            activations.iter().filter(|result| result.is_some()).count(),
            1,
            "exactly one vacant activation may win"
        );
        let deployment_id = activations.into_iter().flatten().next().unwrap();
        let (old_pid, old_start): (i32, String) = sqlx::query_as(
            "SELECT pid, process_start_marker
               FROM fleet_model_deployments
              WHERE id = $1::uuid",
        )
        .bind(&deployment_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let supply = crate::queries::pg_supplied_slots_by_kind(&pool, 32768)
            .await
            .unwrap();
        assert_eq!(supply.code_count, 1);
        sqlx::query(
            "UPDATE fleet_model_deployments
                SET agent_profile_verified_at = NOW() - INTERVAL '4 minutes'
              WHERE id = $1::uuid",
        )
        .bind(&deployment_id)
        .execute(&pool)
        .await
        .unwrap();
        let stale_supply = crate::queries::pg_supplied_slots_by_kind(&pool, 32768)
            .await
            .unwrap();
        assert_eq!(stale_supply.code_count, 0);
        assert_eq!(
            stale_supply.code_endpoints.len(),
            1,
            "stale desired intent remains visible but is not usable supply"
        );
        assert!(
            crate::queries::pg_activate_expected_deployment(
                &pool,
                &deployment_id,
                "activation-node",
                55000,
                &library_id,
                Some("model-a"),
                Some(old_pid),
                Some("wrong-start"),
                "llama.cpp",
                303,
                "start-303",
                "healthy",
                32768,
                1,
                true,
            )
            .await
            .unwrap()
            .is_none(),
            "stale process identity must not replace desired placement"
        );
        assert_eq!(
            crate::queries::pg_activate_expected_deployment(
                &pool,
                &deployment_id,
                "activation-node",
                55000,
                &library_id,
                Some("model-a"),
                Some(old_pid),
                Some(&old_start),
                "llama.cpp",
                303,
                "start-303",
                "healthy",
                32768,
                1,
                true,
            )
            .await
            .unwrap()
            .as_deref(),
            Some(deployment_id.as_str())
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[test]
    fn v283_creates_project_digest_attempt_ledger() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 283)
            .expect("V283 must be registered");
        assert_eq!(migration.name, "project_digest_attempts");
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS project_digest_attempts")
        );
        assert!(
            migration
                .sql
                .contains("CREATE TABLE IF NOT EXISTS project_digest_configs")
        );
        assert!(
            migration
                .sql
                .contains("PRIMARY KEY (config_id, cursor_at, window_end)")
        );
        assert!(migration.sql.contains("delivery_key"));
        assert!(migration.sql.contains("'retryable'"));
        assert!(migration.sql.contains("'ambiguous'"));
        assert!(migration.sql.contains("attempt         BIGINT"));
        assert!(migration.sql.contains("fence           UUID"));
        assert!(migration.sql.contains("acknowledgement JSONB"));
        assert!(
            migration
                .sql
                .contains("project_digest_attempts_delivered_ack_check")
        );
        assert!(
            migration
                .sql
                .contains("guard_project_digest_cursor_regression")
        );
    }

    #[test]
    fn v286_seeds_oauth_distribution_gate_without_overwriting_operator_state() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 286)
            .expect("V286 must be registered");
        assert_eq!(migration.name, "oauth_distribution_gate");
        assert!(migration.sql.contains("'oauth_distribution_enabled'"));
        assert!(migration.sql.contains("'true'"));
        assert!(migration.sql.contains("ON CONFLICT (key) DO NOTHING"));
    }

    #[test]
    fn v287_seeds_ssh_mesh_auto_repair_gate_without_overwriting_operator_state() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 287)
            .expect("V287 must be registered");
        assert_eq!(migration.name, "ssh_mesh_auto_repair_gate");
        assert!(migration.sql.contains("'ssh_mesh_auto_repair_enabled'"));
        assert!(migration.sql.contains("'true'"));
        assert!(migration.sql.contains("ON CONFLICT (key) DO NOTHING"));
    }

    #[test]
    fn v288_registers_ubuntu_2604_detection_idempotently() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 288)
            .expect("V288 must be registered");
        assert_eq!(migration.name, "ubuntu_2604_software_projection");
        assert!(migration.sql.contains("'os-ubuntu-26.04'"));
        assert!(migration.sql.contains("'Ubuntu 26.04 LTS'"));
        assert!(migration.sql.contains("\"method\":\"os_release\""));
        assert!(
            migration
                .sql
                .contains("\"expected_version_prefix\":\"26.04\"")
        );
        assert!(migration.sql.contains("ON CONFLICT (id) DO UPDATE SET"));
        assert!(
            migration
                .sql
                .contains("COALESCE(software_registry.detection")
        );
    }

    #[tokio::test]
    async fn v290_runner_repairs_database_already_recorded_at_original_v289() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("prepare a current schema");
        switch_test_roster_to_legacy_tables(&pool).await;
        reset_enrollment_authority_to_original_v289(&pool).await;

        let prior_version: i32 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _migrations")
                .fetch_one(&pool)
                .await
                .expect("read rewound migration version");
        assert_eq!(prior_version, 289);

        let final_version = run_postgres_migrations(&pool)
            .await
            .expect("V290 runner repair should apply");
        assert_eq!(final_version, 294);
        let migration_name: String =
            sqlx::query_scalar("SELECT name FROM _migrations WHERE version = 290")
                .fetch_one(&pool)
                .await
                .expect("V290 should be durably recorded");
        assert_eq!(migration_name, "secure_enrollment_hardening");
        validate_secure_enrollment_schema(&pool)
            .await
            .expect("V290 repair must produce the exact reviewed authority");

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v290_accepts_already_hardened_v289_shape_idempotently() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("prepare a current schema");
        switch_test_roster_to_legacy_tables(&pool).await;
        sqlx::raw_sql(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS idx_computers_enrollment_canonical_name
                ON computers (lower(name));
            CREATE UNIQUE INDEX IF NOT EXISTS idx_computers_enrollment_primary_ip
                ON computers (primary_ip)
                WHERE NULLIF(primary_ip, '') IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_fleet_workers_enrollment_canonical_name
                ON fleet_workers (lower(name));
            CREATE UNIQUE INDEX IF NOT EXISTS idx_fleet_workers_enrollment_ip
                ON fleet_workers (ip)
                WHERE NULLIF(ip, '') IS NOT NULL;
            "#,
        )
        .execute(&pool)
        .await
        .expect("simulate the already-hardened legacy roster indexes");
        sqlx::query(
            "COMMENT ON TABLE fleet_enrollment_tokens IS \
             'forgefleet secure enrollment authority schema v289; forward-only migrations only'",
        )
        .execute(&pool)
        .await
        .expect("simulate the already-hardened V289 deployment");
        sqlx::query("DELETE FROM _migrations WHERE version >= 290")
            .execute(&pool)
            .await
            .expect("rewind V290 and every later test marker");

        let final_version = run_postgres_migrations(&pool)
            .await
            .expect("V290 should accept the reviewed hardened V289 shape");
        assert_eq!(final_version, 294);
        validate_secure_enrollment_schema(&pool)
            .await
            .expect("idempotent V290 must preserve the exact authority");

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn v290_rejects_familiar_named_but_weakened_index_drift() {
        let Some((admin, pool, db_name)) = create_fresh_temp_db().await else {
            return;
        };

        run_postgres_migrations(&pool)
            .await
            .expect("prepare a current schema");
        sqlx::raw_sql(
            r#"
            DROP INDEX public.idx_fleet_enrollment_tokens_pending_ip;
            CREATE UNIQUE INDEX idx_fleet_enrollment_tokens_pending_ip
                ON fleet_enrollment_tokens (intended_ip)
                WHERE consumed_at IS NULL;
            "#,
        )
        .execute(&pool)
        .await
        .expect("install familiar-named weak test index");

        let error = sqlx::raw_sql(schema::SCHEMA_V290_SECURE_ENROLLMENT_HARDENING)
            .execute(&pool)
            .await
            .expect_err("V290 must reject drift instead of guessing a repair");
        assert!(
            error
                .to_string()
                .contains("neither immutable v289 nor reviewed hardened authority"),
            "unexpected V290 drift error: {error}"
        );
        assert!(
            validate_secure_enrollment_schema(&pool).await.is_err(),
            "the validator must also reject the weakened familiar-named index"
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[test]
    fn v289_remains_the_immutable_original_enrollment_authority() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 289)
            .expect("V289 must be registered");
        assert_eq!(migration.name, "secure_enrollment_tokens");
        for required in [
            "token_hash       BYTEA PRIMARY KEY",
            "octet_length(token_hash) = 32",
            "CREATE TABLE IF NOT EXISTS fleet_enrollment_tokens",
            "WHERE consumed_at IS NULL",
        ] {
            assert!(
                migration.sql.contains(required),
                "immutable V289 lacks original schema contract: {required}"
            );
        }
        for hardened_only in [
            "revoked_at",
            "idx_fleet_enrollment_tokens_pending_name",
            "idx_fleet_enrollment_tokens_pending_ip",
            "fleet_enrollment_tokens_canonical_leader",
            "fleet_enrollment_tokens_revocation",
        ] {
            assert!(
                !migration.sql.contains(hardened_only),
                "immutable V289 was rewritten with V290 artifact: {hardened_only}"
            );
        }
        assert!(!migration.sql.contains("plaintext"));
        assert!(!migration.sql.contains("token_value"));
    }

    #[test]
    fn v290_is_exact_shape_forward_enrollment_repair() {
        let migration = PG_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 290)
            .expect("V290 must be registered");
        assert_eq!(migration.name, "secure_enrollment_hardening");
        for required in [
            "is_old_v289",
            "is_hardened",
            "neither immutable v289 nor reviewed hardened authority",
            "ADD COLUMN revoked_at",
            "fleet_enrollment_tokens_canonical_leader",
            "fleet_enrollment_tokens_revocation",
            "DROP INDEX public.idx_fleet_enrollment_tokens_expiry",
            "idx_fleet_enrollment_tokens_pending_name",
            "idx_fleet_enrollment_tokens_pending_ip",
            "schema v290; forward-only migrations only",
        ] {
            assert!(
                migration.sql.contains(required),
                "V290 lacks controlled repair contract: {required}"
            );
        }
        assert!(!migration.sql.contains("token_value"));
    }
}
