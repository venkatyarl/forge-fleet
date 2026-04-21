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

pub const SCHEMA_V14_COMPUTERS_AND_PORTFOLIO: &str = r#"
-- ─── V14: Computers as first-class + software registry + model portfolio ──
-- Adds the new data model layer described in
-- /Users/venkat/.claude/plans/we-are-mixing-two-streamed-sky.md
--
-- These tables coexist with the existing fleet_nodes / fleet_models tables.
-- Later phases migrate callers over, then drop the old tables.

-- ─── Physical computer identity ─────────────────────────────────────────
CREATE TABLE IF NOT EXISTS computers (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                    TEXT NOT NULL UNIQUE,
    primary_ip              TEXT NOT NULL,
    all_ips                 JSONB NOT NULL DEFAULT '[]',
    hostname                TEXT,
    mac_addresses           JSONB NOT NULL DEFAULT '[]',
    os_family               TEXT NOT NULL,                  -- macos|linux-ubuntu|linux-dgx|windows
    os_distribution         TEXT,                            -- ubuntu-24.04, macos-26.4, etc.
    os_version              TEXT,                            -- installed version
    os_version_latest       TEXT,                            -- denormalized from software_registry
    os_upgrade_available    BOOLEAN NOT NULL DEFAULT false,
    os_version_checked_at   TIMESTAMPTZ,
    cpu_cores               INT,
    total_ram_gb            INT,
    total_disk_gb           INT,
    has_gpu                 BOOLEAN NOT NULL DEFAULT false,
    gpu_kind                TEXT,                            -- none|integrated|apple_silicon|nvidia_cuda|amd_rocm
    gpu_count               INT NOT NULL DEFAULT 0,
    gpu_model               TEXT,
    gpu_vram_gb             FLOAT,                            -- per-GPU VRAM, NULL for unified
    gpu_total_vram_gb       FLOAT,
    cuda_version            TEXT,
    metal_version           TEXT,
    rocm_version            TEXT,
    gpu_driver_version      TEXT,
    ssh_user                TEXT NOT NULL,
    ssh_port                INT NOT NULL DEFAULT 22,
    ssh_public_key          TEXT,                            -- this computer's ed25519 pub key
    enrolled_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at            TIMESTAMPTZ,
    offline_since           TIMESTAMPTZ,                     -- set when status → sdown/odown
    status_changed_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status                  TEXT NOT NULL DEFAULT 'pending', -- pending|online|sdown|odown|offline|maintenance
    metadata                JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_computers_last_seen ON computers(last_seen_at);
CREATE INDEX IF NOT EXISTS idx_computers_status ON computers(status);

-- Downtime history (append-only log)
CREATE TABLE IF NOT EXISTS computer_downtime_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    computer_id     UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    offline_at      TIMESTAMPTZ NOT NULL,
    online_at       TIMESTAMPTZ,
    duration_sec    INT,
    cause           TEXT,       -- odown | graceful_shutdown | revive_initiated
    resolved_by     TEXT        -- pulse_return | revive_success | manual
);
CREATE INDEX IF NOT EXISTS idx_downtime_by_computer
    ON computer_downtime_events(computer_id, offline_at DESC);

-- SSH mesh trust (replaces fleet_mesh_status logically; both coexist for now)
CREATE TABLE IF NOT EXISTS computer_trust (
    source_computer_id   UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    target_computer_id   UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    target_host_key      TEXT NOT NULL,
    verified_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_probe_at        TIMESTAMPTZ,
    last_probe_status    TEXT,
    PRIMARY KEY (source_computer_id, target_computer_id)
);

-- ─── ForgeFleet install record ──────────────────────────────────────────
CREATE TABLE IF NOT EXISTS fleet_members (
    computer_id         UUID PRIMARY KEY REFERENCES computers(id) ON DELETE CASCADE,
    role                TEXT NOT NULL DEFAULT 'member',   -- leader|member (elected)
    election_priority   INT NOT NULL DEFAULT 50,
    gh_account          TEXT,
    runtime             TEXT NOT NULL,                    -- mlx|llamacpp|vllm
    models_dir          TEXT,
    disk_quota_pct      INT NOT NULL DEFAULT 80,
    enrolled_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata            JSONB NOT NULL DEFAULT '{}'
);

-- Elected leader singleton (one row ever)
CREATE TABLE IF NOT EXISTS fleet_leader_state (
    singleton_key     TEXT PRIMARY KEY DEFAULT 'current',
    computer_id       UUID NOT NULL REFERENCES computers(id),
    member_name       TEXT NOT NULL,
    epoch             BIGINT NOT NULL,
    elected_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    reason            TEXT,
    heartbeat_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (singleton_key = 'current')
);

-- ─── OpenClaw install record ────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS openclaw_installations (
    computer_id          UUID PRIMARY KEY REFERENCES computers(id) ON DELETE CASCADE,
    mode                 TEXT NOT NULL DEFAULT 'node',   -- gateway|node
    gateway_url          TEXT,
    last_reconfigured_at TIMESTAMPTZ,
    config_path          TEXT NOT NULL DEFAULT '~/.openclaw/openclaw.json',
    metadata             JSONB NOT NULL DEFAULT '{}'
);

-- ─── Software registry + per-computer install record ────────────────────
CREATE TABLE IF NOT EXISTS software_registry (
    id                     TEXT PRIMARY KEY,         -- "ff", "openclaw", "os-macos", ...
    display_name           TEXT NOT NULL,
    kind                   TEXT NOT NULL,            -- binary|runtime|service|os
    applies_to_os_family   TEXT,                      -- NULL = applies everywhere
    version_source         JSONB NOT NULL,
    upgrade_playbook       JSONB NOT NULL,
    rollback_playbook      JSONB NOT NULL DEFAULT '{}',
    latest_version         TEXT,
    latest_version_at      TIMESTAMPTZ,
    release_notes_url      TEXT,
    requires_restart       BOOLEAN NOT NULL DEFAULT false,
    requires_reboot        BOOLEAN NOT NULL DEFAULT false,
    metadata               JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS computer_software (
    computer_id               UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    software_id               TEXT NOT NULL REFERENCES software_registry(id) ON DELETE CASCADE,
    installed_version         TEXT,
    install_source            TEXT,                     -- brew|apt|dpkg|pip|pipx|npm|cargo|direct|...
    install_source_identifier TEXT,
    install_path              TEXT,
    first_seen_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_checked_at           TIMESTAMPTZ,
    last_upgraded_at          TIMESTAMPTZ,
    status                    TEXT NOT NULL DEFAULT 'ok',
    last_upgrade_error        TEXT,
    -- Free-form JSON for signals that don't fit any other column
    -- (e.g. `{"git_state":"pushed"}` for ff_git / forgefleetd_git rows).
    metadata                  JSONB NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (computer_id, software_id)
);
-- Idempotent add for existing deployments (the CREATE TABLE above runs
-- only on fresh DBs; running fleets predate this column).
ALTER TABLE computer_software
    ADD COLUMN IF NOT EXISTS metadata JSONB NOT NULL DEFAULT '{}'::jsonb;
CREATE INDEX IF NOT EXISTS idx_computer_software_status ON computer_software(status);
CREATE INDEX IF NOT EXISTS idx_computer_software_by_software ON computer_software(software_id);
CREATE INDEX IF NOT EXISTS idx_computer_software_by_source ON computer_software(install_source);

-- ─── Model portfolio + per-computer presence + deployments ──────────────
CREATE TABLE IF NOT EXISTS model_catalog (
    id                    TEXT PRIMARY KEY,
    display_name          TEXT NOT NULL,
    family                TEXT NOT NULL,
    parameter_count       TEXT,
    architecture          TEXT,
    license               TEXT,
    tasks                 JSONB NOT NULL DEFAULT '[]',
    input_modalities      JSONB NOT NULL DEFAULT '[]',
    output_modalities     JSONB NOT NULL DEFAULT '[]',
    languages             JSONB NOT NULL DEFAULT '[]',
    upstream_source       TEXT NOT NULL DEFAULT 'huggingface',
    upstream_id           TEXT,
    upstream_latest_rev   TEXT,
    upstream_checked_at   TIMESTAMPTZ,
    release_date          DATE,
    quantization          TEXT,
    file_size_gb          FLOAT,
    context_window        INT,
    recommended_runtime   JSONB NOT NULL DEFAULT '[]',
    required_gpu_kind     TEXT,                        -- apple_silicon|nvidia_cuda|amd_rocm|NULL
    min_vram_gb           FLOAT,
    cpu_runnable          BOOLEAN NOT NULL DEFAULT true,
    quality_tier          TEXT NOT NULL DEFAULT 'standard',
    lifecycle_status      TEXT NOT NULL DEFAULT 'active',
    replaced_by           TEXT REFERENCES model_catalog(id),
    retirement_reason     TEXT,
    retirement_date       DATE,
    added_by              TEXT,
    notes                 TEXT,
    benchmark_results     JSONB NOT NULL DEFAULT '{}',
    metadata              JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_model_catalog_by_task ON model_catalog USING GIN (tasks);
CREATE INDEX IF NOT EXISTS idx_model_catalog_by_family ON model_catalog(family);
CREATE INDEX IF NOT EXISTS idx_model_catalog_by_lifecycle ON model_catalog(lifecycle_status);
CREATE INDEX IF NOT EXISTS idx_model_catalog_by_tier ON model_catalog(quality_tier);

CREATE TABLE IF NOT EXISTS computer_models (
    computer_id     UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    model_id        TEXT NOT NULL REFERENCES model_catalog(id),
    file_path       TEXT NOT NULL,
    size_gb         FLOAT,
    present         BOOLEAN NOT NULL DEFAULT true,
    downloaded_at   TIMESTAMPTZ,
    last_seen_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status          TEXT NOT NULL DEFAULT 'ok',     -- ok|revision_available|missing|corrupt
    metadata        JSONB NOT NULL DEFAULT '{}',
    PRIMARY KEY (computer_id, model_id)
);

CREATE TABLE IF NOT EXISTS computer_model_deployments (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    computer_id             UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    model_id                TEXT NOT NULL REFERENCES model_catalog(id),
    runtime                 TEXT NOT NULL,
    endpoint                TEXT NOT NULL,
    openai_compatible       BOOLEAN NOT NULL DEFAULT true,
    context_window          INT,
    parallel_slots          INT,
    pid                     INT,
    status                  TEXT NOT NULL DEFAULT 'loading', -- loading|active|idle|error|stopping|stopped
    cluster_id              TEXT,
    cluster_role            TEXT,
    cluster_peers           JSONB NOT NULL DEFAULT '[]',
    tensor_parallel_size    INT NOT NULL DEFAULT 1,
    pipeline_parallel_size  INT NOT NULL DEFAULT 1,
    ram_allocated_gb        FLOAT,
    vram_allocated_gb       FLOAT,
    started_at              TIMESTAMPTZ,
    stopped_at              TIMESTAMPTZ,
    last_status_change      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata                JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_deployments_by_computer ON computer_model_deployments(computer_id);
CREATE INDEX IF NOT EXISTS idx_deployments_by_model ON computer_model_deployments(model_id);
CREATE INDEX IF NOT EXISTS idx_deployments_by_cluster ON computer_model_deployments(cluster_id);

-- Required task portfolio (operator declares "fleet must always cover X")
CREATE TABLE IF NOT EXISTS fleet_task_coverage (
    task                  TEXT PRIMARY KEY,
    min_models_loaded     INT NOT NULL DEFAULT 1,
    preferred_model_ids   JSONB NOT NULL DEFAULT '[]',
    priority              TEXT NOT NULL DEFAULT 'normal', -- critical|normal|nice-to-have
    notes                 TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─── Docker container tracking (reported by Pulse) ──────────────────────
CREATE TABLE IF NOT EXISTS computer_docker_containers (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    computer_id         UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    project_name        TEXT,                    -- compose project (forgefleet, hireflow360, ...)
    compose_file        TEXT,
    container_name      TEXT NOT NULL,
    container_id        TEXT,
    image               TEXT,
    ports               JSONB NOT NULL DEFAULT '[]',
    status              TEXT NOT NULL DEFAULT 'unknown',  -- running|stopped|exited|paused|restarting
    health              TEXT,                    -- healthy|unhealthy|starting|none
    last_status_change  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    first_seen_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata            JSONB NOT NULL DEFAULT '{}',
    UNIQUE (computer_id, container_name)
);
CREATE INDEX IF NOT EXISTS idx_docker_by_project ON computer_docker_containers(project_name);
CREATE INDEX IF NOT EXISTS idx_docker_by_status ON computer_docker_containers(status);

-- ─── HA: database replicas + backups (Phase 6 preparation) ──────────────
CREATE TABLE IF NOT EXISTS database_replicas (
    computer_id                 UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    database_kind               TEXT NOT NULL,    -- postgres|redis|nats
    role                        TEXT NOT NULL,    -- primary|replica|sentinel_voter
    status                      TEXT NOT NULL,    -- running|syncing|promoting|stopped|failed
    lag_bytes                   BIGINT,
    last_sync_at                TIMESTAMPTZ,
    promoted_at                 TIMESTAMPTZ,
    bootstrapped_from_backup_id UUID,
    notes                       TEXT,
    PRIMARY KEY (computer_id, database_kind)
);

CREATE TABLE IF NOT EXISTS backups (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    database_kind          TEXT NOT NULL,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    size_bytes             BIGINT NOT NULL,
    source_computer_id     UUID NOT NULL REFERENCES computers(id),
    checksum_sha256        TEXT NOT NULL,
    file_name              TEXT NOT NULL,
    distribution_status    JSONB NOT NULL DEFAULT '{}',
    verified_restorable_at TIMESTAMPTZ,
    retention_tier         TEXT NOT NULL DEFAULT 'recent' -- recent|daily|weekly
);
CREATE INDEX IF NOT EXISTS idx_backups_by_kind_created ON backups(database_kind, created_at DESC);
"#;

/// Phase 9: Project Management — projects, milestones, work items, outputs,
/// branches, environments, CI runs, and work-item relations.
///
/// Adds a first-class project registry that replaces the old "Mission Control"
/// term. `projects.id` is a stable TEXT slug (matches
/// `config/projects.toml`). Work items and their outputs reference the
/// project slug. Every row is idempotent via the usual `IF NOT EXISTS` guards.
pub const SCHEMA_V15_PROJECT_MANAGEMENT: &str = r#"
-- ─── V15: Project Management (projects, work items, outputs, branches) ────
-- See plan: we-are-mixing-two-streamed-sky.md §Phase 9.

CREATE TABLE IF NOT EXISTS projects (
    id                  TEXT PRIMARY KEY,
    display_name        TEXT NOT NULL,
    compose_file        TEXT,
    repo_url            TEXT,
    default_branch      TEXT NOT NULL DEFAULT 'main',
    main_commit_sha     TEXT,
    main_commit_message TEXT,
    main_committed_at   TIMESTAMPTZ,
    main_committed_by   TEXT,
    main_last_synced_at TIMESTAMPTZ,
    target_computers    JSONB NOT NULL DEFAULT '[]',
    health_endpoint     TEXT,
    status              TEXT NOT NULL DEFAULT 'active',
    metadata            JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS milestones (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  TEXT NOT NULL REFERENCES projects(id),
    name        TEXT NOT NULL,
    description TEXT,
    due_date    DATE,
    status      TEXT NOT NULL DEFAULT 'active'
);

CREATE TABLE IF NOT EXISTS work_items (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id          TEXT NOT NULL REFERENCES projects(id),
    milestone_id        UUID REFERENCES milestones(id),
    parent_id           UUID REFERENCES work_items(id),
    kind                TEXT NOT NULL,
    title               TEXT NOT NULL,
    description         TEXT,
    labels              JSONB NOT NULL DEFAULT '[]',
    status              TEXT NOT NULL DEFAULT 'idea',
    priority            TEXT NOT NULL DEFAULT 'normal',
    assigned_to         TEXT,
    assigned_computer   TEXT,
    branch_name         TEXT,
    pr_url              TEXT,
    brain_node_ids      JSONB NOT NULL DEFAULT '[]',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by          TEXT NOT NULL,
    started_at          TIMESTAMPTZ,
    completed_at        TIMESTAMPTZ,
    due_date            DATE,
    estimated_hours     FLOAT,
    metadata            JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_work_items_project_status
    ON work_items(project_id, status);
CREATE INDEX IF NOT EXISTS idx_work_items_assigned
    ON work_items(assigned_to, status);

CREATE TABLE IF NOT EXISTS work_outputs (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    work_item_id        UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    kind                TEXT NOT NULL,
    title               TEXT,
    file_path           TEXT,
    file_size_bytes     BIGINT,
    mime_type           TEXT,
    commit_sha          TEXT,
    repo_url            TEXT,
    produced_by_human   TEXT,
    produced_by_agent   TEXT,
    produced_on_computer TEXT,
    llm_model_id        TEXT REFERENCES model_catalog(id),
    llm_model_version   TEXT,
    llm_tokens_input    INT,
    llm_tokens_output   INT,
    llm_cost_estimate   FLOAT,
    produced_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    review_required     BOOLEAN NOT NULL DEFAULT false,
    review_status       TEXT,
    reviewed_by         TEXT,
    reviewed_at         TIMESTAMPTZ,
    review_notes        TEXT,
    metadata            JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_work_outputs_by_model
    ON work_outputs(llm_model_id);
CREATE INDEX IF NOT EXISTS idx_work_outputs_by_computer
    ON work_outputs(produced_on_computer);

CREATE TABLE IF NOT EXISTS project_branches (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id          TEXT NOT NULL REFERENCES projects(id),
    branch_name         TEXT NOT NULL,
    created_by          TEXT NOT NULL,
    assigned_computer   TEXT,
    assigned_agent      TEXT,
    purpose             TEXT,
    last_commit_sha     TEXT,
    last_commit_message TEXT,
    last_commit_at      TIMESTAMPTZ,
    pr_number           INT,
    pr_url              TEXT,
    pr_state            TEXT,
    status              TEXT NOT NULL DEFAULT 'active',
    merged_at           TIMESTAMPTZ,
    merged_sha          TEXT,
    UNIQUE (project_id, branch_name)
);

CREATE TABLE IF NOT EXISTS project_environments (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id           TEXT NOT NULL REFERENCES projects(id),
    name                 TEXT NOT NULL,
    target_computers     JSONB NOT NULL DEFAULT '[]',
    deployed_commit_sha  TEXT,
    deployed_tag         TEXT,
    deployed_at          TIMESTAMPTZ,
    deployed_by          TEXT,
    deploy_trigger       TEXT,
    deploy_status        TEXT,
    health_endpoint      TEXT,
    last_health_check_at TIMESTAMPTZ,
    health_status        TEXT,
    url                  TEXT,
    metadata             JSONB NOT NULL DEFAULT '{}',
    UNIQUE (project_id, name)
);

CREATE TABLE IF NOT EXISTS project_ci_runs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      TEXT NOT NULL REFERENCES projects(id),
    branch_name     TEXT NOT NULL,
    commit_sha      TEXT NOT NULL,
    workflow_name   TEXT,
    run_id          TEXT,
    run_url         TEXT,
    status          TEXT NOT NULL,
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    triggered_by    TEXT
);
CREATE INDEX IF NOT EXISTS idx_ci_runs_by_branch
    ON project_ci_runs(project_id, branch_name, started_at DESC);

CREATE TABLE IF NOT EXISTS work_item_relations (
    from_id         UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    to_id           UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    relation_type   TEXT NOT NULL,
    PRIMARY KEY (from_id, to_id, relation_type)
);
"#;

pub const SCHEMA_V16_OBSERVABILITY: &str = r#"
-- V16: observability — metrics history, alert policies, alert events
-- Uses plain Postgres (no TimescaleDB extension dependency; if available, add
-- SELECT create_hypertable later — not blocking).

CREATE TABLE IF NOT EXISTS computer_metrics_history (
    computer_id          UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    recorded_at          TIMESTAMPTZ NOT NULL,
    cpu_pct              FLOAT,
    ram_pct              FLOAT,
    ram_used_gb          FLOAT,
    disk_free_gb         FLOAT,
    gpu_pct              FLOAT,
    llm_ram_allocated_gb FLOAT,
    llm_queue_depth      INT,
    llm_active_requests  INT,
    llm_tokens_per_sec   FLOAT,
    PRIMARY KEY (computer_id, recorded_at)
);
CREATE INDEX IF NOT EXISTS idx_metrics_by_time ON computer_metrics_history(recorded_at DESC);

CREATE TABLE IF NOT EXISTS alert_policies (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT NOT NULL UNIQUE,
    description     TEXT,
    metric          TEXT NOT NULL,          -- 'cpu_pct' | 'ram_pct' | 'disk_free_gb' | 'llm_queue_depth' | 'computer_status' | ...
    scope           TEXT NOT NULL DEFAULT 'any_computer',  -- 'any_computer' | 'specific' (with computer_id) | 'leader_only'
    scope_computer_id UUID REFERENCES computers(id),
    condition       TEXT NOT NULL,          -- '> 90' | '< 10' | "== 'offline'"
    duration_secs   INT NOT NULL DEFAULT 300,
    severity        TEXT NOT NULL DEFAULT 'warning',  -- 'info' | 'warning' | 'critical'
    cooldown_secs   INT NOT NULL DEFAULT 3600,
    channel         TEXT NOT NULL DEFAULT 'telegram',  -- 'telegram' | 'log' | 'webhook' | 'openclaw'
    enabled         BOOLEAN NOT NULL DEFAULT true,
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS alert_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    policy_id       UUID NOT NULL REFERENCES alert_policies(id),
    computer_id     UUID REFERENCES computers(id),
    fired_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at     TIMESTAMPTZ,
    value           FLOAT,
    value_text      TEXT,
    message         TEXT,
    channel_result  TEXT  -- 'sent' | 'failed: <reason>' | 'muted'
);
CREATE INDEX IF NOT EXISTS idx_alert_events_policy ON alert_events(policy_id, fired_at DESC);
CREATE INDEX IF NOT EXISTS idx_alert_events_unresolved ON alert_events(resolved_at) WHERE resolved_at IS NULL;
"#;

pub const SCHEMA_V17_SECURITY_HARDENING: &str = r#"
-- V17: Security hardening — secrets rotation, SSH key revocation, pulse HMAC.
--
-- 1) Extend fleet_secrets with rotation tracking. expires_at=NULL means
--    "never expires". rotate_before_days is the warning window before
--    expires_at. rotation_count records how many times rotate() has run.
ALTER TABLE fleet_secrets
    ADD COLUMN IF NOT EXISTS expires_at           TIMESTAMPTZ;
ALTER TABLE fleet_secrets
    ADD COLUMN IF NOT EXISTS rotate_before_days   INT NOT NULL DEFAULT 90;
ALTER TABLE fleet_secrets
    ADD COLUMN IF NOT EXISTS rotation_count       INT NOT NULL DEFAULT 0;
ALTER TABLE fleet_secrets
    ADD COLUMN IF NOT EXISTS last_rotated_at      TIMESTAMPTZ;

-- 2) SSH key revocation edges. Records that a particular public key
--    (identified by fingerprint) was removed from `target_node`'s
--    authorized_keys. Written by ssh_key_manager::revoke_computer_trust.
CREATE TABLE IF NOT EXISTS fleet_ssh_revocations (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    revoked_node    TEXT NOT NULL,         -- whose key was revoked
    key_fingerprint TEXT NOT NULL,         -- fingerprint of the revoked pubkey
    target_node     TEXT NOT NULL,         -- host we removed it from
    revoked_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_by      TEXT,                  -- user/system tag
    success         BOOLEAN NOT NULL DEFAULT true,
    last_error      TEXT
);
CREATE INDEX IF NOT EXISTS idx_ssh_revocations_revoked
    ON fleet_ssh_revocations(revoked_node);
CREATE INDEX IF NOT EXISTS idx_ssh_revocations_target
    ON fleet_ssh_revocations(target_node);

-- 3) `computer_trust` — mark edges as revoked without deleting history.
-- The table is created in V14; this adds the revoked columns if missing.
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
               WHERE table_name = 'computer_trust') THEN
        BEGIN
            ALTER TABLE computer_trust
                ADD COLUMN IF NOT EXISTS revoked_at  TIMESTAMPTZ;
            ALTER TABLE computer_trust
                ADD COLUMN IF NOT EXISTS revoked_by  TEXT;
        EXCEPTION WHEN others THEN
            -- column_add may race across concurrent migrations; ignore.
            NULL;
        END;
    END IF;
END $$;
"#;

/// V18: Tailscale-only / WAN computer support.
///
/// Adds `network_scope` to the `computers` table. Values:
///   - `lan`            (default) — has a LAN IP; WoL / direct probing works
///   - `tailscale_only`           — only reachable over Tailscale; no WoL
///   - `wan`                      — publicly routable; no WoL, off-site
///
/// This hint lets `revive::ReviveManager` skip WoL for computers whose
/// only reachable address is over an overlay network (magic packets are
/// link-local and won't traverse Tailscale / the internet).
pub const SCHEMA_V18_NETWORK_SCOPE: &str = r#"
ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS network_scope TEXT NOT NULL DEFAULT 'lan';
"#;

/// V19: Phase 12 — shared NFS volumes, power scheduling, and training jobs.
///
/// 1) `shared_volumes` / `shared_volume_mounts` — NFS exports (one row per
///    exported directory on the host node) plus a join table tracking which
///    computers have mounted it and in what state.
/// 2) `computer_schedules` — cron-driven sleep/wake/restart rules per computer.
///    Evaluated once a minute by the leader's power scheduler. An optional
///    `condition` expression (e.g. `idle_minutes > 120`) is parsed against
///    pulse beats at evaluation time.
/// 3) `training_jobs` — LoRA / full fine-tune orchestration. `loss_curve` is
///    an append-only JSON array of {step, loss, ts} samples; when a run
///    completes the resulting adapter is registered in `model_catalog` and
///    `result_model_id` is populated.
pub const SCHEMA_V19_STORAGE_POWER_TRAINING: &str = r#"
CREATE TABLE IF NOT EXISTS shared_volumes (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name              TEXT NOT NULL UNIQUE,
    host_computer_id  UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    export_path       TEXT NOT NULL,                         -- e.g. /Users/venkat/models on host
    mount_path        TEXT NOT NULL,                         -- where it appears on clients, e.g. ~/models
    nfs_version       TEXT NOT NULL DEFAULT '4',
    read_only         BOOLEAN NOT NULL DEFAULT false,
    size_gb           FLOAT,
    used_gb           FLOAT,
    purpose           TEXT,                                   -- models | training_data | outputs
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata          JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_shared_volumes_host ON shared_volumes(host_computer_id);

CREATE TABLE IF NOT EXISTS shared_volume_mounts (
    volume_id       UUID NOT NULL REFERENCES shared_volumes(id) ON DELETE CASCADE,
    computer_id     UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    mount_path      TEXT,                                    -- override; falls back to shared_volumes.mount_path
    mounted_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status          TEXT NOT NULL DEFAULT 'mounting',        -- mounting|mounted|stale|unmounted
    last_check_at   TIMESTAMPTZ,
    last_error      TEXT,
    PRIMARY KEY (volume_id, computer_id)
);

CREATE TABLE IF NOT EXISTS computer_schedules (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    computer_id      UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    kind             TEXT NOT NULL,                           -- sleep | wake | restart
    cron_expr        TEXT NOT NULL,                           -- e.g. '0 0 * * *'
    condition        TEXT,                                    -- e.g. 'idle_minutes > 120'
    enabled          BOOLEAN NOT NULL DEFAULT true,
    last_fired_at    TIMESTAMPTZ,
    last_result      TEXT,                                    -- ok | skipped: ... | error: ...
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by       TEXT
);
CREATE INDEX IF NOT EXISTS idx_computer_schedules_enabled
    ON computer_schedules(enabled);
CREATE INDEX IF NOT EXISTS idx_computer_schedules_by_computer
    ON computer_schedules(computer_id);

CREATE TABLE IF NOT EXISTS training_jobs (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                   TEXT NOT NULL,
    base_model_id          TEXT REFERENCES model_catalog(id),
    training_data_path     TEXT NOT NULL,
    adapter_output_path    TEXT,
    training_type          TEXT NOT NULL DEFAULT 'lora',     -- lora | full_finetune | dpo
    computer_id            UUID REFERENCES computers(id),
    status                 TEXT NOT NULL DEFAULT 'queued',   -- queued|running|completed|failed|cancelled
    started_at             TIMESTAMPTZ,
    completed_at           TIMESTAMPTZ,
    loss_curve             JSONB NOT NULL DEFAULT '[]',
    params                 JSONB NOT NULL DEFAULT '{}',      -- epochs, lr, batch_size, lora_rank
    result_model_id        TEXT REFERENCES model_catalog(id),
    deferred_task_id       UUID,                              -- deferred_tasks row that drives execution
    error_message          TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by             TEXT
);
CREATE INDEX IF NOT EXISTS idx_training_jobs_status
    ON training_jobs(status);
CREATE INDEX IF NOT EXISTS idx_training_jobs_by_computer
    ON training_jobs(computer_id);
"#;

/// V20: `port_registry` — canonical inventory of every port ForgeFleet uses.
///
/// Seeded from `config/ports.toml` on daemon startup and via `ff ports seed`.
/// The registry is the source of truth that firewall rules, docker-compose
/// mappings, and `ff ports scan <computer>` cross-reference at runtime.
pub const SCHEMA_V20_PORT_REGISTRY: &str = r#"
CREATE TABLE IF NOT EXISTS port_registry (
    port            INT PRIMARY KEY,
    service         TEXT NOT NULL,
    kind            TEXT NOT NULL,       -- control_plane|database|coordination|llm_inference|system
    description     TEXT NOT NULL,
    exposed_on      TEXT NOT NULL,       -- "all_members" | "leader_only" | "taylor" | ...
    scope           TEXT NOT NULL DEFAULT 'lan',  -- lan | public_via_proxy
    managed_by      TEXT,
    status          TEXT NOT NULL DEFAULT 'active', -- active | planned | deprecated
    metadata        JSONB NOT NULL DEFAULT '{}',
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_port_registry_kind ON port_registry(kind);
CREATE INDEX IF NOT EXISTS idx_port_registry_scope ON port_registry(scope);
"#;

// ─── V21: Drop computer_model_deployments.model_id FK ───────────────────
//
// Pulse beats can report LLM servers whose `model.id` is a Huggingface
// repo slug, an Ollama tag (`qwen2.5-coder:14b`), or a GGUF filename —
// none of which are guaranteed to exist in `model_catalog`. The FK was
// blocking the materializer from persisting those deployment rows,
// which in turn caused `/api/llm/servers` to return an empty list.
//
// Going forward, `model_id` is a free-form string column; `model_catalog`
// remains the authoritative registry but is no longer a hard dependency.
pub const SCHEMA_V21_DROP_DEPLOYMENT_FK: &str = r#"
ALTER TABLE computer_model_deployments
    DROP CONSTRAINT IF EXISTS computer_model_deployments_model_id_fkey;
"#;

// ─── V22: Drop computer_models.model_id FK ──────────────────────────────
//
// Same rationale as V21 but for the `computer_models` (model-presence)
// table. Pulse-reported on-disk models carry ids that aren't guaranteed
// to exist in `model_catalog`; the FK was aborting the materializer's
// per-beat transaction before it could even reach the deployment upserts,
// which is why V21 alone didn't fix `/api/llm/servers` returning empty.
pub const SCHEMA_V22_DROP_MODEL_PRESENCE_FK: &str = r#"
ALTER TABLE computer_models
    DROP CONSTRAINT IF EXISTS computer_models_model_id_fkey;
"#;

// ─── V23: Sub-agent slots ───────────────────────────────────────────────
//
// Adds the `sub_agents` table that the agent coordinator uses to claim a
// concurrency slot on a target computer before dispatching a work item to
// that computer's local LLM. One row per (computer, slot) — the daemon
// seeds slot 0..N-1 for each computer, where N comes from
// `fleet_nodes.sub_agent_count` (falls back to cpu_cores/4 on first run).
//
// The unique (computer_id, slot) index enforces that a given slot is
// always addressable at most once; the claim path uses a transactional
// UPDATE WHERE status='idle' so two dispatchers can't grab the same slot.
pub const SCHEMA_V23_SUB_AGENTS: &str = r#"
CREATE TABLE IF NOT EXISTS sub_agents (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    computer_id           UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    slot                  INT NOT NULL,
    status                TEXT NOT NULL DEFAULT 'idle',
    current_work_item_id  UUID REFERENCES work_items(id),
    started_at            TIMESTAMPTZ,
    workspace_dir         TEXT NOT NULL,
    model_preference      TEXT,
    last_heartbeat_at     TIMESTAMPTZ,
    metadata              JSONB NOT NULL DEFAULT '{}',
    UNIQUE (computer_id, slot)
);
CREATE INDEX IF NOT EXISTS idx_sub_agents_status ON sub_agents(status);
CREATE INDEX IF NOT EXISTS idx_sub_agents_computer ON sub_agents(computer_id);
"#;

// ─── V24: External tools (GitHub-hosted CLI/MCP package manager) ────────
//
// Fleet-wide catalog of developer tools hosted on GitHub (e.g.
// `code-review-graph`, `context-mode`) that expose a CLI entrypoint,
// an MCP stdio server, or both. Mirrors the shape of `software_registry`
// + `computer_software` (schema V14) but scoped to "things we install
// via cargo/npm/pip/git-build from a GitHub URL" as opposed to
// OS-level packages tracked in `software_registry`.
//
//   external_tools          — catalog (one row per tool)
//   computer_external_tools — per-computer install state
//
// Drift detection + install dispatch reuse the same building blocks as
// the software_registry path (github_release upstream check, deferred
// task queue, finalizer hook).
//
// See `config/external_tools.toml` for the seed format.
pub const SCHEMA_V24_EXTERNAL_TOOLS: &str = r#"
CREATE TABLE IF NOT EXISTS external_tools (
    id                  TEXT PRIMARY KEY,
    display_name        TEXT NOT NULL,
    github_url          TEXT NOT NULL,
    kind                TEXT NOT NULL DEFAULT 'cli',  -- cli | mcp | both
    install_method      TEXT NOT NULL,                 -- cargo_install | npm_global | pip | git_build | binary_release
    install_spec        JSONB NOT NULL DEFAULT '{}',
    cli_entrypoint      TEXT,                          -- command added to PATH (e.g. "crg")
    mcp_server_command  TEXT,                          -- command run as MCP stdio server, if kind=mcp|both
    register_as_mcp     BOOLEAN NOT NULL DEFAULT false,
    version_source      JSONB NOT NULL DEFAULT '{}',
    upgrade_playbook    JSONB NOT NULL DEFAULT '{}',
    latest_version      TEXT,
    latest_version_at   TIMESTAMPTZ,
    intake_source       TEXT,                          -- 'direct' | 'social' (hint for where this entry came from)
    intake_reference    TEXT,                          -- original URL (GitHub or social media)
    added_by            TEXT,
    added_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata            JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS computer_external_tools (
    computer_id         UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    tool_id             TEXT NOT NULL REFERENCES external_tools(id) ON DELETE CASCADE,
    installed_version   TEXT,
    install_source      TEXT,                          -- cargo | npm | pip | direct | git_build
    install_path        TEXT,
    mcp_registered      BOOLEAN NOT NULL DEFAULT false,
    first_seen_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_checked_at     TIMESTAMPTZ,
    last_upgraded_at    TIMESTAMPTZ,
    status              TEXT NOT NULL DEFAULT 'ok',    -- ok | upgrade_available | upgrading | installing | install_failed | missing
    last_error          TEXT,
    PRIMARY KEY (computer_id, tool_id)
);
CREATE INDEX IF NOT EXISTS cet_status_idx ON computer_external_tools(status);
CREATE INDEX IF NOT EXISTS cet_by_tool_idx ON computer_external_tools(tool_id);
"#;

// ─── V25: Social media ingest ───────────────────────────────────────────
//
// Ingest pipeline for short-form social posts (Twitter/X, Instagram,
// TikTok, YouTube). The operator sends a URL; we shell out to yt-dlp to
// pull media + metadata, sample frames via ffmpeg, then run a
// vision-capable LLM over the frames to extract URLs, tool mentions,
// OCR text, code snippets, and a summary. This is one of the intake
// paths for the external_tools subsystem (V24).
//
// Status values: queued | fetching | analyzing | done | failed.
pub const SCHEMA_V25_SOCIAL_MEDIA_INGEST: &str = r#"
CREATE TABLE IF NOT EXISTS social_media_posts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    url             TEXT NOT NULL,
    platform        TEXT NOT NULL,              -- twitter | instagram | tiktok | youtube | other
    author          TEXT,
    caption         TEXT,
    media_items     JSONB NOT NULL DEFAULT '[]', -- [{kind:image|video|audio, local_path, mime, bytes, frame_count?}]
    extracted_text  TEXT,                        -- OCR + transcription combined
    analysis        JSONB,                       -- vision-LLM output: {summary, detected_urls, detected_tools, entities, sentiment}
    status          TEXT NOT NULL DEFAULT 'queued', -- queued | fetching | analyzing | done | failed
    ingested_by     TEXT,                        -- user or agent name
    ingested_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    analyzed_at     TIMESTAMPTZ,
    last_error      TEXT
);
CREATE INDEX IF NOT EXISTS smp_status_idx ON social_media_posts(status);
CREATE INDEX IF NOT EXISTS smp_platform_idx ON social_media_posts(platform);
"#;

// ─── V26: Cloud LLM providers (OpenAI/Anthropic/Moonshot/Google) ────────────
//
// Lets the gateway route `/v1/chat/completions` requests whose `model`
// field matches a provider's `model_prefix` (e.g. `claude-*`, `openai/*`,
// `kimi/*`, `gemini/*`) off the fleet to the provider's public API.
// Credentials live in `fleet_secrets` (schema V9) — this table only holds
// the provider config + a pointer (`secret_key`) to the secret row.
//
//   cloud_llm_providers — one row per provider (catalog)
//   cloud_llm_usage     — per-request usage/cost/latency ledger
//
// OAuth (auth_kind='oauth2') is schema-ready but NOT wired in the gateway
// this pass — see TODO(oauth) in crates/ff-gateway/src/cloud_llm.rs.
pub const SCHEMA_V26_CLOUD_LLM_PROVIDERS: &str = r#"
CREATE TABLE IF NOT EXISTS cloud_llm_providers (
    id                TEXT PRIMARY KEY,
    display_name      TEXT NOT NULL,
    base_url          TEXT NOT NULL,
    auth_kind         TEXT NOT NULL,
    secret_key        TEXT NOT NULL,
    oauth_token_secret TEXT,
    oauth_token_url   TEXT,
    oauth_client_id   TEXT,
    model_prefix      TEXT NOT NULL,
    request_format    TEXT NOT NULL DEFAULT 'openai_chat',
    enabled           BOOLEAN NOT NULL DEFAULT true,
    metadata          JSONB NOT NULL DEFAULT '{}',
    added_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS cloud_llm_usage (
    id                BIGSERIAL PRIMARY KEY,
    provider_id       TEXT NOT NULL REFERENCES cloud_llm_providers(id),
    model             TEXT NOT NULL,
    tokens_input      INT,
    tokens_output     INT,
    cost_usd          NUMERIC(10, 6),
    session_id        TEXT,
    used_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    request_duration_ms INT
);
CREATE INDEX IF NOT EXISTS cloud_llm_usage_by_provider ON cloud_llm_usage(provider_id, used_at DESC);
"#;

// ─── V27: Pool aliases on fleet_task_coverage ───────────────────────────────
//
// Adds an optional `alias` column to `fleet_task_coverage` so gateway clients
// can say `model="coder"` / `model="multimodal"` / `model="thinking"` instead
// of an exact HuggingFace model id. At routing time the gateway expands the
// alias into the row's `preferred_model_ids` and picks the lowest-load live
// endpoint whose `model.id` matches any member. The column is UNIQUE so each
// alias maps to exactly one pool.
pub const SCHEMA_V27_POOL_ALIASES: &str = r#"
ALTER TABLE fleet_task_coverage
    ADD COLUMN IF NOT EXISTS alias TEXT UNIQUE;

CREATE INDEX IF NOT EXISTS fleet_task_coverage_alias_idx
    ON fleet_task_coverage(alias);
"#;

// ─── V28: Seed software_registry with canonical rows ────────────────────
//
// Retires `config/software.toml` — the DB (`software_registry`) is now
// the sole source of truth. Operator edits via `ff software add/remove`
// are preserved across upgrades because each row uses ON CONFLICT (id)
// DO NOTHING.
//
// `latest_version` / `latest_version_at` are NOT seeded here — those
// columns are owned by the upstream-check loop (see
// `ff_agent::software_upstream`) and must stay NULL on first insert so
// the loop reliably flips rows into `upgrade_available` the first time
// a real check runs.
pub const SCHEMA_V28_SOFTWARE_REGISTRY_SEED: &str = r#"
-- ForgeFleet's own -------------------------------------------------------
INSERT INTO software_registry
    (id, display_name, kind, applies_to_os_family,
     version_source, upgrade_playbook, requires_restart, requires_reboot)
VALUES
  ('ff',
   'ForgeFleet CLI (ff)',
   'binary',
   NULL,
   '{"method":"cmd","args":["ff","--version"],"regex":"ff (\\S+)"}'::jsonb,
   '{
     "macos":"cd ~/taylorProjects/forge-fleet && git pull && cargo build --release -p ff && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/ff",
     "linux-ubuntu":"cd ~/taylorProjects/forge-fleet && git pull && cargo build --release -p ff && install -m 755 target/release/ff ~/.local/bin/ff",
     "linux-dgx":"cd ~/taylorProjects/forge-fleet && git pull && cargo build --release -p ff && install -m 755 target/release/ff ~/.local/bin/ff"
   }'::jsonb,
   false, false),

  ('forgefleetd',
   'ForgeFleet Daemon (forgefleetd)',
   'binary',
   NULL,
   '{"method":"cmd","args":["forgefleetd","--version"],"regex":"forgefleetd (\\S+)"}'::jsonb,
   '{
     "macos":"cd ~/taylorProjects/forge-fleet && git pull && cargo build --release -p forgefleetd && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && codesign --force --sign - ~/.local/bin/forgefleetd",
     "linux-ubuntu":"cd ~/taylorProjects/forge-fleet && git pull && cargo build --release -p forgefleetd && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd",
     "linux-dgx":"cd ~/taylorProjects/forge-fleet && git pull && cargo build --release -p forgefleetd && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd"
   }'::jsonb,
   true, false),

  ('ff_git',
   'ff (git SHA of built binary)',
   'binary',
   NULL,
   '{"method":"self_built"}'::jsonb,
   '{
     "macos":"cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/ff",
     "linux":"scp -o BatchMode=yes -o StrictHostKeyChecking=accept-new venkat@192.168.5.100:~/.local/bin/ff ~/.local/bin/ff.new && install -m 755 ~/.local/bin/ff.new ~/.local/bin/ff && rm ~/.local/bin/ff.new && systemctl --user restart forgefleet-daemon.service"
   }'::jsonb,
   false, false),

  ('forgefleetd_git',
   'forgefleetd (git SHA of built binary)',
   'binary',
   NULL,
   '{"method":"self_built"}'::jsonb,
   '{
     "macos":"cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && codesign --force --sign - ~/.local/bin/forgefleetd && launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd",
     "linux":"scp -o BatchMode=yes -o StrictHostKeyChecking=accept-new venkat@192.168.5.100:~/.local/bin/forgefleetd ~/.local/bin/forgefleetd.new && install -m 755 ~/.local/bin/forgefleetd.new ~/.local/bin/forgefleetd && rm ~/.local/bin/forgefleetd.new && systemctl --user restart forgefleet-node.service"
   }'::jsonb,
   true, false),

-- Agent platforms -------------------------------------------------------
  ('openclaw',
   'OpenClaw Agent',
   'binary',
   NULL,
   '{"method":"cmd","args":["openclaw","--version"],"regex":"OpenClaw (\\S+)"}'::jsonb,
   '{"all":"curl -fsSL https://openclaw.ai/install.sh | bash"}'::jsonb,
   true, false),

-- Developer tools -------------------------------------------------------
  ('gh',
   'GitHub CLI',
   'binary',
   NULL,
   '{"method":"github_release","repo":"cli/cli"}'::jsonb,
   '{
     "macos":"brew upgrade gh",
     "linux-ubuntu":"sudo apt-get update && sudo apt-get -y install --only-upgrade gh",
     "linux-dgx":"sudo apt-get update && sudo apt-get -y install --only-upgrade gh",
     "windows-winget":"winget upgrade --id GitHub.cli --silent --accept-source-agreements --accept-package-agreements",
     "windows-choco":"choco upgrade gh -y"
   }'::jsonb,
   false, false),

  ('op',
   '1Password CLI',
   'binary',
   NULL,
   '{"method":"cmd","args":["op","--version"],"regex":"(\\S+)"}'::jsonb,
   '{
     "macos-brew-cask":"brew upgrade --cask 1password-cli",
     "linux-ubuntu":"curl -sS https://downloads.1password.com/linux/keys/1password.asc | sudo gpg --dearmor --output /usr/share/keyrings/1password-archive-keyring.gpg && echo ''deb [arch=amd64 signed-by=/usr/share/keyrings/1password-archive-keyring.gpg] https://downloads.1password.com/linux/debian/amd64 stable main'' | sudo tee /etc/apt/sources.list.d/1password.list && sudo apt-get update && sudo apt-get -y install --only-upgrade 1password-cli",
     "linux-dgx":"sudo apt-get update && sudo apt-get -y install --only-upgrade 1password-cli",
     "windows-winget":"winget upgrade --id AgileBits.1Password.CLI --silent --accept-source-agreements --accept-package-agreements",
     "windows-choco":"choco upgrade 1password-cli -y"
   }'::jsonb,
   false, false),

  ('rustup',
   'Rustup (Rust toolchain manager)',
   'binary',
   NULL,
   '{"method":"cmd","args":["rustup","--version"],"regex":"rustup (\\S+)"}'::jsonb,
   '{"all":"rustup self update && rustup update stable"}'::jsonb,
   false, false),

-- Inference runtimes ----------------------------------------------------
  ('llama.cpp',
   'llama.cpp (llama-server)',
   'runtime',
   NULL,
   '{"method":"cmd","args":["llama-server","--version"],"regex":"version: (\\S+)"}'::jsonb,
   '{
     "macos":"cd ~/llama.cpp && git pull && cmake --build build --config Release -j",
     "linux-ubuntu":"cd ~/llama.cpp && git pull && cmake --build build --config Release -j",
     "linux-dgx":"cd ~/llama.cpp && git pull && cmake --build build --config Release -j"
   }'::jsonb,
   true, false),

  ('mlx_lm',
   'MLX-LM (Apple Silicon inference)',
   'runtime',
   'macos',
   '{"method":"pip","package":"mlx-lm"}'::jsonb,
   '{"macos":"pip install -U mlx-lm"}'::jsonb,
   true, false),

  ('vllm',
   'vLLM',
   'runtime',
   NULL,
   '{"method":"pip","package":"vllm"}'::jsonb,
   '{
     "linux-ubuntu":"pip install -U vllm",
     "linux-dgx":"pip install -U vllm"
   }'::jsonb,
   true, false),

  ('ollama',
   'Ollama',
   'runtime',
   NULL,
   '{"method":"cmd","args":["ollama","--version"],"regex":"ollama version is (\\S+)"}'::jsonb,
   '{
     "macos":"brew upgrade ollama",
     "linux-ubuntu":"curl -fsSL https://ollama.com/install.sh | sh",
     "linux-dgx":"curl -fsSL https://ollama.com/install.sh | sh",
     "windows-winget":"winget upgrade --id Ollama.Ollama --silent --accept-source-agreements --accept-package-agreements",
     "windows-choco":"choco upgrade ollama -y"
   }'::jsonb,
   true, false),

-- System runtimes -------------------------------------------------------
  ('node',
   'Node.js 22',
   'runtime',
   NULL,
   '{"method":"cmd","args":["node","--version"],"regex":"v(\\S+)"}'::jsonb,
   '{
     "macos-brew":"brew upgrade node@22",
     "linux-ubuntu":"curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - && sudo apt-get install -y nodejs",
     "linux-dgx":"curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - && sudo apt-get install -y nodejs",
     "windows-winget":"winget upgrade --id OpenJS.NodeJS.LTS --silent --accept-source-agreements --accept-package-agreements",
     "windows-choco":"choco upgrade nodejs-lts -y"
   }'::jsonb,
   false, false),

  ('python',
   'Python 3',
   'runtime',
   NULL,
   '{"method":"cmd","args":["python3","--version"],"regex":"Python (\\S+)"}'::jsonb,
   '{
     "macos-brew":"brew upgrade python@3.12",
     "linux-ubuntu":"sudo apt-get update && sudo apt-get -y install --only-upgrade python3",
     "linux-dgx":"sudo apt-get update && sudo apt-get -y install --only-upgrade python3",
     "windows-winget":"winget upgrade --id Python.Python.3.12 --silent --accept-source-agreements --accept-package-agreements",
     "windows-choco":"choco upgrade python -y"
   }'::jsonb,
   false, false),

  ('docker',
   'Docker',
   'runtime',
   NULL,
   '{"method":"cmd","args":["docker","--version"],"regex":"Docker version (\\S+?),"}'::jsonb,
   '{
     "macos-brew-cask":"brew upgrade --cask docker",
     "linux-ubuntu":"sudo apt-get update && sudo apt-get -y install --only-upgrade docker.io",
     "linux-dgx":"sudo apt-get update && sudo apt-get -y install --only-upgrade docker.io",
     "windows-winget":"winget upgrade --id Docker.DockerDesktop --silent --accept-source-agreements --accept-package-agreements",
     "windows-choco":"choco upgrade docker-desktop -y"
   }'::jsonb,
   true, false),

-- Operating systems -----------------------------------------------------
  ('os-macos',
   'macOS',
   'os',
   'macos',
   '{"method":"sw_vers"}'::jsonb,
   '{"macos":"sudo softwareupdate -i -a --restart"}'::jsonb,
   true, true),

  ('os-ubuntu-22.04',
   'Ubuntu 22.04 LTS (Jammy)',
   'os',
   'linux-ubuntu',
   '{"method":"apt_dist","codename":"jammy"}'::jsonb,
   '{"linux-ubuntu":"sudo apt-get update && sudo apt-get -y dist-upgrade"}'::jsonb,
   true, true),

  ('os-ubuntu-24.04',
   'Ubuntu 24.04 LTS (Noble)',
   'os',
   'linux-ubuntu',
   '{"method":"apt_dist","codename":"noble"}'::jsonb,
   '{"linux-ubuntu":"sudo apt-get update && sudo apt-get -y dist-upgrade"}'::jsonb,
   true, true),

  ('os-dgx',
   'NVIDIA DGX OS',
   'os',
   'linux-dgx',
   '{"method":"cmd","args":["cat","/etc/dgx-release"],"regex":"DGX_SWBUILD_VERSION=(\\S+)"}'::jsonb,
   '{"linux-dgx":"sudo apt-get update && sudo apt-get -y install --only-upgrade dgx-release"}'::jsonb,
   true, true),

  ('os-windows',
   'Microsoft Windows',
   'os',
   'windows',
   '{"method":"cmd","args":["powershell","-NoProfile","-Command","(Get-CimInstance Win32_OperatingSystem).Version"],"regex":"(\\S+)"}'::jsonb,
   '{
     "windows-winget":"winget upgrade --all --silent --accept-source-agreements --accept-package-agreements",
     "windows":"powershell -NoProfile -Command \"Install-Module PSWindowsUpdate -Force -Scope CurrentUser -AcceptLicense; Get-WindowsUpdate -Install -AcceptAll -AutoReboot\""
   }'::jsonb,
   true, true)

ON CONFLICT (id) DO NOTHING;
"#;

// ─── V29: fix V28's arch-blind Linux playbook ────────────────────────────
//
// V28 seeded `ff_git` and `forgefleetd_git` with a Linux playbook that
// scp'd the leader's binary from Taylor (aarch64-apple-darwin) onto
// Linux (x86_64) members. The binary can't exec — fleet-wide Exit 137 /
// "Exec format error". Linux members must rebuild locally.
//
// Also: the V28 key was `linux`, but the resolver (`resolve_upgrade_plans`)
// only checks `<os_family>-<install_source>`, `<os_family>`, and `all`.
// Our `os_family` values are `linux-ubuntu` / `linux-dgx` — never bare
// `linux` — so the V28 entry was doubly broken (wrong command AND dead
// key). V29 writes both `linux-ubuntu` and `linux-dgx` and drops the
// obsolete `linux` key.
pub const SCHEMA_V29_FIX_FF_GIT_LINUX_PLAYBOOK: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = (upgrade_playbook - 'linux')
       || jsonb_build_object(
           'linux-ubuntu',
           'export PATH=$HOME/.cargo/bin:$PATH && cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service',
           'linux-dgx',
           'export PATH=$HOME/.cargo/bin:$PATH && cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service'
       )
 WHERE id = 'ff_git';

UPDATE software_registry
   SET upgrade_playbook = (upgrade_playbook - 'linux')
       || jsonb_build_object(
           'linux-ubuntu',
           'export PATH=$HOME/.cargo/bin:$PATH && cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service',
           'linux-dgx',
           'export PATH=$HOME/.cargo/bin:$PATH && cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service'
       )
 WHERE id = 'forgefleetd_git';
"#;

// ─── V30: self-heal ~/projects/forge-fleet checkout + retire ~/taylorProjects ─
//
// V29 assumed every member already had a valid `~/projects/forge-fleet`
// checkout of `github.com/venkatyarl/forge-fleet`. In practice many nodes
// still had `~/taylorProjects/forge-fleet` from the pre-migration era or
// had an empty `~/projects/` directory. The V29 playbook hung on first
// `git pull` or failed with "fatal: not a git repository."
//
// V30 embeds a self-healing prologue in each playbook command:
//   1. Drops any stale `~/taylorProjects/forge-fleet` (repo moved GitHub
//      account).
//   2. Verifies the existing `~/projects/forge-fleet` checkout's remote
//      matches the expected URL; if not, wipes and re-clones.
//   3. Clones fresh if the checkout is missing.
//   4. Falls through to `git pull --ff-only` + `cargo build`.
// Idempotent; the prologue is smart enough to keep a valid checkout and
// only wipe when the remote is wrong or the checkout is missing.

pub const SCHEMA_V30_PLAYBOOK_SELF_HEAL_REPO: &str = r#"
-- Shared prologue (bash): ensures ~/projects/forge-fleet is a fresh checkout
-- of github.com/venkatyarl/forge-fleet before cargo build runs.
--
-- Algorithm:
--   - Drop stale ~/taylorProjects/forge-fleet (the repo moved GitHub accounts)
--   - If ~/projects/forge-fleet/.git exists, verify remote; if wrong, wipe
--   - Clone if missing
--   - git pull --ff-only

UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
       'macos',
       'rm -rf ~/taylorProjects/forge-fleet 2>/dev/null; mkdir -p ~/projects; if [ -d ~/projects/forge-fleet/.git ]; then ACTUAL=$(cd ~/projects/forge-fleet && git remote get-url origin 2>/dev/null); EXPECTED=https://github.com/venkatyarl/forge-fleet; case "$ACTUAL" in "$EXPECTED"|"$EXPECTED.git") : ;; *) rm -rf ~/projects/forge-fleet ;; esac; fi; [ ! -d ~/projects/forge-fleet/.git ] && git clone https://github.com/venkatyarl/forge-fleet ~/projects/forge-fleet; cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/ff',
       'linux-ubuntu',
       'export PATH=$HOME/.cargo/bin:$PATH; rm -rf ~/taylorProjects/forge-fleet 2>/dev/null; mkdir -p ~/projects; if [ -d ~/projects/forge-fleet/.git ]; then ACTUAL=$(cd ~/projects/forge-fleet && git remote get-url origin 2>/dev/null); EXPECTED=https://github.com/venkatyarl/forge-fleet; case "$ACTUAL" in "$EXPECTED"|"$EXPECTED.git") : ;; *) rm -rf ~/projects/forge-fleet ;; esac; fi; [ ! -d ~/projects/forge-fleet/.git ] && git clone https://github.com/venkatyarl/forge-fleet ~/projects/forge-fleet; cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service',
       'linux-dgx',
       'export PATH=$HOME/.cargo/bin:$PATH; rm -rf ~/taylorProjects/forge-fleet 2>/dev/null; mkdir -p ~/projects; if [ -d ~/projects/forge-fleet/.git ]; then ACTUAL=$(cd ~/projects/forge-fleet && git remote get-url origin 2>/dev/null); EXPECTED=https://github.com/venkatyarl/forge-fleet; case "$ACTUAL" in "$EXPECTED"|"$EXPECTED.git") : ;; *) rm -rf ~/projects/forge-fleet ;; esac; fi; [ ! -d ~/projects/forge-fleet/.git ] && git clone https://github.com/venkatyarl/forge-fleet ~/projects/forge-fleet; cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service'
   )
 WHERE id = 'ff_git';

UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
       'macos',
       'rm -rf ~/taylorProjects/forge-fleet 2>/dev/null; mkdir -p ~/projects; if [ -d ~/projects/forge-fleet/.git ]; then ACTUAL=$(cd ~/projects/forge-fleet && git remote get-url origin 2>/dev/null); EXPECTED=https://github.com/venkatyarl/forge-fleet; case "$ACTUAL" in "$EXPECTED"|"$EXPECTED.git") : ;; *) rm -rf ~/projects/forge-fleet ;; esac; fi; [ ! -d ~/projects/forge-fleet/.git ] && git clone https://github.com/venkatyarl/forge-fleet ~/projects/forge-fleet; cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && codesign --force --sign - ~/.local/bin/forgefleetd && launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd',
       'linux-ubuntu',
       'export PATH=$HOME/.cargo/bin:$PATH; rm -rf ~/taylorProjects/forge-fleet 2>/dev/null; mkdir -p ~/projects; if [ -d ~/projects/forge-fleet/.git ]; then ACTUAL=$(cd ~/projects/forge-fleet && git remote get-url origin 2>/dev/null); EXPECTED=https://github.com/venkatyarl/forge-fleet; case "$ACTUAL" in "$EXPECTED"|"$EXPECTED.git") : ;; *) rm -rf ~/projects/forge-fleet ;; esac; fi; [ ! -d ~/projects/forge-fleet/.git ] && git clone https://github.com/venkatyarl/forge-fleet ~/projects/forge-fleet; cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service',
       'linux-dgx',
       'export PATH=$HOME/.cargo/bin:$PATH; rm -rf ~/taylorProjects/forge-fleet 2>/dev/null; mkdir -p ~/projects; if [ -d ~/projects/forge-fleet/.git ]; then ACTUAL=$(cd ~/projects/forge-fleet && git remote get-url origin 2>/dev/null); EXPECTED=https://github.com/venkatyarl/forge-fleet; case "$ACTUAL" in "$EXPECTED"|"$EXPECTED.git") : ;; *) rm -rf ~/projects/forge-fleet ;; esac; fi; [ ! -d ~/projects/forge-fleet/.git ] && git clone https://github.com/venkatyarl/forge-fleet ~/projects/forge-fleet; cd ~/projects/forge-fleet && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service'
   )
 WHERE id = 'forgefleetd_git';
"#;

// ─── V31: source_tree_path column + template playbook ─────────────────────
//
// Move the per-computer source-tree location into its own column so the
// upgrade playbook becomes a clean template with no embedded one-time
// migration logic. `resolve_upgrade_plans` substitutes `{{source_tree_path}}`
// per-target at dispatch time. Canonical defaults:
//   Taylor (dev workstation) → ~/projects/forge-fleet
//   All other members        → ~/.forgefleet/sub-agent-0/forge-fleet
// `~/taylorProjects` is retired; the runtime relocation itself is a
// separate one-shot operator-run migration (`ff fleet migrate-source-trees`).
pub const SCHEMA_V31_SOURCE_TREE_PATH: &str = r#"
-- Track where each computer's forge-fleet checkout lives. Default
-- differs by role: leader (Taylor) develops in ~/projects/forge-fleet;
-- non-leader members clone into their sub-agent-0 workspace.

ALTER TABLE computers ADD COLUMN IF NOT EXISTS source_tree_path TEXT;

-- Backfill: Taylor → ~/projects; all others → ~/.forgefleet/sub-agent-0.
UPDATE computers
   SET source_tree_path = '~/projects/forge-fleet'
 WHERE LOWER(name) = 'taylor' AND source_tree_path IS NULL;

UPDATE computers
   SET source_tree_path = '~/.forgefleet/sub-agent-0/forge-fleet'
 WHERE LOWER(name) <> 'taylor' AND source_tree_path IS NULL;

-- Replace V30's embedded-migration playbook with a clean template-based
-- one. `{{source_tree_path}}` is substituted per-target at dispatch time
-- by resolve_upgrade_plans.
UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
       'macos',
       'mkdir -p "$(dirname {{source_tree_path}})" && [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}" && cd "{{source_tree_path}}" && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/ff',
       'linux-ubuntu',
       'export PATH=$HOME/.cargo/bin:$PATH && mkdir -p "$(dirname {{source_tree_path}})" && [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}" && cd "{{source_tree_path}}" && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service',
       'linux-dgx',
       'export PATH=$HOME/.cargo/bin:$PATH && mkdir -p "$(dirname {{source_tree_path}})" && [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}" && cd "{{source_tree_path}}" && git pull --ff-only && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service'
   )
 WHERE id = 'ff_git';

UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
       'macos',
       'mkdir -p "$(dirname {{source_tree_path}})" && [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}" && cd "{{source_tree_path}}" && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && codesign --force --sign - ~/.local/bin/forgefleetd && launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd',
       'linux-ubuntu',
       'export PATH=$HOME/.cargo/bin:$PATH && mkdir -p "$(dirname {{source_tree_path}})" && [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}" && cd "{{source_tree_path}}" && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service',
       'linux-dgx',
       'export PATH=$HOME/.cargo/bin:$PATH && mkdir -p "$(dirname {{source_tree_path}})" && [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}" && cd "{{source_tree_path}}" && git pull --ff-only && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service'
   )
 WHERE id = 'forgefleetd_git';
"#;

// ─── V32: playbook production bug-fixes surfaced by the 05:58 UTC auto-run ──
//
// V31 shipped the template playbook but repeated V30's three production bugs
// (plus a fourth I hadn't yet caught). The 05:58 autonomous tick surfaced
// all four on real fleet nodes:
//
//   (a) `git pull --ff-only: Cannot fast-forward to multiple branches`
//       on sophie (forgefleetd_git). Stray remote-tracking refs collide.
//       Fix: replace with `git fetch origin main && git reset --hard
//       origin/main` — idempotent, no ref collisions.
//
//   (b) `Failed to connect to bus: No medium found` on veronica (ff_git).
//       systemctl --user needs XDG_RUNTIME_DIR to reach the session bus
//       when invoked from a non-interactive shell. Fix: export
//       XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}".
//
//   (c) `sh: cargo: command not found` on ace (forgefleetd_git — macOS).
//       defer-worker shell on macOS doesn't inherit ~/.cargo/bin. V31 added
//       PATH export for Linux but not macOS. Fix: add to macos entries too.
//
//   (d) (Discovered by V31 agent as a predicted-next-gap.) Tildes don't
//       expand inside double-quoted strings. `cd "~/..."` fails. Fixed in
//       auto_upgrade.rs substitution — `~/` → `$HOME/` at dispatch time. No
//       DB change needed; included in the same logical commit.

pub const SCHEMA_V32_PLAYBOOK_BUGFIXES: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
       'macos',
       'export PATH="$HOME/.cargo/bin:$PATH" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/ff',
       'linux-ubuntu',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service',
       'linux-dgx',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && systemctl --user restart forgefleet-daemon.service'
   )
 WHERE id = 'ff_git';

UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
       'macos',
       'export PATH="$HOME/.cargo/bin:$PATH" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && codesign --force --sign - ~/.local/bin/forgefleetd && launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd',
       'linux-ubuntu',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service',
       'linux-dgx',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && systemctl --user restart forgefleet-node.service'
   )
 WHERE id = 'forgefleetd_git';
"#;

// ─── V33: forgefleet / ForgeFleet CLI aliases (project-name discoverability) ─
//
// External agents (Codex, Claude Code CLI, OpenClaw tool runners, third-party
// automation) often search for a binary by project name. Installing the ff
// binary only as `ff` forces every caller to know the short alias upfront.
// V33 adds the symlink creation step to every ff_git playbook so running
// `ff fleet upgrade ff_git` (or the autonomous tick) materializes both
// `forgefleet` and `ForgeFleet` aliases on every worker.
//
// Also bootstrap-node-template.sh §6 (build step) creates the same symlinks
// on first enrollment so new boxes (Rihanna, Beyonce going forward) get the
// aliases without waiting for an upgrade.

pub const SCHEMA_V33_CLI_ALIASES: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
       'macos',
       'export PATH="$HOME/.cargo/bin:$PATH" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/ff && ln -sf ~/.local/bin/ff ~/.local/bin/forgefleet && ln -sf ~/.local/bin/ff ~/.local/bin/ForgeFleet',
       'linux-ubuntu',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && ln -sf ~/.local/bin/ff ~/.local/bin/forgefleet && ln -sf ~/.local/bin/ff ~/.local/bin/ForgeFleet && systemctl --user restart forgefleet-daemon.service',
       'linux-dgx',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p ff-terminal && install -m 755 target/release/ff ~/.local/bin/ff && ln -sf ~/.local/bin/ff ~/.local/bin/forgefleet && ln -sf ~/.local/bin/ff ~/.local/bin/ForgeFleet && systemctl --user restart forgefleet-daemon.service'
   )
 WHERE id = 'ff_git';
"#;

// ─── V34: retire config/alert_policies.toml → Postgres ──────────────────────
//
// Per the operator's DB-first directive: TOML catalogs are bootstrap-only,
// so runtime alert-policy edits go straight to Postgres. This migration
// idempotently UPSERTs the six canonical policies previously seeded from
// `config/alert_policies.toml` (operator-defined rows are preserved because
// ON CONFLICT (name) DO NOTHING).
//
// UPSERT key is `name` (UNIQUE) — `id` is a generated UUID we don't pin.
pub const SCHEMA_V34_RETIRE_ALERT_POLICIES_TOML: &str = r#"
INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('computer_offline',
   'Computer has been ODOWN for more than 5 minutes',
   'computer_status', 'any_computer', '== ''odown''',
   300, 'critical', 3600, 'telegram', true),

  ('high_cpu',
   'CPU sustained above 90% for 5 minutes',
   'cpu_pct', 'any_computer', '> 90',
   300, 'warning', 1800, 'log', true),

  ('low_disk_space',
   'Free disk space below 20 GB',
   'disk_free_gb', 'any_computer', '< 20',
   600, 'warning', 86400, 'telegram', true),

  ('high_llm_queue',
   'LLM queue depth above 10 requests for 2 minutes',
   'llm_queue_depth', 'any_computer', '> 10',
   120, 'info', 900, 'log', true),

  ('leader_heartbeat_stale',
   'Leader''s Postgres heartbeat older than 60 seconds',
   'leader_heartbeat_age_secs', 'leader_only', '> 60',
   30, 'critical', 600, 'telegram', true),

  ('secret_expiring_soon',
   'A fleet_secret is within 14 days of expiry',
   'secret_expiry_days_remaining', 'leader_only', '< 14',
   60, 'warning', 86400, 'telegram', true)

ON CONFLICT (name) DO NOTHING;
"#;

// ─── V35: retire config/cloud_llm_providers.toml → Postgres ─────────────────
//
// Per the DB-first directive: `cloud_llm_providers` is populated at DB
// migration time rather than from a TOML. Operator edits via SQL or a
// future `ff cloud-llm add` survive re-runs because of
// ON CONFLICT (id) DO NOTHING.
//
// Credentials are NEVER stored here — `secret_key` is a pointer into
// `fleet_secrets` (schema V9).
pub const SCHEMA_V35_RETIRE_CLOUD_LLM_PROVIDERS_TOML: &str = r#"
INSERT INTO cloud_llm_providers
    (id, display_name, base_url, auth_kind, secret_key,
     model_prefix, request_format, enabled)
VALUES
  ('openai',
   'OpenAI (ChatGPT)',
   'https://api.openai.com/v1',
   'api_key', 'cloud.openai.api_key',
   'openai/', 'openai_chat', true),

  ('anthropic',
   'Anthropic (Claude)',
   'https://api.anthropic.com/v1',
   'api_key', 'cloud.anthropic.api_key',
   'claude-', 'anthropic_messages', true),

  ('moonshot',
   'Moonshot (Kimi)',
   'https://api.moonshot.ai/v1',
   'api_key', 'cloud.moonshot.api_key',
   'kimi/', 'openai_chat', true),

  ('google',
   'Google (Gemini)',
   'https://generativelanguage.googleapis.com/v1beta',
   'api_key', 'cloud.google.api_key',
   'gemini/', 'google_generate_content', true),

  ('xai_grok',
   'xAI (Grok)',
   'https://api.x.ai/v1',
   'api_key', 'cloud.xai_grok.api_key',
   'grok/', 'openai_chat', true),

  ('groq',
   'Groq',
   'https://api.groq.com/openai/v1',
   'api_key', 'cloud.groq.api_key',
   'groq/', 'openai_chat', true),

  ('deepseek',
   'DeepSeek',
   'https://api.deepseek.com/v1',
   'api_key', 'cloud.deepseek.api_key',
   'deepseek/', 'openai_chat', true),

  ('mistral',
   'Mistral',
   'https://api.mistral.ai/v1',
   'api_key', 'cloud.mistral.api_key',
   'mistral/', 'openai_chat', true),

  ('fireworks',
   'Fireworks AI',
   'https://api.fireworks.ai/inference/v1',
   'api_key', 'cloud.fireworks.api_key',
   'fireworks/', 'openai_chat', true),

  ('together',
   'Together AI',
   'https://api.together.xyz/v1',
   'api_key', 'cloud.together.api_key',
   'together/', 'openai_chat', true),

  ('perplexity',
   'Perplexity',
   'https://api.perplexity.ai',
   'api_key', 'cloud.perplexity.api_key',
   'perplexity/', 'openai_chat', true),

  ('openrouter',
   'OpenRouter (aggregator)',
   'https://openrouter.ai/api/v1',
   'api_key', 'cloud.openrouter.api_key',
   'openrouter/', 'openai_chat', true),

  -- Cohere's v2 chat format is non-OpenAI-shaped. Kept disabled until the
  -- cohere_chat_v2 translator lands in crates/ff-gateway/src/cloud_llm.rs.
  ('cohere',
   'Cohere',
   'https://api.cohere.com/v2',
   'api_key', 'cloud.cohere.api_key',
   'cohere/', 'cohere_chat_v2', false)

ON CONFLICT (id) DO NOTHING;
"#;

// ─── V36: retire config/task_coverage.toml → Postgres ───────────────────────
//
// Per the DB-first directive: runtime task-coverage edits go straight to
// Postgres. This migration idempotently UPSERTs the seven canonical task
// coverage requirements previously seeded from `config/task_coverage.toml`.
// Operator edits survive because ON CONFLICT (task) DO NOTHING.
//
// The CoverageGuard (ff-agent::coverage_guard) reads `fleet_task_coverage`
// on every tick; nothing else changes.
pub const SCHEMA_V36_RETIRE_TASK_COVERAGE_TOML: &str = r#"
INSERT INTO fleet_task_coverage
    (task, min_models_loaded, preferred_model_ids, priority, alias)
VALUES
  ('text-generation',
   1, '[]'::jsonb, 'critical', 'general'),

  ('code',
   1, '[]'::jsonb, 'critical', 'coder'),

  ('feature-extraction',
   1,
   '["bge-large-en-v1.5","qwen3-embedding-8b"]'::jsonb,
   'normal', NULL),

  ('automatic-speech-recognition',
   1, '[]'::jsonb, 'normal', 'audio'),

  ('image-text-to-text',
   1, '[]'::jsonb, 'nice-to-have', 'multimodal'),

  ('chain-of-thought',
   1,
   '["qwen3.5-35b-a3b-4bit-mlx"]'::jsonb,
   'normal', 'thinking'),

  ('code-generation',
   1,
   '["qwen3-coder-30b-a3b","qwen3.6-35b-a3b"]'::jsonb,
   'normal', 'code')

ON CONFLICT (task) DO NOTHING;
"#;
