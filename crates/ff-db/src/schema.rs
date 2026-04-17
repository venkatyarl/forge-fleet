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
    port            INTEGER NOT NULL DEFAULT 55000,
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

/// Postgres-only schema: fleet config tables (nodes, models, settings).
///
/// Applied as Postgres migration version 7.
/// These tables replace fleet.toml as single source of truth for fleet config.
pub const SCHEMA_V7_FLEET_POSTGRES: &str = r#"
-- ─── Fleet Nodes ──────────────────────────────────────────────────────────
-- Replaces [nodes.*] sections in fleet.toml.
CREATE TABLE IF NOT EXISTS fleet_nodes (
    name            TEXT PRIMARY KEY,
    ip              TEXT NOT NULL,
    ssh_user        TEXT NOT NULL DEFAULT 'root',
    ram_gb          INTEGER NOT NULL DEFAULT 0,
    cpu_cores       INTEGER NOT NULL DEFAULT 0,
    os              TEXT NOT NULL DEFAULT '',
    role            TEXT NOT NULL DEFAULT 'worker',
    election_priority INTEGER NOT NULL DEFAULT 50,
    hardware        TEXT NOT NULL DEFAULT '',
    alt_ips         JSONB NOT NULL DEFAULT '[]',
    capabilities    JSONB NOT NULL DEFAULT '{}',
    preferences     JSONB NOT NULL DEFAULT '{}',
    resources       JSONB NOT NULL DEFAULT '{}',
    status          TEXT NOT NULL DEFAULT 'online',
    registered_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─── Fleet Models ─────────────────────────────────────────────────────────
-- Replaces [nodes.*.models.*] sections in fleet.toml.
CREATE TABLE IF NOT EXISTS fleet_models (
    id              TEXT PRIMARY KEY,
    node_name       TEXT NOT NULL REFERENCES fleet_nodes(name),
    slug            TEXT NOT NULL,
    name            TEXT NOT NULL,
    family          TEXT NOT NULL DEFAULT '',
    port            INTEGER NOT NULL,
    tier            INTEGER NOT NULL DEFAULT 2,
    local_model     BOOLEAN NOT NULL DEFAULT true,
    lifecycle       TEXT NOT NULL DEFAULT 'production',
    mode            TEXT NOT NULL DEFAULT 'always_on',
    preferred_workloads JSONB NOT NULL DEFAULT '[]',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(node_name, slug)
);

CREATE INDEX IF NOT EXISTS idx_fleet_models_node ON fleet_models(node_name);

-- ─── Fleet Settings ───────────────────────────────────────────────────────
-- Replaces [general], [scheduling], [ports], [llm], [enrollment], etc.
CREATE TABLE IF NOT EXISTS fleet_settings (
    key             TEXT PRIMARY KEY,
    value           JSONB NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#;

/// Postgres-only schema: task provenance columns + routing log table.
///
/// Applied as Postgres migration version 8.
/// IF NOT EXISTS / IF NOT EXISTS guards make this idempotent.
pub const SCHEMA_V13_VIRTUAL_BRAIN: &str = r#"
-- ─── V13: Virtual Brain — unified knowledge graph + channel-agnostic chat ──
-- See plan: gentle-questing-valley.md
-- NOTE: pgvector (CREATE EXTENSION vector) is deferred to V14 since it
-- requires server-side installation. V13 runs on any Postgres 14+.

-- ─── Users + channel identity mapping ──────────────────────────────────────

CREATE TABLE IF NOT EXISTS brain_users (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name         TEXT NOT NULL UNIQUE,
    display_name TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS brain_channel_identities (
    channel     TEXT NOT NULL,
    external_id TEXT NOT NULL,
    user_id     UUID NOT NULL REFERENCES brain_users(id),
    PRIMARY KEY (channel, external_id)
);

-- ─── Threads (many per user, portable across devices) ──────────────────────

CREATE TABLE IF NOT EXISTS brain_threads (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES brain_users(id),
    slug            TEXT NOT NULL,
    title           TEXT,
    icon            TEXT,
    project         TEXT,
    status          TEXT NOT NULL DEFAULT 'active',
    last_message_at TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, slug)
);
CREATE INDEX IF NOT EXISTS idx_brain_threads_user
    ON brain_threads(user_id, last_message_at DESC) WHERE status = 'active';

-- Which thread each device/channel is currently pointing at.
CREATE TABLE IF NOT EXISTS brain_thread_attachments (
    channel     TEXT NOT NULL,
    external_id TEXT NOT NULL,
    user_id     UUID NOT NULL REFERENCES brain_users(id),
    thread_id   UUID NOT NULL REFERENCES brain_threads(id),
    attached_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (channel, external_id)
);
CREATE INDEX IF NOT EXISTS idx_brain_attachments_thread
    ON brain_thread_attachments(thread_id);

-- ─── Messages ──────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS brain_messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    thread_id   UUID NOT NULL REFERENCES brain_threads(id) ON DELETE CASCADE,
    user_id     UUID NOT NULL REFERENCES brain_users(id),
    channel     TEXT NOT NULL,
    external_id TEXT NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    metadata    JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_brain_messages_thread
    ON brain_messages(thread_id, created_at);

-- ─── Stack + Backlog archives (live state is in Redis) ─────────────────────

CREATE TABLE IF NOT EXISTS brain_stack_archive (
    id           UUID PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES brain_users(id),
    thread_id    UUID REFERENCES brain_threads(id),
    title        TEXT NOT NULL,
    context      TEXT,
    push_reason  TEXT,
    pushed_at    TIMESTAMPTZ NOT NULL,
    popped_at    TIMESTAMPTZ,
    archived_from_thread BOOLEAN NOT NULL DEFAULT false,
    archived_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_stack_archive_user_thread
    ON brain_stack_archive(user_id, thread_id, pushed_at DESC);

CREATE TABLE IF NOT EXISTS brain_backlog_archive (
    id                   UUID PRIMARY KEY,
    user_id              UUID NOT NULL REFERENCES brain_users(id),
    project              TEXT NOT NULL,
    title                TEXT NOT NULL,
    priority             TEXT NOT NULL,
    from_thread_id       UUID REFERENCES brain_threads(id),
    completed_at         TIMESTAMPTZ NOT NULL,
    completed_by_channel TEXT,
    tags                 TEXT[] NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_backlog_archive_project
    ON brain_backlog_archive(user_id, project, completed_at DESC);

-- ─── Vault knowledge graph ─────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS brain_vault_nodes (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    path            TEXT UNIQUE NOT NULL,
    title           TEXT NOT NULL,
    node_type       TEXT,
    project         TEXT,
    tags            TEXT[] NOT NULL DEFAULT '{}',
    extends_path    TEXT,
    applies_to      TEXT[] NOT NULL DEFAULT '{}',
    from_thread     TEXT,
    confidence      REAL,
    content_hash    TEXT NOT NULL,
    valid_from      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_until     TIMESTAMPTZ,
    superseded_by   UUID REFERENCES brain_vault_nodes(id),
    hits            INT NOT NULL DEFAULT 0,
    references_     INT NOT NULL DEFAULT 0,
    last_accessed   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    community_id    INT,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_vault_nodes_project_current
    ON brain_vault_nodes(project) WHERE valid_until IS NULL;
CREATE INDEX IF NOT EXISTS idx_vault_nodes_tags
    ON brain_vault_nodes USING GIN(tags);
CREATE INDEX IF NOT EXISTS idx_vault_nodes_superseded
    ON brain_vault_nodes(superseded_by) WHERE superseded_by IS NOT NULL;

CREATE TABLE IF NOT EXISTS brain_vault_edges (
    src_id     UUID NOT NULL REFERENCES brain_vault_nodes(id) ON DELETE CASCADE,
    dst_id     UUID NOT NULL REFERENCES brain_vault_nodes(id) ON DELETE CASCADE,
    edge_type  TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    provenance TEXT NOT NULL DEFAULT 'extracted',
    PRIMARY KEY (src_id, dst_id, edge_type)
);
CREATE INDEX IF NOT EXISTS idx_vault_edges_src ON brain_vault_edges(src_id, edge_type);
CREATE INDEX IF NOT EXISTS idx_vault_edges_dst ON brain_vault_edges(dst_id, edge_type);

-- Chunk embeddings table is deferred to V14 (requires pgvector extension).
-- For now, embeddings are stored as JSON arrays in rag_chunks.metadata if needed.

-- ─── Communities (Leiden clustering) ───────────────────────────────────────

CREATE TABLE IF NOT EXISTS brain_communities (
    id            SERIAL PRIMARY KEY,
    label         TEXT,
    god_node_id   UUID REFERENCES brain_vault_nodes(id),
    member_count  INT NOT NULL DEFAULT 0,
    color         TEXT,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─── Knowledge candidates (extractor output, pending approval) ─────────────

CREATE TABLE IF NOT EXISTS brain_knowledge_candidates (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL REFERENCES brain_users(id),
    thread_id     UUID REFERENCES brain_threads(id),
    action        TEXT NOT NULL,
    kind          TEXT,
    title         TEXT,
    body          TEXT,
    tags          TEXT[] NOT NULL DEFAULT '{}',
    project       TEXT,
    extends_path  TEXT,
    applies_to    TEXT[] NOT NULL DEFAULT '{}',
    target_path   TEXT,
    from_thread   TEXT,
    confidence    REAL,
    status        TEXT NOT NULL DEFAULT 'pending',
    reviewed_at   TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_brain_candidates_pending
    ON brain_knowledge_candidates(user_id, status, created_at)
    WHERE status = 'pending';

-- ─── Reminders ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS brain_reminders (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES brain_users(id),
    thread_id       UUID REFERENCES brain_threads(id),
    content         TEXT NOT NULL,
    remind_at       TIMESTAMPTZ NOT NULL,
    channel_pref    TEXT,
    status          TEXT NOT NULL DEFAULT 'pending',
    snoozed_until   TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fired_at        TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_reminders_pending
    ON brain_reminders(remind_at) WHERE status = 'pending';
"#;

pub const SCHEMA_V12_ONBOARDING: &str = r#"
-- ─── V12: Self-service onboarding foundation ──────────────────────────────
-- New tables for SSH key tracking + mesh verification, plus ALTER TABLE on
-- fleet_nodes for sub-agent fan-out, GitHub identity, and installed-tool
-- version tracking. See plan: gentle-questing-valley.md §3–§3h for design.

-- SSH public keys per node. Separate from fleet_nodes so we can stash both
-- the daemon user's pubkey AND the machine's host keys (multiple per node).
CREATE TABLE IF NOT EXISTS fleet_node_ssh_keys (
    node_name    TEXT NOT NULL REFERENCES fleet_nodes(name) ON DELETE CASCADE,
    key_purpose  TEXT NOT NULL,             -- 'user' | 'host'
    public_key   TEXT NOT NULL,             -- full OpenSSH format line
    key_type     TEXT NOT NULL,             -- 'ed25519' | 'rsa' | 'ecdsa'
    fingerprint  TEXT NOT NULL,             -- sha256:... from ssh-keygen -lf
    added_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (node_name, fingerprint)
);
CREATE INDEX IF NOT EXISTS idx_ssh_keys_node_purpose
    ON fleet_node_ssh_keys (node_name, key_purpose);

-- Bidirectional SSH reachability matrix. One row per ordered (src, dst) pair.
-- Written by the mesh-propagation deferred task and the periodic re-verify
-- tick; read by the dashboard and `ff fleet ssh-mesh-check`.
CREATE TABLE IF NOT EXISTS fleet_mesh_status (
    src_node     TEXT NOT NULL,
    dst_node     TEXT NOT NULL,
    status       TEXT NOT NULL,             -- 'ok' | 'failed' | 'pending'
    last_checked TIMESTAMPTZ,
    last_error   TEXT,
    attempts     INT NOT NULL DEFAULT 0,
    PRIMARY KEY (src_node, dst_node)
);
CREATE INDEX IF NOT EXISTS idx_mesh_status_dst ON fleet_mesh_status (dst_node);
CREATE INDEX IF NOT EXISTS idx_mesh_status_status ON fleet_mesh_status (status);

-- Extend fleet_nodes for onboarding features:
--   sub_agent_count — how many concurrent worker slots this node serves
--   gh_account       — which GitHub identity this node is authenticated against
--   tooling          — JSONB map of {tool: {current, latest, checked_at}}
ALTER TABLE fleet_nodes
    ADD COLUMN IF NOT EXISTS sub_agent_count INT  NOT NULL DEFAULT 1;
ALTER TABLE fleet_nodes
    ADD COLUMN IF NOT EXISTS gh_account      TEXT;
ALTER TABLE fleet_nodes
    ADD COLUMN IF NOT EXISTS tooling         JSONB NOT NULL DEFAULT '{}';
"#;

pub const SCHEMA_V11_MODEL_LIFECYCLE: &str = r#"
-- ─── Model Lifecycle (catalog / library / deployments / jobs) ─────────────
-- Splits the old `fleet_models` concept into:
--   catalog      = what we *can* download (curated + dynamic)
--   library      = what's on disk per node (inventory)
--   deployments  = what's running per node right now (processes)
--   jobs         = in-flight downloads/deletions/swaps (progress tracking)

-- Add a runtime column to fleet_nodes if it doesn't already exist.
-- Values: "llama.cpp" | "mlx" | "vllm" | "unknown"
ALTER TABLE fleet_nodes ADD COLUMN IF NOT EXISTS runtime TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE fleet_nodes ADD COLUMN IF NOT EXISTS models_dir TEXT NOT NULL DEFAULT '~/models';
ALTER TABLE fleet_nodes ADD COLUMN IF NOT EXISTS disk_quota_pct INT NOT NULL DEFAULT 80;

-- Catalog: global list of models available for download.
-- Populated from config/model_catalog.toml on migration and refreshable via `ff model sync-catalog`.
CREATE TABLE IF NOT EXISTS fleet_model_catalog (
    id                  TEXT PRIMARY KEY,            -- slug, e.g. "qwen3-coder-30b"
    name                TEXT NOT NULL,               -- display name
    family              TEXT NOT NULL,               -- qwen / gemma / llama / etc
    parameters          TEXT NOT NULL,               -- "30B"
    tier                INT NOT NULL,                -- 1..4
    description         TEXT,
    gated               BOOLEAN NOT NULL DEFAULT FALSE,
    preferred_workloads JSONB NOT NULL DEFAULT '[]', -- ["code", "chat", "reasoning"]
    variants            JSONB NOT NULL DEFAULT '[]', -- [{runtime, quant, hf_repo, size_gb}, ...]
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Library: what's on disk per node (one row per {node, catalog_id, variant}).
CREATE TABLE IF NOT EXISTS fleet_model_library (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_name       TEXT NOT NULL REFERENCES fleet_nodes(name) ON DELETE CASCADE,
    catalog_id      TEXT NOT NULL,                           -- may reference fleet_model_catalog.id
    runtime         TEXT NOT NULL,                           -- 'llama.cpp' | 'mlx' | 'vllm'
    quant           TEXT,                                    -- e.g. 'Q4_K_M' or '4bit'
    file_path       TEXT NOT NULL,                           -- absolute path on node
    size_bytes      BIGINT NOT NULL DEFAULT 0,
    sha256          TEXT,                                    -- nullable; verified on demand
    downloaded_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at    TIMESTAMPTZ,
    source_url      TEXT,                                    -- e.g. hf://repo or local path
    UNIQUE (node_name, file_path)
);

CREATE INDEX IF NOT EXISTS idx_model_library_node ON fleet_model_library (node_name);
CREATE INDEX IF NOT EXISTS idx_model_library_catalog ON fleet_model_library (catalog_id);

-- Deployments: currently running llama-server / mlx_lm.server / vllm processes.
CREATE TABLE IF NOT EXISTS fleet_model_deployments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_name       TEXT NOT NULL REFERENCES fleet_nodes(name) ON DELETE CASCADE,
    library_id      UUID REFERENCES fleet_model_library(id) ON DELETE SET NULL,
    catalog_id      TEXT,                                    -- redundant but useful for offline queries
    runtime         TEXT NOT NULL,
    port            INT NOT NULL,
    pid             INT,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_health_at  TIMESTAMPTZ,
    health_status   TEXT NOT NULL DEFAULT 'starting',        -- starting | healthy | unhealthy | stopped
    context_window  INT,
    tokens_used     BIGINT NOT NULL DEFAULT 0,
    request_count   BIGINT NOT NULL DEFAULT 0,
    UNIQUE (node_name, port)
);

CREATE INDEX IF NOT EXISTS idx_model_deployments_node ON fleet_model_deployments (node_name);
CREATE INDEX IF NOT EXISTS idx_model_deployments_health ON fleet_model_deployments (health_status);

-- Jobs: in-flight operations with progress tracking.
-- Kinds: 'download' | 'delete' | 'load' | 'unload' | 'swap' | 'convert' | 'transfer' | 'verify'
CREATE TABLE IF NOT EXISTS fleet_model_jobs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_name       TEXT NOT NULL,
    kind            TEXT NOT NULL,
    target_catalog_id TEXT,                                  -- for download/load/swap
    target_library_id UUID,                                  -- for delete/load/unload/convert
    params          JSONB NOT NULL DEFAULT '{}',             -- kind-specific options
    status          TEXT NOT NULL DEFAULT 'queued',          -- queued | running | completed | failed | cancelled
    progress_pct    REAL NOT NULL DEFAULT 0,                 -- 0..100
    bytes_done      BIGINT,
    bytes_total     BIGINT,
    eta_seconds     INT,
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error_message   TEXT
);

CREATE INDEX IF NOT EXISTS idx_model_jobs_node_status ON fleet_model_jobs (node_name, status);
CREATE INDEX IF NOT EXISTS idx_model_jobs_created ON fleet_model_jobs (created_at DESC);

-- Disk usage snapshots: periodic sampling of disk free/used for quota monitoring.
CREATE TABLE IF NOT EXISTS fleet_disk_usage (
    node_name       TEXT NOT NULL REFERENCES fleet_nodes(name) ON DELETE CASCADE,
    sampled_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    models_dir      TEXT NOT NULL,
    total_bytes     BIGINT NOT NULL,
    used_bytes      BIGINT NOT NULL,
    free_bytes      BIGINT NOT NULL,
    models_bytes    BIGINT NOT NULL DEFAULT 0,               -- just the models dir
    PRIMARY KEY (node_name, sampled_at)
);
CREATE INDEX IF NOT EXISTS idx_disk_usage_latest ON fleet_disk_usage (node_name, sampled_at DESC);
"#;

pub const SCHEMA_V10_DEFERRED_TASKS: &str = r#"
-- ─── Deferred Task Queue ──────────────────────────────────────────────────
-- Persistent queue for work that can't run right now (offline node, future time,
-- event trigger). Leader schedules, any daemon can worker-claim via SKIP LOCKED.
CREATE TABLE IF NOT EXISTS deferred_tasks (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by       TEXT,                          -- user@host or service tag
    title            TEXT NOT NULL,                 -- one-line human summary
    -- What to run
    kind             TEXT NOT NULL,                 -- 'shell' | 'http' | 'agent_run'
    payload          JSONB NOT NULL,                -- shape depends on kind
    -- When to run
    trigger_type     TEXT NOT NULL,                 -- 'node_online' | 'at_time' | 'manual' | 'now'
    trigger_spec     JSONB NOT NULL DEFAULT '{}',   -- e.g. {"node": "ace"} or {"at": "..."}
    -- Execution routing
    preferred_node   TEXT,                          -- null = any node may claim
    required_caps    JSONB NOT NULL DEFAULT '[]',   -- e.g. ["llm", "qwen-coder"]
    -- Status machine
    status           TEXT NOT NULL DEFAULT 'pending',  -- pending | dispatchable | running | completed | failed | cancelled
    attempts         INT NOT NULL DEFAULT 0,
    max_attempts     INT NOT NULL DEFAULT 5,
    next_attempt_at  TIMESTAMPTZ,                   -- null until scheduler decides
    claimed_by       TEXT,                          -- node name that is running it
    claimed_at       TIMESTAMPTZ,
    last_error       TEXT,
    result           JSONB,
    completed_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_deferred_tasks_status_next
    ON deferred_tasks (status, next_attempt_at);
CREATE INDEX IF NOT EXISTS idx_deferred_tasks_preferred_node
    ON deferred_tasks (preferred_node) WHERE status IN ('pending', 'dispatchable');
CREATE INDEX IF NOT EXISTS idx_deferred_tasks_trigger
    ON deferred_tasks (trigger_type) WHERE status = 'pending';
"#;

pub const SCHEMA_V9_FLEET_SECRETS: &str = r#"
-- ─── Fleet Secrets ────────────────────────────────────────────────────────
-- Shared secrets (API tokens, etc.) readable by every fleet node.
-- Plaintext at rest — acceptable for a trusted internal fleet.
-- Future: encrypt with a fleet master key from macOS Keychain / Linux keyring.
CREATE TABLE IF NOT EXISTS fleet_secrets (
    key         TEXT PRIMARY KEY,           -- e.g. "huggingface.token"
    value       TEXT NOT NULL,              -- raw secret value
    description TEXT,                       -- human-readable purpose
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by  TEXT                        -- node or user that set it
);
"#;

pub const SCHEMA_V8_TASK_PROVENANCE: &str = r#"
-- ALTER TABLE tasks: add provenance columns (IF NOT EXISTS guards for idempotency)
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS origin_node TEXT;      -- which node created this task
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS parent_task_id TEXT;   -- spawning task ID (for sub-tasks)
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS reply_to_node TEXT;    -- where to POST result callback

-- task_routing_log: full breadcrumb of every node hop
CREATE TABLE IF NOT EXISTS task_routing_log (
    id          BIGSERIAL PRIMARY KEY,
    task_id     TEXT NOT NULL,
    from_node   TEXT NOT NULL,
    to_node     TEXT NOT NULL,
    reason      TEXT NOT NULL DEFAULT '',
    routed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_routing_log_task ON task_routing_log(task_id, id);
CREATE INDEX IF NOT EXISTS idx_routing_log_from ON task_routing_log(from_node);
CREATE INDEX IF NOT EXISTS idx_routing_log_to ON task_routing_log(to_node);
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
