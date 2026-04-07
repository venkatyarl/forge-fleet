//! Table definitions for ForgeFleet embedded SQLite.
//!
//! All DDL lives here as constants. The migration runner applies these
//! in order. Keep table names in sync with `queries.rs`.

/// SQL to create all ForgeFleet tables.
/// Applied as migration version 1 — the initial schema.
pub const SCHEMA_V1: &str = r#"
-- ─── Nodes ─────────────────────────────────────────────────────────────────
-- Every fleet node (taylor, james, marcus, etc.)
CREATE TABLE IF NOT EXISTS nodes (
    id              TEXT PRIMARY KEY,               -- UUID
    name            TEXT NOT NULL UNIQUE,            -- human name ("taylor")
    host            TEXT NOT NULL,                   -- IP or hostname
    port            INTEGER NOT NULL DEFAULT 51800,
    role            TEXT NOT NULL DEFAULT 'worker',  -- leader | worker
    election_priority INTEGER NOT NULL DEFAULT 99,
    status          TEXT NOT NULL DEFAULT 'online',  -- online | degraded | offline | starting | maintenance
    hardware_json   TEXT NOT NULL DEFAULT '{}',      -- serialized Hardware struct
    models_json     TEXT NOT NULL DEFAULT '[]',      -- list of model IDs loaded
    last_heartbeat  TEXT,                            -- ISO 8601
    registered_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- ─── Models ────────────────────────────────────────────────────────────────
-- LLM models available in the fleet.
CREATE TABLE IF NOT EXISTS models (
    id              TEXT PRIMARY KEY,               -- e.g. "qwen3-32b-q4"
    name            TEXT NOT NULL,                   -- human-readable
    tier            INTEGER NOT NULL,                -- 1–4
    params_b        REAL NOT NULL,                   -- parameter count (billions)
    quant           TEXT NOT NULL DEFAULT 'Q4_K_M',
    path            TEXT NOT NULL DEFAULT '',         -- GGUF path on node
    ctx_size        INTEGER NOT NULL DEFAULT 8192,
    runtime         TEXT NOT NULL DEFAULT 'llama_cpp',
    nodes_json      TEXT NOT NULL DEFAULT '[]',      -- node names that serve this model
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- ─── Tasks ─────────────────────────────────────────────────────────────────
-- Agent tasks dispatched by the leader.
CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,               -- UUID
    kind            TEXT NOT NULL,                   -- "shell_command" | "model_inference"
    payload_json    TEXT NOT NULL DEFAULT '{}',      -- serialized task kind fields
    status          TEXT NOT NULL DEFAULT 'pending', -- pending | running | completed | failed | cancelled
    assigned_node   TEXT,                            -- node name
    priority        INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at      TEXT,
    completed_at    TEXT,
    FOREIGN KEY (assigned_node) REFERENCES nodes(name)
);

-- ─── Task Results ──────────────────────────────────────────────────────────
-- Output from completed tasks.
CREATE TABLE IF NOT EXISTS task_results (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT NOT NULL UNIQUE,
    success         INTEGER NOT NULL DEFAULT 0,      -- boolean
    output          TEXT NOT NULL DEFAULT '',
    duration_ms     INTEGER NOT NULL DEFAULT 0,
    completed_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (task_id) REFERENCES tasks(id)
);

-- ─── Memories ──────────────────────────────────────────────────────────────
-- Persistent memory entries (Mem0 / Claude-Mem style).
CREATE TABLE IF NOT EXISTS memories (
    id              TEXT PRIMARY KEY,               -- UUID
    namespace       TEXT NOT NULL DEFAULT 'default', -- grouping: "user", "project", "system"
    key             TEXT NOT NULL,                    -- lookup key
    content         TEXT NOT NULL,                    -- the memory content
    embedding_json  TEXT,                             -- optional vector embedding as JSON array
    metadata_json   TEXT NOT NULL DEFAULT '{}',
    importance      REAL NOT NULL DEFAULT 0.5,        -- 0.0–1.0 relevance score
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at      TEXT,                             -- optional TTL
    UNIQUE(namespace, key)
);

CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
CREATE INDEX IF NOT EXISTS idx_memories_importance ON memories(importance DESC);

-- ─── Sessions ──────────────────────────────────────────────────────────────
-- Chat / agent sessions (from ff-sessions).
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,               -- UUID
    channel         TEXT NOT NULL DEFAULT 'unknown', -- telegram | discord | slack | web
    user_id         TEXT,                             -- external user ID
    node_name       TEXT,                             -- which node handles this session
    status          TEXT NOT NULL DEFAULT 'active',   -- active | closed | expired
    metadata_json   TEXT NOT NULL DEFAULT '{}',
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_activity   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at       TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_channel ON sessions(channel);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status);

-- ─── Cron Jobs ─────────────────────────────────────────────────────────────
-- Scheduled recurring tasks.
CREATE TABLE IF NOT EXISTS cron_jobs (
    id              TEXT PRIMARY KEY,               -- UUID
    name            TEXT NOT NULL UNIQUE,
    schedule        TEXT NOT NULL,                    -- cron expression e.g. "0 */6 * * *"
    task_kind       TEXT NOT NULL,                    -- what to run
    payload_json    TEXT NOT NULL DEFAULT '{}',
    enabled         INTEGER NOT NULL DEFAULT 1,       -- boolean
    node_affinity   TEXT,                             -- preferred node, NULL = any
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- ─── Cron Runs ─────────────────────────────────────────────────────────────
-- Execution history for cron jobs.
CREATE TABLE IF NOT EXISTS cron_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_id     TEXT NOT NULL,
    started_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    completed_at    TEXT,
    success         INTEGER,                          -- boolean, NULL while running
    output          TEXT NOT NULL DEFAULT '',
    duration_ms     INTEGER,
    FOREIGN KEY (cron_job_id) REFERENCES cron_jobs(id)
);

CREATE INDEX IF NOT EXISTS idx_cron_runs_job ON cron_runs(cron_job_id);

-- ─── Audit Log ─────────────────────────────────────────────────────────────
-- Immutable log of significant events.
CREATE TABLE IF NOT EXISTS audit_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    event_type      TEXT NOT NULL,                    -- "leader_elected" | "node_joined" | "task_completed" | etc.
    actor           TEXT NOT NULL DEFAULT 'system',    -- who triggered it
    target          TEXT,                              -- what was affected
    details_json    TEXT NOT NULL DEFAULT '{}',
    node_name       TEXT                               -- where it happened
);

CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp);
CREATE INDEX IF NOT EXISTS idx_audit_event_type ON audit_log(event_type);

-- ─── Config KV ─────────────────────────────────────────────────────────────
-- Key-value configuration store.
CREATE TABLE IF NOT EXISTS config_kv (
    key             TEXT PRIMARY KEY,
    value           TEXT NOT NULL,
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
"#;

/// Table names for programmatic access.
/// SQL to add task ownership/lease tracking tables.
///
/// Applied as migration version 2.
pub const SCHEMA_V2_TASK_OWNERSHIP: &str = r#"
-- ─── Task Ownership / Leases ─────────────────────────────────────────────
-- Single-writer ownership with lease expiry and handoff support.
CREATE TABLE IF NOT EXISTS task_ownership (
    task_id          TEXT PRIMARY KEY,
    owner_node       TEXT NOT NULL,
    lease_expires_at TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'claimed', -- claimed | handoff_requested | released
    handoff_target   TEXT,
    updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_task_ownership_owner ON task_ownership(owner_node);
CREATE INDEX IF NOT EXISTS idx_task_ownership_status ON task_ownership(status);
CREATE INDEX IF NOT EXISTS idx_task_ownership_lease ON task_ownership(lease_expires_at);

-- Ownership event history for handoff/release auditing.
CREATE TABLE IF NOT EXISTS ownership_events (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id          TEXT NOT NULL,
    event_type       TEXT NOT NULL,
    from_owner       TEXT,
    to_owner         TEXT,
    reason           TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_ownership_events_task ON ownership_events(task_id, id);
"#;

/// SQL to add autonomy-policy decision/event persistence.
///
/// Applied as migration version 3.
pub const SCHEMA_V3_AUTONOMY_EVENTS: &str = r#"
-- ─── Autonomy Events ─────────────────────────────────────────────────────
-- Policy decisions emitted before autonomous execution.
CREATE TABLE IF NOT EXISTS autonomy_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type      TEXT NOT NULL,
    action_type     TEXT NOT NULL,
    decision        TEXT NOT NULL,
    reason          TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_autonomy_events_created_at ON autonomy_events(created_at);
CREATE INDEX IF NOT EXISTS idx_autonomy_events_event_type ON autonomy_events(event_type);
"#;

/// SQL to add Telegram media ingest metadata persistence.
///
/// Applied as migration version 4.
pub const SCHEMA_V4_TELEGRAM_MEDIA_INGEST: &str = r#"
CREATE TABLE IF NOT EXISTS telegram_media_ingest (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id         TEXT NOT NULL,
    message_id      TEXT NOT NULL,
    media_kind      TEXT NOT NULL,
    local_path      TEXT NOT NULL,
    mime_type       TEXT,
    size_bytes      INTEGER,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_tg_media_ingest_chat_created
    ON telegram_media_ingest(chat_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tg_media_ingest_message
    ON telegram_media_ingest(message_id);
"#;

/// SQL to add live fleet runtime node registry persistence.
///
/// Applied as migration version 5.
pub const SCHEMA_V5_FLEET_NODE_RUNTIME: &str = r#"
-- ─── Fleet Runtime Node Registry ──────────────────────────────────────────
-- Live source of truth for node runtime state (heartbeats + capabilities).
CREATE TABLE IF NOT EXISTS fleet_node_runtime (
    node_id                      TEXT PRIMARY KEY,
    hostname                     TEXT NOT NULL,
    ips_json                     TEXT NOT NULL DEFAULT '[]',
    role                         TEXT NOT NULL DEFAULT 'worker',
    reported_status              TEXT NOT NULL DEFAULT 'online',
    last_heartbeat               TEXT NOT NULL,
    resources_json               TEXT NOT NULL DEFAULT '{}',
    services_json                TEXT NOT NULL DEFAULT '[]',
    models_json                  TEXT NOT NULL DEFAULT '[]',
    capabilities_json            TEXT NOT NULL DEFAULT '{}',
    stale_degraded_after_secs    INTEGER NOT NULL DEFAULT 90,
    stale_offline_after_secs     INTEGER NOT NULL DEFAULT 180,
    updated_at                   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_fleet_runtime_hostname
    ON fleet_node_runtime(hostname);
CREATE INDEX IF NOT EXISTS idx_fleet_runtime_heartbeat
    ON fleet_node_runtime(last_heartbeat);
"#;

/// SQL to add explicit fleet enrollment event history.
///
/// Applied as migration version 6.
pub const SCHEMA_V6_FLEET_ENROLLMENT_EVENTS: &str = r#"
CREATE TABLE IF NOT EXISTS fleet_enrollment_events (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id           TEXT,
    hostname          TEXT,
    outcome           TEXT NOT NULL,                    -- accepted | rejected
    reason            TEXT,
    role              TEXT,
    service_version   TEXT,
    addresses_json    TEXT NOT NULL DEFAULT '[]',
    capabilities_json TEXT NOT NULL DEFAULT '{}',
    metadata_json     TEXT NOT NULL DEFAULT '{}',
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_fleet_enrollment_events_created
    ON fleet_enrollment_events(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_fleet_enrollment_events_node
    ON fleet_enrollment_events(node_id, created_at DESC);
"#;

pub const TABLES: &[&str] = &[
    "nodes",
    "models",
    "tasks",
    "task_results",
    "task_ownership",
    "ownership_events",
    "autonomy_events",
    "telegram_media_ingest",
    "fleet_node_runtime",
    "fleet_enrollment_events",
    "memories",
    "sessions",
    "cron_jobs",
    "cron_runs",
    "audit_log",
    "config_kv",
];
