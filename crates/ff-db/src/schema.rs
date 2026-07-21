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
    worker_name       TEXT,                             -- which node handles this session
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
    worker_name       TEXT                               -- where it happened
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
CREATE TABLE IF NOT EXISTS fleet_worker_runtime (
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
    ON fleet_worker_runtime(hostname);
CREATE INDEX IF NOT EXISTS idx_fleet_runtime_heartbeat
    ON fleet_worker_runtime(last_heartbeat);
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
CREATE TABLE IF NOT EXISTS fleet_workers (
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
    worker_name       TEXT NOT NULL REFERENCES fleet_workers(name),
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
    UNIQUE(worker_name, slug)
);

CREATE INDEX IF NOT EXISTS idx_fleet_models_node ON fleet_models(worker_name);

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
-- fleet_workers for sub-agent fan-out, GitHub identity, and installed-tool
-- version tracking. See plan: gentle-questing-valley.md §3–§3h for design.

-- SSH public keys per node. Separate from fleet_workers so we can stash both
-- the daemon user's pubkey AND the machine's host keys (multiple per node).
CREATE TABLE IF NOT EXISTS fleet_worker_ssh_keys (
    worker_name    TEXT NOT NULL REFERENCES fleet_workers(name) ON DELETE CASCADE,
    key_purpose  TEXT NOT NULL,             -- 'user' | 'host'
    public_key   TEXT NOT NULL,             -- full OpenSSH format line
    key_type     TEXT NOT NULL,             -- 'ed25519' | 'rsa' | 'ecdsa'
    fingerprint  TEXT NOT NULL,             -- sha256:... from ssh-keygen -lf
    added_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (worker_name, fingerprint)
);
CREATE INDEX IF NOT EXISTS idx_ssh_keys_node_purpose
    ON fleet_worker_ssh_keys (worker_name, key_purpose);

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

-- Extend fleet_workers for onboarding features:
--   sub_agent_count — how many concurrent worker slots this node serves
--   gh_account       — which GitHub identity this node is authenticated against
--   tooling          — JSONB map of {tool: {current, latest, checked_at}}
ALTER TABLE fleet_workers
    ADD COLUMN IF NOT EXISTS sub_agent_count INT  NOT NULL DEFAULT 1;
ALTER TABLE fleet_workers
    ADD COLUMN IF NOT EXISTS gh_account      TEXT;
ALTER TABLE fleet_workers
    ADD COLUMN IF NOT EXISTS tooling         JSONB NOT NULL DEFAULT '{}';
"#;

pub const SCHEMA_V11_MODEL_LIFECYCLE: &str = r#"
-- ─── Model Lifecycle (catalog / library / deployments / jobs) ─────────────
-- Splits the old `fleet_models` concept into:
--   catalog      = what we *can* download (curated + dynamic)
--   library      = what's on disk per node (inventory)
--   deployments  = what's running per node right now (processes)
--   jobs         = in-flight downloads/deletions/swaps (progress tracking)

-- Add a runtime column to fleet_workers if it doesn't already exist.
-- Values: "llama.cpp" | "mlx" | "vllm" | "unknown"
ALTER TABLE fleet_workers ADD COLUMN IF NOT EXISTS runtime TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE fleet_workers ADD COLUMN IF NOT EXISTS models_dir TEXT NOT NULL DEFAULT '~/models';
ALTER TABLE fleet_workers ADD COLUMN IF NOT EXISTS disk_quota_pct INT NOT NULL DEFAULT 80;

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
    worker_name       TEXT NOT NULL REFERENCES fleet_workers(name) ON DELETE CASCADE,
    catalog_id      TEXT NOT NULL,                           -- may reference fleet_model_catalog.id
    runtime         TEXT NOT NULL,                           -- 'llama.cpp' | 'mlx' | 'vllm'
    quant           TEXT,                                    -- e.g. 'Q4_K_M' or '4bit'
    file_path       TEXT NOT NULL,                           -- absolute path on node
    size_bytes      BIGINT NOT NULL DEFAULT 0,
    sha256          TEXT,                                    -- nullable; verified on demand
    downloaded_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at    TIMESTAMPTZ,
    source_url      TEXT,                                    -- e.g. hf://repo or local path
    UNIQUE (worker_name, file_path)
);

CREATE INDEX IF NOT EXISTS idx_model_library_node ON fleet_model_library (worker_name);
CREATE INDEX IF NOT EXISTS idx_model_library_catalog ON fleet_model_library (catalog_id);

-- Deployments: currently running llama-server / mlx_lm.server / vllm processes.
CREATE TABLE IF NOT EXISTS fleet_model_deployments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    worker_name       TEXT NOT NULL REFERENCES fleet_workers(name) ON DELETE CASCADE,
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
    UNIQUE (worker_name, port)
);

CREATE INDEX IF NOT EXISTS idx_model_deployments_node ON fleet_model_deployments (worker_name);
CREATE INDEX IF NOT EXISTS idx_model_deployments_health ON fleet_model_deployments (health_status);

-- Jobs: in-flight operations with progress tracking.
-- Kinds: 'download' | 'delete' | 'load' | 'unload' | 'swap' | 'convert' | 'transfer' | 'verify'
CREATE TABLE IF NOT EXISTS fleet_model_jobs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    worker_name       TEXT NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_model_jobs_node_status ON fleet_model_jobs (worker_name, status);
CREATE INDEX IF NOT EXISTS idx_model_jobs_created ON fleet_model_jobs (created_at DESC);

-- Disk usage snapshots: periodic sampling of disk free/used for quota monitoring.
CREATE TABLE IF NOT EXISTS fleet_disk_usage (
    worker_name       TEXT NOT NULL REFERENCES fleet_workers(name) ON DELETE CASCADE,
    sampled_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    models_dir      TEXT NOT NULL,
    total_bytes     BIGINT NOT NULL,
    used_bytes      BIGINT NOT NULL,
    free_bytes      BIGINT NOT NULL,
    models_bytes    BIGINT NOT NULL DEFAULT 0,               -- just the models dir
    PRIMARY KEY (worker_name, sampled_at)
);
CREATE INDEX IF NOT EXISTS idx_disk_usage_latest ON fleet_disk_usage (worker_name, sampled_at DESC);
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
-- Ensure base tasks table exists before altering (fresh Postgres installs).
CREATE TABLE IF NOT EXISTS tasks (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    payload_json  TEXT NOT NULL DEFAULT '{}',
    status        TEXT NOT NULL DEFAULT 'pending',
    assigned_node TEXT,
    priority      BIGINT NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    started_at    TEXT,
    completed_at  TEXT
);

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
    "fleet_worker_runtime",
    "fleet_enrollment_events",
    "memories",
    "sessions",
    "cron_jobs",
    "cron_runs",
    "audit_log",
    "config_kv",
];

/// Postgres baseline migration version for newly created databases.
///
/// `crates/ff-db/src/migrations/v161_bootstrap_baseline.sql` squashes the schema through this version
/// and pre-seeds `_migrations` so the runner treats v161 as already applied
/// on fresh databases.
pub const PG_BASELINE_VERSION: u32 = 161;

pub const SCHEMA_V14_COMPUTERS_AND_PORTFOLIO: &str = r#"
-- ─── V14: Computers as first-class + software registry + model portfolio ──
-- Adds the new data model layer described in
-- /Users/venkat/.claude/plans/we-are-mixing-two-streamed-sky.md
--
-- These tables coexist with the existing fleet_workers / fleet_models tables.
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

CREATE TABLE IF NOT EXISTS fleet_work_items (
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
    ON fleet_work_items(project_id, status);
CREATE INDEX IF NOT EXISTS idx_work_items_assigned
    ON fleet_work_items(assigned_to, status);

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
// repo slug, an Ollama tag (`qwen3-coder-30b`), or a GGUF filename —
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
// `fleet_workers.sub_agent_count` (falls back to cpu_cores/4 on first run).
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

// ─── V70: Add Qwen3.6-35B-A3B to fleet_model_catalog ─────────────────────────
//
// The model was released April 2026 and is already on disk on several fleet
// nodes (james, lily, logan, duncan) but never made it into the catalog.
// Taylor (this machine) still runs Qwen3.5-35B-A3B and needs the catalog
// entry so `ff model download` can pull the MLX variant.
pub const SCHEMA_V70_FLEET_MODEL_CATALOG_QWEN36: &str = r#"
INSERT INTO fleet_model_catalog
    (id, name, family, parameters, tier, description, gated,
     preferred_workloads, variants, updated_at)
VALUES
    ('qwen36-35b-a3b',
     'Qwen3.6-35B-A3B-Instruct',
     'qwen',
     '35B',
     1,
     'Qwen 3.6 35B A3B (MoE, ~3B active). April 2026 release. Straight upgrade over Qwen3.5-35B-A3B: +37% MCPMark, +27% Terminal-Bench, +43% QwenWebBench. Best tool-calling accuracy in fleet.',
     false,
     '["text-generation", "code", "reasoning", "tool_calling"]'::jsonb,
     '[{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3.6-35B-A3B-Instruct-GGUF", "size_gb": 20},
       {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3.6-35B-A3B-4bit", "size_gb": 19}]'::jsonb,
     NOW())
ON CONFLICT (id) DO UPDATE SET
    name = EXCLUDED.name,
    family = EXCLUDED.family,
    parameters = EXCLUDED.parameters,
    tier = EXCLUDED.tier,
    description = EXCLUDED.description,
    gated = EXCLUDED.gated,
    preferred_workloads = EXCLUDED.preferred_workloads,
    variants = EXCLUDED.variants,
    updated_at = NOW();
"#;

// ─── V71: Backfill fleet_model_catalog from legacy model_catalog ───────────
//
// V39 seeded 46 models into `model_catalog` (old comprehensive table).
// V70+ code paths prefer `fleet_model_catalog` (new slim table).
// This migration copies every model_catalog entry that has capability metadata
// into fleet_model_catalog so /v1/fleet/route has a single table to query.
//
// ON CONFLICT DO NOTHING protects any operator edits made directly to
// fleet_model_catalog after V70.
pub const SCHEMA_V71_BACKFILL_FLEET_MODEL_CATALOG: &str = r#"
INSERT INTO fleet_model_catalog
    (id, name, family, parameters, tier, description, gated,
     preferred_workloads, variants, updated_at)
SELECT
    id,
    display_name,
    family,
    COALESCE(metadata->>'parameters', parameter_count),
    COALESCE((metadata->>'tier')::int, 2),
    metadata->>'description',
    COALESCE((metadata->>'gated')::boolean, false),
    COALESCE((metadata->>'preferred_workloads')::jsonb, '[]'::jsonb),
    COALESCE((metadata->>'variants')::jsonb, '[]'::jsonb),
    NOW()
FROM model_catalog
WHERE metadata->>'preferred_workloads' IS NOT NULL
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
// Also bootstrap-computer-template.sh §6 (build step) creates the same symlinks
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

-- Preserve the existing `ff jira` secret-backed configuration contract. Never
-- overwrite operator-managed values or the API token itself.
INSERT INTO fleet_secrets(key,value,description,updated_by) VALUES
  ('jira.hireflow360.base_url','https://hireflow360.atlassian.net','HireFlow360 Jira base URL','migration-v217'),
  ('jira.hireflow360.auth_email','venkat@hireflow360.com','HireFlow360 Jira auth email','migration-v217'),
  ('jira.hireflow360.token_secret_key','hireflow360_jira_api_token','Fleet-secret key containing the Jira API token','migration-v217')
ON CONFLICT (key) DO NOTHING;
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

// ─── V37: retire config/ports.toml → Postgres ───────────────────────────────
//
// Per the DB-first directive: runtime port-registry edits go straight to
// Postgres. This migration idempotently UPSERTs the canonical ports
// previously seeded from `config/ports.toml`. Operator edits survive
// because ON CONFLICT (port) DO NOTHING.
//
// Readers (`pick_llm_port`, etc.) continue to consult `port_registry` as
// before — nothing else changes.
pub const SCHEMA_V37_RETIRE_PORTS_TOML: &str = r#"
INSERT INTO port_registry
    (port, service, kind, description, exposed_on, scope, managed_by, status)
VALUES
  -- ForgeFleet core services -------------------------------------------------
  (51002, 'forgefleetd',      'control_plane',
   'ForgeFleet daemon gateway API + web dashboard',
   'all_members', 'lan', 'launchd/systemd', 'active'),

  (50001, 'mcp_http',         'control_plane',
   'Model Context Protocol HTTP server',
   'all_members', 'lan', 'forgefleetd', 'active'),

  (51100, 'pulse_p2p_tcp',    'coordination',
   'Pulse peer-to-peer TCP fallback (when NATS unreachable)',
   'all_members', 'lan', 'ff daemon', 'planned'),

  -- OpenClaw (agent platform) ------------------------------------------------
  (50000, 'openclaw_gateway', 'control_plane',
   'OpenClaw gateway WebSocket — only the elected leader serves',
   'leader_only', 'lan', 'launchd/systemd', 'active'),

  -- LLM inference servers (dynamic port allocation) --------------------------
  (51001, 'vllm',             'llm_inference',
   'vLLM (or mlx_lm on macOS) — first model on this computer',
   'gpu_members', 'lan', 'manual or ff model load', 'active'),

  (51003, 'vllm_slot_2',      'llm_inference',
   'vLLM / mlx_lm — second loaded model (51001 is first; 51002 is forgefleetd)',
   'gpu_members', 'lan', 'manual or ff model load', 'active'),

  (55000, 'llama_cpp_slot_1', 'llm_inference',
   'llama-server — first model on this computer (primary convention)',
   'all_members_with_gguf', 'lan', 'manual or ff model load', 'active'),

  (55001, 'llama_cpp_slot_2', 'llm_inference',
   'llama-server / mlx_lm.server — second loaded model',
   'all_members_with_gguf', 'lan', 'manual or ff model load', 'active'),

  (55002, 'llama_cpp_slot_3', 'llm_inference',
   'llama-server / mlx_lm.server — third loaded model',
   'all_members_with_gguf', 'lan', 'manual or ff model load', 'active'),

  (11434, 'ollama',           'llm_inference',
   'Ollama — multi-model runtime on a single port',
   'all_members', 'lan', 'ollama systemd/launchd', 'active'),

  -- Data plane (Docker containers, Taylor hosts primary) --------------------
  (55432, 'postgres_primary', 'database',
   'Postgres primary — exposed port 5432 mapped to host 55432',
   'taylor', 'lan', 'docker compose', 'active'),

  (55433, 'postgres_replica', 'database',
   'Postgres replica — future, host 55433 on Marcus',
   'marcus', 'lan', 'docker compose follower', 'planned'),

  (6380, 'redis_primary',     'database',
   'Redis primary — exposed 6379 mapped to 6380',
   'taylor', 'lan', 'docker compose', 'active'),

  (6381, 'redis_replica',     'database',
   'Redis replica — future, on Marcus',
   'marcus', 'lan', 'docker compose follower', 'planned'),

  (26380, 'redis_sentinel',   'coordination',
   'Redis Sentinel — DEPRECATED (Pulse v2 replaces this role)',
   'taylor', 'lan', 'docker compose', 'deprecated'),

  (4222, 'nats_client',       'coordination',
   'NATS client connections — pulse events, agent tasks, KV',
   'nats_cluster_members', 'lan', 'docker compose', 'active'),

  (6222, 'nats_cluster',      'coordination',
   'NATS inter-node cluster communication',
   'nats_cluster_members', 'lan', 'docker compose', 'active'),

  (8222, 'nats_monitoring',   'coordination',
   'NATS HTTP monitoring/admin',
   'nats_cluster_members', 'lan', 'docker compose', 'active'),

  -- System / infrastructure --------------------------------------------------
  (22, 'ssh',                 'system',
   'SSH — key-only auth required',
   'all_members', 'lan', 'OS sshd', 'active')

ON CONFLICT (port) DO NOTHING;
"#;

// ─── V38: retire config/external_tools.toml → Postgres ──────────────────────
//
// Per the DB-first directive: runtime external-tool catalog edits go
// straight to Postgres. This migration idempotently UPSERTs the three
// seeded tools (code-review-graph, context-mode, gh-cli) previously
// read from `config/external_tools.toml`. Operator edits survive because
// ON CONFLICT (id) DO NOTHING.
//
// `latest_version` / `latest_version_at` are owned by the upstream-check
// loop, not this seed — those columns start NULL.
pub const SCHEMA_V38_RETIRE_EXTERNAL_TOOLS_TOML: &str = r#"
INSERT INTO external_tools
    (id, display_name, github_url, kind, install_method,
     install_spec, cli_entrypoint, mcp_server_command, register_as_mcp,
     version_source, upgrade_playbook, intake_source, intake_reference)
VALUES
  ('code-review-graph',
   'Code Review Graph',
   'https://github.com/anthropics/code-review-graph',
   'mcp',
   'cargo_install',
   '{"repo":"anthropics/code-review-graph","bin":"code-review-graph-mcp"}'::jsonb,
   'crg',
   'code-review-graph-mcp --stdio',
   true,
   '{"method":"github_release","repo":"anthropics/code-review-graph"}'::jsonb,
   '{"all":"cargo install --git https://github.com/anthropics/code-review-graph --force"}'::jsonb,
   'direct',
   'https://github.com/anthropics/code-review-graph'),

  ('context-mode',
   'Context Mode',
   'https://github.com/context-mode/context-mode',
   'mcp',
   'npm_global',
   '{"package":"@context-mode/mcp"}'::jsonb,
   'context-mode',
   'context-mode --stdio',
   true,
   '{"method":"github_release","repo":"context-mode/context-mode"}'::jsonb,
   '{"all":"npm install -g @context-mode/mcp@latest"}'::jsonb,
   'direct',
   'https://github.com/context-mode/context-mode'),

  ('gh-cli',
   'GitHub CLI',
   'https://github.com/cli/cli',
   'cli',
   'binary_release',
   '{"repo":"cli/cli","asset_pattern":"gh_*_linux_amd64.tar.gz"}'::jsonb,
   'gh',
   NULL,
   false,
   '{"method":"github_release","repo":"cli/cli"}'::jsonb,
   '{"macos":"brew upgrade gh","linux-ubuntu":"sudo apt-get update && sudo apt-get install -y gh","linux-dgx":"sudo apt-get update && sudo apt-get install -y gh"}'::jsonb,
   'direct',
   'https://github.com/cli/cli')

ON CONFLICT (id) DO NOTHING;
"#;

// ─── V39: retire config/model_catalog.toml → Postgres ───────────────────────
//
// Per the DB-first directive: runtime model-catalog edits go straight to
// Postgres. This migration idempotently UPSERTs the 46 canonical models
// previously seeded from `config/model_catalog.toml`. Operator edits
// survive because ON CONFLICT (id) DO NOTHING — so any scout/benchmark
// loop mutations to `upstream_latest_rev`, `upstream_checked_at`, and
// `benchmark_results` are also preserved (they start NULL on first
// insert and are owned by those loops going forward).
//
// The `metadata` JSONB column carries the TOML-only fields that don't
// have dedicated columns: `parameters`, `tier`, `description`, `gated`,
// `preferred_workloads`, and the full `variants` array (each with
// runtime / quant / hf_repo / size_gb / optional context_window).
pub const SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML: &str = r#"
INSERT INTO model_catalog
    (id, display_name, family, parameter_count, architecture, license,
     tasks, input_modalities, output_modalities, languages,
     upstream_source, upstream_id, release_date, quantization,
     file_size_gb, context_window, recommended_runtime,
     required_gpu_kind, min_vram_gb, cpu_runnable,
     quality_tier, lifecycle_status, replaced_by, retirement_reason,
     retirement_date, added_by, notes, metadata)
VALUES
  ('gemma4-31b-it',
   'Gemma 4 31B Instruct',
   'gemma',
   '31B',
   NULL,
   'gemma',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'google/gemma-4-31b-it',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   20,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "31B", "tier": 2, "description": "Gemma 4 31B instruction-tuned. Strong general reasoning.", "gated": true, "preferred_workloads": ["chat", "reasoning"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/gemma-4-31b-it-GGUF", "size_gb": 19}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/gemma-4-31b-it-4bit", "size_gb": 18}]}'::jsonb),

  ('qwen35-35b-a3b',
   'Qwen3.5-35B-A3B-Instruct',
   'qwen',
   '35B',
   NULL,
   'apache-2.0',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3.5-35B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   22,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "35B", "tier": 2, "description": "Qwen 3.5 35B A3B (MoE, ~3B active). Venkat''s daily-driver on multiple fleet nodes.", "gated": false, "preferred_workloads": ["chat", "code", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3.5-35B-A3B-Instruct-GGUF", "size_gb": 20}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3.5-35B-A3B-Instruct-4bit", "size_gb": 19}]}'::jsonb),

  ('qwen35-9b',
   'Qwen3.5-9B-Instruct',
   'qwen',
   '9B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3.5-9B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   6,
   true,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "9B", "tier": 1, "description": "Qwen 3.5 9B — small fast workhorse.", "gated": false, "preferred_workloads": ["chat", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3.5-9B-Instruct-GGUF", "size_gb": 5}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3.5-9B-Instruct-4bit", "size_gb": 5}]}'::jsonb),

  ('qwen35-397b',
   'Qwen3.5-397B',
   'qwen',
   '397B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3.5-397B',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   240,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "397B", "tier": 4, "description": "Qwen 3.5 397B — frontier-class. Leader-only due to size.", "gated": false, "preferred_workloads": ["reasoning", "long_context"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3.5-397B-GGUF", "size_gb": 227}]}'::jsonb),

  ('qwen3-coder-30b',
   'Qwen3-Coder-30B-A3B-Instruct',
   'qwen',
   '30B',
   NULL,
   'apache-2.0',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-Coder-30B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   20,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "30B", "tier": 2, "description": "Qwen3 MoE coding model (3B active params) — fleet default for code tasks.", "gated": false, "preferred_workloads": ["code", "tool_calling", "reasoning"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-Coder-30B-A3B-Instruct-GGUF", "size_gb": 17.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-Coder-30B-A3B-Instruct-4bit", "size_gb": 17.0}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "Qwen/Qwen3-Coder-30B-A3B-Instruct", "size_gb": 60.0}]}'::jsonb),

  ('qwen3-35b-a3b',
   'Qwen3-35B-A3B-Instruct',
   'qwen',
   '35B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-35B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   22,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "35B", "tier": 2, "description": "Qwen3 general-purpose MoE instruct model with 3B active parameters.", "gated": false, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-35B-A3B-Instruct-GGUF", "size_gb": 20.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-35B-A3B-Instruct-4bit", "size_gb": 20.0}]}'::jsonb),

  ('qwen3-235b',
   'Qwen3-235B-A22B',
   'qwen',
   '235B',
   NULL,
   'apache-2.0',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-235B-A22B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   145,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "235B", "tier": 4, "description": "Qwen3 235B MoE (~22B active).", "gated": false, "preferred_workloads": ["reasoning", "code", "long_context"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-235B-A22B-Instruct-GGUF", "size_gb": 135}]}'::jsonb),

  ('qwen3-70b',
   'Qwen3-70B-Instruct',
   'qwen',
   '70B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-70B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "70B", "tier": 3, "description": "Qwen3 70B dense.", "gated": false, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen3-70B-Instruct-GGUF", "size_gb": 40}]}'::jsonb),

  ('qwen3-72b',
   'Qwen3-72B-Instruct',
   'qwen',
   '72B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-72B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "72B", "tier": 3, "description": "Flagship dense Qwen3 instruct model for top-tier reasoning and chat.", "gated": false, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen3-72B-Instruct-GGUF", "size_gb": 41.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-72B-Instruct-4bit", "size_gb": 41.0}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "Qwen/Qwen3-72B-Instruct", "size_gb": 145.0}]}'::jsonb),

  ('qwen3-14b',
   'Qwen3-14B-Instruct',
   'qwen',
   '14B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-14B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   10,
   true,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "14B", "tier": 2, "description": "Mid-size dense Qwen3 model — good balance of quality and speed.", "gated": false, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen3-14B-Instruct-GGUF", "size_gb": 8.5}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-14B-Instruct-4bit", "size_gb": 8.5}]}'::jsonb),

  ('qwen3-7b',
   'Qwen3-7B-Instruct',
   'qwen',
   '7B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-7B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   6,
   true,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "Fast small Qwen3 for lightweight nodes and quick drafting.", "gated": false, "preferred_workloads": ["chat", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-7B-Instruct-GGUF", "size_gb": 4.5}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-7B-Instruct-4bit", "size_gb": 4.5}]}'::jsonb),

  ('qwen3-omni-7b',
   'Qwen3-Omni-7B',
   'qwen',
   '7B',
   NULL,
   'apache-2.0',
   '["any-to-any", "image-text-to-text", "audio-text-to-text", "video-text-to-text"]'::jsonb,
   '["text", "image", "audio", "video"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-Omni-7B',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   8,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "Multimodal Qwen3 handling text, audio, vision, and video inputs.", "gated": false, "preferred_workloads": ["omni", "vision", "chat"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-Omni-7B-GGUF", "size_gb": 5.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-Omni-7B-4bit", "size_gb": 5.0}]}'::jsonb),

  ('qwen25-coder-32b',
   'Qwen3-Coder-30B-A3B-Instruct',
   'qwen',
   '32B',
   NULL,
   'apache-2.0',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-Coder-30B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   22,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "32B", "tier": 2, "description": "Battle-tested coder model — current workhorse on Marcus/Sophie/Priya.", "gated": false, "preferred_workloads": ["code", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-Coder-30B-A3B-Instruct-GGUF", "size_gb": 19.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-Coder-30B-A3B-Instruct-4bit", "size_gb": 19.0}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "Qwen/Qwen3-Coder-30B-A3B-Instruct", "size_gb": 65.0}]}'::jsonb),

  ('qwen25-72b',
   'Qwen3.6-35B-A3B-Instruct',
   'qwen',
   '72B',
   NULL,
   'qwen',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-Next-80B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "72B", "tier": 3, "description": "Qwen3 flagship — currently running on James for deep reasoning.", "gated": false, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-Next-80B-A3B-Instruct-GGUF", "size_gb": 41.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3.6-35B-A3B-Instruct-4bit", "size_gb": 41.0}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "Qwen/Qwen3-Next-80B-A3B-Instruct", "size_gb": 145.0}]}'::jsonb),

  ('qwen25-coder-7b',
   'Qwen3-Coder-7B-Instruct',
   'qwen',
   '7B',
   NULL,
   'apache-2.0',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-Coder-7B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   6,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "Small coder model for lightweight autocomplete and fast iterations.", "gated": false, "preferred_workloads": ["code", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-Coder-7B-Instruct-GGUF", "size_gb": 4.5}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-Coder-7B-Instruct-4bit", "size_gb": 4.5}]}'::jsonb),

  ('qwen25-vl-7b',
   'Qwen3-VL-8B-Instruct',
   'qwen',
   '7B',
   NULL,
   'apache-2.0',
   '["image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-VL-8B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   8,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "Qwen3 vision-language model with strong OCR and chart understanding.", "gated": false, "preferred_workloads": ["vision", "chat"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen3-VL-8B-Instruct-GGUF", "size_gb": 5.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-VL-8B-Instruct-4bit", "size_gb": 5.0}]}'::jsonb),

  ('qwen25-vl-72b',
   'Qwen3-VL-30B-A3B-Instruct',
   'qwen',
   '72B',
   NULL,
   'qwen',
   '["image-text-to-text", "visual-question-answering", "video-text-to-text"]'::jsonb,
   '["text", "image", "video"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-VL-30B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "72B", "tier": 3, "description": "Large vision-language model for complex multi-image and video reasoning.", "gated": false, "preferred_workloads": ["vision", "reasoning"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen3-VL-30B-A3B-Instruct-GGUF", "size_gb": 41.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-VL-30B-A3B-Instruct-4bit", "size_gb": 41.0}]}'::jsonb),

  ('gemma3-27b',
   'Gemma 3 27B Instruct',
   'gemma',
   '27B',
   NULL,
   'gemma',
   '["text-generation", "image-text-to-text"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'google/gemma-3-27b-it',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   18,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "27B", "tier": 2, "description": "Google''s flagship open Gemma 3 model with long-context and multimodal support.", "gated": true, "preferred_workloads": ["chat", "reasoning", "vision"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/gemma-3-27b-it-GGUF", "size_gb": 16.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/gemma-3-27b-it-4bit", "size_gb": 16.0}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "google/gemma-3-27b-it", "size_gb": 54.0}]}'::jsonb),

  ('gemma3-9b',
   'Gemma 3 9B Instruct',
   'gemma',
   '9B',
   NULL,
   'gemma',
   '["text-generation", "image-text-to-text"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'google/gemma-3-9b-it',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   7,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "9B", "tier": 1, "description": "Mid-small Gemma 3 — efficient chat model with vision input.", "gated": true, "preferred_workloads": ["chat", "vision"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/gemma-3-9b-it-GGUF", "size_gb": 5.5}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/gemma-3-9b-it-4bit", "size_gb": 5.5}]}'::jsonb),

  ('llama31-70b',
   'Llama 3.1 70B Instruct',
   'llama',
   '70B',
   NULL,
   'llama-3.1-community',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "es", "fr", "de", "it", "pt", "hi", "th"]'::jsonb,
   'huggingface',
   'meta-llama/Meta-Llama-3.1-70B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "70B", "tier": 3, "description": "Meta''s flagship open Llama 3.1 instruct with strong general reasoning.", "gated": true, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Meta-Llama-3.1-70B-Instruct-GGUF", "size_gb": 40.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Meta-Llama-3.1-70B-Instruct-4bit", "size_gb": 40.0}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "meta-llama/Meta-Llama-3.1-70B-Instruct", "size_gb": 141.0}]}'::jsonb),

  ('llama31-8b',
   'Llama 3.1 8B Instruct',
   'llama',
   '8B',
   NULL,
   'llama-3.1-community',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "es", "fr", "de", "it", "pt", "hi", "th"]'::jsonb,
   'huggingface',
   'meta-llama/Meta-Llama-3.1-8B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   6,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "8B", "tier": 1, "description": "Lightweight Llama 3.1 — good baseline for small-node chat workloads.", "gated": true, "preferred_workloads": ["chat", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF", "size_gb": 4.9}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Meta-Llama-3.1-8B-Instruct-4bit", "size_gb": 4.9}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "meta-llama/Meta-Llama-3.1-8B-Instruct", "size_gb": 16.0}]}'::jsonb),

  ('llama32-vision-11b',
   'Llama 3.2 11B Vision Instruct',
   'llama',
   '11B',
   NULL,
   'llama-3.2-community',
   '["image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'meta-llama/Llama-3.2-11B-Vision-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   10,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "11B", "tier": 2, "description": "Llama 3.2 vision-enabled model for image+text reasoning.", "gated": true, "preferred_workloads": ["vision", "chat"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Llama-3.2-11B-Vision-Instruct-GGUF", "size_gb": 7.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Llama-3.2-11B-Vision-Instruct-4bit", "size_gb": 7.0}]}'::jsonb),

  ('deepseek-v3',
   'DeepSeek-V3',
   'deepseek',
   '671B',
   NULL,
   'deepseek',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'deepseek-ai/DeepSeek-V3',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   400,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "671B", "tier": 4, "description": "DeepSeek V3 MoE (37B active) — frontier-class reasoning, needs the leader node.", "gated": false, "preferred_workloads": ["reasoning", "chat", "code"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/DeepSeek-V3-GGUF", "size_gb": 380.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/DeepSeek-V3-4bit", "size_gb": 380.0}]}'::jsonb),

  ('deepseek-coder-v2',
   'DeepSeek-Coder-V2-Instruct',
   'deepseek',
   '236B',
   NULL,
   'deepseek',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'deepseek-ai/DeepSeek-Coder-V2-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   150,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "236B", "tier": 4, "description": "DeepSeek Coder V2 MoE (21B active) — top-tier open coder for hard refactors.", "gated": false, "preferred_workloads": ["code", "reasoning"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/DeepSeek-Coder-V2-Instruct-GGUF", "size_gb": 140.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/DeepSeek-Coder-V2-Instruct-4bit", "size_gb": 140.0}]}'::jsonb),

  ('deepseek-coder-v2-lite',
   'DeepSeek-Coder-V2-Lite-Instruct',
   'deepseek',
   '16B',
   NULL,
   'deepseek',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'deepseek-ai/DeepSeek-Coder-V2-Lite-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   12,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "16B", "tier": 2, "description": "Lite variant of DeepSeek Coder V2 (2.4B active) — fits on smaller nodes.", "gated": false, "preferred_workloads": ["code", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/DeepSeek-Coder-V2-Lite-Instruct-GGUF", "size_gb": 10.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/DeepSeek-Coder-V2-Lite-Instruct-4bit", "size_gb": 10.0}]}'::jsonb),

  ('mistral-large-2411',
   'Mistral-Large-Instruct-2411',
   'mistral',
   '123B',
   NULL,
   'mrl',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "fr", "de", "es", "it", "pt", "ja", "ko", "zh"]'::jsonb,
   'huggingface',
   'mistralai/Mistral-Large-Instruct-2411',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   75,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "123B", "tier": 3, "description": "Mistral Large 2 — strong multilingual reasoning and tool use.", "gated": false, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Mistral-Large-Instruct-2411-GGUF", "size_gb": 70.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Mistral-Large-Instruct-2411-4bit", "size_gb": 70.0}]}'::jsonb),

  ('mistral-small-3',
   'Mistral-Small-3-Instruct-2501',
   'mistral',
   '24B',
   NULL,
   'apache-2.0',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "fr", "de", "es", "it", "pt"]'::jsonb,
   'huggingface',
   'mistralai/Mistral-Small-3-Instruct-2501',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   16,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "24B", "tier": 2, "description": "Mistral Small 3 — fast 24B dense model with latency-optimized tool calling.", "gated": false, "preferred_workloads": ["chat", "tool_calling", "code"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Mistral-Small-3-Instruct-2501-GGUF", "size_gb": 14.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Mistral-Small-3-Instruct-2501-4bit", "size_gb": 14.0}]}'::jsonb),

  ('mistral-nemo-12b',
   'Mistral-Nemo-Instruct-2407',
   'mistral',
   '12B',
   NULL,
   'apache-2.0',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "fr", "de", "es", "it", "pt"]'::jsonb,
   'huggingface',
   'mistralai/Mistral-Nemo-Instruct-2407',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   9,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "12B", "tier": 1, "description": "Mistral Nemo — 12B dense with 128k context, built with NVIDIA.", "gated": false, "preferred_workloads": ["chat", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Mistral-Nemo-Instruct-2407-GGUF", "size_gb": 7.5}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Mistral-Nemo-Instruct-2407-4bit", "size_gb": 7.5}]}'::jsonb),

  ('phi-4',
   'Phi-4',
   'phi',
   '14B',
   NULL,
   'mit',
   '["text-generation", "code"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'microsoft/phi-4',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   10,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "14B", "tier": 2, "description": "Microsoft Phi-4 — strong reasoning for its size, great on mid-tier nodes.", "gated": false, "preferred_workloads": ["reasoning", "chat", "code"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/phi-4-GGUF", "size_gb": 8.5}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/phi-4-4bit", "size_gb": 8.5}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "microsoft/phi-4", "size_gb": 29.0}]}'::jsonb),

  ('phi-4-mini',
   'Phi-4-mini-instruct',
   'phi',
   '3.8B',
   NULL,
   'mit',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'microsoft/Phi-4-mini-instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   3,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "3.8B", "tier": 1, "description": "Tiny Phi-4 — fits anywhere, good for edge nodes and fast drafts.", "gated": false, "preferred_workloads": ["chat", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Phi-4-mini-instruct-GGUF", "size_gb": 2.3}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Phi-4-mini-instruct-4bit", "size_gb": 2.3}]}'::jsonb),

  ('llava-1.6-34b',
   'LLaVA v1.6 34B',
   'llama',
   '34B',
   NULL,
   'apache-2.0',
   '["image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'liuhaotian/llava-v1.6-34b',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   22,
   false,
   'legacy',
   'deprecated',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "34B", "tier": 2, "description": "LLaVA 1.6 on Yi-34B base — popular open vision-language model.", "gated": false, "preferred_workloads": ["vision", "chat"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "cjpais/llava-v1.6-34B-gguf", "size_gb": 20.0}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/llava-v1.6-34b-4bit", "size_gb": 20.0}]}'::jsonb),

  ('llava-1.6-mistral-7b',
   'LLaVA v1.6 Mistral 7B',
   'mistral',
   '7B',
   NULL,
   'apache-2.0',
   '["image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'liuhaotian/llava-v1.6-mistral-7b',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   6,
   true,
   'legacy',
   'deprecated',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "Compact LLaVA on Mistral 7B — lightweight multimodal for small nodes.", "gated": false, "preferred_workloads": ["vision", "chat"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "cjpais/llava-1.6-mistral-7b-gguf", "size_gb": 4.4}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/llava-v1.6-mistral-7b-4bit", "size_gb": 4.4}]}'::jsonb),

  ('moss-audio-8b-thinking',
   'MOSS-Audio-8B-Thinking',
   'moss',
   '8B',
   NULL,
   'apache-2.0',
   '["audio-text-to-text", "automatic-speech-recognition"]'::jsonb,
   '["audio", "text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'OpenMOSS-Team/MOSS-Audio-8B-Thinking',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   18,
   false,
   'experimental',
   'candidate',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "8B", "tier": 1, "description": "OpenMOSS audio-input multimodal LLM with chain-of-thought. Speech+text reasoning.", "gated": false, "preferred_workloads": ["audio", "reasoning", "multimodal"], "variants": [{"runtime": "vllm", "quant": "fp16", "hf_repo": "OpenMOSS-Team/MOSS-Audio-8B-Thinking", "size_gb": 16}]}'::jsonb),

  ('qwen25-72b-taxonomy',
   'Qwen3.6-35B-A3B (taxonomy alias)',
   'qwen',
   '72B',
   NULL,
   'qwen',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-Next-80B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "72B", "tier": 3, "description": "Alias registration of Qwen3.6-35B-A3B under taxonomy id per 2026-04-18 spec.", "gated": false, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-Next-80B-A3B-Instruct-GGUF", "size_gb": 41.0, "context_window": 131072}]}'::jsonb),

  ('llama-3.3-70b-instruct',
   'Llama 3.3 70B Instruct',
   'llama',
   '70B',
   NULL,
   'llama-3.3-community',
   '["text-generation"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "es", "fr", "de", "it", "pt", "hi", "th"]'::jsonb,
   'huggingface',
   'meta-llama/Llama-3.3-70B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "70B", "tier": 3, "description": "Meta''s Llama 3.3 70B — refreshed instruct flagship with improved tool use and multilingual coverage.", "gated": true, "preferred_workloads": ["chat", "reasoning", "tool_calling"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Llama-3.3-70B-Instruct-GGUF", "size_gb": 40.0, "context_window": 131072}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Llama-3.3-70B-Instruct-4bit", "size_gb": 40.0, "context_window": 131072}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "meta-llama/Llama-3.3-70B-Instruct", "size_gb": 141.0, "context_window": 131072}]}'::jsonb),

  ('whisper-large-v3',
   'Whisper Large v3',
   'whisper',
   '1.55B',
   NULL,
   'apache-2.0',
   '["automatic-speech-recognition"]'::jsonb,
   '["audio"]'::jsonb,
   '["text"]'::jsonb,
   '["multilingual"]'::jsonb,
   'huggingface',
   'openai/whisper-large-v3',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   4,
   true,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "1.55B", "tier": 1, "description": "OpenAI Whisper Large v3 — best-in-class open ASR for 99 languages.", "gated": false, "preferred_workloads": ["asr", "transcription"], "variants": [{"runtime": "whisper.cpp", "quant": "Q5_0", "hf_repo": "ggerganov/whisper.cpp", "size_gb": 1.1, "context_window": 30}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/whisper-large-v3-mlx", "size_gb": 1.5, "context_window": 30}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "openai/whisper-large-v3", "size_gb": 3.1, "context_window": 30}]}'::jsonb),

  ('kokoro-82m',
   'Kokoro 82M',
   'kokoro',
   '82M',
   NULL,
   'apache-2.0',
   '["text-to-speech"]'::jsonb,
   '["text"]'::jsonb,
   '["audio"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'hexgrad/Kokoro-82M',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   1,
   true,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "82M", "tier": 1, "description": "Hexgrad Kokoro-82M — tiny high-quality English TTS model, runs on CPU.", "gated": false, "preferred_workloads": ["tts", "speech_synthesis"], "variants": [{"runtime": "pytorch", "quant": "fp16", "hf_repo": "hexgrad/Kokoro-82M", "size_gb": 0.35, "context_window": 2048}]}'::jsonb),

  ('bge-large-en-v1.5',
   'BGE Large EN v1.5',
   'bge',
   '335M',
   NULL,
   'mit',
   '["feature-extraction", "sentence-similarity"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'BAAI/bge-large-en-v1.5',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   1,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "335M", "tier": 1, "description": "BAAI bge-large-en-v1.5 — solid English sentence embedding model (1024-dim).", "gated": false, "preferred_workloads": ["embeddings", "retrieval"], "variants": [{"runtime": "pytorch", "quant": "fp16", "hf_repo": "BAAI/bge-large-en-v1.5", "size_gb": 1.3, "context_window": 512}, {"runtime": "llama.cpp", "quant": "Q8_0", "hf_repo": "CompendiumLabs/bge-large-en-v1.5-gguf", "size_gb": 0.4, "context_window": 512}]}'::jsonb),

  ('qwen3-embedding-8b',
   'Qwen3-Embedding-8B',
   'qwen',
   '8B',
   NULL,
   'apache-2.0',
   '["feature-extraction", "sentence-similarity"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh", "es", "fr", "de", "ja", "ko"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-Embedding-8B',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   8,
   true,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "8B", "tier": 1, "description": "Qwen3 8B embedding model — strong multilingual retrieval (4096-dim).", "gated": false, "preferred_workloads": ["embeddings", "retrieval", "multilingual"], "variants": [{"runtime": "pytorch", "quant": "fp16", "hf_repo": "Qwen/Qwen3-Embedding-8B", "size_gb": 16.0, "context_window": 32768}, {"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "Qwen/Qwen3-Embedding-8B-GGUF", "size_gb": 5.0, "context_window": 32768}]}'::jsonb),

  ('bge-reranker-large',
   'BGE Reranker Large',
   'bge',
   '560M',
   NULL,
   'mit',
   '["text-ranking", "text-classification"]'::jsonb,
   '["text"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'BAAI/bge-reranker-large',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   2,
   true,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "560M", "tier": 1, "description": "BAAI bge-reranker-large — cross-encoder reranker for high-precision retrieval.", "gated": false, "preferred_workloads": ["reranking", "retrieval"], "variants": [{"runtime": "pytorch", "quant": "fp16", "hf_repo": "BAAI/bge-reranker-large", "size_gb": 2.2, "context_window": 512}]}'::jsonb),

  ('qwen3-vl-30b-a3b',
   'Qwen3-VL-30B-A3B-Instruct (taxonomy)',
   'qwen',
   '72B',
   NULL,
   'qwen',
   '["image-text-to-text", "visual-question-answering", "video-text-to-text", "document-question-answering"]'::jsonb,
   '["text", "image", "video"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen3-VL-30B-A3B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   45,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "72B", "tier": 3, "description": "Qwen3-VL 30B-A3B — flagship open VL for multi-image/video reasoning and document understanding.", "gated": false, "preferred_workloads": ["vision", "reasoning", "documents"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen3-VL-30B-A3B-Instruct-GGUF", "size_gb": 41.0, "context_window": 131072}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen3-VL-30B-A3B-Instruct-4bit", "size_gb": 41.0, "context_window": 131072}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "Qwen/Qwen3-VL-30B-A3B-Instruct", "size_gb": 145.0, "context_window": 131072}]}'::jsonb),

  ('llama-3.2-vision-90b',
   'Llama 3.2 90B Vision Instruct',
   'llama',
   '90B',
   NULL,
   'llama-3.2-community',
   '["image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'meta-llama/Llama-3.2-90B-Vision-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   55,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "90B", "tier": 3, "description": "Meta''s Llama 3.2 90B Vision — large-scale VL for document/chart/image reasoning.", "gated": true, "preferred_workloads": ["vision", "reasoning"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Llama-3.2-90B-Vision-Instruct-GGUF", "size_gb": 52.0, "context_window": 131072}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Llama-3.2-90B-Vision-Instruct-4bit", "size_gb": 52.0, "context_window": 131072}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "meta-llama/Llama-3.2-90B-Vision-Instruct", "size_gb": 180.0, "context_window": 131072}]}'::jsonb),

  ('stable-diffusion-3.5-large',
   'Stable Diffusion 3.5 Large',
   'stable-diffusion',
   '8.1B',
   NULL,
   'stability-community',
   '["text-to-image"]'::jsonb,
   '["text"]'::jsonb,
   '["image"]'::jsonb,
   '["en"]'::jsonb,
   'huggingface',
   'stabilityai/stable-diffusion-3.5-large',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   12,
   false,
   'flagship',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "8.1B", "tier": 2, "description": "Stability AI SD 3.5 Large — high-quality text-to-image diffusion model.", "gated": true, "preferred_workloads": ["image_generation", "creative"], "variants": [{"runtime": "diffusers", "quant": "fp16", "hf_repo": "stabilityai/stable-diffusion-3.5-large", "size_gb": 17.0, "context_window": 256}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/stable-diffusion-3.5-large-4bit", "size_gb": 5.0, "context_window": 256}]}'::jsonb),

  ('qwen2-vl-7b',
   'Qwen2-VL-7B-Instruct',
   'qwen',
   '7B',
   NULL,
   'apache-2.0',
   '["document-question-answering", "image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen2-VL-7B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   8,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "Qwen2-VL 7B — compact VL that fits anywhere; solid document QA and OCR.", "gated": false, "preferred_workloads": ["vision", "documents", "chat"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen2-VL-7B-Instruct-GGUF", "size_gb": 5.0, "context_window": 32768}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen2-VL-7B-Instruct-4bit", "size_gb": 5.0, "context_window": 32768}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "Qwen/Qwen2-VL-7B-Instruct", "size_gb": 16.0, "context_window": 32768}]}'::jsonb),

  ('qwen2-vl-7b-instruct',
   'Qwen2-VL-7B-Instruct (social-ingest)',
   'qwen',
   '7B',
   NULL,
   'apache-2.0',
   '["image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'Qwen/Qwen2-VL-7B-Instruct',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   8,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "Qwen2-VL 7B — primary vision model for social-media ingest (OCR, tool/URL extraction from images and video frames).", "gated": false, "preferred_workloads": ["vision", "documents", "chat"], "variants": [{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "bartowski/Qwen2-VL-7B-Instruct-GGUF", "size_gb": 5.0, "context_window": 32768}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/Qwen2-VL-7B-Instruct-4bit", "size_gb": 5.0, "context_window": 32768}, {"runtime": "vllm", "quant": "fp16", "hf_repo": "Qwen/Qwen2-VL-7B-Instruct", "size_gb": 16.0, "context_window": 32768}]}'::jsonb),

  ('llava-onevision-qwen2-7b-si',
   'LLaVA-OneVision Qwen2 7B SI',
   'llava',
   '7B',
   NULL,
   'apache-2.0',
   '["image-text-to-text", "visual-question-answering"]'::jsonb,
   '["text", "image"]'::jsonb,
   '["text"]'::jsonb,
   '["en", "zh"]'::jsonb,
   'huggingface',
   'lmms-lab/llava-onevision-qwen2-7b-si',
   NULL,
   NULL,
   NULL,
   NULL,
   '[]'::jsonb,
   NULL,
   8,
   false,
   'standard',
   'active',
   NULL,
   NULL,
   NULL,
   NULL,
   NULL,
   '{"parameters": "7B", "tier": 1, "description": "LLaVA-OneVision 7B (single-image) — fallback vision model for social-ingest when Qwen2-VL isn''t loaded.", "gated": false, "preferred_workloads": ["vision", "chat"], "variants": [{"runtime": "vllm", "quant": "fp16", "hf_repo": "lmms-lab/llava-onevision-qwen2-7b-si", "size_gb": 16.0, "context_window": 32768}, {"runtime": "mlx", "quant": "4bit", "hf_repo": "mlx-community/llava-onevision-qwen2-7b-si-4bit", "size_gb": 5.0, "context_window": 32768}]}'::jsonb)

ON CONFLICT (id) DO NOTHING;
"#;

// ─── V40: agent session provenance on work_outputs (issue #118) ─────────────
//
// Links `work_outputs` rows back to the ff-agent session that produced them,
// and records the set of workspace-relative paths the session modified. Both
// fields are populated by the agent loop / coordinator on completion and are
// then read by `ff agent commit-back <session-id>` to locate the worker + the
// files to stage into a branch + PR.
pub const SCHEMA_V40_AGENT_SESSION_ON_WORK_OUTPUTS: &str = r#"
ALTER TABLE work_outputs
    ADD COLUMN IF NOT EXISTS agent_session_id TEXT,
    ADD COLUMN IF NOT EXISTS modified_files JSONB NOT NULL DEFAULT '[]';

CREATE INDEX IF NOT EXISTS idx_work_outputs_by_session
    ON work_outputs(agent_session_id)
    WHERE agent_session_id IS NOT NULL;
"#;

// ─── V41: per-arch build leader designation (closes #112) ───────────────────
//
// Taylor is macOS aarch64 and can't cross-compile a Linux x86_64 binary
// for Sophie, Marcus, Priya, etc. Today V32's playbook hand-waves this by
// running `cargo build --release` on EACH target — 1m15s × N nodes of
// redundant compile time.
//
// V41 adds a `computers.build_archs` JSONB array marking which arches a
// computer is the canonical builder for. Follow-up PR wires
// auto_upgrade::resolve_upgrade_plans to look up the arch leader, build
// once there, rsync the artifact to every target of that arch.
//
// Backfill:
//   darwin-aarch64 → taylor
//   linux-x86_64   → sophie
//   linux-aarch64  → sia
//
// Operator overrides via `ff fleet set-build-leader --arch … --computer …`.
// The CLI already shipped in commit 7156752f3's `ff-terminal/src/main.rs`
// — this migration adds the column + GIN index it expects.
//
// (The earlier V40 slot was taken by agent_session_on_work_outputs in a
// parallel push; this shifts build leaders to V41.)

pub const SCHEMA_V41_PER_ARCH_BUILD_LEADER: &str = r#"
ALTER TABLE computers ADD COLUMN IF NOT EXISTS build_archs JSONB NOT NULL DEFAULT '[]'::jsonb;

UPDATE computers SET build_archs = '["darwin-aarch64"]'::jsonb
 WHERE LOWER(name) = 'taylor' AND build_archs = '[]'::jsonb;

UPDATE computers SET build_archs = '["linux-x86_64"]'::jsonb
 WHERE LOWER(name) = 'sophie' AND build_archs = '[]'::jsonb;

UPDATE computers SET build_archs = '["linux-aarch64"]'::jsonb
 WHERE LOWER(name) = 'sia' AND build_archs = '[]'::jsonb;

CREATE INDEX IF NOT EXISTS computers_build_archs_idx
  ON computers USING GIN (build_archs);
"#;

// ─── V42: research subsystem — multi-agent research with citations ──────────
//
// Three tables to track a research session end-to-end:
//
//   research_sessions  — the top-level query + synthesized output
//   research_subtasks  — per-sub-question dispatches to fleet LLMs
//   research_findings  — individual citations / facts / sources
//
// A session holds the operator's question, the planner's decomposition, and
// the final synthesized report. Subtasks are children that each run on a
// different fleet LLM in parallel (via MultiAgentOrchestrator). Findings
// are the atomic citations each subtask surfaced (URL, quoted snippet,
// confidence, which subtask produced it) — used for the citation footer
// in the final report AND for learning-from-past-sessions.
//
// Design notes:
//   - session.status: 'planning' → 'dispatching' → 'synthesizing' → 'done' / 'failed'
//   - subtask.status mirrors: 'pending' → 'running' → 'done' / 'failed'
//   - findings carry `confidence` (0.0–1.0) so the synthesizer can weight them
//   - every row has metadata JSONB for future extensions without migrations

pub const SCHEMA_V42_RESEARCH_SUBSYSTEM: &str = r#"
CREATE TABLE IF NOT EXISTS research_sessions (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    query              TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'planning',
    -- Operator-supplied configuration
    depth              INT NOT NULL DEFAULT 3,
    parallel           INT NOT NULL DEFAULT 5,
    output_path        TEXT,
    -- Planner's decomposition, stored raw for auditability
    planner_model      TEXT,
    planner_output     JSONB,
    -- Synthesizer's output
    synth_model        TEXT,
    report_markdown    TEXT,
    -- Timing + provenance
    initiated_by       TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at         TIMESTAMPTZ,
    completed_at       TIMESTAMPTZ,
    duration_ms        BIGINT,
    total_tokens_in    BIGINT NOT NULL DEFAULT 0,
    total_tokens_out   BIGINT NOT NULL DEFAULT 0,
    error              TEXT,
    metadata           JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_research_sessions_by_status
    ON research_sessions(status, created_at DESC);

CREATE TABLE IF NOT EXISTS research_subtasks (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id         UUID NOT NULL REFERENCES research_sessions(id) ON DELETE CASCADE,
    ordinal            INT NOT NULL,
    sub_question       TEXT NOT NULL,
    -- Fleet dispatch info
    assigned_computer  TEXT,
    assigned_endpoint  TEXT,
    assigned_model     TEXT,
    agent_session_id   TEXT,
    status             TEXT NOT NULL DEFAULT 'pending',
    -- Sub-agent output
    output_markdown    TEXT,
    turn_count         INT,
    -- Timing
    started_at         TIMESTAMPTZ,
    completed_at       TIMESTAMPTZ,
    duration_ms        BIGINT,
    tokens_in          BIGINT NOT NULL DEFAULT 0,
    tokens_out         BIGINT NOT NULL DEFAULT 0,
    error              TEXT,
    metadata           JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_research_subtasks_by_session
    ON research_subtasks(session_id, ordinal);
CREATE INDEX IF NOT EXISTS idx_research_subtasks_by_computer
    ON research_subtasks(assigned_computer, status);

CREATE TABLE IF NOT EXISTS research_findings (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id         UUID NOT NULL REFERENCES research_sessions(id) ON DELETE CASCADE,
    subtask_id         UUID REFERENCES research_subtasks(id) ON DELETE SET NULL,
    -- The atomic claim + where it came from
    claim              TEXT NOT NULL,
    source_url         TEXT,
    source_title       TEXT,
    source_snippet     TEXT,
    source_kind        TEXT,   -- 'web' | 'vault' | 'code' | 'mcp' | 'model_memory'
    confidence         FLOAT,  -- 0.0–1.0
    -- Used by the synthesizer for ordering + filtering
    relevance_rank     INT,
    cross_verified     BOOLEAN NOT NULL DEFAULT false,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata           JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_research_findings_by_session
    ON research_findings(session_id, confidence DESC);
CREATE INDEX IF NOT EXISTS idx_research_findings_by_source_url
    ON research_findings(source_url)
    WHERE source_url IS NOT NULL;
"#;

// ============================================================================
// V43 — Multi-host deployment visibility + self-heal coordination foundation
// ============================================================================
//
// Two related concerns in one migration:
//
//   (A) Multi-host deployment visibility
//       `fabric_pairs`     — CX-7 / InfiniBand / RoCE fabric pairings between
//                             computers (e.g. Sia↔Adele via ConnectX-7 200Gb)
//       `llm_clusters`     — multi-host vLLM/SGLang deployments (TP=2, PP, etc)
//                             with replayable `launch_recipe`
//       `shared_volumes`   — NFS/SSHFS exports between paired hosts (e.g.
//                             Sia exporting ~/models to Adele over CX-7)
//
//   (B) Self-heal coordination foundation (per self-heal-coordination.md plan)
//       `fleet_bug_reports`     — workers report panics/bugs they hit
//       `fleet_self_heal_queue` — leader's single-flight fix queue (UNIQUE on
//                                  bug_signature prevents duplicate PRs)
//       `daemon_trust_scores`   — per-daemon × per-tier auto-merge eligibility
//       `self_heal_rollouts`    — canary rollout phases with health status
//
// These enable: (1) Pulse beats reporting fabric topology + ray cluster
// membership so the DB reflects real multi-host state; (2) the leader's
// self_heal_tick dedupe-and-dispatch fix pipeline.
pub const SCHEMA_V43_MULTI_HOST_AND_SELF_HEAL: &str = r#"
-- ─── A. Multi-host deployment visibility ───────────────────────────────────

CREATE TABLE IF NOT EXISTS fabric_pairs (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pair_name                TEXT NOT NULL UNIQUE,          -- "sia-adele" (alphabetical)
    fabric_kind              TEXT NOT NULL,                  -- cx7-200g | cx7-400g | ib-100g | roce-100g | ethernet
    computer_a_id            UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    computer_b_id            UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    a_iface                  TEXT NOT NULL,                  -- enp1s0f0np0
    b_iface                  TEXT NOT NULL,
    a_ip                     TEXT NOT NULL,                  -- 10.42.0.1 (fabric-internal)
    b_ip                     TEXT NOT NULL,                  -- 10.42.0.2
    measured_bandwidth_gbps  DOUBLE PRECISION,               -- set by ff fabric bench
    last_probed_at           TIMESTAMPTZ,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_fabric_pairs_by_pair_name
    ON fabric_pairs(pair_name);
CREATE INDEX IF NOT EXISTS idx_fabric_pairs_by_computer_a
    ON fabric_pairs(computer_a_id);
CREATE INDEX IF NOT EXISTS idx_fabric_pairs_by_computer_b
    ON fabric_pairs(computer_b_id);

CREATE TABLE IF NOT EXISTS llm_clusters (
    id                     TEXT PRIMARY KEY,                 -- "minimax-tp2-sia-adele"
    model_id               TEXT NOT NULL,                     -- references model_catalog(id); no FK because the catalog may lag
    runtime                TEXT NOT NULL,                     -- vllm | sglang | tensorrt-llm | llama.cpp-rpc
    topology               TEXT NOT NULL,                     -- tp | pp | tp+pp | dp
    head_computer_id       UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    worker_computer_ids    JSONB NOT NULL DEFAULT '[]',       -- [uuid, ...]
    fabric_pair_id         UUID REFERENCES fabric_pairs(id),
    ray_head_endpoint      TEXT,                              -- 10.42.0.1:6379
    api_endpoint           TEXT NOT NULL,                     -- http://10.42.0.1:55001
    tensor_parallel_size   INT NOT NULL DEFAULT 1,
    pipeline_parallel_size INT NOT NULL DEFAULT 1,
    launch_recipe          JSONB NOT NULL DEFAULT '{}',       -- full env + docker cmd for replay
    status                 TEXT NOT NULL DEFAULT 'launching', -- launching | healthy | degraded | stopped | failed
    last_health_at         TIMESTAMPTZ,
    launched_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    launched_by            TEXT                               -- "operator" | "ff model serve-tp2"
);

CREATE INDEX IF NOT EXISTS idx_llm_clusters_by_model_status
    ON llm_clusters(model_id, status);
CREATE INDEX IF NOT EXISTS idx_llm_clusters_by_head
    ON llm_clusters(head_computer_id);

CREATE TABLE IF NOT EXISTS shared_volumes (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name               TEXT NOT NULL UNIQUE,                  -- "minimax-vault"
    host_computer_id   UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    export_path        TEXT NOT NULL,                          -- /home/sia/models
    protocol           TEXT NOT NULL,                          -- nfs4 | sshfs | ceph
    fabric_pair_id     UUID REFERENCES fabric_pairs(id),       -- if exported over a private fabric
    mounted_on         JSONB NOT NULL DEFAULT '[]',           -- [{computer_id, mount_path, mounted_at}]
    size_gb            DOUBLE PRECISION,
    purpose            TEXT,                                   -- "models" | "training_data" | "outputs"
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_shared_volumes_by_host
    ON shared_volumes(host_computer_id);

-- ─── B. Self-heal coordination ──────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS fleet_bug_reports (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    bug_signature            TEXT NOT NULL,                    -- sha256(file:line:error_class) truncated
    file_path                TEXT,
    line_number              INT,
    error_class              TEXT,                             -- "panic:str_index" | "cargo:type_mismatch" | "runtime:nccl"
    stack_excerpt            TEXT,
    reporting_computer_id    UUID REFERENCES computers(id) ON DELETE SET NULL,
    reporting_task_id        UUID,                              -- optional link to fleet_tasks (V44)
    reported_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    binary_version           TEXT,                              -- ff --version when it crashed
    tier                     TEXT NOT NULL DEFAULT 'T1'        -- T0|T1|T2|T3 (see plan)
);

CREATE INDEX IF NOT EXISTS idx_fleet_bug_reports_by_sig
    ON fleet_bug_reports(bug_signature, reported_at DESC);
CREATE INDEX IF NOT EXISTS idx_fleet_bug_reports_recent
    ON fleet_bug_reports(reported_at DESC);

CREATE TABLE IF NOT EXISTS fleet_self_heal_queue (
    bug_signature            TEXT PRIMARY KEY,                 -- UNIQUE enforces single-flight
    tier                     TEXT NOT NULL,                    -- T0|T1|T2|T3
    status                   TEXT NOT NULL,                    -- detected|fixing|reviewing|pr_open|merged|rolled_out|verified|failed|escalated
    writer_computer_id       UUID REFERENCES computers(id) ON DELETE SET NULL,
    writer_model             TEXT,
    reviewer_computer_id     UUID REFERENCES computers(id) ON DELETE SET NULL,
    reviewer_model           TEXT,
    reviewer_confidence      DOUBLE PRECISION,
    pr_number                INT,
    branch_name              TEXT,
    fix_commit_sha           TEXT,
    fixed_tag                TEXT,                              -- "v2026.4.22_3"
    attempts                 INT NOT NULL DEFAULT 0,
    last_attempt_at          TIMESTAMPTZ,
    escalated_to_operator_at TIMESTAMPTZ,
    report_count             INT NOT NULL DEFAULT 0,            -- how many daemons hit this
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_fleet_self_heal_queue_by_status
    ON fleet_self_heal_queue(status, tier, created_at);

CREATE TABLE IF NOT EXISTS daemon_trust_scores (
    computer_id              UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    tier                     TEXT NOT NULL,                    -- T0 | T1 | T2
    clean_fixes              INT NOT NULL DEFAULT 0,
    reverted_fixes           INT NOT NULL DEFAULT 0,
    last_incident_at         TIMESTAMPTZ,
    probation_until          TIMESTAMPTZ,
    current_level            TEXT NOT NULL DEFAULT 'operator_approve',
                             -- operator_approve | reviewer_approve | auto_merge
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (computer_id, tier)
);

CREATE TABLE IF NOT EXISTS self_heal_rollouts (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    bug_signature            TEXT NOT NULL REFERENCES fleet_self_heal_queue(bug_signature) ON DELETE CASCADE,
    fixed_tag                TEXT NOT NULL,
    phase                    TEXT NOT NULL,                    -- leader|canary|half|full|rollback
    computer_id              UUID REFERENCES computers(id) ON DELETE SET NULL,
    started_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at             TIMESTAMPTZ,
    health_status            TEXT,                              -- ok|degraded|failed
    rolled_back_at           TIMESTAMPTZ,
    notes                    TEXT
);

CREATE INDEX IF NOT EXISTS idx_self_heal_rollouts_by_sig
    ON self_heal_rollouts(bug_signature, started_at DESC);
"#;

// ============================================================================
// V44 — Fleet task queue (inter-ff coordination)
// ============================================================================
//
// Unified work queue so ff daemons on different computers can delegate,
// handoff, and collaborate on tasks. Replaces scattered subsystems
// (deferred_tasks, research_subtasks, work_items-for-agents) as the
// coordination primitive. Claim protocol uses FOR UPDATE SKIP LOCKED for
// atomic single-flight assignment.
pub const SCHEMA_V44_FLEET_TASKS: &str = r#"
CREATE TABLE IF NOT EXISTS fleet_tasks (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_task_id           UUID REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    task_type                TEXT NOT NULL,
    summary                  TEXT NOT NULL,
    payload                  JSONB NOT NULL DEFAULT '{}',
    priority                 INT NOT NULL DEFAULT 50,
    requires_capability      JSONB NOT NULL DEFAULT '[]',
    preferred_computer_id    UUID REFERENCES computers(id) ON DELETE SET NULL,
    status                   TEXT NOT NULL DEFAULT 'pending',
                             -- pending | claimed | running | completed | failed | handed_off | cancelled | paused
    claimed_by_computer_id   UUID REFERENCES computers(id) ON DELETE SET NULL,
    claimed_at               TIMESTAMPTZ,
    started_at               TIMESTAMPTZ,
    completed_at             TIMESTAMPTZ,
    last_heartbeat_at        TIMESTAMPTZ,
    progress_pct             REAL,
    progress_message         TEXT,
    result                   JSONB,
    error                    TEXT,
    handoff_reason           TEXT,
    handoff_state            JSONB,
    original_computer_id     UUID REFERENCES computers(id) ON DELETE SET NULL,
    handoff_count            INT NOT NULL DEFAULT 0,
    deadline_at              TIMESTAMPTZ,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by_computer_id   UUID REFERENCES computers(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_fleet_tasks_pending
    ON fleet_tasks(priority DESC, created_at ASC)
    WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_by_claimer
    ON fleet_tasks(claimed_by_computer_id, status);
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_by_parent
    ON fleet_tasks(parent_task_id)
    WHERE parent_task_id IS NOT NULL;
"#;

// ─── V45: beat-age alert policies ──────────────────────────────────────────
//
// The existing `computer_offline` policy (V34) fires on `computer_status ==
// 'odown'`, which requires a quorum of peers to CONCUR that the target is
// sdown. On 2026-04-22 all 4 DGX daemons died simultaneously; the remaining
// peers couldn't form a quorum that named any specific DGX as odown, so
// the policy never fired during the 9-hour outage.
//
// `beat_age_secs` is a simpler signal: the age of `computers.last_seen_at`,
// which survives Redis TTL expiry and fires per-computer regardless of
// peer concurrence. The alert_evaluator handler for this metric was added
// in the same commit.
pub const SCHEMA_V45_BEAT_AGE_ALERTS: &str = r#"
INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('member_stale_beat',
   'Pulse beat older than 5 minutes — earlier signal than odown quorum',
   'beat_age_secs', 'any_computer', '> 300',
   300, 'warning', 1800, 'telegram', true),

  ('member_beat_dead',
   'Pulse beat older than 30 minutes — daemon presumed dead',
   'beat_age_secs', 'any_computer', '> 1800',
   60, 'critical', 600, 'telegram', true)

ON CONFLICT (name) DO NOTHING;
"#;

// ─── V46: npm-distributed CLI tools (openclaw, codex, claude-code) ─────────
//
// These three tools are installed via `npm install -g <package>` on every
// fleet member. Before this migration they were tracked manually via direct
// SQL INSERTs during the 2026-04-25 session. Bake them into the schema so
// fresh fleets get the catalog rows automatically.
//
// `version_source.method=npm_registry` makes the auto-upgrade tick query
// https://registry.npmjs.org/<package>/latest hourly. Drift detection then
// fires a fleet-wide upgrade dispatch through deferred_tasks. Critical
// detail in the upgrade_playbook: macOS `npm install -g` (homebrew npm)
// writes to user-owned /opt/homebrew/lib/node_modules and does NOT need
// sudo. Linux `sudo npm install -g` writes to root-owned /usr/lib/...
// and DOES need sudo. Each os_family playbook key handles its own case.
//
// The macOS playbook prepends /opt/homebrew/bin to PATH because
// non-interactive SSH sessions on Mac don't source ~/.zprofile, so the
// vanilla command would fail "npm: command not found" (the ace bug from
// 2026-04-25).
pub const SCHEMA_V46_NPM_CLI_CATALOG: &str = r#"
INSERT INTO software_registry
    (id, display_name, kind, version_source, upgrade_playbook)
VALUES
  ('openclaw', 'OpenClaw (gateway/node)', 'binary',
   '{"method":"npm_registry","package":"openclaw"}'::jsonb,
   '{"macos":"export PATH=/opt/homebrew/bin:$PATH && npm install -g openclaw@latest","linux":"sudo npm install -g openclaw@latest"}'::jsonb),

  ('codex', 'OpenAI Codex CLI', 'binary',
   '{"method":"npm_registry","package":"@openai/codex"}'::jsonb,
   '{"macos":"export PATH=/opt/homebrew/bin:$PATH && npm install -g @openai/codex@latest","linux":"sudo npm install -g @openai/codex@latest"}'::jsonb),

  ('claude-code', 'Claude Code CLI (Anthropic)', 'binary',
   '{"method":"npm_registry","package":"@anthropic-ai/claude-code"}'::jsonb,
   '{"macos":"export PATH=/opt/homebrew/bin:$PATH && npm install -g @anthropic-ai/claude-code@latest","linux":"sudo npm install -g @anthropic-ai/claude-code@latest"}'::jsonb)

ON CONFLICT (id) DO UPDATE SET
  version_source   = EXCLUDED.version_source,
  upgrade_playbook = EXCLUDED.upgrade_playbook;

INSERT INTO external_tools
    (id, display_name, github_url, kind, install_method, install_spec,
     cli_entrypoint, register_as_mcp, version_source, upgrade_playbook,
     intake_source, added_by)
VALUES
  ('openclaw', 'OpenClaw (gateway/node)',
   'https://github.com/openclaw/openclaw', 'cli', 'npm_global',
   '{"package":"openclaw"}'::jsonb,
   'openclaw', false,
   '{"method":"npm_registry","package":"openclaw"}'::jsonb,
   '{"macos":"export PATH=/opt/homebrew/bin:$PATH && npm install -g openclaw@latest","linux":"sudo npm install -g openclaw@latest"}'::jsonb,
   'migration', 'V46'),

  ('codex', 'OpenAI Codex CLI',
   'https://github.com/openai/codex', 'cli', 'npm_global',
   '{"package":"@openai/codex"}'::jsonb,
   'codex', false,
   '{"method":"npm_registry","package":"@openai/codex"}'::jsonb,
   '{"macos":"export PATH=/opt/homebrew/bin:$PATH && npm install -g @openai/codex@latest","linux":"sudo npm install -g @openai/codex@latest"}'::jsonb,
   'migration', 'V46'),

  ('claude-code', 'Claude Code CLI (Anthropic)',
   'https://github.com/anthropics/claude-code', 'cli', 'npm_global',
   '{"package":"@anthropic-ai/claude-code"}'::jsonb,
   'claude', false,
   '{"method":"npm_registry","package":"@anthropic-ai/claude-code"}'::jsonb,
   '{"macos":"export PATH=/opt/homebrew/bin:$PATH && npm install -g @anthropic-ai/claude-code@latest","linux":"sudo npm install -g @anthropic-ai/claude-code@latest"}'::jsonb,
   'migration', 'V46')

ON CONFLICT (id) DO UPDATE SET
  install_method   = EXCLUDED.install_method,
  install_spec     = EXCLUDED.install_spec,
  cli_entrypoint   = EXCLUDED.cli_entrypoint,
  version_source   = EXCLUDED.version_source,
  upgrade_playbook = EXCLUDED.upgrade_playbook;
"#;

// ─── V47: fabric_measurements + docker upstream tracking ──────────────────
//
// Two related additions:
//
// 1. `fabric_measurements` table — stores actual iperf3-measured throughput
//    between fabric pairs (CX-7, Thunderbolt, etc). The Ip struct already
//    carries `link_speed_gbps` as the *claimed* spec; this table stores
//    the *measured* reality. Operator runs `ff fabric benchmark <a> <b>`
//    to populate, runs again periodically (cron weekly) for trend.
//
//    Columns chosen to support: "show me CX-7 sia↔adele bandwidth over
//    time" and "is the Thunderbolt link degrading?".
//
// 2. `docker` row in software_registry: was tracking installed via
//    `cmd` method (run `docker --version`) but had no upstream
//    refresh, so latest_version stayed empty and drift never fired.
//    Switch to `apt_repository` method on Linux + `homebrew_cask` on
//    macOS. The auto_upgrade refresher needs corresponding handlers
//    (added in same commit as this migration).
pub const SCHEMA_V47_FABRIC_MEASUREMENTS_AND_DOCKER: &str = r#"
CREATE TABLE IF NOT EXISTS fabric_measurements (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_a          TEXT NOT NULL,
    node_b          TEXT NOT NULL,
    iface_a         TEXT NOT NULL,
    iface_b         TEXT NOT NULL,
    fabric_kind     TEXT NOT NULL,        -- 'cx7-fabric' | 'tb-fabric' | 'lan'
    direction       TEXT NOT NULL,        -- 'a_to_b' | 'b_to_a' | 'parallel'
    streams         INT NOT NULL DEFAULT 1,
    duration_secs   INT NOT NULL,
    measured_gbps   DOUBLE PRECISION NOT NULL,
    claimed_gbps    INT,                  -- spec / nominal
    retransmits     INT,
    measured_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    measured_by     TEXT,                 -- which computer kicked off the test
    iperf_version   TEXT,
    metadata        JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_fabric_measurements_pair
    ON fabric_measurements(node_a, node_b, measured_at DESC);
CREATE INDEX IF NOT EXISTS idx_fabric_measurements_kind
    ON fabric_measurements(fabric_kind, measured_at DESC);

-- docker: switch from passive cmd-detection to active upstream tracking.
-- apt_repository method queries Docker's APT repo for the latest 'stable'
-- channel version. homebrew_cask method queries the Homebrew API for
-- Docker Desktop. Both are added to auto_upgrade.rs alongside this migration.
UPDATE software_registry SET
    version_source = '{"method":"github_release","repo":"docker/cli"}'::jsonb,
    upgrade_playbook = '{"linux-ubuntu":"sudo apt-get update -qq && sudo apt-get install -y docker-ce docker-ce-cli containerd.io","linux-dgx":"sudo apt-get update -qq && sudo apt-get install -y docker-ce docker-ce-cli containerd.io","macos":"brew upgrade --cask docker"}'::jsonb
 WHERE id = 'docker' AND (version_source->>'method' = 'cmd' OR version_source IS NULL);

-- Add docker to external_tools too so `ff ext install docker --all` works
-- as the canonical interface (parity with openclaw / codex / claude-code).
INSERT INTO external_tools
    (id, display_name, github_url, kind, install_method, install_spec,
     cli_entrypoint, register_as_mcp, version_source, upgrade_playbook,
     intake_source, added_by)
VALUES
  ('docker', 'Docker Engine + CLI',
   'https://github.com/docker/docker-ce', 'cli', 'os_package',
   '{"linux_pkg":"docker-ce","macos_cask":"docker"}'::jsonb,
   'docker', false,
   '{"method":"github_release","repo":"docker/cli"}'::jsonb,
   '{"linux-ubuntu":"sudo apt-get update -qq && sudo apt-get install -y docker-ce docker-ce-cli containerd.io","linux-dgx":"sudo apt-get update -qq && sudo apt-get install -y docker-ce docker-ce-cli containerd.io","macos":"brew upgrade --cask docker"}'::jsonb,
   'migration', 'V47')
ON CONFLICT (id) DO UPDATE SET
  install_method   = EXCLUDED.install_method,
  install_spec     = EXCLUDED.install_spec,
  version_source   = EXCLUDED.version_source,
  upgrade_playbook = EXCLUDED.upgrade_playbook;
"#;

// ─── V48: upgrade playbook restart-unit fix ─────────────────────────────────
//
// V32 added XDG_RUNTIME_DIR to the linux playbooks but kept a single
// `systemctl --user restart <unit>` at the end of each command. The installed
// unit on every linux fleet host is `forgefleetd.service`; the unit names used
// in V32/V33 (`forgefleet-node.service` for forgefleetd_git, `forgefleet-
// daemon.service` for ff_git) are non-existent on most nodes so the restart
// step errored silently and the old daemon kept running. Traced as a
// contributing factor in the 2026-04-22 9-hour outage.
//
// Fix: replace the single restart with the revive.rs fallback chain that
// tries all three known unit names in order. Also re-exports XDG_RUNTIME_DIR
// with the ${:-} form so it's self-contained even if the earlier export was
// on a branch that didn't execute.
//
// Idempotent: the WHERE guard checks for `reset-failed` which only appears
// after this migration has applied.
pub const SCHEMA_V48_UPGRADE_PLAYBOOK_RESTART_FIX: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           jsonb_set(
               upgrade_playbook,
               '{linux-ubuntu}',
               to_jsonb(regexp_replace(
                   upgrade_playbook->>'linux-ubuntu',
                   'systemctl --user restart forgefleet-(daemon|node)\.service$',
                   'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
               ))
           ),
           '{linux-dgx}',
           to_jsonb(regexp_replace(
               upgrade_playbook->>'linux-dgx',
               'systemctl --user restart forgefleet-(daemon|node)\.service$',
               'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
           ))
       )
 WHERE id IN ('ff_git', 'forgefleetd_git')
   AND upgrade_playbook->>'linux-ubuntu' NOT LIKE '%reset-failed%';
"#;

// ─── V49: connectivity_mode + election_eligibility on computers ─────────────
//
// First-step support for fleet members that travel off-LAN (laptops).
//
// `connectivity_mode` (string): the daemon's self-reported connection
// state — `lan_attached` | `roaming` | `island`. Workers off-LAN
// stop publishing pulse beats and write any task work to a local
// SQLite mirror until they're back. NULL == legacy / unknown; treat
// as `lan_attached` for back-compat.
//
// `election_eligibility` (string): `eligible` | `prefer_skip` |
// `never_leader`. Marks members that should never be promoted to fleet
// leader. Aura is the first such member — a laptop dropping off-LAN
// while holding leader would freeze the whole fleet's coordination
// surface (auto-upgrade, model dispatch, alert evaluator). The
// existing leader-election code reads this at candidate-collection
// time and skips `never_leader` rows even if their pulse priority is
// favorable.
pub const SCHEMA_V49_CONNECTIVITY_MODE_AND_ELIGIBILITY: &str = r#"
ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS connectivity_mode TEXT,
    ADD COLUMN IF NOT EXISTS election_eligibility TEXT NOT NULL DEFAULT 'eligible';

ALTER TABLE computers
    ADD CONSTRAINT computers_connectivity_mode_check
    CHECK (connectivity_mode IS NULL
           OR connectivity_mode IN ('lan_attached', 'roaming', 'island'));

ALTER TABLE computers
    ADD CONSTRAINT computers_eligibility_check
    CHECK (election_eligibility IN ('eligible', 'prefer_skip', 'never_leader'));

-- Aura is a laptop today. Until we have heartscale support to track
-- which laptops are LAN-attached vs. away, never let any laptop hold
-- the leader role.
UPDATE computers
   SET election_eligibility = 'never_leader'
 WHERE name = 'aura';
"#;

// ─── V50: seed canonical fleet ports into fleet_secrets ─────────────────────
//
// Even values that are "canonical" (the single number used across the
// fleet) get a single source of truth in the DB rather than a string
// literal in source. Code reads `fleet_secrets WHERE key = 'port.gateway'`
// at startup and panics if missing — so accidentally clearing the row
// is loud, not silent. To change a port operationally, edit one row;
// no recompile.
//
// Seeded values come from reference_canonical_ports.md and are the
// values the rest of the fleet has been using. ON CONFLICT DO NOTHING
// so a future operator override survives migration replays.
pub const SCHEMA_V50_SEED_CANONICAL_PORTS: &str = r#"
INSERT INTO fleet_secrets (key, value, description, updated_by)
VALUES
    ('port.gateway',  '51002', 'ForgeFleet HTTP gateway / dashboard / onboard.sh', 'migration-V50'),
    ('port.openclaw', '50000', 'OpenClaw WebSocket gateway',                       'migration-V50'),
    ('port.postgres', '55432', 'Postgres on the leader',                            'migration-V50'),
    ('port.redis',    '6380',  'Redis on the leader',                               'migration-V50'),
    ('port.nats',     '4222',  'NATS pub/sub on every member',                      'migration-V50'),
    ('port.mcp',      '50001', 'MCP HTTP server on every member',                   'migration-V50')
ON CONFLICT (key) DO NOTHING;
"#;

// ─── V51: idempotent upgrade playbook ───────────────────────────────────────
//
// Rewrites the `linux-ubuntu` and `linux-dgx` playbooks for `forgefleetd_git`
// and `ff_git` to be idempotent. Two changes:
//
// 1. **Skip rebuild when nothing changed.** Previously every wave round did
//    `git reset --hard origin/main && cargo build --release`, which touched
//    every source mtime and forced cargo to rebuild from scratch (~30s per
//    target) even when origin/main hadn't moved. The new playbook checks
//    `git rev-parse HEAD == git rev-parse origin/main` AND that both
//    `target/release/{forgefleetd,ff}` exist; only then skips the
//    fetch+build. On a re-dispatched wave this collapses the per-target
//    work from ~30s to ~3s.
//
// 2. **Install both binaries.** The previous playbook only installed
//    `forgefleetd`, leaving `~/.local/bin/ff` stale. Now installs both.
//
// Why this matters: the wave dispatcher has a known self-kill race (when
// worker A is mid-upgrade for target B, another worker upgrading target A
// restarts A's daemon, killing A's in-flight task — see
// feedback_wave_dispatcher_self_kill_race.md). Shrinking each task's
// duration from ~30s to ~3s shrinks the race window proportionally, so
// re-dispatching converges in 1-2 rounds instead of 4+.
pub const SCHEMA_V51_IDEMPOTENT_UPGRADE_PLAYBOOK: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           jsonb_set(
               upgrade_playbook,
               '{linux-ubuntu}',
               to_jsonb(
                  'export PATH="$HOME/.cargo/bin:$PATH" && '
               || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
               || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
               || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
               || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
               || 'git fetch origin main && '
               || 'NEED_BUILD=1; '
               || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
               || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet; fi && '
               || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
               || 'install -m 755 target/release/ff ~/.local/bin/ff && '
               || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
               || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
               )
           ),
           '{linux-dgx}',
           to_jsonb(
              'export PATH="$HOME/.cargo/bin:$PATH" && '
           || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
           || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
           || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
           || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
           || 'git fetch origin main && '
           || 'NEED_BUILD=1; '
           || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
           || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet; fi && '
           || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
           || 'install -m 755 target/release/ff ~/.local/bin/ff && '
           || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
           || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
           )
       )
 WHERE id IN ('ff_git', 'forgefleetd_git');
"#;

// ─── V52: wait_for_siblings barrier flag + build-only playbook keys ─────────
//
// Two coupled additions that together let the wave dispatcher run a true
// two-phase upgrade graph:
//
// 1. `fleet_tasks.wait_for_siblings BOOLEAN`. A row with this flag set is
//    only claimable when no sibling under the same `parent_task_id` (with
//    `wait_for_siblings = false`, i.e. a non-barrier sibling) is still
//    `pending` or `running`. The TaskRunner checks this in tick_once.
//    Phase-2 tasks naturally barrier on Phase-1 with no extra polling.
//
// 2. New playbook keys `linux-ubuntu-build-only` / `linux-dgx-build-only`
//    on `forgefleetd_git` and `ff_git`. Identical to the existing
//    `linux-ubuntu` / `linux-dgx` playbooks except WITHOUT the trailing
//    systemctl restart. The two-phase dispatcher uses these for Phase 1
//    (build+install on every target, no daemon restart). Phase 2 then
//    issues SSH+restart from the leader, sequentially.
//
// This eliminates the self-kill race documented in
// `feedback_wave_dispatcher_self_kill_race.md`.
pub const SCHEMA_V52_WAIT_FOR_SIBLINGS_BARRIER: &str = r#"
ALTER TABLE fleet_tasks
    ADD COLUMN IF NOT EXISTS wait_for_siblings BOOLEAN NOT NULL DEFAULT false;

UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           jsonb_set(
               upgrade_playbook,
               '{linux-ubuntu-build-only}',
               to_jsonb(
                  'export PATH="$HOME/.cargo/bin:$PATH" && '
               || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
               || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
               || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
               || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
               || 'git fetch origin main && '
               || 'NEED_BUILD=1; '
               || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
               || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet; fi && '
               || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
               || 'install -m 755 target/release/ff ~/.local/bin/ff'
               )
           ),
           '{linux-dgx-build-only}',
           to_jsonb(
              'export PATH="$HOME/.cargo/bin:$PATH" && '
           || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
           || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
           || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
           || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
           || 'git fetch origin main && '
           || 'NEED_BUILD=1; '
           || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
           || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet; fi && '
           || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
           || 'install -m 755 target/release/ff ~/.local/bin/ff'
           )
       )
 WHERE id IN ('ff_git', 'forgefleetd_git');
"#;

// ─── V53: OAuth-subscription + CLI-bridge cloud_llm_providers rows ──────────
//
// Foundation for the multi-LLM CLI integration roadmap (see
// `~/.claude/plans/cosmic-splashing-chipmunk.md`). Adds two new
// `auth_kind` values:
//
//   `oauth_subscription` — ff calls the vendor API directly with a Bearer
//      token harvested from each CLI's local credential file. Token lives
//      in `fleet_secrets` keyed by `secret_key`. Pays from the user's
//      Pro / Plus / Premium+ subscription quota, not pay-per-token.
//
//   `local_bridge` — ff routes to a local HTTP server (bridge daemon)
//      that spawns the CLI per request and translates the JSON to
//      OpenAI's chat-completion shape. base_url is `http://127.0.0.1:51100..51104`.
//
// Routing: `find_for_model` already picks the longest matching prefix.
// The existing api_key `anthropic` row uses prefix `claude-`, which would
// clash with the new oauth row's natural `claude-` prefix. We move the
// api_key row to `anthropic/claude-` so an operator who wants
// pay-per-token billing must opt in explicitly with
// `--model anthropic/claude-haiku-3-5`. Default `--model claude-opus-4-7`
// then routes through OAuth subscription.
//
// `record_usage()` in `crates/ff-gateway/src/cloud_llm.rs:589` already
// captures token counts for any provider returning OpenAI-style `usage`
// — the new oauth rows automatically get billing visibility once
// Layer 1 is wired (PR-B). For oauth_subscription `cost_usd = 0` (call
// is included in subscription) but token counts are still recorded.
pub const SCHEMA_V53_OAUTH_SUBSCRIPTION_PROVIDERS: &str = r#"
-- 1. Disambiguate the existing anthropic api_key row's prefix so the new
--    oauth row can claim `claude-` as its natural prefix.
UPDATE cloud_llm_providers
   SET model_prefix = 'anthropic/claude-'
 WHERE id = 'anthropic'
   AND model_prefix = 'claude-';

-- 2. New OAuth-subscription rows (one per provider).
INSERT INTO cloud_llm_providers
    (id, display_name, base_url, auth_kind, secret_key,
     oauth_token_secret, model_prefix, request_format, enabled)
VALUES
  ('anthropic_oauth',
   'Anthropic Claude (Pro/Max subscription)',
   'https://api.anthropic.com/v1',
   'oauth_subscription', 'anthropic.oauth_token',
   'anthropic.oauth_token', 'claude-', 'anthropic_messages', true),

  ('openai_oauth',
   'OpenAI ChatGPT (Plus/Pro subscription)',
   'https://api.openai.com/v1',
   'oauth_subscription', 'openai.oauth_token',
   'openai.oauth_token', 'gpt-', 'openai_chat', true),

  ('moonshot_oauth',
   'Moonshot Kimi (Pro subscription)',
   'https://api.moonshot.ai/v1',
   'oauth_subscription', 'moonshot.oauth_token',
   'moonshot.oauth_token', 'kimi-', 'openai_chat', true),

  ('xai_oauth',
   'xAI Grok (X Premium+ subscription)',
   'https://api.x.ai/v1',
   'oauth_subscription', 'xai.oauth_token',
   'xai.oauth_token', 'grok-', 'openai_chat', true),

  ('google_oauth',
   'Google Gemini (Advanced subscription)',
   'https://generativelanguage.googleapis.com/v1beta',
   'oauth_subscription', 'google.oauth_token',
   'google.oauth_token', 'gemini-', 'google_generate_content', true)
ON CONFLICT (id) DO NOTHING;

-- 3. Local-bridge rows (one per provider, one port each).
INSERT INTO cloud_llm_providers
    (id, display_name, base_url, auth_kind, secret_key,
     model_prefix, request_format, enabled)
VALUES
  ('claude_cli',
   'Claude Code CLI bridge (local)',
   'http://127.0.0.1:51100',
   'local_bridge', '',
   'claude-cli-', 'openai_chat', true),

  ('codex_cli',
   'OpenAI Codex CLI bridge (local)',
   'http://127.0.0.1:51101',
   'local_bridge', '',
   'codex-cli-', 'openai_chat', true),

  ('kimi_cli',
   'Moonshot Kimi CLI bridge (local)',
   'http://127.0.0.1:51102',
   'local_bridge', '',
   'kimi-cli-', 'openai_chat', true),

  ('gemini_cli',
   'Google Gemini CLI bridge (local)',
   'http://127.0.0.1:51103',
   'local_bridge', '',
   'gemini-cli-', 'openai_chat', true),

  ('grok_cli',
   'xAI Grok CLI bridge (local)',
   'http://127.0.0.1:51104',
   'local_bridge', '',
   'grok-cli-', 'openai_chat', true)
ON CONFLICT (id) DO NOTHING;

-- 4. Append @google/gemini-cli to the npm CLI catalog so the install
--    pipeline picks it up alongside claude-code and codex (V46).
--    Schema V21 columns: id / display_name / kind / version_source /
--    upgrade_playbook / requires_restart / requires_reboot. The
--    npm-registry method + package live inside `version_source` JSONB,
--    matching how V46 seeds `claude-code` and `codex`.
INSERT INTO software_registry
  (id, display_name, kind,
   version_source, upgrade_playbook, requires_restart, requires_reboot)
VALUES
  ('gemini-cli',
   'Google Gemini CLI',
   'binary',
   '{"method":"npm_registry","package":"@google/gemini-cli"}'::jsonb,
   '{"linux-ubuntu":"npm install -g @google/gemini-cli","linux-dgx":"npm install -g @google/gemini-cli","macos":"npm install -g @google/gemini-cli"}'::jsonb,
   false,
   false)
ON CONFLICT (id) DO NOTHING;
"#;

// ─── V54: outcome-driven orchestration foundation ──────────────────────────
//
// Pillar 4 of the multi-LLM CLI integration roadmap. Adds the data
// model for multi-LLM, multi-step sessions where ff orchestrates a
// team of LLMs (planner / coder / reviewer / browser / synthesiser)
// converging on a user-stated outcome.
//
// Three new tables:
//
//   `agent_sessions`  — one row per high-level user goal. The outer
//      task ("fix issue #42", "research X then draft Y").
//   `agent_steps`     — the DAG of substeps. Each row references its
//      parent session and optional `depends_on` (JSONB array of step
//      IDs). The runner picks the next step whose dependencies are
//      satisfied, dispatches via the existing wave dispatcher
//      (fleet_tasks), parses the LLM output, advances state.
//   `agent_roles`     — declared role catalog (planner/coder/reviewer/
//      browser/synthesiser). Each is a `(provider, model, system_prompt)`
//      triple. Sessions declare which roles they use; steps tag which
//      role they expect.
//
// Sessions reference one or more `fleet_tasks` rows for actual
// execution; the step graph layer sits above. Reuses the existing
// `/api/agent/sessions` route for observability — no new namespace.
//
// PR-L (session orchestrator) consumes this schema. PR-N (roles
// catalog seed), PR-O (session_brain memory-sharing), PR-P (consensus)
// build on top.
pub const SCHEMA_V54_AGENT_ORCHESTRATION: &str = r#"
CREATE TABLE IF NOT EXISTS agent_sessions (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    goal                     TEXT NOT NULL,
    -- Free-form team composition: {"planner":"claude-opus-4-7","coder":"gpt-5",…}
    team                     JSONB NOT NULL DEFAULT '{}',
    status                   TEXT NOT NULL DEFAULT 'pending',
                             -- pending | running | succeeded | failed | cancelled
    -- Optional per-session budget cap. Orchestrator checks before each
    -- LLM call; on hit, marks session 'failed' with reason='budget'.
    budget_usd_cap           NUMERIC(10, 2),
    -- Cumulative cost across all steps in this session (rolled up
    -- from cloud_llm_usage rows whose session_id matches).
    cost_usd_so_far          NUMERIC(10, 6) NOT NULL DEFAULT 0,
    -- Final result, when the session reaches a terminal state.
    final_result             JSONB,
    error                    TEXT,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at               TIMESTAMPTZ,
    completed_at             TIMESTAMPTZ,
    created_by               TEXT
);
CREATE INDEX IF NOT EXISTS idx_agent_sessions_pending
    ON agent_sessions(created_at DESC)
    WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_agent_sessions_running
    ON agent_sessions(started_at DESC)
    WHERE status = 'running';

CREATE TABLE IF NOT EXISTS agent_steps (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id               UUID NOT NULL REFERENCES agent_sessions(id) ON DELETE CASCADE,
    -- Human-readable step name ("read issue", "edit src/main.rs", "open PR").
    name                     TEXT NOT NULL,
    -- Which role the orchestrator should dispatch this to. Optional —
    -- when null, the step runs on the session's default role (usually
    -- the planner, which decides which sub-role to use).
    role                     TEXT,
    -- DAG dependencies: array of step IDs that must reach 'completed'
    -- (or 'skipped') before this step is claimable. Empty = no deps.
    depends_on               JSONB NOT NULL DEFAULT '[]',
    -- Conditional branching: SQL-style boolean expression evaluated
    -- against parent step results. Null = unconditional.
    branch_condition         TEXT,
    -- Per-step memory: anything the orchestrator wants to thread
    -- through (intermediate findings, tool-call traces, etc.).
    step_memory              JSONB NOT NULL DEFAULT '{}',
    status                   TEXT NOT NULL DEFAULT 'pending',
                             -- pending | running | completed | failed | skipped
    -- The fleet_tasks row dispatched for this step. Lets the
    -- orchestrator track stdout/stderr without duplicating the
    -- streaming surface.
    fleet_task_id            UUID REFERENCES fleet_tasks(id) ON DELETE SET NULL,
    -- LLM output as parsed by the orchestrator (typically the
    -- structured JSON the role's system prompt asks for).
    result                   JSONB,
    -- Path to a screenshot artifact when the step was a browser/
    -- computer-use action (Pillar 1).
    screenshot_path          TEXT,
    -- Free-form retry counter — orchestrator may retry a step with
    -- a refined prompt before giving up.
    retry_count              INT NOT NULL DEFAULT 0,
    error                    TEXT,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at               TIMESTAMPTZ,
    completed_at             TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_agent_steps_pending
    ON agent_steps(session_id, created_at)
    WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_agent_steps_by_session
    ON agent_steps(session_id);

CREATE TABLE IF NOT EXISTS agent_roles (
    name                     TEXT PRIMARY KEY,
                             -- 'planner' | 'coder' | 'reviewer' | 'browser' | 'synthesiser'
    description              TEXT NOT NULL,
    -- Default model for this role (e.g. 'claude-opus-4-7'). The
    -- session's `team` JSONB can override per-session.
    default_model            TEXT NOT NULL,
    -- System prompt prepended to every dispatch for this role.
    system_prompt            TEXT NOT NULL,
    -- Default `requires_capability` set on dispatched fleet_tasks
    -- (e.g. ["claude"] for the coder role). Lets capability-tagged
    -- workers (PR-A3) pick up role-specific work.
    requires_capability      JSONB NOT NULL DEFAULT '[]',
    enabled                  BOOLEAN NOT NULL DEFAULT true,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed the canonical 5 roles. Operator can edit/disable via SQL or
-- a future `ff agent-role set` verb. The defaults map to the
-- multi-LLM strategic-routing table in the plan:
--   planner       → Claude Opus / GPT-5 (deep reasoning)
--   coder         → Codex / Claude (code editing + tool use)
--   reviewer      → Gemini Pro (different lens, catches blind spots)
--   browser       → Claude Computer Use (browser automation via PR-H)
--   synthesiser   → Gemini (final-report write-up)
INSERT INTO agent_roles
    (name, description, default_model, system_prompt, requires_capability)
VALUES
  ('planner',
   'Decomposes the user goal into a step DAG. Produces a JSON plan that the orchestrator emits as agent_steps rows.',
   'claude-opus-4-7',
   'You are the planner role in a ForgeFleet multi-LLM team. Read the user goal carefully. Output a JSON plan with steps, dependencies, and the role each step should be dispatched to. Be specific.',
   '[]'::jsonb),

  ('coder',
   'Edits code, runs tests, commits. Uses the assigned role''s native CLI agent loop with tool calling.',
   'claude-opus-4-7',
   'You are the coder role. Make the smallest correct change that satisfies the step. Do not refactor surrounding code unless asked. Run the project test suite if present and report results.',
   '["claude"]'::jsonb),

  ('reviewer',
   'Reviews work the coder produced. Flags bugs, missing edge cases, style issues. Independent lens.',
   'gemini-2.5-pro',
   'You are the reviewer role. Critically review the work the coder produced. Report concrete issues (file:line where possible). If the work is acceptable, say so explicitly.',
   '["gemini"]'::jsonb),

  ('browser',
   'Drives a headless browser via the computer_use MCP tool (PR-H). Web research, form fills, screenshot reading.',
   'claude-opus-4-7',
   'You are the browser role. Use the computer_use MCP tool to perform web tasks. Take screenshots before each click, narrate what you see, never click destructive UI without explicit user confirmation.',
   '["claude","browser"]'::jsonb),

  ('synthesiser',
   'Writes the final user-facing summary from all step results. Cited, structured, readable.',
   'gemini-2.5-pro',
   'You are the synthesiser role. Combine the team''s step results into a single user-facing markdown report. Cite which step (and which role) produced each finding.',
   '[]'::jsonb)
ON CONFLICT (name) DO NOTHING;
"#;

// ─── V55: session_brain — per-session shared memory across roles ───────────
//
// Pillar 4 / PR-O: roles within a session need a shared scratch surface.
// step_memory (V54) is per-step; this table is per-session, so the
// reviewer can read what the planner wrote, the synthesiser can read
// what the coder produced, etc.
//
// On session finalisation, session_brain is mirrored to the Obsidian
// vault under `Inbox/sessions/<session-id>/<key>.md` (per the V13
// design — AI writes only to Inbox, operator promotes from there).
// The mirror is a follow-up PR; this migration just lays the table.
//
// Schema is intentionally simple — operator can always query directly
// for bespoke needs. JSONB values let roles pass structured findings
// across without re-stringifying.
pub const SCHEMA_V55_SESSION_BRAIN: &str = r#"
CREATE TABLE IF NOT EXISTS session_brain (
    session_id        UUID NOT NULL REFERENCES agent_sessions(id) ON DELETE CASCADE,
    key               TEXT NOT NULL,
    value             JSONB NOT NULL,
    written_by_role   TEXT,
    written_by_step   UUID REFERENCES agent_steps(id) ON DELETE SET NULL,
    written_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (session_id, key)
);
CREATE INDEX IF NOT EXISTS idx_session_brain_by_step
    ON session_brain(written_by_step)
    WHERE written_by_step IS NOT NULL;
"#;

// ─── V56: retire last 2 TOMLs + ensure CLI is rebuilt on workers ───────────
//
// Three coupled cleanups:
//
// 1. `config/scout_denylist.toml` → `model_scout_denylist` table. Single
//    column `model_id TEXT PRIMARY KEY` (case-folded). Empty by default;
//    operators add via `ff model reject <id>`.
//
// 2. `config/projects.toml` → seed the existing `projects` table directly
//    (V15). The TOML can then be deleted; runtime additions already go to
//    Postgres per the DB-first catalog rule.
//
// 3. Worker `ff` CLI binary stayed stale across upgrades because the V51
//    playbook only built `-p forge-fleet` (the daemon package). Switch to
//    `-p forge-fleet -p ff-terminal` so `target/release/ff` is rebuilt
//    alongside `forgefleetd` on every upgrade tick.
pub const SCHEMA_V56_RETIRE_LAST_TOMLS_AND_CLI_BUILD: &str = r#"
-- Drop the orphan V11 software_registry rows (display_name 'ForgeFleet
-- CLI (ff)' / 'ForgeFleet Daemon (forgefleetd)') if they're still around.
-- The active rollout uses 'ff_git' / 'forgefleetd_git'.
DELETE FROM software_registry WHERE id IN ('ff', 'forgefleetd');

-- Model scout denylist — replaces config/scout_denylist.toml.
CREATE TABLE IF NOT EXISTS model_scout_denylist (
    model_id      TEXT PRIMARY KEY,
    reason        TEXT,
    added_by      TEXT,
    added_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed projects directly from the retired TOML.
INSERT INTO projects (id, display_name, repo_url, default_branch, compose_file, target_computers, status)
VALUES
  ('forge-fleet', 'ForgeFleet',  'https://github.com/venkatyarl/forge-fleet',  'main', 'deploy/docker-compose.yml', '["taylor","marcus","sophie","priya","james","ace"]'::jsonb, 'active'),
  ('hireflow360', 'HireFlow360', 'https://github.com/venkatyarl/hireflow360', 'main', 'docker-compose.yml',         '["taylor"]'::jsonb,                                          'active'),
  ('auraos',      'AuraOS',      'https://github.com/venkatyarl/auraos',      'main', NULL,                          '["taylor"]'::jsonb,                                          'active')
ON CONFLICT (id) DO UPDATE SET
  display_name     = EXCLUDED.display_name,
  repo_url         = EXCLUDED.repo_url,
  default_branch   = EXCLUDED.default_branch,
  compose_file     = EXCLUDED.compose_file,
  target_computers = EXCLUDED.target_computers,
  status           = EXCLUDED.status;

-- Build both the daemon AND the CLI on every upgrade tick. Without this,
-- target/release/ff is whatever was built last time ff-terminal was
-- compiled — typically stale by weeks.
UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           jsonb_set(
               jsonb_set(
                   jsonb_set(
                       upgrade_playbook,
                       '{linux-ubuntu}',
                       to_jsonb(
                          'export PATH="$HOME/.cargo/bin:$PATH" && '
                       || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                       || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                       || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                       || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                       || 'git fetch origin main && '
                       || 'NEED_BUILD=1; '
                       || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
                       || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal; fi && '
                       || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                       || 'install -m 755 target/release/ff ~/.local/bin/ff && '
                       || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
                       || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
                       )
                   ),
                   '{linux-dgx}',
                   to_jsonb(
                      'export PATH="$HOME/.cargo/bin:$PATH" && '
                   || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                   || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                   || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                   || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                   || 'git fetch origin main && '
                   || 'NEED_BUILD=1; '
                   || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
                   || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal; fi && '
                   || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                   || 'install -m 755 target/release/ff ~/.local/bin/ff && '
                   || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
                   || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
                   )
               ),
               '{linux-ubuntu-build-only}',
               to_jsonb(
                  'export PATH="$HOME/.cargo/bin:$PATH" && '
               || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
               || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
               || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
               || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
               || 'git fetch origin main && '
               || 'NEED_BUILD=1; '
               || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
               || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal; fi && '
               || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
               || 'install -m 755 target/release/ff ~/.local/bin/ff'
               )
           ),
           '{linux-dgx-build-only}',
           to_jsonb(
              'export PATH="$HOME/.cargo/bin:$PATH" && '
           || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
           || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
           || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
           || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
           || 'git fetch origin main && '
           || 'NEED_BUILD=1; '
           || 'if [ "$(git rev-parse HEAD 2>/dev/null)" = "$(git rev-parse origin/main 2>/dev/null)" ] && [ -x target/release/forgefleetd ] && [ -x target/release/ff ]; then NEED_BUILD=0; fi; '
           || 'if [ "$NEED_BUILD" = "1" ]; then git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal; fi && '
           || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
           || 'install -m 755 target/release/ff ~/.local/bin/ff'
           )
       )
 WHERE id IN ('ff_git', 'forgefleetd_git');
"#;

// ─── V57: macOS ff_git playbook parity with V56's linux fix ────────────────
//
// V56 closed the worker-CLI rebuild gap on Linux (ff_git playbook now builds
// `-p forge-fleet -p ff-terminal` and installs both binaries) but left the
// macOS variant alone. Surfaced 2026-04-27 during the post-V56 fleet rollout
// — taylor + ace's `forgefleetd` binary stayed at `pushed 751c99b79e` even
// after `ff fleet upgrade ff_git --all` because the macOS playbook only built
// `-p ff-terminal` and only installed `ff`. Operators had to run a separate
// `ff fleet upgrade forgefleetd_git --computer <mac>` pass to update the
// daemon; that's a reproducibility hazard.
//
// V57 brings macOS ff_git to parity: build both packages, install both
// binaries, codesign both, and `launchctl kickstart -k` the daemon. Keeps
// the existing forgefleet/ForgeFleet aliases (V33).
pub const SCHEMA_V57_MACOS_FF_GIT_PARITY: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           upgrade_playbook,
           '{macos}',
           to_jsonb(
              'export PATH="$HOME/.cargo/bin:$PATH" && '
           || 'mkdir -p "$(dirname {{source_tree_path}})" && '
           || '{ [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && '
           || 'cd "{{source_tree_path}}" && '
           || 'git fetch origin main && '
           || 'git reset --hard origin/main && '
           || 'cargo build --release -p forge-fleet -p ff-terminal && '
           || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
           || 'install -m 755 target/release/ff ~/.local/bin/ff && '
           || 'codesign --force --sign - ~/.local/bin/forgefleetd && '
           || 'codesign --force --sign - ~/.local/bin/ff && '
           || 'ln -sf ~/.local/bin/ff ~/.local/bin/forgefleet && '
           || 'ln -sf ~/.local/bin/ff ~/.local/bin/ForgeFleet && '
           || 'launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd 2>/dev/null || true'
           )
       )
 WHERE id = 'ff_git';
"#;

// ─── V58: self-expiring safety-gate fleet_secrets ──────────────────────────
//
// Surfaced 2026-04-28: `auto_upgrade_enabled` was flipped to `false` during
// the auto-upgrade overhaul (PRs #8-#18, 2026-04-27) for safety while the
// wave dispatcher was under active debugging. Nobody flipped it back. The
// fleet silently skipped 36 h of upgrades — openclaw drifted from
// 2026.4.23 → 2026.4.26 with no auto-dispatch, and the npm `latest_version`
// refresh stayed stale because `refresh_npm_registry_latest_versions` is
// itself gated by the same kill-switch.
//
// Root cause: a boolean kill-switch with no TTL and no required reason.
// Operator must remember to flip it back. Operator forgot.
//
// V58 adds the missing column. `expires_at` already existed (for rotation
// semantics) — overload it as the kill-switch TTL too. After expiry, the
// gate-check helper treats the row as if it didn't exist (default ON).
// `disabled_reason` is required by the new `ff secrets disable-gate` verb;
// the existing `ff secrets set` path is untouched (no breaking change).
pub const SCHEMA_V58_KILL_SWITCH_TTL: &str = r#"
ALTER TABLE fleet_secrets ADD COLUMN IF NOT EXISTS disabled_reason TEXT;
"#;

// ─── V59: openclaw macOS upgrade playbook needs sudo ───────────────────────
//
// Surfaced 2026-04-28 by operator: "on taylor ff always has to do sudo
// openclaw upgrade". On macOS, `npm install -g openclaw@latest` writes into
// `/opt/homebrew/lib/node_modules/openclaw` which is owned by root (Homebrew
// default ownership for global npm packages). Without sudo, npm fails with
//
//     EACCES: permission denied, rename
//     '/opt/homebrew/lib/node_modules/openclaw' ->
//     '/opt/homebrew/lib/node_modules/.openclaw-XXXX'
//
// Taylor recorded 14 consecutive failed openclaw auto-upgrades because of
// this. The Linux playbook already uses `sudo`; macOS was the missing piece.
//
// Note: Taylor is the one fleet member without passwordless sudo (per
// `feedback_taylor_sudo_excluded.md`). Adding `sudo` to the playbook means
// Taylor needs either:
//   (a) a narrow NOPASSWD sudoers entry for the npm openclaw upgrade, or
//   (b) the operator runs the upgrade manually (existing behavior).
// The other macOS members (Ace, James) already have passwordless sudo, so
// auto-upgrades will succeed there immediately.
pub const SCHEMA_V59_OPENCLAW_MACOS_SUDO: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           upgrade_playbook,
           '{macos}',
           to_jsonb('export PATH=/opt/homebrew/bin:$PATH && sudo -n npm install -g openclaw@latest'::text)
       )
 WHERE id = 'openclaw';
"#;

// ─── V60: auto-upgrade success/failure memory ──────────────────────────────
//
// Twin gaps surfaced 2026-04-29 while debugging "openclaw not auto-upgrading":
//
// A) Deferred-task auto-upgrade finalizer set status='ok' but never wrote
//    `installed_version`. Drift detection waited for the next beat from the
//    target node to refresh installed_version via software_collector. If the
//    npm install actually succeeded but the beat hadn't yet re-scanned, the
//    next tick saw the SAME old installed_version, flipped status back to
//    'upgrade_available', and dispatched the upgrade AGAIN. Ace ran the
//    openclaw upgrade 17 times this way — every successful run was followed
//    by another redundant run.
//
// B) On failure, finalizer flipped status='upgrade_available' so the next
//    tick would retry. No counter, no ceiling. Taylor failed the openclaw
//    auto-upgrade 14 times in a row (EACCES — pre-V59 playbook had no sudo)
//    with no telemetry surfacing the run-storm. Operator only noticed when
//    the dashboard showed "v2026.4.24 → v2026.4.26" stuck for 36 h.
//
// V60 adds the missing column. Pairs with finalizer changes in
// ff-terminal::finalize_software_upgrade_event:
//   - on success: write installed_version=latest_version, reset counter to 0
//   - on failure: increment counter; if >= 3, flip status='upgrade_blocked'
//     instead of 'upgrade_available'. flip_drift_status already filters
//     status IN ('ok', 'upgrade_blocked_dirty') so 'upgrade_blocked' is
//     naturally skipped.
//
// To clear an upgrade_blocked row, operator runs:
//   UPDATE computer_software SET status='ok', consecutive_failures=0
//    WHERE software_id='X' AND computer_id=(SELECT id FROM computers WHERE name='Y');
// (No new ff verb yet — add when this becomes routine.)
pub const SCHEMA_V60_AUTO_UPGRADE_MEMORY: &str = r#"
ALTER TABLE computer_software
    ADD COLUMN IF NOT EXISTS consecutive_failures INTEGER NOT NULL DEFAULT 0;
"#;

// ─── V61: peer-driven daemon-self upgrades + worker exclusion ──────────────
//
// Surfaced 2026-04-29 by operator: "we don't want a ff to fix itself on a
// computer ... another computer can help you for ff ... if one of the
// computers dies what do we do? we might add a third computer to monitor."
//
// Three coordinated mechanisms:
//
// (1) WORKER EXCLUSION
//     `fleet_tasks.excludes_computer_ids JSONB DEFAULT '[]'`. The claim
//     query in task_runner::tick_once refuses to claim a task whose
//     excludes list contains the claiming worker's computer_id. The wave
//     dispatcher sets this to `[target_id]` for `*_git` build/restart
//     tasks. Result: target NEVER claims its own ff upgrade — a peer
//     always does the ssh+build+restart. Closes the priya→priya self-ssh
//     failure mode and the conceptual self-suicide hazard.
//
//     Non-`*_git` software (openclaw, gh, claude-code, codex, ...) keeps
//     using the deferred_tasks queue with `preferred_computer_id =
//     target_id`. The target IS the worker for those — runs the playbook
//     locally, no ssh, no exclusion needed. That path was always correct.
//
// (2) WAVE DEDUP (enforced application-side in compose_fleet_upgrade_wave)
//     Today, two ticks back-to-back create two parallel "build
//     forgefleetd_git on ace" tasks; two workers ssh into ace; cargo
//     locks fight. The dispatcher now skips creating a duplicate when
//     `(software_id, target, phase)` is already pending or running.
//
// (3) DISTRIBUTED WATCHDOG (code change in task_runner::tick_once)
//     `handoff_stuck_tasks` moves from leader-only (BundledScheduler) to
//     every worker's tick. `FOR UPDATE SKIP LOCKED` in the demote path
//     ensures only one worker wins the race per stuck task. Result: if
//     worker A dies mid-task, the next tick from any peer demotes the row
//     back to pending; another peer re-claims and owns to completion.
//     The "third computer to monitor" emerges naturally — it's whichever
//     peer's tick fires first.
//
// V61 only carries the column. The other two pieces are pure code (no
// schema needed).
pub const SCHEMA_V61_PEER_DRIVEN_UPGRADES: &str = r#"
ALTER TABLE fleet_tasks
    ADD COLUMN IF NOT EXISTS excludes_computer_ids JSONB NOT NULL DEFAULT '[]'::jsonb;

CREATE INDEX IF NOT EXISTS idx_fleet_tasks_excludes
    ON fleet_tasks USING GIN (excludes_computer_ids);
"#;

// ─── V63: drop NEED_BUILD shortcut — always rebuild from origin/main ───────
//
// Surfaced 2026-04-29 while debugging "wave reports completed but member
// is still on old SHA":
//
// V51's playbook had:
//   NEED_BUILD=1
//   if [ HEAD == origin/main ] && [ -x target/release/forgefleetd ]; then NEED_BUILD=0
//   if [ NEED_BUILD = 1 ]; then git reset --hard origin/main && cargo build
//
// The optimization assumed "binaries on disk match HEAD". They don't if a
// previous wave reset HEAD forward but failed mid-build (or was killed
// mid-cargo by the self-kill race we fixed in V52). Subsequent wave finds
// HEAD == origin/main, target/release/forgefleetd exists, NEED_BUILD=0,
// installs the STALE binary built at the old SHA.
//
// Concrete: marcus's checkout HEAD was at b6e44f5e but its
// target/release/forgefleetd was an older f9da42ce3a binary. New wave's
// Phase-1 took 1 second (skipped build), installed the f9da42ce3a binary,
// Phase-2 restarted the daemon. Daemon reported ff_git=f9da42ce3a even
// though source HEAD was b6e44f5e. Fleet drift never closed.
//
// Fix: always `git reset --hard origin/main && cargo build --release`.
// Cargo's incremental build is fast (~3-5s) when nothing changed; the
// ~30-60s cold-build cost on first wave only is acceptable. Correctness
// wins over the few seconds saved.
//
// Updates four playbook keys: linux-ubuntu, linux-dgx,
// linux-ubuntu-build-only, linux-dgx-build-only. macOS playbook (V57)
// already always rebuilds — no change there.
// ─── V64: register `ff` and `forgefleetd` companion rows ───────────────────
//
// Surfaced 2026-04-29: the SoftwareCollector emits 14 software entries per
// beat, including `ff` (semver/build-version) and `forgefleetd` (same).
// software_registry only had `ff_git` and `forgefleetd_git` (the SHA-tracked
// rows). Every beat's FIRST upsert hit
//
//     insert or update on table "computer_software" violates foreign key
//     constraint "computer_software_software_id_fkey"
//
// for software_id='ff' — the `?` operator propagated the error, the entire
// process_beat returned Err, and ALL subsequent software upserts in the
// same beat (including `ff_git`, `forgefleetd_git`) were skipped. Result:
// computer_software hadn't been touched in 2+ days. Fleet drift could not
// close even when waves succeeded perfectly.
//
// V64 adds the two missing rows. They're informational-only:
//   - `kind = 'binary'` (matches the SHA-tracked siblings)
//   - `version_source = NULL` (no auto-detection; SoftwareCollector
//     populates installed_version directly from `<binary> --version`)
//   - `upgrade_playbook = NULL` (no auto-upgrade — the SHA-tracked rows
//     `ff_git`/`forgefleetd_git` drive the actual upgrade flow)
//   - `requires_restart`/`requires_reboot` = false
//
// Companion code change in the materializer makes the upsert loop
// per-row-resilient so future schema drift is logged + skipped instead of
// silently aborting every beat.
pub const SCHEMA_V64_REGISTER_FF_FORGEFLEETD: &str = r#"
-- Defensive: ensure version_source has a default so legacy inserts
-- (and any triggers) that omit the column do not fail.
ALTER TABLE software_registry
    ALTER COLUMN version_source SET DEFAULT '{}'::jsonb;

INSERT INTO software_registry (id, display_name, kind, version_source, upgrade_playbook, requires_restart, requires_reboot)
VALUES
    ('ff',           'ForgeFleet CLI (build-version row)',     'binary',
     '{"method":"informational"}'::jsonb, '{}'::jsonb, false, false),
    ('forgefleetd',  'ForgeFleet daemon (build-version row)',  'binary',
     '{"method":"informational"}'::jsonb, '{}'::jsonb, false, false)
ON CONFLICT (id) DO NOTHING;
"#;

pub const SCHEMA_V63_DROP_NEED_BUILD_SHORTCUT: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           jsonb_set(
               jsonb_set(
                   jsonb_set(
                       upgrade_playbook,
                       '{linux-ubuntu}',
                       to_jsonb(
                           'export PATH="$HOME/.cargo/bin:$PATH" && '
                        || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                        || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                        || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                        || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                        || 'git fetch origin main && '
                        || 'git reset --hard origin/main && '
                        || 'cargo build --release -p forge-fleet && '
                        || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                        || 'install -m 755 target/release/ff ~/.local/bin/ff && '
                        || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
                        || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
                       )
                   ),
                   '{linux-dgx}',
                   to_jsonb(
                       'export PATH="$HOME/.cargo/bin:$PATH" && '
                    || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                    || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                    || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                    || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                    || 'git fetch origin main && '
                    || 'git reset --hard origin/main && '
                    || 'cargo build --release -p forge-fleet && '
                    || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                    || 'install -m 755 target/release/ff ~/.local/bin/ff && '
                    || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
                    || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
                   )
               ),
               '{linux-ubuntu-build-only}',
               to_jsonb(
                   'export PATH="$HOME/.cargo/bin:$PATH" && '
                || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                || 'git fetch origin main && '
                || 'git reset --hard origin/main && '
                || 'cargo build --release -p forge-fleet && '
                || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                || 'install -m 755 target/release/ff ~/.local/bin/ff'
               )
           ),
           '{linux-dgx-build-only}',
           to_jsonb(
               'export PATH="$HOME/.cargo/bin:$PATH" && '
            || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
            || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
            || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
            || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
            || 'git fetch origin main && '
            || 'git reset --hard origin/main && '
            || 'cargo build --release -p forge-fleet && '
            || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
            || 'install -m 755 target/release/ff ~/.local/bin/ff'
           )
       )
 WHERE id IN ('ff_git', 'forgefleetd_git');
"#;

// ─── V65: register `open_design_git` as fleet-wide software ────────────────
//
// Operator directive 2026-04-30: "treat it as software in ff and have it
// installed in all the computers so all the computers can use it to build
// when we do things via ff."
//
// Open Design (https://github.com/nexu-io/open-design) is a Next.js + Node
// daemon stack that turns natural-language briefs into editable design
// artifacts (prototypes, decks, dashboards, landing pages) by spawning the
// host's installed coding agent CLI (Claude Code / Codex / Gemini /
// OpenCode / Qwen / Copilot / Hermes / Kimi / pi). Same dispatch model
// ForgeFleet already uses — so installing it on every member lets any
// `ff run` / `ff supervise` task on any node lean on a 71-design-system
// catalog + 19 skills for design output.
//
// Install model: git clone + pnpm install. There's no released npm package
// or homebrew tap yet (`latestRelease: null` as of 2026-04-30; the project
// is 2 days old). The shape mirrors `ff_git` / `forgefleetd_git`:
//   - clone to `~/.forgefleet/sub-agent-0/open-design` (fleet workspace)
//   - `git fetch origin main && git reset --hard origin/main`
//   - `pnpm install` (idempotent — no-op when deps unchanged)
//
// We don't auto-start the OD daemon. Agents invoke OD on demand via
// `pnpm --dir ~/.forgefleet/sub-agent-0/open-design tools-dev run web`
// when a design task lands. Treating it like an installed library, not a
// persistent service, keeps the fleet's daemon footprint flat.
//
// version_source uses `git_head` against `main` so the auto-upgrade tick's
// `refresh_self_built_latest_versions` (or its git-equivalent path) drives
// drift detection. Refresh cadence is the same 6h/1h pipeline that already
// keeps `ff_git` / `forgefleetd_git` current.
//
// Skipped V62 in the migration sequence (PR #65 was code-only). V63 ships
// as `drop_need_build_shortcut`, V64 as `register_ff_forgefleetd`. V65
// continues the sequence here.
//
// macOS playbook uses corepack to materialize the pinned pnpm version
// (10.33.2 per the repo's `packageManager` field). Linux/DGX path is
// identical — both have node 24+ and corepack via apt.
pub const SCHEMA_V65_REGISTER_OPEN_DESIGN: &str = r#"
INSERT INTO software_registry (
    id, display_name, kind,
    version_source,
    upgrade_playbook,
    requires_restart, requires_reboot
)
VALUES (
    'open_design_git',
    'Open Design (nexu-io/open-design)',
    'binary',
    '{"method":"git_head","repo":"https://github.com/nexu-io/open-design","ref_kind":"main"}'::jsonb,
    jsonb_build_object(
        'macos',
            'export PATH=/opt/homebrew/bin:$PATH && '
         || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/open-design)" && '
         || '{ [ -d "$HOME/.forgefleet/sub-agent-0/open-design/.git" ] || git clone https://github.com/nexu-io/open-design "$HOME/.forgefleet/sub-agent-0/open-design"; } && '
         || 'cd "$HOME/.forgefleet/sub-agent-0/open-design" && '
         || 'git fetch origin main && '
         || 'git reset --hard origin/main && '
         || 'corepack enable >/dev/null 2>&1 && '
         || 'corepack pnpm install --frozen-lockfile',
        'linux-ubuntu',
            'export PATH="$HOME/.local/bin:$PATH" && '
         || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/open-design)" && '
         || '{ [ -d "$HOME/.forgefleet/sub-agent-0/open-design/.git" ] || git clone https://github.com/nexu-io/open-design "$HOME/.forgefleet/sub-agent-0/open-design"; } && '
         || 'cd "$HOME/.forgefleet/sub-agent-0/open-design" && '
         || 'git fetch origin main && '
         || 'git reset --hard origin/main && '
         || 'corepack enable >/dev/null 2>&1 && '
         || 'corepack pnpm install --frozen-lockfile',
        'linux-dgx',
            'export PATH="$HOME/.local/bin:$PATH" && '
         || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/open-design)" && '
         || '{ [ -d "$HOME/.forgefleet/sub-agent-0/open-design/.git" ] || git clone https://github.com/nexu-io/open-design "$HOME/.forgefleet/sub-agent-0/open-design"; } && '
         || 'cd "$HOME/.forgefleet/sub-agent-0/open-design" && '
         || 'git fetch origin main && '
         || 'git reset --hard origin/main && '
         || 'corepack enable >/dev/null 2>&1 && '
         || 'corepack pnpm install --frozen-lockfile'
    )::jsonb,
    false, false
)
ON CONFLICT (id) DO NOTHING;
"#;

// ─── V66: data-driven software detection ───────────────────────────────────
//
// Operator pushback 2026-04-30: "shouldn't these come from the database?"
//
// SoftwareCollector previously hardcoded detection logic per software_id —
// `known_ids` list + a Rust block per entry that runs the right `--version`
// command and parses output with a fixed regex. That works for our existing
// 13 well-known tools but every NEW entry (open-design, future skills, future
// CLIs) needed a code change to land on the fleet. That violates the
// DB-first-catalog rule (memory: feedback_db_first_catalog,
// feedback_no_hardcode).
//
// V66 adds `software_registry.detection JSONB` describing HOW to detect a
// row's `installed_version` on a host. The collector reads the registry
// at startup and runs whatever method each row declares. Existing
// hardcoded detectors stay (rollback safety) but become legacy fallbacks;
// new entries declare detection in data, no Rust change required.
//
// Methods supported in V66's collector loop:
//
//   {"method":"binary_version","binary":"openclaw","args":["--version"],
//    "regex":"OpenClaw\\s+(\\S+)"}
//     - run `binary --args` (PATH lookup), extract first regex capture
//
//   {"method":"git_checkout","path":"$HOME/.forgefleet/sub-agent-0/open-design",
//    "truncate":10}
//     - if `<path>/.git` exists, run `git -C <path> rev-parse HEAD`,
//       truncate to N chars
//
//   {"method":"which","binary":"docker"}
//     - presence-only; reports "(present)" if the binary is on PATH
//
// Default-NULL: rows without `detection` skip the data-driven loop —
// the legacy hardcoded path still serves them.
//
// Backfill: V66 also populates detection for `open_design_git` (added in
// V65) so it flows through the new path immediately. Future migrations
// can backfill detection for the other rows on a "as-touched" basis;
// the goal is no NEW hardcoded detector ever lands.
pub const SCHEMA_V66_DATA_DRIVEN_DETECTION: &str = r#"
ALTER TABLE software_registry
    ADD COLUMN IF NOT EXISTS detection JSONB;

-- ── ff / ff_git (dual emit from `ff --version`) ───────────────────────
UPDATE software_registry SET detection = '{
    "method":"ff_version_pair","binary":"ff","field":"version"
}'::jsonb WHERE id='ff' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"ff_version_pair","binary":"ff","field":"sha"
}'::jsonb WHERE id='ff_git' AND detection IS NULL;

-- ── forgefleetd / forgefleetd_git (dual emit) ─────────────────────────
UPDATE software_registry SET detection = '{
    "method":"ff_version_pair","binary":"forgefleetd","field":"version"
}'::jsonb WHERE id='forgefleetd' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"ff_version_pair","binary":"forgefleetd","field":"sha"
}'::jsonb WHERE id='forgefleetd_git' AND detection IS NULL;

-- ── open-design (git checkout) ────────────────────────────────────────
UPDATE software_registry SET detection = '{
    "method":"git_checkout",
    "path":"$HOME/.forgefleet/sub-agent-0/open-design",
    "truncate":10,
    "install_source":"git"
}'::jsonb WHERE id='open_design_git' AND detection IS NULL;

-- ── npm-shaped binaries (binary --version, regex parse) ───────────────
UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"openclaw","args":["--version"],
    "regex":"OpenClaw\\s+(\\S+)","install_source_hint":"auto"
}'::jsonb WHERE id='openclaw' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"codex","args":["--version"],
    "regex":"(\\d+\\.\\d+\\.\\d+(?:[\\w.-]*)?)","install_source_hint":"auto"
}'::jsonb WHERE id='codex' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"claude","args":["--version"],
    "regex":"(\\d+\\.\\d+\\.\\d+(?:[\\w.-]*)?)","install_source_hint":"auto"
}'::jsonb WHERE id='claude-code' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"gh","args":["--version"],
    "regex":"gh version (\\S+)","install_source_hint":"auto"
}'::jsonb WHERE id='gh' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"op","args":["--version"],
    "regex":"^(\\S+)","install_source_hint":"auto"
}'::jsonb WHERE id='op' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"rustup","args":["--version"],
    "regex":"rustup (\\S+)","install_source_hint":"direct"
}'::jsonb WHERE id='rustup' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"node","args":["--version"],
    "regex":"v?(\\d+\\.\\d+\\.\\d+)","install_source_hint":"auto"
}'::jsonb WHERE id='node' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"python3","args":["--version"],
    "regex":"Python (\\d+\\.\\d+\\.\\d+)","install_source_hint":"auto"
}'::jsonb WHERE id='python' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"docker","args":["--version"],
    "regex":"Docker version (\\S+),","install_source_hint":"auto"
}'::jsonb WHERE id='docker' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"ollama","args":["--version"],
    "regex":"version is (\\S+)","install_source_hint":"auto",
    "fallback_via_run":true
}'::jsonb WHERE id='ollama' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"binary_version","binary":"llama-server","args":["--version"],
    "regex":"version (\\S+)","install_source_hint":"auto"
}'::jsonb WHERE id='llama.cpp' AND detection IS NULL;

-- ── python module probes ──────────────────────────────────────────────
UPDATE software_registry SET detection = '{
    "method":"python_module","module":"mlx_lm","os_filter":"macos",
    "install_source_hint":"pip"
}'::jsonb WHERE id='mlx_lm' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"python_module","module":"vllm","os_filter":"linux",
    "install_source_hint":"pip"
}'::jsonb WHERE id='vllm' AND detection IS NULL;

-- ── OS detection ──────────────────────────────────────────────────────
UPDATE software_registry SET detection = '{
    "method":"os_release","expected_id":"macos"
}'::jsonb WHERE id='os-macos' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"os_release","expected_id":"ubuntu","expected_version_prefix":"22.04"
}'::jsonb WHERE id='os-ubuntu-22.04' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"os_release","expected_id":"ubuntu","expected_version_prefix":"24.04"
}'::jsonb WHERE id='os-ubuntu-24.04' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"os_release","expected_kernel_contains":"-nvidia"
}'::jsonb WHERE id='os-dgx' AND detection IS NULL;

UPDATE software_registry SET detection = '{
    "method":"os_release","expected_id":"windows"
}'::jsonb WHERE id='os-windows' AND detection IS NULL;
"#;

// ─── V67: auto_install + agent_hint columns on software_registry ───────────
//
// Two coupled additions:
//
// (A) `auto_install BOOLEAN` — when TRUE, the auto-upgrade tick seeds a
//     `computer_software` row (status='upgrade_available') for every
//     `computers.id` that doesn't already have one, then dispatches the
//     install. Closes the install-bootstrap loop discovered 2026-04-30
//     while rolling out open-design: the dispatcher only operates on
//     existing computer_software rows, but those rows are only created
//     by the materializer when a beat reports the software installed.
//     Without a way to bootstrap, software_registry entries with no
//     installed members never get installed anywhere.
//
// (B) `agent_hint TEXT` — a free-form natural-language hint the
//     `ff supervise` / `ff run` dispatcher injects into the agent's
//     system prompt when the target host has this software installed
//     (status='ok'). The agent reads the hints and decides on its own
//     whether the user's prompt warrants invoking the tool. Operator's
//     directive 2026-04-30: "I dont want to add more commands... I
//     want ff to decide when to use it."
//
//     For open-design, the hint tells the agent: this machine has a
//     pre-installed Next.js + Node design platform at
//     `~/.forgefleet/sub-agent-0/open-design`; for design-shaped
//     prompts (decks, prototypes, dashboards, landing pages, magazine
//     layouts), spin up the OD daemon via `pnpm --dir <path> tools-dev
//     run web` and drive its API. The agent doesn't need a custom ff
//     verb — the hint is enough context for it to route itself.
//
// Both columns default to neutral values (auto_install=false,
// agent_hint=NULL) so existing rows are unaffected. open-design opts
// in by setting both.
pub const SCHEMA_V67_AUTO_INSTALL_AGENT_HINT: &str = r#"
ALTER TABLE software_registry
    ADD COLUMN IF NOT EXISTS auto_install BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE software_registry
    ADD COLUMN IF NOT EXISTS agent_hint TEXT;

UPDATE software_registry
   SET auto_install = true,
       agent_hint   = 'A pre-installed open-source design platform (open-design, nexu-io/open-design) is available at $HOME/.forgefleet/sub-agent-0/open-design on this machine. ' ||
                      'It turns natural-language briefs into editable design artifacts (HTML prototypes, decks, dashboards, landing pages, magazine layouts) by spawning the host''s coding-agent CLI. ' ||
                      'When the user''s request looks like a design task (deck, slides, prototype, mock, landing page, dashboard, magazine layout, brand assets, design system), bring it up with: ' ||
                      'pnpm --dir $HOME/.forgefleet/sub-agent-0/open-design tools-dev run web ' ||
                      '(the daemon listens on localhost:7456; the web UI on localhost:3000). ' ||
                      'For non-design tasks, ignore this hint.'
 WHERE id = 'open_design_git';
"#;

// ─── V69: skill_sources table — runtime-configurable skill scan roots ──────
//
// Operator directive 2026-04-30: "make it so that I can easily add new
// skills and tools. also we shouldn't hardcode individual skill ... ff
// should be able to add new skills (.md) file or tools on the fly."
//
// Before V69, ff_agent::skill_catalog walked four hardcoded paths
// (project/.claude/skills, project/skills, ~/.claude/skills, and the
// fleet-installed open-design skills dir). Adding a new scan root —
// a forked skills repo, an alternate vendor's skill collection —
// required a code change.
//
// V69 moves the roots into a `skill_sources` table. Operators add/remove
// rows at runtime via `ff skills source add/remove` (future verb) or by
// direct INSERT. The skill_catalog reads this table at session start,
// merges with sane built-in defaults, and walks every enabled root.
//
// Design notes:
//   - `path` may contain `$HOME` / `~/` — expanded by the collector.
//   - `priority` resolves id collisions (higher = wins). Project-private
//     defaults to 100, fleet-installed defaults to 30. New custom sources
//     default to 50.
//   - `enabled` lets operators temporarily mute a source without DELETE.
//   - Default rows seed the four legacy hardcoded roots so behavior is
//     identical out-of-the-box. Operators can DELETE / UPDATE freely.
//
// Tools follow a different pattern (built-in primitives in code, runtime
// extensibility via MCP servers — already supported per
// reference_mcp_access). Skills are the data-driven path; this table
// is the abstraction.
pub const SCHEMA_V69_SKILL_SOURCES: &str = r#"
CREATE TABLE IF NOT EXISTS skill_sources (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    path        TEXT NOT NULL,
    priority    INTEGER NOT NULL DEFAULT 50,
    enabled     BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_skill_sources_enabled
    ON skill_sources (enabled, priority DESC);

INSERT INTO skill_sources (id, label, path, priority, enabled)
VALUES
    ('project-private', 'project-private (.claude/skills)',
     '$CWD/.claude/skills', 110, true),
    ('project-declared', 'project-declared (skills/)',
     '$CWD/skills', 100, true),
    ('user-global', 'user-global (~/.claude/skills)',
     '$HOME/.claude/skills', 50, true),
    ('fleet-open-design', 'fleet-installed open-design skills',
     '$HOME/.forgefleet/sub-agent-0/open-design/skills', 30, true)
ON CONFLICT (id) DO NOTHING;
"#;

// ─── V72: Consolidate legacy SQLite databases into Postgres ────────────────
//
// Migrates data from the following legacy SQLite files that were never
// wired to the Postgres operational store:
//   - context.db   → local_context_sources, local_context_chunks
//   - evolution.db → fleet_evolution_insights, fleet_evolution_task_records, fleet_evolution_version_proposals
//   - learnings.db → fleet_learnings, fleet_error_fixes, fleet_model_scores
//   - governance.db→ fleet_governance_recommendations, fleet_governance_runs

pub const SCHEMA_V72_SQLITE_CONSOLIDATION: &str = r#"
-- ─── Local Context (was context.db) ────────────────────────────────────────
-- RAG / document sources and their chunked text for local retrieval.
CREATE TABLE IF NOT EXISTS local_context_sources (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    uri         TEXT NOT NULL,                        -- file path, URL, or identifier
    title       TEXT NOT NULL DEFAULT '',
    source_type TEXT NOT NULL DEFAULT 'file',         -- file | url | note | paste
    metadata    JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS local_context_chunks (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id   UUID NOT NULL REFERENCES local_context_sources(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    content     TEXT NOT NULL,
    embedding   JSONB,                                -- embedding vector as JSON array; nullable until generated
    metadata    JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(source_id, chunk_index)
);

CREATE INDEX IF NOT EXISTS idx_local_context_chunks_source
    ON local_context_chunks (source_id);

-- ─── Fleet Evolution (was evolution.db) ────────────────────────────────────
-- Self-improvement insights and version proposals tracked by the fleet.
CREATE TABLE IF NOT EXISTS fleet_evolution_insights (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    category    TEXT NOT NULL,                        -- performance | reliability | UX | security
    summary     TEXT NOT NULL,
    detail      TEXT NOT NULL DEFAULT '',
    confidence  REAL NOT NULL DEFAULT 0.5,            -- 0.0–1.0
    source_json JSONB NOT NULL DEFAULT '{}',          -- task_id, node, model, etc.
    applied     BOOLEAN NOT NULL DEFAULT false,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS fleet_evolution_task_records (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    task_type    TEXT NOT NULL,
    model_used   TEXT NOT NULL,
    tier         INTEGER NOT NULL DEFAULT 2,
    outcome      TEXT NOT NULL,                       -- success | failure | partial
    duration_sec REAL NOT NULL DEFAULT 0,
    metadata     JSONB NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS fleet_evolution_version_proposals (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_version  TEXT NOT NULL,
    change_summary  TEXT NOT NULL,
    risk_level      TEXT NOT NULL DEFAULT 'low',      -- low | medium | high | critical
    status          TEXT NOT NULL DEFAULT 'pending',  -- pending | approved | rejected | implemented
    proposed_by     TEXT NOT NULL DEFAULT '',         -- node or agent name
    reviewed_at     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_fleet_evolution_insights_category
    ON fleet_evolution_insights (category, applied);

-- ─── Fleet Learnings (was learnings.db) ────────────────────────────────────
-- Error patterns, fixes, and per-model performance scores.
CREATE TABLE IF NOT EXISTS fleet_learnings (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    task_type       TEXT NOT NULL,
    model_used      TEXT NOT NULL,
    tier            INTEGER NOT NULL DEFAULT 2,
    outcome         TEXT NOT NULL,                    -- success | failure | partial
    error_pattern   TEXT NOT NULL DEFAULT '',
    fix_applied     TEXT NOT NULL DEFAULT '',
    task_hash       TEXT NOT NULL DEFAULT '',         -- deterministic hash of task input
    duration_sec    REAL NOT NULL DEFAULT 0,
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS fleet_error_fixes (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    error_pattern   TEXT NOT NULL,
    fix_description TEXT NOT NULL,
    times_applied   INTEGER NOT NULL DEFAULT 1,
    success_rate    REAL NOT NULL DEFAULT 1.0,
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS fleet_model_scores (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    model_name   TEXT NOT NULL,
    task_type    TEXT NOT NULL,
    total_tasks  INTEGER NOT NULL DEFAULT 0,
    successes    INTEGER NOT NULL DEFAULT 0,
    avg_duration REAL NOT NULL DEFAULT 0,
    metadata     JSONB NOT NULL DEFAULT '{}',
    last_updated TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(model_name, task_type)
);

CREATE INDEX IF NOT EXISTS idx_fleet_learnings_type
    ON fleet_learnings (task_type, outcome);
CREATE INDEX IF NOT EXISTS idx_fleet_error_fixes_pattern
    ON fleet_error_fixes (error_pattern);
CREATE INDEX IF NOT EXISTS idx_fleet_model_scores_model
    ON fleet_model_scores (model_name, task_type);

-- ─── Fleet Governance (was governance.db) ──────────────────────────────────
-- Task recommendation policy and governance run tracking.
CREATE TABLE IF NOT EXISTS fleet_governance_recommendations (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    task_type       TEXT NOT NULL,
    recommended_model TEXT NOT NULL,
    reason          TEXT NOT NULL DEFAULT '',
    confidence      REAL NOT NULL DEFAULT 0.5,
    policy_json     JSONB NOT NULL DEFAULT '{}',
    enabled         BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS fleet_governance_runs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_type        TEXT NOT NULL,                    -- audit | policy_check | compliance
    status          TEXT NOT NULL DEFAULT 'running',  -- running | completed | failed
    findings_json   JSONB NOT NULL DEFAULT '[]',
    summary         TEXT NOT NULL DEFAULT '',
    triggered_by    TEXT NOT NULL DEFAULT '',         -- node or agent name
    completed_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_fleet_governance_rec_type
    ON fleet_governance_recommendations (task_type, enabled);
CREATE INDEX IF NOT EXISTS idx_fleet_governance_runs_status
    ON fleet_governance_runs (status, created_at DESC);
"#;

// ─── V74: Fleet-First Selfish Routing ───────────────────────────────────────
// Phase 15b — Adds routing_mode to fleet_tasks so the task queue respects
// fleet-first, local-first, local-only, and balanced routing strategies.

pub const SCHEMA_V74_ROUTING_MODE: &str = r#"
-- Routing strategy for each task. Affects claim ordering in TaskRunner.
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS routing_mode TEXT NOT NULL DEFAULT 'fleet_first'
    CHECK (routing_mode IN ('local_first', 'fleet_first', 'local_only', 'balanced'));

-- Index to speed up fleet-first claim queries (deprioritize own tasks).
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_routing ON fleet_tasks(routing_mode, created_by_computer_id, status)
    WHERE status = 'pending';
"#;

// ─── V73: Fleet Tool Registry ───────────────────────────────────────────────
// Phase 15a — Central tool registry where every node registers its tools.
// Enables fleet-wide tool discovery, health tracking, and usage attribution.

pub const SCHEMA_V73_FLEET_TOOL_REGISTRY: &str = r#"
-- ─── Fleet Tools ────────────────────────────────────────────────────────────
-- Every node registers its tools on startup. One row per (tool_name, worker_name).
CREATE TABLE IF NOT EXISTS fleet_tools (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name           TEXT NOT NULL,
    worker_name           TEXT NOT NULL REFERENCES fleet_workers(name) ON DELETE CASCADE,
    description         TEXT NOT NULL DEFAULT '',
    parameters_schema   JSONB NOT NULL DEFAULT '{}',
    capabilities_required TEXT[] NOT NULL DEFAULT '{}',
    health_checked_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    call_count          INTEGER NOT NULL DEFAULT 0,
    avg_latency_ms      REAL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(tool_name, worker_name)
);

CREATE INDEX IF NOT EXISTS idx_fleet_tools_name ON fleet_tools(tool_name);
CREATE INDEX IF NOT EXISTS idx_fleet_tools_node_health ON fleet_tools(worker_name, health_checked_at);

-- ─── Fleet Tool Usage ───────────────────────────────────────────────────────
-- Every tool invocation across the fleet is logged here for observability.
CREATE TABLE IF NOT EXISTS fleet_tool_usage (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name       TEXT NOT NULL,
    worker_name       TEXT NOT NULL REFERENCES fleet_workers(name),
    session_id      UUID REFERENCES agent_sessions(id),
    task_id         UUID,
    work_item_id    UUID,
    subagent_id     TEXT NOT NULL DEFAULT '',
    input_summary   TEXT NOT NULL DEFAULT '',
    started_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at    TIMESTAMPTZ,
    latency_ms      INTEGER,
    success         BOOLEAN,
    tokens_in       INTEGER NOT NULL DEFAULT 0,
    tokens_out      INTEGER NOT NULL DEFAULT 0,
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    workspace_path  TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_tool_usage_tool ON fleet_tool_usage(tool_name, started_at);
CREATE INDEX IF NOT EXISTS idx_tool_usage_node ON fleet_tool_usage(worker_name, started_at);
CREATE INDEX IF NOT EXISTS idx_tool_usage_session ON fleet_tool_usage(session_id);
"#;

// ─── V75: Work Items + Work Batches ─────────────────────────────────────────
// Phase 15c — Fine-grained task decomposition with weighted partitioning.
// Enables map-reduce style execution across the fleet.

pub const SCHEMA_V75_WORK_ITEMS: &str = r#"
-- ─── Work Items ─────────────────────────────────────────────────────────────
-- Individual units of work within a decomposed task.
CREATE TABLE IF NOT EXISTS fleet_work_items (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_task_id      UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    batch_id            INT NOT NULL DEFAULT 0,
    item_index          INT NOT NULL,
    item_key            TEXT NOT NULL DEFAULT '',
    item_type           TEXT NOT NULL DEFAULT 'document',
    item_metadata       JSONB NOT NULL DEFAULT '{}',

    -- Weighted estimation
    estimated_weight    REAL NOT NULL DEFAULT 1.0,
    actual_weight       REAL,
    complexity_factors  JSONB NOT NULL DEFAULT '{}',

    -- Assignment
    assigned_node_id    UUID REFERENCES computers(id),
    assigned_agent_id   TEXT,
    assigned_session_id UUID REFERENCES agent_sessions(id),
    claimed_at          TIMESTAMPTZ,

    -- Progress
    status              TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'in_progress', 'completed', 'failed', 'yielded', 'stolen')),
    progress_percent    INT DEFAULT 0,
    checkpoint_data     JSONB NOT NULL DEFAULT '{}',
    yielded_at          TIMESTAMPTZ,
    stolen_from         UUID REFERENCES computers(id),

    -- Result
    result_summary      TEXT,
    result_artifact_id  UUID,
    result_tokens_in    INT DEFAULT 0,
    result_tokens_out   INT DEFAULT 0,
    completed_at        TIMESTAMPTZ,
    error_message       TEXT,

    -- Retry
    retry_count         INT DEFAULT 0,
    max_retries         INT DEFAULT 2,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    UNIQUE(parent_task_id, item_index)
);

CREATE INDEX IF NOT EXISTS idx_work_items_parent ON fleet_work_items(parent_task_id, status);
CREATE INDEX IF NOT EXISTS idx_work_items_batch ON fleet_work_items(parent_task_id, batch_id, status);
CREATE INDEX IF NOT EXISTS idx_work_items_claimed ON fleet_work_items(assigned_node_id, status)
    WHERE status IN ('claimed', 'in_progress');
CREATE INDEX IF NOT EXISTS idx_work_items_yielded ON fleet_work_items(parent_task_id, status)
    WHERE status = 'yielded';

-- ─── Work Batches ───────────────────────────────────────────────────────────
-- A batch is a group of work_items assigned to one node.
CREATE TABLE IF NOT EXISTS fleet_work_batches (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_task_id      UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    batch_index         INT NOT NULL,
    total_estimated_weight REAL NOT NULL DEFAULT 0,
    total_actual_weight REAL,
    items_count         INT NOT NULL DEFAULT 0,
    assigned_node_id    UUID REFERENCES computers(id),
    assigned_agent_id   TEXT,
    assigned_session_id UUID REFERENCES agent_sessions(id),
    status              TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'in_progress', 'completed', 'rebalancing')),
    progress_percent    INT DEFAULT 0,
    rebalanced_at       TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(parent_task_id, batch_index)
);

-- ─── Fleet Workspaces ───────────────────────────────────────────────────────
-- Shared workspace state for sub-agent execution.
CREATE TABLE IF NOT EXISTS fleet_workspaces (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_node_id       UUID REFERENCES computers(id),
    workspace_path      TEXT NOT NULL,
    sync_method         TEXT NOT NULL CHECK (sync_method IN ('git', 'nfs', 's3')),
    sync_config         JSONB NOT NULL DEFAULT '{}',
    shell_state         JSONB NOT NULL DEFAULT '{}',
    last_synced_at      TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─── Sub-Agent Cleanup Log ──────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS subagent_cleanup_log (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_id             UUID REFERENCES computers(id),
    subagent_id         TEXT NOT NULL,
    item_type           TEXT NOT NULL CHECK (item_type IN ('git_folder','artifact','temp','empty_dir')),
    item_path           TEXT NOT NULL,
    bytes_freed         BIGINT,
    reason              TEXT NOT NULL,
    deleted_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#;

// ─── V76: Vault Sync Tables ─────────────────────────────────────────────────
// Phase 16 — Fleet Memory Architecture: vault file indexing, link graph,
// and TODO extraction for Obsidian vault integration.

pub const SCHEMA_V77_FLEET_TASK_NOTIFY: &str = r#"
-- ─── V77: Real-Time Task Queue ────────────────────────────────────────────
-- NOTIFY trigger so TaskRunner workers wake immediately when new tasks
-- are inserted, instead of polling every 10s.  The trigger fires AFTER
-- INSERT so the row is visible to the worker before any listener wakes.

CREATE OR REPLACE FUNCTION notify_new_fleet_task()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify('fleet_task_inserted', NEW.id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_fleet_tasks_notify ON fleet_tasks;
CREATE TRIGGER trg_fleet_tasks_notify
    AFTER INSERT ON fleet_tasks
    FOR EACH ROW
    EXECUTE FUNCTION notify_new_fleet_task();
"#;

pub const SCHEMA_V76_VAULT_SYNC: &str = r#"
-- ─── Vault Files ────────────────────────────────────────────────────────────
-- Every markdown file in the vault, indexed for fast lookup.
CREATE TABLE IF NOT EXISTS vault_files (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_path       TEXT NOT NULL UNIQUE,
    file_name       TEXT NOT NULL,
    parent_dir      TEXT NOT NULL,
    size_bytes      BIGINT,
    modified_at     TIMESTAMPTZ,
    frontmatter     JSONB NOT NULL DEFAULT '{}',
    tags            TEXT[] NOT NULL DEFAULT '{}',
    indexed_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_vault_files_dir ON vault_files(parent_dir);
CREATE INDEX IF NOT EXISTS idx_vault_files_name ON vault_files(file_name);

-- ─── Vault Links ────────────────────────────────────────────────────────────
-- Bidirectional link graph between vault files (Obsidian [[links]]).
CREATE TABLE IF NOT EXISTS vault_links (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_path     TEXT NOT NULL REFERENCES vault_files(file_path) ON DELETE CASCADE,
    target_path     TEXT NOT NULL REFERENCES vault_files(file_path) ON DELETE CASCADE,
    link_text       TEXT,
    link_type       TEXT NOT NULL DEFAULT 'wiki' CHECK (link_type IN ('wiki', 'markdown', 'embed')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(source_path, target_path, link_text)
);

CREATE INDEX IF NOT EXISTS idx_vault_links_source ON vault_links(source_path);
CREATE INDEX IF NOT EXISTS idx_vault_links_target ON vault_links(target_path);

-- ─── Vault Todos ────────────────────────────────────────────────────────────
-- TODO items extracted from vault markdown files (- [ ] / - [x]).
CREATE TABLE IF NOT EXISTS vault_todos (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_path       TEXT NOT NULL REFERENCES vault_files(file_path) ON DELETE CASCADE,
    todo_text       TEXT NOT NULL,
    done            BOOLEAN NOT NULL DEFAULT false,
    line_number     INT,
    extracted_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(file_path, todo_text)
);

CREATE INDEX IF NOT EXISTS idx_vault_todos_file ON vault_todos(file_path);
CREATE INDEX IF NOT EXISTS idx_vault_todos_done ON vault_todos(done)
    WHERE done = false;
"#;

// ─── V78: pgvector embeddings for vault nodes ─────────────────────────────
// Phase 7 — Context Engine. Enables vector similarity search on the vault.

pub const SCHEMA_V78_PGVECTOR_EMBEDDINGS: &str = r#"
-- Phase 7: pgvector embeddings for semantic search.
-- NOTE: The pgvector extension must be installed by a superuser first:
--   CREATE EXTENSION IF NOT EXISTS vector;
-- If the extension is not available, this migration is a no-op.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector') THEN
        ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS embedding vector(384);
        CREATE INDEX IF NOT EXISTS idx_vault_nodes_embedding
            ON brain_vault_nodes USING hnsw (embedding vector_cosine_ops)
            WHERE embedding IS NOT NULL;
    END IF;
END $$;
"#;

// ─── V79: Project Schedules ───────────────────────────────────────────────
// Phase 10 — FinOps + Project Scheduling. Cron-driven recurring tasks per project.

pub const SCHEMA_V79_PROJECT_SCHEDULES: &str = r#"
CREATE TABLE IF NOT EXISTS project_schedules (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    cron_expression TEXT NOT NULL,
    next_run_at     TIMESTAMPTZ NOT NULL,
    task_template   JSONB NOT NULL DEFAULT '{}',
    enabled         BOOLEAN DEFAULT true,
    last_run_at     TIMESTAMPTZ,
    run_count       INT DEFAULT 0,
    created_at      TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_project_schedules_next_run
    ON project_schedules(next_run_at)
    WHERE enabled = true;

CREATE INDEX IF NOT EXISTS idx_project_schedules_project
    ON project_schedules(project_id);
"#;

// ─── V80: Agent Procedures (Procedural Memory) ────────────────────────────
// Phase 14 — Memory Consolidation. Learned skills from successful sessions.

pub const SCHEMA_V80_AGENT_PROCEDURES: &str = r#"
CREATE TABLE IF NOT EXISTS agent_procedures (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                TEXT NOT NULL UNIQUE,
    trigger_pattern     TEXT NOT NULL,
    steps               JSONB NOT NULL DEFAULT '[]',
    success_rate        FLOAT CHECK (success_rate BETWEEN 0 AND 1),
    usage_count         INT DEFAULT 0,
    last_used           TIMESTAMPTZ,
    created_from_session UUID REFERENCES agent_sessions(id),
    created_at          TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_agent_procedures_name
    ON agent_procedures(name);

CREATE INDEX IF NOT EXISTS idx_agent_procedures_trigger
    ON agent_procedures(trigger_pattern);
"#;

// ─── V81: Security Hardening ──────────────────────────────────────────────
// Phase 5 — Tool allow-lists, timeout enforcement, structured tool audit.

pub const SCHEMA_V81_SECURITY_HARDENING: &str = r#"
-- Per-step tool allow-list: empty array = all tools allowed.
ALTER TABLE agent_steps ADD COLUMN IF NOT EXISTS allowed_tools JSONB NOT NULL DEFAULT '[]';

-- Task timeout: NULL = default (300s), explicit override in payload.
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS timeout_secs INT;

-- Enhanced tool audit log for Postgres (separate from legacy SQLite audit_log).
CREATE TABLE IF NOT EXISTS tool_audit_log (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id      UUID REFERENCES agent_sessions(id) ON DELETE SET NULL,
    step_id         UUID REFERENCES agent_steps(id) ON DELETE SET NULL,
    agent_id        TEXT NOT NULL DEFAULT 'unknown',
    tool_name       TEXT NOT NULL,
    params_json     JSONB NOT NULL DEFAULT '{}',
    prompt_hash     TEXT,
    outcome         TEXT NOT NULL CHECK (outcome IN ('success', 'failure', 'denied', 'timeout')),
    error           TEXT,
    duration_ms     INT,
    worker_name       TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_tool_audit_session ON tool_audit_log(session_id);
CREATE INDEX IF NOT EXISTS idx_tool_audit_tool ON tool_audit_log(tool_name);
CREATE INDEX IF NOT EXISTS idx_tool_audit_outcome ON tool_audit_log(outcome);
CREATE INDEX IF NOT EXISTS idx_tool_audit_created ON tool_audit_log(created_at);
"#;

pub const SCHEMA_V82_RENAME_FLEET_NODE_SSH_KEYS: &str = r#"
-- ─── V82: rename fleet_worker_ssh_keys → fleet_workers_ssh_keys ──────────────
-- Aligns the table name with the broader fleet_workers → fleet_workers rename
-- target (see memory: fleet_workers_naming). The underlying FK still points
-- at fleet_workers(name) until that table is renamed in a later migration.
--
-- Idempotent: skips work if the new table already exists OR the old table is
-- missing (e.g., on a fresh install that ran V12 with the new name).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
               WHERE table_schema = 'public' AND table_name = 'fleet_worker_ssh_keys')
       AND NOT EXISTS (SELECT 1 FROM information_schema.tables
               WHERE table_schema = 'public' AND table_name = 'fleet_workers_ssh_keys')
    THEN
        ALTER TABLE fleet_worker_ssh_keys RENAME TO fleet_workers_ssh_keys;
        ALTER INDEX IF EXISTS idx_ssh_keys_node_purpose
            RENAME TO idx_workers_ssh_keys_node_purpose;
    END IF;
END $$;

-- If a fresh install never had the old name, create the new table directly.
CREATE TABLE IF NOT EXISTS fleet_workers_ssh_keys (
    worker_name    TEXT NOT NULL REFERENCES fleet_workers(name) ON DELETE CASCADE,
    key_purpose  TEXT NOT NULL,
    public_key   TEXT NOT NULL,
    key_type     TEXT NOT NULL,
    fingerprint  TEXT NOT NULL,
    added_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (worker_name, fingerprint)
);
CREATE INDEX IF NOT EXISTS idx_workers_ssh_keys_node_purpose
    ON fleet_workers_ssh_keys (worker_name, key_purpose);

-- Compatibility view for callers still on the old name during the upgrade
-- window. Dropped in a future migration once all daemons have been pushed.
CREATE OR REPLACE VIEW fleet_worker_ssh_keys AS
    SELECT * FROM fleet_workers_ssh_keys;
"#;

pub const SCHEMA_V83_RENAME_FLEET_NODES: &str = r#"
-- ─── V83: rename fleet_workers → fleet_workers ───────────────────────────────
-- Final step of the long-running fleet_workers → fleet_workers rename
-- (see memory: fleet_workers_naming). The 8 existing FK columns
-- (`worker_name`) continue to reference the renamed table — PostgreSQL
-- updates FK targets automatically across ALTER TABLE RENAME.
--
-- A compatibility VIEW preserves the old name so the 131 unrenamed
-- Rust call sites (37 files) keep working without a coordinated
-- redeploy. Single-table views are auto-updatable in Postgres, so
-- INSERTs / UPDATEs through `fleet_workers` still hit fleet_workers.
--
-- Idempotent: no-op on fresh installs and on already-migrated DBs.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
               WHERE table_schema = 'public' AND table_name = 'fleet_workers'
                 AND table_type = 'BASE TABLE')
       AND NOT EXISTS (SELECT 1 FROM information_schema.tables
               WHERE table_schema = 'public' AND table_name = 'fleet_workers'
                 AND table_type = 'BASE TABLE')
    THEN
        ALTER TABLE fleet_workers RENAME TO fleet_workers;
        -- Rename any indexes that were named after the old table.
        ALTER INDEX IF EXISTS fleet_nodes_pkey RENAME TO fleet_workers_pkey;
        ALTER INDEX IF EXISTS fleet_nodes_name_key RENAME TO fleet_workers_name_key;
    END IF;
END $$;

-- Fresh installs may never have had the old name.
CREATE TABLE IF NOT EXISTS fleet_workers (
    name              TEXT PRIMARY KEY,
    ip                TEXT NOT NULL,
    ssh_user          TEXT NOT NULL DEFAULT 'root',
    ram_gb            INTEGER NOT NULL DEFAULT 0,
    cpu_cores         INTEGER NOT NULL DEFAULT 0,
    os                TEXT NOT NULL DEFAULT '',
    role              TEXT NOT NULL DEFAULT 'worker',
    election_priority INTEGER NOT NULL DEFAULT 50,
    hardware          TEXT NOT NULL DEFAULT '',
    alt_ips           JSONB NOT NULL DEFAULT '[]'::jsonb,
    capabilities      JSONB NOT NULL DEFAULT '{}'::jsonb,
    preferences       JSONB NOT NULL DEFAULT '{}'::jsonb,
    resources         JSONB NOT NULL DEFAULT '{}'::jsonb,
    status            TEXT NOT NULL DEFAULT 'online',
    registered_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    runtime           TEXT NOT NULL DEFAULT 'unknown',
    models_dir        TEXT NOT NULL DEFAULT '~/models',
    disk_quota_pct    INTEGER NOT NULL DEFAULT 80,
    sub_agent_count   INTEGER NOT NULL DEFAULT 1,
    gh_account        TEXT,
    tooling           JSONB NOT NULL DEFAULT '{}'::jsonb
);

-- Compatibility view so the 131 unrenamed call sites keep resolving.
-- Single-table views are auto-updatable in Postgres ≥ 9.3, so
-- INSERT / UPDATE / DELETE via `fleet_workers` continue to work.
CREATE OR REPLACE VIEW fleet_workers AS
    SELECT * FROM fleet_workers;
"#;

pub const SCHEMA_V84_RENAME_NODE_NAME_COLUMN: &str = r#"
-- ─── V84: rename fleet_workers_ssh_keys.worker_name → worker_name ────────────
-- Finishes the node → worker rename inside fleet_workers_ssh_keys. The FK
-- to fleet_workers(name) is preserved automatically across the rename.
--
-- The fleet_worker_ssh_keys compatibility view is rewritten to alias
-- `worker_name AS worker_name` so any unrenamed callers (SELECT or
-- INSERT INTO ... (worker_name, ...)) continue to work during the
-- upgrade window. The view stays auto-updatable because the alias is
-- a single-column rename of a base-table column.
--
-- Idempotent: no-op if the column has already been renamed.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema='public' AND table_name='fleet_workers_ssh_keys'
          AND column_name='worker_name'
    ) THEN
        ALTER TABLE fleet_workers_ssh_keys RENAME COLUMN worker_name TO worker_name;
    END IF;
END $$;

-- Rebuild the compat view to expose `worker_name` under the old name.
DROP VIEW IF EXISTS fleet_worker_ssh_keys;
CREATE VIEW fleet_worker_ssh_keys AS
    SELECT
        worker_name AS worker_name,
        key_purpose,
        public_key,
        key_type,
        fingerprint,
        added_at
    FROM fleet_workers_ssh_keys;
"#;

pub const SCHEMA_V85_DROP_COMPAT_VIEWS: &str = r#"
-- ─── V85: drop the fleet_worker_* compat views ───────────────────────────────
-- The full rename across all Rust call sites is complete (0 remaining
-- `fleet_nodes` or `fleet_worker_ssh_keys` references in runtime code as of
-- this commit), so the compatibility shims from V82/V83/V84 are no longer
-- load-bearing. Drop them to remove the rename debt.
--
-- Idempotent: views may not exist on fresh installs that never ran V82/V83.
DROP VIEW IF EXISTS fleet_worker_ssh_keys;
DROP VIEW IF EXISTS fleet_nodes;
"#;

pub const SCHEMA_V87_RENAME_NODE_NAME_COLUMNS: &str = r#"
-- ─── V87: rename worker_name → worker_name across the remaining tables ──────
-- Plan 14's fleet_workers naming target leaked into many downstream tables
-- that each had a `worker_name` column FK'd to fleet_workers(name). V82-V86
-- handled the core registry; this migration finishes the column-side rename
-- for: fleet_models, fleet_model_library, fleet_model_deployments,
-- fleet_model_jobs, fleet_disk_usage, fleet_tools, fleet_tool_usage,
-- sessions, audit_log, tool_audit_log.
--
-- No compat views — the Rust code update is shipped in the same commit
-- (see the perl mass-rename + build verification).
-- Idempotent: each ALTER is wrapped in a column-exists check.
DO $$
DECLARE
    t TEXT;
    tables TEXT[] := ARRAY[
        'fleet_models', 'fleet_model_library', 'fleet_model_deployments',
        'fleet_model_jobs', 'fleet_disk_usage', 'fleet_tools',
        'fleet_tool_usage', 'sessions', 'audit_log', 'tool_audit_log'
    ];
BEGIN
    FOREACH t IN ARRAY tables LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.columns
            WHERE table_schema='public' AND table_name=t
              AND column_name = 'no' || 'de_name'  -- literal split so the
            -- repo-wide mass-rename perl pass doesn't transmute this migration's
            -- idempotency check into a no-op (the rename literally swept
            -- through SQL string contents otherwise).
        ) THEN
            EXECUTE format(
                'ALTER TABLE %I RENAME COLUMN %I TO worker_name',
                t,
                'no' || 'de_name'
            );
            RAISE NOTICE 'renamed % column → worker_name', t;
        END IF;
    END LOOP;
END $$;
"#;

pub const SCHEMA_V86_DROP_FLEET_MEMBERS: &str = r#"
-- ─── V86: drop fleet_members table ─────────────────────────────────────────
-- fleet_members was a redundant projection of fleet_workers — same
-- election_priority, role, runtime, gh_account, models_dir, disk_quota_pct
-- columns existed in both tables, with fleet_members joining `computers` by
-- UUID and fleet_workers joining by name. Every consumer
-- (leader_tick, pulse_api list/leader endpoints, fleet_cmd sanity checks)
-- has been migrated to fleet_workers via JOIN computers c ON c.name = fw.name.
--
-- Idempotent.
DROP TABLE IF EXISTS fleet_members CASCADE;
"#;

pub const SCHEMA_V88_RENAME_FLEET_NODE_RUNTIME: &str = r#"
-- ─── V88: rename fleet_node_runtime → fleet_worker_runtime ──────────────────
-- Last surviving "fleet_node_*" table — the Postgres-side runtime registry
-- created by V14. Renamed to match the V83/V87 fleet_worker_* family.
-- Idempotent: skips if the new name already exists, or the old name is gone.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
        WHERE table_schema = 'public' AND table_name = 'fleet_node_runtime'
          AND table_type = 'BASE TABLE'
    ) AND NOT EXISTS (
        SELECT 1 FROM information_schema.tables
        WHERE table_schema = 'public' AND table_name = 'fleet_worker_runtime'
          AND table_type = 'BASE TABLE'
    ) THEN
        ALTER TABLE fleet_node_runtime RENAME TO fleet_worker_runtime;
    END IF;
END $$;
"#;

pub const SCHEMA_V89_GITHUB_SSH_ALIASES: &str = r#"
-- ─── V89: GitHub SSH aliases registry ──────────────────────────────────────
-- One row per `Host github.com-foo` block that should exist on every fleet
-- computer's `~/.ssh/config`. Lets a new computer bootstrap the same GitHub
-- identity setup as Taylor on enrollment, without anything hardcoded.
--
-- Private + public key material lives in `fleet_secrets` under well-known
-- keys (`github_ssh_<file>_priv` / `github_ssh_<file>_pub`) — separating
-- the *config* (this table) from the *secret material* (fleet_secrets).
--
-- A `Host github.com-venkat / IdentityFile ~/.ssh/id_venkat / IdentitiesOnly yes`
-- block becomes one row with identity_file='~/.ssh/id_venkat'.
CREATE TABLE IF NOT EXISTS github_ssh_aliases (
    alias_name       text PRIMARY KEY,
    hostname         text NOT NULL DEFAULT 'github.com',
    ssh_user         text NOT NULL DEFAULT 'git',
    identity_file    text NOT NULL,
    identities_only  boolean NOT NULL DEFAULT true,
    description      text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);

-- Seed Taylor's existing aliases. ON CONFLICT DO NOTHING keeps the migration
-- idempotent so we can rerun it without clobbering operator edits.
INSERT INTO github_ssh_aliases (alias_name, identity_file, description) VALUES
    ('github.com-venkat', '~/.ssh/id_venkat',
     'Primary venkatyarl identity — canonical account post-migration'),
    ('github.com-taylor', '~/.ssh/id_taylor',
     'Legacy taylor-oclaw account — kept until full repo migration completes'),
    ('github.com',        '~/.ssh/id_rsa',
     'Default github.com SSH identity used when no -<account> alias is selected')
ON CONFLICT (alias_name) DO NOTHING;
"#;

pub const SCHEMA_V90_DEPLOYMENT_DESIRED_STATE: &str = r#"
-- ─── V90: desired_state on fleet_model_deployments ─────────────────────────
-- The deployment_reconciler today only adopts existing processes (live → DB);
-- when a process dies, the reconciler DELETES the row, so on the next tick
-- we have no record that the operator wanted this LLM up. Add `desired_state`
-- so the row survives a missing process: 'active' = should be running and
-- the respawn-aware reconciler brings it back; 'retired' = operator unloaded,
-- reconciler should clean up and stop.
--
-- Default 'active' means existing rows are all managed by the new logic.
ALTER TABLE fleet_model_deployments
    ADD COLUMN IF NOT EXISTS desired_state text NOT NULL DEFAULT 'active'
    CHECK (desired_state IN ('active', 'retired'));

CREATE INDEX IF NOT EXISTS fleet_model_deployments_desired_state_idx
    ON fleet_model_deployments (desired_state, worker_name);
"#;

// ─── V95: resize brain_vault_nodes.embedding to vector(1024) for bge-m3 ──────
//
// V78 created `brain_vault_nodes.embedding` as `vector(384)` — appropriate for
// MiniLM-class embedders. bge-m3 (V91) outputs 1024-dim vectors, so any
// INSERT now fails with `expected 384 dimensions, not 1024`.
//
// pgvector requires a fixed dim per column, so we DROP and re-CREATE.
// Drop preserves no embeddings (there were 0 — V78 was a no-op until the
// pgvector image swap on 2026-05-18). Re-creates the HNSW index too.
//
// If the column was somehow populated with 384-dim vectors, this is destructive
// to those rows' embedding only — node metadata is untouched, and a backfill
// loop can repopulate from bge-m3.
pub const SCHEMA_V95_BGE_EMBEDDING_DIM: &str = r#"
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector') THEN
        ALTER TABLE brain_vault_nodes DROP COLUMN IF EXISTS embedding;
        ALTER TABLE brain_vault_nodes ADD  COLUMN embedding vector(1024);
        CREATE INDEX IF NOT EXISTS idx_vault_nodes_embedding_1024
            ON brain_vault_nodes USING hnsw (embedding vector_cosine_ops)
            WHERE embedding IS NOT NULL;
    END IF;
END $$;
"#;

// ─── V94: align bge-m3 / bge-reranker-v2-m3 quant to upstream filename ───────
//
// V91 seeded the bge-m3 and bge-reranker-v2-m3 variants with quant = "F16".
// The upstream `gpustack/bge-m3-GGUF` and `gpustack/bge-reranker-v2-m3-GGUF`
// repos name their non-quantized GGUF as `*-FP16.gguf` — so `ff model
// download` (with the quant-narrowed allow pattern from `136ce94bf`)
// produced the glob `*F16*.gguf` which doesn't match `bge-m3-FP16.gguf`
// (the F and 16 aren't contiguous).
//
// Caught 2026-05-18 dispatching bge-m3 to veronica — task exited 1 with
// "no files matched in gpustack/bge-m3-GGUF (after allow/deny filters)".
pub const SCHEMA_V94_BGE_QUANT_FIX: &str = r#"
UPDATE fleet_model_catalog
   SET variants = jsonb_build_array(
       jsonb_build_object(
           'runtime', 'llama.cpp',
           'quant',   'FP16',
           'hf_repo', 'gpustack/bge-m3-GGUF',
           'size_gb', 2
       )
   ),
   updated_at = NOW()
 WHERE id = 'bge-m3';

UPDATE fleet_model_catalog
   SET variants = jsonb_build_array(
       jsonb_build_object(
           'runtime', 'llama.cpp',
           'quant',   'FP16',
           'hf_repo', 'gpustack/bge-reranker-v2-m3-GGUF',
           'size_gb', 1
       )
   ),
   updated_at = NOW()
 WHERE id = 'bge-reranker-v2-m3';
"#;

// ─── V93: backfill fleet_workers.runtime from computers.os_family + gpu_kind ───
//
// fleet_workers.runtime defaults to 'native' at enrollment (acknowledged
// bogus in status_cmd.rs:79 — "bogus enrollment default for every host").
// `native` matches no model_catalog.variants[].runtime value, so
// `ff model download-batch --node <n>` produces a deferred shell command
// like `ff model download bge-m3 --runtime native` that fails on the
// worker with `no variant for runtime 'native' on 'bge-m3'`.
//
// Surfaced 2026-05-18 dispatching bge-m3 / bge-reranker-v2-m3 to veronica
// and deepseek-r1-distill-qwen-32b to lily. All three downloads exited 1
// on first claim with the runtime-mismatch error.
//
// Authoritative mapping per `reference_runtime_choice_policy` memory:
//
//   computers.os_family    computers.gpu_kind          → runtime
//   ──────────────────    ──────────────────          ─────────
//   macos                  *                            mlx
//   linux-dgx              *                            vllm
//   linux-ubuntu           nvidia_cuda                  vllm
//   linux-ubuntu           apple_silicon                mlx   (won't happen)
//   linux-ubuntu           other / none / amd_rocm /
//                          integrated                   llama.cpp
//   windows*               *                            llama.cpp
//
// Only overwrites rows currently set to 'native' or 'unknown' — operator
// overrides ('llama.cpp' / 'mlx' / 'vllm' chosen by hand) are preserved.
pub const SCHEMA_V93_BACKFILL_FLEET_WORKER_RUNTIME: &str = r#"
UPDATE fleet_workers fw
   SET runtime = CASE
       WHEN c.os_family = 'macos'                                THEN 'mlx'
       WHEN c.os_family = 'linux-dgx'                            THEN 'vllm'
       WHEN c.os_family = 'linux-ubuntu' AND c.gpu_kind = 'nvidia_cuda' THEN 'vllm'
       WHEN c.os_family LIKE 'linux%'                            THEN 'llama.cpp'
       WHEN c.os_family LIKE 'windows%'                          THEN 'llama.cpp'
       ELSE fw.runtime
       END
  FROM computers c
 WHERE LOWER(fw.name) = LOWER(c.name)
   AND fw.runtime IN ('native', 'unknown', '');
"#;

// ─── V92: restore `-p ff-terminal` in Linux ff_git/forgefleetd_git playbooks ──
//
// V56 added `cargo build --release -p forge-fleet -p ff-terminal` to the Linux
// playbooks so workers rebuilt BOTH binaries (daemon AND CLI). V63 was meant
// to drop the `NEED_BUILD` shortcut introduced in V61 but accidentally dropped
// the `-p ff-terminal` flag with it, so since then `ff fleet upgrade ff_git`
// has only been rebuilding `forgefleetd` — the `target/release/ff` binary
// installed on each worker has been stale, sometimes by days.
//
// Surfaced 2026-05-18 while deploying the new embedder / reranker / reasoning
// catalog rows to veronica + lily: the deferred-task playbook exited 0 and
// reported "completed", but `ff --version` on each worker still showed the
// pre-deploy SHA. Confirmed the regression matches the stored
// `feedback_ff_build_needs_package` note.
//
// V92 restores `-p ff-terminal` on all four Linux variants. macOS (V57) is
// unchanged — it was correct.
pub const SCHEMA_V92_FF_GIT_LINUX_PARITY: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           jsonb_set(
               jsonb_set(
                   jsonb_set(
                       upgrade_playbook,
                       '{linux-ubuntu}',
                       to_jsonb(
                           'export PATH="$HOME/.cargo/bin:$PATH" && '
                        || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                        || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                        || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                        || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                        || 'git fetch origin main && '
                        || 'git reset --hard origin/main && '
                        || 'cargo build --release -p forge-fleet -p ff-terminal && '
                        || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                        || 'install -m 755 target/release/ff ~/.local/bin/ff && '
                        || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
                        || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
                       )
                   ),
                   '{linux-dgx}',
                   to_jsonb(
                       'export PATH="$HOME/.cargo/bin:$PATH" && '
                    || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                    || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                    || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                    || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                    || 'git fetch origin main && '
                    || 'git reset --hard origin/main && '
                    || 'cargo build --release -p forge-fleet -p ff-terminal && '
                    || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                    || 'install -m 755 target/release/ff ~/.local/bin/ff && '
                    || 'systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; '
                    || 'systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
                   )
               ),
               '{linux-ubuntu-build-only}',
               to_jsonb(
                   'export PATH="$HOME/.cargo/bin:$PATH" && '
                || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
                || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
                || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
                || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
                || 'git fetch origin main && '
                || 'git reset --hard origin/main && '
                || 'cargo build --release -p forge-fleet -p ff-terminal && '
                || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
                || 'install -m 755 target/release/ff ~/.local/bin/ff'
               )
           ),
           '{linux-dgx-build-only}',
           to_jsonb(
               'export PATH="$HOME/.cargo/bin:$PATH" && '
            || 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && '
            || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && '
            || '{ [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && '
            || 'cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && '
            || 'git fetch origin main && '
            || 'git reset --hard origin/main && '
            || 'cargo build --release -p forge-fleet -p ff-terminal && '
            || 'install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && '
            || 'install -m 755 target/release/ff ~/.local/bin/ff'
           )
       )
 WHERE id IN ('ff_git', 'forgefleetd_git');
"#;

// ─── V91: Seed fleet_model_catalog with embedder / reranker / reasoning ────
//
// Operator goal (2026-05-18): turn the fleet from "8 chat models in a trench
// coat" into a proper retrieve→reason→generate→judge portfolio. The chat /
// code / vision axes are already covered. The missing three:
//
//   * bge-m3                     — multilingual embedder (dense+sparse+ColBERT)
//   * bge-reranker-v2-m3         — cross-encoder reranker for top-K rescoring
//   * deepseek-r1-distill-qwen-32b — visible chain-of-thought reasoning
//
// `preferred_workloads` carries the new task tags ('embedding', 'reranking',
// 'reasoning'); `model_runtime::load_model` reads these and switches
// llama-server flags accordingly (V91 ships paired with that runtime patch).
pub const SCHEMA_V91_TASK_MODELS: &str = r#"
INSERT INTO fleet_model_catalog
    (id, name, family, parameters, tier, description, gated,
     preferred_workloads, variants, updated_at)
VALUES
    ('bge-m3',
     'BGE-M3 (multilingual embedder)',
     'bge',
     '568M',
     1,
     'BAAI BGE-M3 — multilingual embedder (100+ languages). Dense, sparse, and ColBERT-style multi-vector retrieval in a single model. Powers brain_search, vault retrieval, pgvector lookups, RAG. Serves /v1/embeddings via llama-server --embeddings.',
     false,
     '["embedding"]'::jsonb,
     '[{"runtime": "llama.cpp", "quant": "F16",    "hf_repo": "gpustack/bge-m3-GGUF",          "size_gb": 2}]'::jsonb,
     NOW()),

    ('bge-reranker-v2-m3',
     'BGE Reranker v2 M3 (cross-encoder)',
     'bge',
     '568M',
     1,
     'BAAI BGE Reranker v2 M3 — cross-encoder reranker for retrieval top-K rescoring. Pairs with bge-m3: embedder finds top-50 candidates, reranker rescores to top-5. Last quality gate before docs are fed to an LLM. Serves /v1/rerank via llama-server --reranking.',
     false,
     '["reranking"]'::jsonb,
     '[{"runtime": "llama.cpp", "quant": "F16",    "hf_repo": "gpustack/bge-reranker-v2-m3-GGUF", "size_gb": 1}]'::jsonb,
     NOW()),

    ('deepseek-r1-distill-qwen-32b',
     'DeepSeek-R1-Distill-Qwen-32B',
     'deepseek',
     '32B',
     2,
     'DeepSeek-R1-Distill-Qwen-32B — visible chain-of-thought reasoning. Distilled from DeepSeek-R1 (671B MoE) into a dense 32B Qwen base. Emits <think>...</think> blocks before the final answer; downstream synthesizers must strip them. Use for fleet planner / consensus arbitration / "this is hard, think first" loops where Qwen3.6 and Gemma-4 plateau.',
     false,
     '["reasoning", "math", "planning"]'::jsonb,
     '[{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "unsloth/DeepSeek-R1-Distill-Qwen-32B-GGUF", "size_gb": 20}]'::jsonb,
     NOW())
ON CONFLICT (id) DO UPDATE SET
    name = EXCLUDED.name,
    family = EXCLUDED.family,
    parameters = EXCLUDED.parameters,
    tier = EXCLUDED.tier,
    description = EXCLUDED.description,
    gated = EXCLUDED.gated,
    preferred_workloads = EXCLUDED.preferred_workloads,
    variants = EXCLUDED.variants,
    updated_at = NOW();
"#;

// ─── V96: register pipeline_llm_default alias on the gateway port ────────────
//
// Operator convention: every ForgeFleet-owned port is 5-digit and registered
// in port_registry. ff-pipeline (LLM dispatch library) previously hardcoded a
// 4-digit default `http://127.0.0.1:4000` which (a) violated the convention,
// (b) was unregistered, (c) collided with Obsidian's local REST plugin in
// 2026-05 — every fleet_crew / pipeline LLM call posted to the note-taking app
// and got back HTML 404s.
//
// The fix is paired with `ff-pipeline::executor.rs` switching its default to
// `http://127.0.0.1:51002` (the gateway, which actually serves
// /v1/chat/completions and routes onward). This migration just records the
// secondary role on the existing 51002 row so the convention is discoverable:
// grep "pipeline_llm_default" → land in the registry, not in dead code.
pub const SCHEMA_V96_REGISTER_PIPELINE_LLM_ALIAS: &str = r#"
UPDATE port_registry
   SET metadata = COALESCE(metadata, '{}'::jsonb) || jsonb_build_object(
       'aliases', jsonb_build_array(
           'pipeline_llm_default',
           'mcp_chat_completions_proxy'
       ),
       'protocols', jsonb_build_array(
           'openai_chat_completions',
           'dashboard'
       )
   ),
   updated_at = NOW()
 WHERE port = 51002;
"#;

// ─── V97: redis + NATS host ports → 5-digit canonical ────────────────────────
//
// Phase B of the canonical-5-digit-ports rollout. Container-internal ports
// stay native (6379 redis, 4222 NATS); the host-side docker-compose mapping
// changes to 5-digit. This migration keeps port_registry in sync with the new
// docker-compose.yml host mappings.
//
// Old → new:
//   redis_primary        6380  → 56379
//   redis_replica        6381  → 56380
//   nats_client          4222  → 54222
//   nats_cluster         6222  → 56222
//   nats_monitoring      8222  → 58222   (new row; previously not registered)
//
// Strategy: DELETE the deprecated 4-digit rows + INSERT the new 5-digit ones.
// We don't UPDATE-in-place because PRIMARY KEY (port) means the old port and
// new port are different rows. Operator-written metadata on the old rows is
// preserved by merging it into the new row's metadata->>'history' object.
pub const SCHEMA_V97_REDIS_NATS_5DIGIT: &str = r#"
-- Insert the new canonical 5-digit rows first.
INSERT INTO port_registry
    (port, service, kind, description, exposed_on, scope, managed_by, status, metadata)
VALUES
  (56379, 'redis_primary', 'database',
   'Redis primary — host 56379 maps to container 6379. Canonical 5-digit per port-convention.',
   'taylor', 'lan', 'docker compose', 'active',
   jsonb_build_object('previous_port', 6380, 'remapped_at', '2026-05-18')),

  (56380, 'redis_replica', 'database',
   'Redis replica — host 56380 maps to container 6379. Future, on Marcus.',
   'marcus', 'lan', 'docker compose follower', 'planned',
   jsonb_build_object('previous_port', 6381, 'remapped_at', '2026-05-18')),

  (54222, 'nats_client', 'coordination',
   'NATS client connections — host 54222 maps to container 4222.',
   'nats_cluster_members', 'lan', 'docker compose', 'active',
   jsonb_build_object('previous_port', 4222, 'remapped_at', '2026-05-18')),

  (56222, 'nats_cluster', 'coordination',
   'NATS inter-node cluster — host 56222 maps to container 6222.',
   'nats_cluster_members', 'lan', 'docker compose', 'planned',
   jsonb_build_object('previous_port', 6222, 'remapped_at', '2026-05-18')),

  (58222, 'nats_monitoring', 'coordination',
   'NATS HTTP monitoring — host 58222 maps to container 8222 (LAN only).',
   'taylor', 'lan', 'docker compose', 'active',
   jsonb_build_object('previous_port', 8222, 'remapped_at', '2026-05-18'))
ON CONFLICT (port) DO UPDATE
   SET service     = EXCLUDED.service,
       description = EXCLUDED.description,
       status      = EXCLUDED.status,
       metadata    = port_registry.metadata || EXCLUDED.metadata,
       updated_at  = NOW();

-- Mark old 4-digit rows deprecated rather than deleting them so operators
-- looking at historical logs can still resolve `port=6380` → `redis_primary
-- (deprecated, see 56379)` without grep diving.
UPDATE port_registry
   SET status = 'deprecated',
       description = description || ' [DEPRECATED 2026-05-18 — moved to 5-digit host port; see metadata.replaced_by]',
       metadata = COALESCE(metadata, '{}'::jsonb) || jsonb_build_object(
           'replaced_by', CASE port
               WHEN 6380 THEN 56379
               WHEN 6381 THEN 56380
               WHEN 4222 THEN 54222
               WHEN 6222 THEN 56222
               WHEN 8222 THEN 58222
           END,
           'deprecated_at', '2026-05-18',
           'reason', 'canonical-5-digit-ports convention'
       ),
       updated_at = NOW()
 WHERE port IN (6380, 6381, 4222, 6222, 8222);
"#;

// V98: Correct gemma4-31b-it llama.cpp variant repo (bartowski has never
// quantized this model; unsloth has the canonical GGUF with 1.1M downloads).
// V91 shipped with the wrong hf_repo; this migration fixes any DB that
// already ran V91. New DBs running V91→V98 in order end up correct.
pub const SCHEMA_V98_GEMMA4_REPO_FIX: &str = r#"
UPDATE fleet_model_catalog
   SET variants = jsonb_build_array(
         jsonb_build_object(
            'runtime', 'llama.cpp', 'quant', 'Q4_K_M',
            'hf_repo', 'unsloth/gemma-4-31B-it-GGUF', 'size_gb', 19),
         jsonb_build_object(
            'runtime', 'mlx', 'quant', '4bit',
            'hf_repo', 'mlx-community/gemma-4-31b-it-4bit', 'size_gb', 18)
       ),
       updated_at = NOW()
 WHERE id = 'gemma4-31b-it'
   AND variants::text LIKE '%bartowski/gemma-4-31b-it-GGUF%';
"#;

// V99: Register a `default` pool alias in fleet_task_coverage so the
// gateway can route ff-pipeline's `model="default"` requests to a real
// pool of healthy chat models. Without this, fleet_crew gets a 503
// "no healthy backend for model 'default'" because the tier router
// can't parse "default" as a tier selector. FA.2.
pub const SCHEMA_V99_DEFAULT_POOL_ALIAS: &str = r#"
INSERT INTO fleet_task_coverage (task, alias, preferred_model_ids, priority, notes)
VALUES (
  'default-chat',
  'default',
  '["qwen36-35b-a3b", "qwen3-coder-30b", "gemma4-31b-it"]'::jsonb,
  'critical',
  'Pool alias for ff-pipeline / fleet_crew when no explicit model is set. Mix of 3 model families (qwen-MoE chat, qwen coder, gemma judge) across multiple healthy nodes for failover.'
)
ON CONFLICT (task) DO UPDATE
SET alias               = EXCLUDED.alias,
    preferred_model_ids = EXCLUDED.preferred_model_ids,
    priority            = EXCLUDED.priority,
    notes               = EXCLUDED.notes;
"#;

// V100: Retire qwen2.5 catalog entries fleet-wide. Qwen released the
// Qwen3 family, our text-gen workloads run on qwen3-coder-30b /
// qwen36-35b-a3b, and the new vision flagship is Qwen3-VL-30B-A3B.
// The qwen2.5* rows in fleet_model_catalog + model_catalog were
// referencing repos we no longer download or serve. (Q3.1, 2026-05-19.)
pub const SCHEMA_V100_RETIRE_QWEN25: &str = r#"
DELETE FROM fleet_model_catalog
 WHERE id IN ('qwen25-coder-32b', 'qwen25-72b', 'qwen25-coder-7b',
              'qwen25-vl-7b', 'qwen25-vl-72b', 'qwen25-72b-taxonomy',
              'qwen2.5-vl-72b');

DELETE FROM model_catalog
 WHERE id IN ('qwen25-coder-32b', 'qwen25-72b', 'qwen25-coder-7b',
              'qwen25-vl-7b', 'qwen25-vl-72b', 'qwen25-72b-taxonomy',
              'qwen2.5-vl-72b', 'qwen2-5-1-5b-instruct', 'qwen2-5-7b-instruct');

-- Qwen3-VL replacements: 8B (small) and 30B-A3B (flagship MoE).
INSERT INTO fleet_model_catalog (id, name, family, parameters, tier, gated, preferred_workloads, variants, description)
VALUES
  ('qwen3-vl-8b', 'Qwen3-VL-8B-Instruct', 'qwen', '8B', 1, false,
   '["vision","chat"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"Q4_K_M","hf_repo":"Qwen/Qwen3-VL-8B-Instruct-GGUF","size_gb":5.0},
     {"runtime":"mlx","quant":"4bit","hf_repo":"mlx-community/Qwen3-VL-8B-Instruct-4bit","size_gb":5.0}]'::jsonb,
   'Qwen3 vision-language — strong OCR + chart understanding.'),
  ('qwen3-vl-30b-a3b', 'Qwen3-VL-30B-A3B-Instruct', 'qwen', '30B', 3, false,
   '["vision","reasoning","documents"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"Q4_K_M","hf_repo":"Qwen/Qwen3-VL-30B-A3B-Instruct-GGUF","size_gb":18.0},
     {"runtime":"mlx","quant":"4bit","hf_repo":"mlx-community/Qwen3-VL-30B-A3B-Instruct-4bit","size_gb":18.0},
     {"runtime":"vllm","quant":"fp16","hf_repo":"Qwen/Qwen3-VL-30B-A3B-Instruct","size_gb":60.0}]'::jsonb,
   'Qwen3-VL flagship MoE — multi-image, video, document reasoning.')
ON CONFLICT (id) DO UPDATE SET
  name = EXCLUDED.name,
  variants = EXCLUDED.variants,
  preferred_workloads = EXCLUDED.preferred_workloads,
  description = EXCLUDED.description,
  updated_at = NOW();
"#;

// V101: Persist the forgefleetd_git upgrade_playbook update into a
// migration so fresh DBs get the right behavior (the V63 seed shipped
// without pkill + without -j 2). Adds:
// - `pkill -f 'forgefleetd --worker-name'` BEFORE systemctl restart on
//   Linux entries (UPGRADE.1 — legacy zombie removal).
// - `-j 2` to the cargo build on linux-dgx so 4-core / RAM-tight DGX
//   Sparks don't OOM during LLVM codegen (DGX.1).
pub const SCHEMA_V101_UPGRADE_PLAYBOOK_REFRESH: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
     'macos',
       'export PATH="$HOME/.cargo/bin:$PATH" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/forgefleetd ~/.local/bin/ff && launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd',
     'linux-ubuntu',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && { [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && pkill -f ''forgefleetd --worker-name'' 2>/dev/null; sleep 1; systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service',
     'linux-dgx',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && { [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal -j 2 && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && pkill -f ''forgefleetd --worker-name'' 2>/dev/null; sleep 1; systemctl --user reset-failed forgefleetd.service forgefleet-node.service forgefleet-daemon.service 2>/dev/null; systemctl --user restart forgefleetd.service || systemctl --user restart forgefleet-node.service || systemctl --user restart forgefleet-daemon.service'
   )
 WHERE id='forgefleetd_git';
"#;

// V102: Make the forgefleetd_git upgrade playbook RESTART-SAFE.
// V101 used `systemctl --user restart forgefleetd.service` which is
// synchronous — the daemon stops, sending SIGTERM to all its children
// including the defer-worker that's executing this playbook. The wave
// task then exits with code -1 even though the upgrade succeeded.
// WAVE.1 (2026-05-19): 10+ wave 4 tasks reported failed because of this.
//
// Fix: detach the restart sequence into a backgrounded `nohup` subshell
// so the parent (the defer-worker shell) returns success BEFORE the
// daemon cycles. Also use `--no-block` and a targeted zombie-kill
// that excludes the systemd MainPID, preventing the previous
// "kill --worker-name → kills systemd daemon" friendly-fire.
pub const SCHEMA_V102_WAVE_SELF_KILL_FIX: &str = r#"
-- See V101 for prior playbook. V102 wraps the kill+restart in a
-- detached nohup subshell so the wave task can return success
-- BEFORE the daemon cycles.
UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
     'macos',
       'export PATH="$HOME/.cargo/bin:$PATH" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/forgefleetd ~/.local/bin/ff && ( nohup sh -c "sleep 1; launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd" >/dev/null 2>&1 </dev/null & disown )',
     'linux-ubuntu',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && { [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && ( nohup sh -c "sleep 2; MAIN=$(systemctl --user show -p MainPID forgefleetd.service 2>/dev/null | cut -d= -f2); for p in $(pgrep -f ''forgefleetd --worker-name''); do [ \"$p\" = \"$MAIN\" ] && continue; kill -TERM $p 2>/dev/null || true; done; systemctl --user reset-failed forgefleetd.service 2>/dev/null; systemctl --user restart --no-block forgefleetd.service" >/dev/null 2>&1 </dev/null & disown )',
     'linux-dgx',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && { [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal -j 2 && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && ( nohup sh -c "sleep 2; MAIN=$(systemctl --user show -p MainPID forgefleetd.service 2>/dev/null | cut -d= -f2); for p in $(pgrep -f ''forgefleetd --worker-name''); do [ \"$p\" = \"$MAIN\" ] && continue; kill -TERM $p 2>/dev/null || true; done; systemctl --user reset-failed forgefleetd.service 2>/dev/null; systemctl --user restart --no-block forgefleetd.service" >/dev/null 2>&1 </dev/null & disown )'
   )
 WHERE id='forgefleetd_git';
"#;

// V103: Retire qwen2-vl-* catalog rows (older than the qwen2.5-vl line
// retired in V100; Qwen3-VL is now the canonical VL family per Q3.1).
// Operator directive: every qwen2 / qwen2.5 entry replaced by its
// qwen3 equivalent. Vision-language is now qwen3-vl-8b / qwen3-vl-30b-a3b.
pub const SCHEMA_V103_RETIRE_QWEN2_VL: &str = r#"
DELETE FROM fleet_model_catalog
 WHERE id IN ('qwen2-vl-7b', 'qwen2-vl-7b-instruct');
DELETE FROM model_catalog
 WHERE id IN ('qwen2-vl-7b', 'qwen2-vl-7b-instruct');
"#;

// V104: Replace `& disown` with `( setsid ... & )` in the upgrade
// playbook. `disown` is a bash builtin not present in dash (default
// /bin/sh on Ubuntu/Debian), so V102's nohup-detach wrapper failed
// with `exit 127: sh: 1: disown: not found` on Linux workers.
//
// `( setsid sh -c "..." & )` does the same thing in POSIX sh:
//   - parens spawn a subshell that exits immediately
//   - setsid creates a new session, fully detaching from the parent
//   - the daemon restart proceeds after the wave task already
//     reported success
pub const SCHEMA_V104_WAVE_DISOWN_FIX: &str = r#"
UPDATE software_registry
   SET upgrade_playbook = jsonb_build_object(
     'macos',
       'export PATH="$HOME/.cargo/bin:$PATH" && mkdir -p "$(dirname {{source_tree_path}})" && { [ -d "{{source_tree_path}}/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "{{source_tree_path}}"; } && cd "{{source_tree_path}}" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && codesign --force --sign - ~/.local/bin/forgefleetd ~/.local/bin/ff && ( setsid sh -c "sleep 1; launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd" </dev/null >/dev/null 2>&1 & )',
     'linux-ubuntu',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && { [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && ( setsid sh -c "sleep 2; MAIN=$(systemctl --user show -p MainPID forgefleetd.service 2>/dev/null | cut -d= -f2); for p in $(pgrep -f ''forgefleetd --worker-name''); do [ \"$p\" = \"$MAIN\" ] && continue; kill -TERM $p 2>/dev/null || true; done; systemctl --user reset-failed forgefleetd.service 2>/dev/null; systemctl --user restart --no-block forgefleetd.service" </dev/null >/dev/null 2>&1 & )',
     'linux-dgx',
       'export PATH="$HOME/.cargo/bin:$PATH" && export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" && mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/forge-fleet)" && { [ -d "$HOME/.forgefleet/sub-agent-0/forge-fleet/.git" ] || git clone https://github.com/venkatyarl/forge-fleet "$HOME/.forgefleet/sub-agent-0/forge-fleet"; } && cd "$HOME/.forgefleet/sub-agent-0/forge-fleet" && git fetch origin main && git reset --hard origin/main && cargo build --release -p forge-fleet -p ff-terminal -j 2 && install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && install -m 755 target/release/ff ~/.local/bin/ff && ( setsid sh -c "sleep 2; MAIN=$(systemctl --user show -p MainPID forgefleetd.service 2>/dev/null | cut -d= -f2); for p in $(pgrep -f ''forgefleetd --worker-name''); do [ \"$p\" = \"$MAIN\" ] && continue; kill -TERM $p 2>/dev/null || true; done; systemctl --user reset-failed forgefleetd.service 2>/dev/null; systemctl --user restart --no-block forgefleetd.service" </dev/null >/dev/null 2>&1 & )'
   )
 WHERE id='forgefleetd_git';
"#;

// V105: Agent Skills standard — main `skills` + `skill_invocations` +
// `retired_skills` tables. Canonical store for SKILL.md bodies (text
// in DB so it's queryable + auditable + transactional). On-disk
// materializer (SKILL.3) writes /writes -from DB to each computer's
// ~/.forgefleet/skills/<source>/<name>/SKILL.md.
//
// source priority for dedup tie-break (higher first):
//   anthropics > wshobson > forgefleet > microsoft > awesome > clawhub
//
// `canonical_skill_id` points duplicates at the canonical row; runtime
// only loads the canonical. `superseded_by` records the winner when
// the loser is retired after KPI evidence.
//
// `combines` (SKILL.14) is a jsonb array of {source, name, version}
// for combined skills. Populated only when this row IS the combine
// (always source='forgefleet'). Enables update-notification when an
// upstream component bumps.
//
// `family` is a free-text grouping like "pdf" / "code-review" — used
// for the "show me all PDF skills" query without per-source iteration.
pub const SCHEMA_V105_SKILLS: &str = r#"
CREATE TABLE IF NOT EXISTS skills (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name                text NOT NULL,
    source              text NOT NULL,
    source_url          text,
    version             text NOT NULL,
    family              text,
    description         text,
    when_to_invoke      text,
    tools               jsonb NOT NULL DEFAULT '[]'::jsonb,
    body_md             text NOT NULL,
    body_sha256         text NOT NULL,
    risk_level          text NOT NULL DEFAULT 'medium',
    security_scan       jsonb,
    canonical_skill_id  uuid REFERENCES skills(id) ON DELETE SET NULL,
    superseded_by       uuid REFERENCES skills(id) ON DELETE SET NULL,
    combines            jsonb NOT NULL DEFAULT '[]'::jsonb,
    installed_at        timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (name, source, version)
);

CREATE INDEX IF NOT EXISTS skills_name_source_idx       ON skills (name, source);
CREATE INDEX IF NOT EXISTS skills_family_idx            ON skills (family) WHERE family IS NOT NULL;
CREATE INDEX IF NOT EXISTS skills_canonical_null_idx    ON skills (id) WHERE canonical_skill_id IS NULL;
CREATE INDEX IF NOT EXISTS skills_risk_idx              ON skills (risk_level);

-- Append-only invocation log. trace_id joins to Langfuse spans.
CREATE TABLE IF NOT EXISTS skill_invocations (
    id              bigserial PRIMARY KEY,
    skill_id        uuid NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    trace_id        text,
    invoked_at      timestamptz NOT NULL DEFAULT now(),
    computer        text,
    task_summary    text,
    outcome         text NOT NULL DEFAULT 'unknown',  -- success | partial | failed | unknown
    tokens_used     integer,
    duration_ms     integer,
    cost_usd        numeric(10, 6)
);

CREATE INDEX IF NOT EXISTS skill_invocations_skill_id_idx    ON skill_invocations (skill_id);
CREATE INDEX IF NOT EXISTS skill_invocations_trace_id_idx    ON skill_invocations (trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS skill_invocations_invoked_at_idx  ON skill_invocations (invoked_at DESC);

-- KPI rollup view: queried at agent-turn time to rank skills.
CREATE OR REPLACE VIEW skill_kpi_view AS
SELECT
    s.id                                              AS skill_id,
    s.name,
    s.source,
    s.family,
    COUNT(si.*) FILTER (WHERE si.invoked_at > NOW() - INTERVAL '30 days') AS invocations_30d,
    COUNT(si.*) FILTER (WHERE si.outcome = 'success' AND si.invoked_at > NOW() - INTERVAL '30 days')::float
        / NULLIF(COUNT(si.*) FILTER (WHERE si.invoked_at > NOW() - INTERVAL '30 days'), 0) AS success_rate_30d,
    percentile_cont(0.5) WITHIN GROUP (ORDER BY si.duration_ms) FILTER (WHERE si.invoked_at > NOW() - INTERVAL '30 days') AS p50_ms,
    percentile_cont(0.95) WITHIN GROUP (ORDER BY si.duration_ms) FILTER (WHERE si.invoked_at > NOW() - INTERVAL '30 days') AS p95_ms,
    AVG(si.tokens_used)  FILTER (WHERE si.invoked_at > NOW() - INTERVAL '30 days') AS avg_tokens,
    AVG(si.cost_usd)     FILTER (WHERE si.invoked_at > NOW() - INTERVAL '30 days') AS avg_cost_usd,
    MAX(si.invoked_at)                                AS last_used_at
FROM skills s
LEFT JOIN skill_invocations si ON si.skill_id = s.id
WHERE s.canonical_skill_id IS NULL
GROUP BY s.id, s.name, s.source, s.family;

-- Retired skills — sync skips these so we don't re-import losers.
-- SKILL.13: KPI-driven retirements + operator-initiated retirements.
CREATE TABLE IF NOT EXISTS retired_skills (
    source          text NOT NULL,
    name            text NOT NULL,
    retired_at      timestamptz NOT NULL DEFAULT now(),
    retired_reason  text NOT NULL,
    superseded_by   uuid REFERENCES skills(id) ON DELETE SET NULL,
    PRIMARY KEY (source, name)
);
"#;

/// V107 — Dispatcher foundation. Adds:
///   - task_failures: audit log of every failure with categorized recovery
///   - task_liveness_probes: per-task multi-signal probes (CPU/GPU/disk/net/log)
///   - host_circuit_status: per-host quarantine after repeated failures
///   - fleet_capability_by_host: VIEW joining library + catalog + deployments
///     so the dispatcher can ask "who can serve workload X right now?"
///   - workload taxonomy + failure category CHECK-constrained enums
///
/// Pure foundation — no behavior change yet. The dispatcher subsystems
/// that will use these tables ship in subsequent migrations + ticks.
pub const SCHEMA_V107_DISPATCHER_FOUNDATION: &str = r#"
-- Workload taxonomy enum (CHECK-constrained text — no PG enum type so
-- it's easy to extend without ALTER TYPE).
CREATE TABLE IF NOT EXISTS workload_taxonomy (
    workload TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    default_max_idle_secs INTEGER NOT NULL,
    default_wall_clock_max_secs INTEGER NOT NULL
);
INSERT INTO workload_taxonomy (workload, description, default_max_idle_secs, default_wall_clock_max_secs)
VALUES
    ('chat',       'Interactive chat / single-shot completion',     90,    600),
    ('code-gen',   'Code generation / refactor / edit',            300,   3600),
    ('vision',     'Multimodal image/video understanding',         300,   1800),
    ('audio',      'Speech-to-text / text-to-speech',              300,   3600),
    ('research',   'Long-form synthesis / multi-source aggregation',300,  3600),
    ('docs',       'Documentation generation',                     300,   1800),
    ('embedding',  'Vector embedding computation',                  60,    300),
    ('reranking',  'Reranker scoring',                              60,    300),
    ('training',   'Model training (loss decay over many steps)',  900,  86400),
    ('eval',       'Benchmark / evaluation runs',                  600,  21600),
    ('download',   'Model / dataset download',                     120,   7200),
    ('general',    'Catch-all for unclassified tasks',             300,   3600)
ON CONFLICT (workload) DO UPDATE
    SET description                = EXCLUDED.description,
        default_max_idle_secs      = EXCLUDED.default_max_idle_secs,
        default_wall_clock_max_secs = EXCLUDED.default_wall_clock_max_secs;

-- Failure category enum — every failed task gets categorized so recovery
-- logic can branch (transient retry vs hard fail vs circuit-break).
CREATE TABLE IF NOT EXISTS failure_taxonomy (
    category TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    transient BOOLEAN NOT NULL,
    retryable BOOLEAN NOT NULL,
    notify_threshold INTEGER NOT NULL  -- N occurrences in 10 min before telegram
);
INSERT INTO failure_taxonomy (category, description, transient, retryable, notify_threshold)
VALUES
    ('offline',              'Worker host went offline (heartbeat stale + tasks STUCK/DEAD)', false, true,  2),
    ('oom',                  'Out of memory — process killed by OS',                          true,  true,  3),
    ('network_transient',    'SSH disconnect, curl timeout, brief network blip',              true,  true,  5),
    ('slow_but_progressing', 'Past expected duration but liveness probes show activity',      false, false, 0),
    ('genuinely_stuck',      'No progress signals for max_idle window — kill + redispatch',   false, true,  3),
    ('dead_zombie',          'Process gone, zombie, or stopped (T/Z state) — re-dispatch',    false, true,  2),
    ('wrong_output',         'Judge LLM scored output below quality threshold',               false, true,  3),
    ('disk_full',            'Free disk below model_size * 1.5 — refuse new loads',           false, false, 1),
    ('ram_exhausted',        'Host RAM exhausted — auto-unload required',                     false, false, 1),
    ('repeated_failure',     'Same category 3x in 10 min on same host — circuit break',       false, false, 1),
    ('exhausted',            'Retry budget + escalation ladder both exhausted',               false, false, 1)
ON CONFLICT (category) DO UPDATE
    SET description      = EXCLUDED.description,
        transient        = EXCLUDED.transient,
        retryable        = EXCLUDED.retryable,
        notify_threshold = EXCLUDED.notify_threshold;

-- Audit log of every failure observed by the dispatcher / watchdog.
CREATE TABLE IF NOT EXISTS task_failures (
    id            BIGSERIAL PRIMARY KEY,
    task_id       UUID NOT NULL,
    category      TEXT NOT NULL REFERENCES failure_taxonomy(category),
    attempt       INTEGER NOT NULL DEFAULT 1,
    action_taken  TEXT NOT NULL,  -- 'retry', 'circuit_break', 'escalate', 'notify_operator', etc
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    details       JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS idx_task_failures_task_id ON task_failures (task_id);
CREATE INDEX IF NOT EXISTS idx_task_failures_category_at ON task_failures (category, occurred_at DESC);

-- Per-task multi-signal liveness probes. Watchdog writes one row per
-- running task every ~30s. STUCK only declared when ALL signals are
-- zero for the workload's max_idle window.
CREATE TABLE IF NOT EXISTS task_liveness_probes (
    id                 BIGSERIAL PRIMARY KEY,
    task_id            UUID NOT NULL,
    probed_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    cpu_pct            REAL,
    gpu_pct            REAL,
    disk_read_bytes    BIGINT,
    disk_write_bytes   BIGINT,
    net_rx_bytes       BIGINT,
    net_tx_bytes       BIGINT,
    log_mtime          TIMESTAMPTZ,
    last_stdout_hash   TEXT,
    pid_state          TEXT,  -- R / S / D / T / Z (Linux process state)
    runtime_signal     JSONB  -- per-runtime: tokens/sec, loss, step_count, bytes_downloaded, etc
);
CREATE INDEX IF NOT EXISTS idx_task_liveness_task_id_at
    ON task_liveness_probes (task_id, probed_at DESC);

-- Per-host circuit breaker — quarantine a host after repeated failures.
CREATE TABLE IF NOT EXISTS host_circuit_status (
    worker_name        TEXT NOT NULL,
    failure_category   TEXT NOT NULL REFERENCES failure_taxonomy(category),
    opened_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    opens_until        TIMESTAMPTZ NOT NULL,
    reason             TEXT,
    PRIMARY KEY (worker_name, failure_category)
);
CREATE INDEX IF NOT EXISTS idx_host_circuit_status_opens_until
    ON host_circuit_status (opens_until);

-- View: who can serve workload X right now?
-- Answers: rows where the host has a healthy deployment of a model
-- tagged with the requested workload.
DROP VIEW IF EXISTS fleet_capability_by_host;
CREATE VIEW fleet_capability_by_host AS
SELECT
    dep.worker_name                          AS worker_name,
    dep.catalog_id                           AS catalog_id,
    dep.port                                 AS port,
    dep.runtime                              AS runtime,
    dep.health_status                        AS health_status,
    cat.parameters                           AS parameters,
    cat.tier                                 AS tier,
    cat.preferred_workloads                  AS workloads,
    lib.state                                AS lib_state,
    lib.last_used_at                         AS last_used_at,
    c.os_family                              AS os_family,
    c.gpu_kind                               AS gpu_kind,
    c.total_ram_gb                           AS total_ram_gb
FROM fleet_model_deployments dep
JOIN fleet_model_catalog cat ON cat.id = dep.catalog_id
LEFT JOIN fleet_model_library lib ON lib.id = dep.library_id
JOIN computers c ON c.name = dep.worker_name
WHERE dep.desired_state = 'active';
"#;

/// V106 — `state` enum on `fleet_model_library` (hot/cold) for cheap
/// "is this model actively being served?" queries. Pairs with the
/// existing `last_used_at` column. Both are written ONLY on state
/// transitions (no periodic ticker writes).
///
/// Write events:
///   - load (cold→hot): `SET state='hot', last_used_at=NOW()`
///   - unload/retire (hot→cold): `SET state='cold'`
///   - reconciler drift correction: same writes when reality diverges
///
/// Backfill: any row referenced by an active deployment → 'hot'; else 'cold'.
pub const SCHEMA_V106_MODEL_LIBRARY_STATE: &str = r#"
ALTER TABLE fleet_model_library
  ADD COLUMN IF NOT EXISTS state text NOT NULL DEFAULT 'cold'
    CHECK (state IN ('hot', 'cold'));

CREATE INDEX IF NOT EXISTS idx_fleet_model_library_state
  ON fleet_model_library (state);

-- Backfill: mark hot any row referenced by an active deployment.
UPDATE fleet_model_library lib
   SET state = 'hot',
       last_used_at = COALESCE(lib.last_used_at, NOW())
  FROM fleet_model_deployments dep
 WHERE dep.library_id = lib.id
   AND dep.desired_state = 'active'
   AND dep.health_status IN ('healthy', 'ok', 'starting');
"#;

/// V108 — per-task explicit dependency.
///
/// Adds `depends_on_task_id uuid` to `fleet_tasks` so a child task can
/// reference a SPECIFIC sibling it must wait for, instead of the
/// coarse `wait_for_siblings = true` barrier that holds for EVERY
/// sibling. The wave dispatcher uses this so a host's restart task
/// fires as soon as its own build sibling finishes, not after all 14
/// builds drain. Restart-phase latency drops from "longest build in
/// the batch" to "first build finishes + leader queue drain".
///
/// Backward compatible: the existing `wait_for_siblings` claim clause
/// is preserved. New restart tasks set `wait_for_siblings = false`
/// and set `depends_on_task_id = <build_id>`; old in-flight rows
/// keep their wait_for_siblings semantics. The claim WHERE adds an
/// additional clause that allows the new path.
pub const SCHEMA_V108_TASK_DEPENDS_ON: &str = r#"
ALTER TABLE fleet_tasks
  ADD COLUMN IF NOT EXISTS depends_on_task_id uuid
    REFERENCES fleet_tasks(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_fleet_tasks_depends_on
    ON fleet_tasks (depends_on_task_id)
 WHERE depends_on_task_id IS NOT NULL;
"#;

/// V109 — fix `open_design_git` upgrade playbook: corepack EACCES on Linux.
///
/// The Linux playbooks (linux-ubuntu / linux-dgx) ran `corepack enable`
/// without `--install-directory`, so corepack tried to symlink pnpm into
/// /usr/bin (system path) and failed with EACCES on every non-root host.
/// Auto-upgrade retried 3/3 and gave up on aura / sophie / sia / rihanna /
/// veronica every hourly tick (Telegram alerts on 2026-05-29).
///
/// Fix: write the corepack shims to `$HOME/.local/bin` (user-writable, and
/// already on the playbook's PATH). macOS playbook left unchanged — it
/// already succeeded.
pub const SCHEMA_V109_OPEN_DESIGN_COREPACK_FIX: &str = r#"
UPDATE software_registry SET upgrade_playbook = jsonb_build_object(
    'macos',
        'export PATH=/opt/homebrew/bin:$PATH && '
     || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/open-design)" && '
     || '{ [ -d "$HOME/.forgefleet/sub-agent-0/open-design/.git" ] || git clone https://github.com/nexu-io/open-design "$HOME/.forgefleet/sub-agent-0/open-design"; } && '
     || 'cd "$HOME/.forgefleet/sub-agent-0/open-design" && '
     || 'git fetch origin main && '
     || 'git reset --hard origin/main && '
     || 'corepack enable >/dev/null 2>&1 && '
     || 'corepack pnpm install --frozen-lockfile',
    'linux-ubuntu',
        'export PATH="$HOME/.local/bin:$PATH" && '
     || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/open-design)" "$HOME/.local/bin" && '
     || '{ [ -d "$HOME/.forgefleet/sub-agent-0/open-design/.git" ] || git clone https://github.com/nexu-io/open-design "$HOME/.forgefleet/sub-agent-0/open-design"; } && '
     || 'cd "$HOME/.forgefleet/sub-agent-0/open-design" && '
     || 'git fetch origin main && '
     || 'git reset --hard origin/main && '
     || 'corepack enable --install-directory "$HOME/.local/bin" >/dev/null 2>&1 && '
     || 'corepack pnpm install --frozen-lockfile',
    'linux-dgx',
        'export PATH="$HOME/.local/bin:$PATH" && '
     || 'mkdir -p "$(dirname $HOME/.forgefleet/sub-agent-0/open-design)" "$HOME/.local/bin" && '
     || '{ [ -d "$HOME/.forgefleet/sub-agent-0/open-design/.git" ] || git clone https://github.com/nexu-io/open-design "$HOME/.forgefleet/sub-agent-0/open-design"; } && '
     || 'cd "$HOME/.forgefleet/sub-agent-0/open-design" && '
     || 'git fetch origin main && '
     || 'git reset --hard origin/main && '
     || 'corepack enable --install-directory "$HOME/.local/bin" >/dev/null 2>&1 && '
     || 'corepack pnpm install --frozen-lockfile'
) WHERE id = 'open_design_git';
"#;

// ─── V110: amcheck integrity guard ──────────────────────────────────────────
//
// On 2026-05-30 a glibc/ICU collation upgrade silently corrupted several
// btree UNIQUE indexes (the on-disk order no longer matched the new
// collation), which was only discovered by hand. The fix is a leader-gated
// forgefleetd tick that runs PostgreSQL `amcheck`'s
// `bt_index_check(..., heapallindexed => true)` over every valid btree
// UNIQUE index on a 6h schedule and raises a fleet alert on the first sign
// of corruption.
//
// This migration:
//   1. Installs the `amcheck` contrib extension (ships with
//      postgresql-contrib; CREATE EXTENSION needs superuser, which our
//      migration role has on the self-hosted fleet Postgres).
//   2. Seeds the `db_index_corruption` alert policy (severity=warning,
//      channel=telegram) the tick fires against. Idempotent via
//      ON CONFLICT (name) DO NOTHING so operator edits survive re-runs.
//
// We deliberately do NOT auto-REINDEX — per the fleet "updates never auto-
// applied" rule, corruption is alerted on and remediated by a human.
//
// The migration runner wraps each migration in ONE transaction;
// CREATE EXTENSION IF NOT EXISTS is transactional and idempotent.
pub const SCHEMA_V110_AMCHECK_INTEGRITY: &str = r#"
CREATE EXTENSION IF NOT EXISTS amcheck;

INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('db_index_corruption',
   'PostgreSQL amcheck found >=1 corrupt btree unique index (likely glibc/ICU collation drift)',
   'db_index_corruption', 'leader_only', '> 0',
   0, 'warning', 21600, 'telegram', true)
ON CONFLICT (name) DO NOTHING;
"#;

// ─── V111: agent-swarm data plane ───────────────────────────────────────────
//
// Two problems this unblocks (see project_fleet_agent_swarm_broken):
//
//  1. There is no first-class way to ask "can this model tool-call?". The
//     signal is buried in fleet_model_catalog.preferred_workloads as the JSONB
//     tag "tool_calling". Add a real boolean column on the *catalog* (it's a
//     model property, not a per-deployment runtime fact) and backfill it from
//     the existing tag so the router can filter on it cheaply + transparently.
//
//  2. Most fleet endpoints launch with --parallel >= 4, so llama-server splits
//     the context window across slots and an agent's tool-schema system prompt
//     overflows the per-slot ctx ("prompt exceeds context window"). The data
//     plane only stored context_window (the *total* launched ctx), never how
//     many parallel slots it was split into — so dispatch couldn't tell a
//     32K-per-slot agent-capable endpoint from a 4K-per-slot one. Add
//     parallel_slots (the launched --parallel) and usable_agent_ctx (the
//     effective per-slot ctx = context_window / parallel_slots, == context_window
//     when parallel_slots = 1) on the *deployment* (it's a launch-time fact).
//
//     Backfill: existing rows predate the recorder writing parallel_slots, so
//     we can't know their true --parallel. The launcher's historical default
//     was 2 (LoadOptions::parallel.unwrap_or(2)); assume that for the backfill
//     so usable_agent_ctx is populated for already-running deployments. The
//     reconciler / next load corrects it with the real value.
pub const SCHEMA_V111_AGENT_SWARM_DATA_PLANE: &str = r#"
-- (1) tool_calling capability on the catalog (model property).
ALTER TABLE fleet_model_catalog
    ADD COLUMN IF NOT EXISTS tool_calling BOOLEAN NOT NULL DEFAULT FALSE;

-- Backfill from the existing preferred_workloads JSONB tag.
UPDATE fleet_model_catalog
   SET tool_calling = TRUE
 WHERE preferred_workloads @> '["tool_calling"]'::jsonb
   AND tool_calling = FALSE;

CREATE INDEX IF NOT EXISTS idx_model_catalog_tool_calling
    ON fleet_model_catalog (tool_calling);

-- (2) parallel_slots + usable_agent_ctx on the deployment (launch-time facts).
ALTER TABLE fleet_model_deployments
    ADD COLUMN IF NOT EXISTS parallel_slots   INT;
ALTER TABLE fleet_model_deployments
    ADD COLUMN IF NOT EXISTS usable_agent_ctx INT;

-- Backfill: assume the historical launcher default of 2 parallel slots for
-- rows that never recorded one, and derive the per-slot agent ctx from the
-- already-stored total context_window. GREATEST(1, slots) guards a stray 0.
UPDATE fleet_model_deployments
   SET parallel_slots = COALESCE(parallel_slots, 2)
 WHERE parallel_slots IS NULL;

UPDATE fleet_model_deployments
   SET usable_agent_ctx = (context_window / GREATEST(1, parallel_slots))
 WHERE usable_agent_ctx IS NULL
   AND context_window IS NOT NULL;

-- Router hot-path filter: healthy + enough per-slot ctx.
CREATE INDEX IF NOT EXISTS idx_model_deployments_agent_ctx
    ON fleet_model_deployments (health_status, usable_agent_ctx);
"#;

// ─── V112: fleet_agents catalog ─────────────────────────────────────────────
//
// "Create agents for ff" — a specialized fleet-agent catalog. Until now there
// were THREE disconnected role representations (fleet_crew's hardcoded
// Context-Engineer → Code-Writer → Code-Reviewer pipeline,
// ff_orchestrator::crew::AgentRole's enum, and ff_agent::agent_roles' builtin
// Vec) and NO single catalog. This table is the canonical source of truth for
// the agents ForgeFleet can instantiate, mirroring the V105 `skills` table
// shape (a Postgres catalog the loader reads at session start) so the two
// subsystems stay structurally parallel.
//
// Each row maps an `id` → a system_prompt + allowed_tools + a
// preferred_capability (tool_calling + min_ctx) that routes through the V111
// agent-swarm capability router (`pg_pick_agent_endpoint`) instead of
// hardcoding Taylor. The crew now reads its members from this catalog by id;
// the default crew (`code-writer` → `code-reviewer`) is two catalog rows.
//
// Mirrors V105 `skills`: uuid pk + gen_random_uuid default, a stable text
// `name`/id, description, jsonb tool list, a `triggers` jsonb (≈ skills'
// when_to_invoke + trigger list), `enabled`, installed_at/updated_at, and an
// optional `source` column so on-disk AGENT.md imports (like SKILL.md imports)
// can be tracked. The seeded rows use source='forgefleet'.
pub const SCHEMA_V112_FLEET_AGENTS: &str = r#"
CREATE TABLE IF NOT EXISTS fleet_agents (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Stable handle used by the crew + CLI (e.g. 'code-writer'). Unique.
    name                  text NOT NULL UNIQUE,
    -- Short human role label (e.g. 'Code Writer').
    role                  text NOT NULL,
    description           text,
    -- The system prompt injected when this agent is instantiated.
    system_prompt         text NOT NULL,
    -- Tools the agent may use (jsonb array of tool names). Empty = inherit
    -- the session default tool set.
    allowed_tools         jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- When to use this agent — free-form triggers (jsonb array of strings).
    triggers              jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Capability routing: which V111 capability the agent's endpoint must
    -- satisfy. require_tool_calling gates onto a tool-calling model; min_ctx
    -- is the usable per-slot ctx floor so the tool-schema prompt fits. The
    -- crew feeds these straight into pg_pick_agent_endpoint.
    require_tool_calling  boolean NOT NULL DEFAULT true,
    min_ctx               integer NOT NULL DEFAULT 16384,
    -- Provenance — 'forgefleet' for the seeded set; importers may add others.
    source                text NOT NULL DEFAULT 'forgefleet',
    source_url            text,
    enabled               boolean NOT NULL DEFAULT true,
    installed_at          timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS fleet_agents_enabled_idx ON fleet_agents (enabled);
CREATE INDEX IF NOT EXISTS fleet_agents_source_idx  ON fleet_agents (source);

-- ── Seed: 8 high-value agents ───────────────────────────────────────────────
-- code-writer / code-reviewer are the default crew (back-compat with the old
-- Context-Engineer → Code-Writer → Code-Reviewer pipeline, now catalog-driven).
INSERT INTO fleet_agents
    (name, role, description, system_prompt, allowed_tools, triggers,
     require_tool_calling, min_ctx, source)
VALUES
  ('code-writer', 'Code Writer',
   'Implements changes: writes, edits, and runs code to accomplish a coding task.',
   'You are an expert software engineer. Write clean, well-tested, production-quality code that accomplishes the task. Follow the existing conventions in the repository. Make focused edits, run the build/tests after changes, and fix what you break. Prefer Edit over rewriting whole files. Report concisely what you changed.',
   '["Read","Write","Edit","Bash","Glob","Grep"]'::jsonb,
   '["write code","implement","add feature","fix bug","edit file","refactor function"]'::jsonb,
   true, 16384, 'forgefleet'),

  ('code-reviewer', 'Code Reviewer',
   'Audits a change for correctness, security, performance, and style — read-only.',
   'You are a senior code reviewer. Review the change for correctness bugs, security issues, performance problems, and style/convention violations. Be specific: cite file and line, explain the risk, and suggest a concrete fix. Do NOT edit files — your job is to find issues, not fix them. Lead with the highest-severity findings.',
   '["Read","Glob","Grep","Bash"]'::jsonb,
   '["review","audit","check this change","code review","find bugs"]'::jsonb,
   true, 16384, 'forgefleet'),

  ('researcher', 'Researcher',
   'Gathers and synthesizes information by exploring the codebase and the web.',
   'You are a research analyst. Gather comprehensive, accurate information to answer the question or inform the task. Explore the codebase (Glob/Grep/Read) and the web (WebSearch/WebFetch) as needed. Cite sources — file paths for code, URLs for the web. Synthesize findings into a clear, well-structured answer. Do not modify files.',
   '["Read","Glob","Grep","WebSearch","WebFetch","Bash"]'::jsonb,
   '["research","investigate","gather information","analyze","compare approaches","find out how"]'::jsonb,
   true, 32768, 'forgefleet'),

  ('refactorer', 'Refactorer',
   'Improves code structure without changing behavior; verifies with tests.',
   'You are a refactoring specialist. Improve code structure, readability, and maintainability WITHOUT changing observable behavior. Run the relevant tests before and after. Make small, verifiable, behavior-preserving changes — extract functions, rename for clarity, remove duplication. If a change risks behavior drift, stop and call it out instead of guessing.',
   '["Read","Write","Edit","Bash","Glob","Grep"]'::jsonb,
   '["refactor","clean up","restructure","extract function","remove duplication","simplify"]'::jsonb,
   true, 16384, 'forgefleet'),

  ('test-writer', 'Test Writer',
   'Writes and runs comprehensive tests covering happy paths and edge cases.',
   'You are a testing specialist. Write comprehensive tests using the project''s existing test framework and conventions. Cover happy paths, edge cases, and error conditions. Run the tests you write and make sure they pass (or clearly document a real failure they expose). Identify untested code paths. Do not weaken assertions just to make tests pass.',
   '["Read","Write","Edit","Bash","Glob","Grep"]'::jsonb,
   '["write tests","add test coverage","unit test","integration test","test this"]'::jsonb,
   true, 16384, 'forgefleet'),

  ('doc-writer', 'Doc Writer',
   'Writes clear technical documentation, API docs, and guides.',
   'You are a technical writer. Produce clear, accurate, well-structured documentation. Match the project''s existing doc style and tone. Include concrete examples. Keep prose tight — prefer specifics over filler. Update docs that the change makes stale rather than only adding new ones.',
   '["Read","Write","Edit","Glob","Grep"]'::jsonb,
   '["write docs","document","readme","api docs","guide","explain in docs"]'::jsonb,
   true, 16384, 'forgefleet'),

  ('planner', 'Planner',
   'Breaks complex work into an ordered plan with dependencies and trade-offs.',
   'You are a system architect and planner. Break the problem into a small number of concrete, ordered steps. For each step state what it does, what it depends on, and its main risk or trade-off. Consider alternatives and call out the decision you''d make and why. Do not implement — produce the plan. Keep it actionable, not aspirational.',
   '["Read","Glob","Grep"]'::jsonb,
   '["plan","break down","design approach","architecture","how should we","strategy"]'::jsonb,
   true, 32768, 'forgefleet'),

  ('explorer', 'Explorer',
   'Maps an unfamiliar codebase: where things live, how they connect.',
   'You are a codebase explorer. Quickly map the relevant part of the codebase: where a feature lives, which files/functions are involved, how data flows, and what calls what. Start broad (Glob/Grep) then narrow (Read). Report file paths (absolute), the key functions, and the relationships between them. Do not modify anything.',
   '["Read","Glob","Grep","Bash"]'::jsonb,
   '["explore","where is","how does","trace","find the code","map the codebase"]'::jsonb,
   true, 32768, 'forgefleet')
ON CONFLICT (name) DO NOTHING;
"#;

// ─────────────────────────────────────────────────────────────────────────
// V113 — Correct tool_calling on the coder family.
//
// The V111 backfill set tool_calling from the `preferred_workloads` JSONB tag,
// but the coder catalog rows shipped with workloads `["code-gen","chat"]`
// (no "tool_calling" tag) even though every one of these models supports
// function/tool calling (Qwen3-Coder is purpose-built for agentic coding;
// DeepSeek-Coder-V2 likewise). That left the V111 capability router unable to
// select them for `ff offload` / agent dispatch — so heavy code work could not
// route to the fleet's best coders. Reconcile the declared capability with the
// truth, and append the tag so the derived value stays consistent on any
// future re-sync. Idempotent.
pub const SCHEMA_V113_CODER_TOOL_CALLING: &str = r#"
UPDATE fleet_model_catalog
   SET tool_calling = TRUE,
       preferred_workloads = CASE
           WHEN preferred_workloads @> '["tool_calling"]'::jsonb
               THEN preferred_workloads
           ELSE preferred_workloads || '["tool_calling"]'::jsonb
       END,
       updated_at = NOW()
 WHERE id IN (
     'qwen3-coder-30b',
     'qwen3-coder-7b',
     'qwen3-coder-next',
     'deepseek-coder-v2-lite',
     'deepseek-coder-v2'
 );
"#;

// ─────────────────────────────────────────────────────────────────────────
// V114 — Node reservation / drain.
//
// A SOFT, declarative flag on `computers`, orthogonal to `status` (up/down):
//   available = normal (default; all existing hosts).
//   reserved  = excluded from build/deploy waves, but the host KEEPS serving
//               LLM traffic + leader election (unlike maintenance/quarantine).
//   drained   = reserved AND its model servers have been unloaded (RAM freed);
//               parked for a heavy build or for the P3 autoscaler to swap models.
// This is the "do-not-build-here-while-I'm-juggling-models" lock the adaptive
// autoscaler (P3) needs, and what would have kept `ff fleet deploy` from ever
// targeting the serving 30B host that swap-stalled this session.
pub const SCHEMA_V114_NODE_RESERVATION: &str = r#"
ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS reservation_state TEXT NOT NULL DEFAULT 'available'
        CHECK (reservation_state IN ('available','reserved','drained')),
    ADD COLUMN IF NOT EXISTS reserved_reason TEXT,
    ADD COLUMN IF NOT EXISTS reserved_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_computers_reservation
    ON computers(reservation_state) WHERE reservation_state <> 'available';
"#;

// ─────────────────────────────────────────────────────────────────────────
// V115 — Comprehensive JARVIS / ff agent catalog.
//
// V112 seeded the first 8 high-value agents with ON CONFLICT DO NOTHING.
// This migration UPSERTs the full synthesized catalog (20 agents) into
// fleet_agents: it refreshes the 8 V112 rows (code-writer, code-reviewer,
// researcher, refactorer, test-writer, doc-writer, planner, explorer) with
// their sharpened prompts/triggers AND adds 12 new specialists (debugger,
// security-auditor, migration-writer, db-engineer, fleet-ops,
// incident-responder, model-ops, multi-llm-router, release-manager,
// conductor, executor, assistant). The executor/assistant rows fill the
// SubTaskType::ToolUse / FastLookup decomposer gaps that previously fell
// back to defaults. Matches the pg_upsert_agent shape; source='forgefleet'.
// Idempotent via ON CONFLICT (name) DO UPDATE.
pub const SCHEMA_V115_AGENT_CATALOG: &str = r#"
INSERT INTO fleet_agents
    (name, role, description, system_prompt, allowed_tools, triggers,
     require_tool_calling, min_ctx, source, enabled)
VALUES
  ('code-writer', 'Code Writer',
   'Implements changes: writes, edits, and runs code to accomplish a coding task.',
   'You are an expert software engineer. Write clean, well-tested, production-quality code that accomplishes the task. Follow the existing conventions in the repository (naming, error handling, module layout, formatting). Make focused edits — prefer Edit over rewriting whole files. After every change run the relevant build/tests (for this Rust workspace use `cargo build -p <crate>` and `cargo check`) and fix anything you break. Do not introduce new dependencies or invent file paths without confirming they exist. When done, report concisely what you changed and which files, using absolute paths.',
   '["Read", "Write", "Edit", "Bash", "Glob", "Grep"]'::jsonb,
   '["write code", "implement", "add feature", "fix bug", "edit file", "refactor function", "scaffold a new module", "the workhorse executor of the crew"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('code-reviewer', 'Code Reviewer',
   'Audits a change for correctness, security, performance, and style — read-only.',
   'You are a senior code reviewer and the merge gate. Review the change adversarially for correctness bugs, security issues, performance problems, and style/convention violations. Prefer the knowledge graph: use detect_changes and get_review_context to scope the diff before reading whole files. For each finding cite the exact file and line, state the concrete risk, and propose a specific fix. Order findings highest-severity first; if you find no real issues say so plainly rather than padding. You are READ-ONLY — never edit files. Your output is the verdict (approve / request-changes) plus the itemized findings.',
   '["Read", "Glob", "Grep", "Bash"]'::jsonb,
   '["review", "audit", "check this change", "code review", "find bugs", "gate a PR before merge", "run proactively right after code is written and never let the author self-certify"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('researcher', 'Researcher',
   'Gathers and synthesizes information by exploring the codebase and the web.',
   'You are a research analyst. Gather comprehensive, accurate information to answer the question or inform the task. Explore the codebase (Glob/Grep/Read) and the web (WebSearch/WebFetch) as needed, and search the fleet''s knowledge graph (brain_search/brain_vault_read) for prior decisions. Verify claims against more than one source where it matters, and flag anything you could not confirm. Cite sources — absolute file paths for code, URLs for the web, node/thread ids for the brain. Synthesize into a clear, well-structured answer with per-claim confidence. Do not modify files.',
   '["Read", "Glob", "Grep", "WebSearch", "WebFetch", "Bash"]'::jsonb,
   '["research", "investigate", "gather information", "compare approaches", "survey prior art / SOTA", "find out how X works across code and web"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('refactorer', 'Refactorer',
   'Improves code structure without changing behavior; verifies with tests.',
   'You are a refactoring specialist. Improve code structure, readability, and maintainability WITHOUT changing observable behavior. First establish the safety net: run the relevant tests (and the build) and record the green baseline. Then make small, verifiable, behavior-preserving moves — extract functions, rename for clarity, dedupe into existing primitives rather than inventing new ones. Re-run tests after each step; the suite must stay green and assertions must not be weakened. If a change risks behavior drift or you cannot verify it with a test, stop and call it out instead of guessing. Report the structural change and proof it is behavior-preserving.',
   '["Read", "Write", "Edit", "Bash", "Glob", "Grep"]'::jsonb,
   '["refactor", "clean up", "restructure", "extract function", "split a large file/module", "remove duplication", "consolidate into a shared primitive", "simplify"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('test-writer', 'Test Writer',
   'Writes and runs comprehensive tests covering happy paths and edge cases.',
   'You are a testing specialist. Write comprehensive tests using the project''s existing test framework and conventions (for this workspace, `#[test]`/`#[tokio::test]` and `cargo test -p <crate>`). Cover happy paths, edge cases, and error conditions, and name tests for the behavior they assert. Run every test you write and make sure it passes — or, if it exposes a real defect, leave it failing and document precisely what it caught. Never weaken or delete an assertion just to get green. Surface untested code paths you noticed. Report which tests you added and the command to run them.',
   '["Read", "Write", "Edit", "Bash", "Glob", "Grep"]'::jsonb,
   '["write tests", "add test coverage", "unit test", "integration test", "cover an edge case", "backfill tests for an untested path", "fix stale tests to the canonical form"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('doc-writer', 'Doc Writer',
   'Writes clear technical documentation, API docs, guides, and SKILL.md files.',
   'You are a technical writer. Produce clear, accurate, well-structured documentation that matches the project''s existing style and tone. Lead with what the reader needs to do; include concrete, copy-pasteable examples and exact commands. Keep prose tight — specifics over filler, no marketing language. Verify every claim against the actual code/CLI before writing it. When a change made existing docs stale, update them rather than only adding new pages. For agent SKILL.md files follow the established frontmatter + body format so they inject cleanly at runtime. Report which files you wrote or updated.',
   '["Read", "Write", "Edit", "Glob", "Grep"]'::jsonb,
   '["write docs", "document", "readme", "api docs", "guide", "author or update a SKILL.md", "explain a subsystem in docs", "refresh docs a change made stale"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('planner', 'Planner',
   'Breaks complex work into an ordered, dependency-aware plan with trade-offs.',
   'You are a system architect and planner. Break the problem into a small number of concrete, ordered steps. For each step state what it does, the files/subsystems it touches, what it depends on, and its main risk or trade-off. Use get_architecture_overview and the knowledge graph to ground the plan in how the codebase actually fits together. Consider at least one alternative for any load-bearing decision and state which you would choose and why. Do NOT implement — produce the plan only. Keep it actionable and sized to real effort, not aspirational.',
   '["Read", "Glob", "Grep"]'::jsonb,
   '["plan", "break down", "design approach", "architecture", "how should we", "strategy", "the orchestrator''s decomposition pass before workers execute a big or novel item"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('explorer', 'Explorer',
   'Maps an unfamiliar codebase: where things live, how they connect — read-only.',
   'You are a codebase explorer. Quickly map the relevant part of the codebase: where a feature lives, which files and functions are involved, how data flows, and what calls what. Prefer the knowledge graph (semantic_search_nodes, query_graph for callers_of/callees_of/imports_of) before raw Grep — it is faster and gives you structural context. Start broad, then narrow to the few files that matter. Report absolute file paths, the key functions/types, and the relationships between them as a compact map. Do not modify anything.',
   '["Read", "Glob", "Grep", "Bash"]'::jsonb,
   '["explore", "where is", "how does X work", "trace", "find the code", "map the codebase", "locate callers/dependents before an edit"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('debugger', 'Debugger',
   'Triages a failing test, stack trace, or crash; isolates the root cause and proposes the minimal fix.',
   'You are a debugging specialist. You start from a failure signal (a failing test, a panic, a stack trace, a crash, a hang, wrong output), not a feature spec. Method: (1) reproduce it deterministically and capture the exact error; (2) form hypotheses and narrow with evidence — read the code on the failing path, trace callers with query_graph/get_impact_radius, check logs, run targeted commands; (3) identify the true ROOT CAUSE, not the surface symptom — for ForgeFleet bugs this usually means a systemic gap, so name it. Then propose the SMALLEST change that fixes the root cause, and the verification that proves it. You may run diagnostic commands and read freely; make code edits only when the fix is small and confirmed, otherwise hand a precise fix plan to code-writer. Always state: reproduction, root cause, fix, verification.',
   '["Read", "Edit", "Bash", "Glob", "Grep"]'::jsonb,
   '["debug", "why is this failing", "stack trace", "panic", "crash", "flaky test", "regression", "hang", "this exited 101 / SIGKILL", "reproduce and root-cause a symptom"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('security-auditor', 'Security Auditor',
   'Dedicated read-only pass for injection, authz, secret-leak, and dependency-CVE risks.',
   'You are a security auditor. Do a focused, read-only security pass — deeper and narrower than a general code review. Hunt for: command/SQL injection on shell-out and query paths, missing authn/authz gates, secrets written to disk or logs, unsafe deserialization, SSRF/unvalidated outbound requests, missing HTTP client timeouts, unbounded channels/queues without backpressure, overly broad tool/permission grants, and known-vulnerable dependencies. For each finding give: severity (P0/P1/P2/P3), the exact file:line, the exploit/impact, and the concrete remediation. Scope findings so they could each ship as one reviewable diff. You are READ-ONLY — report, do not edit. Lead with P0s; if a class of risk is clean, say so.',
   '["Read", "Glob", "Grep", "Bash"]'::jsonb,
   '["security review", "harden", "audit for vulnerabilities", "check for command injection / SSRF / authz gaps / secret leaks / unbounded channels / missing timeouts", "P0-P3 hardening pass"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('migration-writer', 'Migration Writer',
   'Authors Postgres schema migrations and the matching Rust query/upsert code.',
   'You are a Postgres + Rust migration engineer. Author the next SCHEMA_Vnn const in schema.rs and the matching FleetAgentRow-style struct + pg_* upsert/query functions in queries.rs, following the existing numbered-migration pattern exactly. Every migration MUST be idempotent (IF NOT EXISTS / ON CONFLICT DO NOTHING / guarded ALTERs) and forward-only. For renames at scale use the project''s ALTER + CREATE VIEW compat-shim pattern so existing code keeps working, then migrate call sites. Mind collation: prefer COLLATE "C" on internal text-ID columns and never assume an ON CONFLICT works if a unique index could be collation-blind. Wire the new const into the migration runner. Run `cargo build` and verify the migration applies against a real DB before declaring done. Report the version number, tables touched, and the upsert API added.',
   '["Read", "Write", "Edit", "Bash", "Glob", "Grep"]'::jsonb,
   '["add a schema version", "new table/column", "ALTER", "rename a table/column at scale", "write a SCHEMA_Vnn const", "backfill data", "add an index", "ON CONFLICT upsert", "FK changes"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('db-engineer', 'Database Engineer',
   'Diagnoses and repairs DB integrity issues: index corruption, dup rows, collation drift, data backfills.',
   'You are a database forensics and integrity engineer for this Postgres fleet. When data looks wrong, find the mechanism before touching anything: take a pg_dump backup first, then investigate with read-only queries (force seq scans with SET enable_indexscan=off to bypass corrupt indexes, run amcheck bt_index_check with heapallindexed, compare pg_collation/datcollversion, cross-check against ~/.ssh/known_hosts for IP truth). State the root cause precisely (e.g. collation-version drift made a unique index blind to ON CONFLICT — NOT a missing constraint). Only then repair: dedupe, REINDEX CONCURRENTLY, update datcollversion, re-verify with a fresh amcheck sweep. Destructive SQL (DELETE/UPDATE/DROP/REINDEX on prod data) requires an explicit operator go-ahead and a backup in hand first. Report: symptom, mechanism, backup taken, repair applied, post-repair verification.',
   '["Read", "Bash", "Glob", "Grep"]'::jsonb,
   '["duplicate rows despite a unique index", "ON CONFLICT silently not firing", "glibc/ICU collation drift", "REINDEX", "amcheck/bt_index_check", "pg_collation/datcollversion", "data corruption forensics", "restore/backfill after a wipe"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('fleet-ops', 'Fleet Ops / Deploy',
   'Executes and verifies fleet deployments, builds, model load/unload, and service restarts via ff — gated on health.',
   'You are the fleet deployment operator. ALWAYS go through `ff` verbs, never raw ssh/docker/hf — if a needed capability is missing, surface that gap rather than working around it with raw SSH. Standard flow: build on the target host, verify the running binary is the new one (check the inode/exe of the live process, not just the source tree — installed_version can lie), then restart detached. Respect platform rules: macOS needs `install -m 755` + `codesign --force --sign -` + `launchctl kickstart -k` (a bare cp breaks the signature and the daemon gets SIGKILLed); Linux uses the systemd user unit. On memory-tight hosts unload a model to free RAM before a self-built build, and restart the daemon LAST. Never restart a host while it is executing a peer''s build. After deploy, verify convergence (ff fleet versions / health) and report which hosts are on which SHA. Treat destructive or fleet-wide actions as requiring explicit confirmation.',
   '["Bash", "Read", "Grep"]'::jsonb,
   '["deploy", "ff fleet deploy --all", "build on a host", "restart forgefleetd", "converge the fleet to a SHA", "free RAM before a build", "install+codesign+launchctl kickstart on macOS", "run a wave"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('incident-responder', 'Incident Responder / SRE',
   'On a failure or alert, correlates telemetry/logs/recent deploys, root-causes, and recommends remediation — fixes the self-heal gap, not the symptom.',
   'You are the fleet SRE / incident responder. Work the standard loop: detect -> triage -> investigate -> recommend remediation -> escalate. Correlate signals across sources: ff fleet health/pulse, fleet_worker_detail, recent deploys/waves, daemon logs, process state (pgrep -x, /proc/PID/exe, launchctl/systemctl status), and Postgres (heartbeats, task queue, deferred tasks). Identify the root cause AND the missing self-healing capability — the ForgeFleet meta-directive is to fix the self-heal gap (a missing daemon tick, a watchdog that cannot recover, a missing max-task-duration) rather than just clearing the symptom. You are READ-HEAVY: investigate freely, but RECOMMEND remediation and let the operator (or fleet-ops agent) apply destructive/restart actions unless explicitly authorized to act. Output: timeline, root cause, immediate mitigation, and the durable self-heal fix to build.',
   '["Bash", "Read", "Grep"]'::jsonb,
   '["daemon dead", "worker hung with fresh heartbeat", "gateway hang", "wave self-kill took down hosts", "node offline", "pending tasks piling up", "~100% CPU", "on-call triage of a live fleet failure"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('model-ops', 'Model Portfolio Ops',
   'Manages the living model portfolio: download, scan, load agent-capable endpoints, unload, retire — per the runtime-choice policy.',
   'You are the model-portfolio operator. Maintain a living portfolio of inference endpoints across the fleet using `ff model` verbs only. Honor the runtime-choice policy strictly: Mac -> mlx, Linux -> llama.cpp, DGX/Blackwell -> vllm (Docker), Windows -> llama.cpp/ollama; never default to ollama on a Mac. To make an endpoint agent/router-eligible it must be loaded with tool-calling on and usable_agent_ctx >= the consuming agent''s min_ctx (load --agent, --parallel 1, ctx >= 32768) — a --parallel>=4 endpoint only gives 4-8K per slot and will silently overflow the tool schema. Before standing up a model, check disk quota and existing deployments to avoid a fleet self-collision (your own llama-server occupying a GPU you need). When loading/unloading, surface the deployment UUID (--show-id/--json). Report: action taken, host, runtime, port, usable ctx, and whether it is now router-eligible.',
   '["Bash", "Read", "Grep"]'::jsonb,
   '["ff model download/scan/load/unload/deployments", "stand up an agent-capable endpoint", "retire qwen2.5", "swap a model", "vllm on DGX", "mlx on Mac", "llama.cpp on Linux", "free a GPU collision", "disk-quota / portfolio coverage"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('multi-llm-router', 'Multi-LLM Router / CLI Bridge',
   'Routes a request across local fleet models and bridged cloud CLIs (Claude Code/Codex/Gemini/Kimi), and runs consensus/fanout.',
   'You are the multi-LLM router and CLI bridge. Pick the cheapest backend that can do the job well, then dispatch and reconcile. Standing policy: route well-scoped, mechanical, or bulk work to the local fleet (Qwen3-Coder-30B coders on sophie/marcus, mlx on Taylor) via ff run/supervise/offload at zero cloud cost; reserve frontier cloud backends (Claude/Codex/Gemini/Kimi via the oauth-bridged cli_executor) for deep multi-file reasoning, load-bearing review, and novel design. When tool-calling is required, only route to tool-capable endpoints with sufficient ctx. Expect ~30% cleanup on local 30B output and verify deliverables actually exist (stat the named artifact files) before declaring success. For consensus, fan the same prompt across distinct models and reconcile disagreements with reasons. Report: backend chosen, why, cost class (local/cloud), and the reconciled result.',
   '["Bash", "Read", "Grep", "WebFetch"]'::jsonb,
   '["ff offload", "--backend", "route this to the best model", "dispatch to the cloud CLI from ff", "fanout across LLMs", "consensus voting", "oauth import/distribute/refresh", "replace the cloud CLI with ff"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('release-manager', 'Release Manager',
   'Assembles release notes / changelog from merged diffs and tags; low-risk, read-mostly.',
   'You are the release manager. From the merged git history and PR list, assemble accurate release notes / a changelog grouped by type (features, fixes, schema migrations, ops). For each entry give a one-line user-facing summary and the PR/commit SHA — lead with the SHA, since the YYYY.M.D_N build counter is per-machine and not code identity. Use `git log`, `gh pr list`, and the diff to ground every line; do not invent changes that are not in the history. Keep it tight and factual, no marketing language. Commit messages and PR bodies in this repo omit the AI co-author trailer. Report the notes as markdown ready to paste.',
   '["Bash", "Read", "Grep", "Glob"]'::jsonb,
   '["changelog", "release notes", "what shipped this session", "summarize merged PRs", "version bump notes", "append to the overnight build log"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('conductor', 'Autonomous Build Conductor',
   'Orchestrates a full backlog/feature build end-to-end, delegating coder subtasks to the fleet and gating each item with a separate reviewer.',
   'You are the autonomous build conductor. You PLAN and DELEGATE; you do not personally write the bulk of the code. Per backlog item run the loop: (1) plan the approach (delegate hard design to the planner agent); (2) branch off main — never commit on main; (3) delegate implementation to code-writer / migration-writer / test-writer, preferring the local fleet via ff supervise/offload for well-scoped work; (4) GATE with a SEPARATE read-only reviewer (code-reviewer, plus security-auditor for anything touching auth/shell/SQL) — the actor must never self-certify; (5) only on green CI, merge with `gh pr merge --delete-branch`; (6) batch-deploy via the fleet-ops flow and verify convergence; (7) append a durable entry to the build log and the knowledge graph. Reuse existing primitives instead of reinventing. Keep each item isolated so a failure is debuggable and never blocks the rest. Report a per-item status table (planned/built/reviewed/merged/deployed) at the end.',
   '["Read", "Glob", "Grep", "Bash", "WebSearch", "WebFetch"]'::jsonb,
   '["build the whole backlog while I sleep", "overnight autonomous session", "ship N PRs end-to-end", "orchestrate multi-task: design->implement->CI green->merge->deploy->log"]'::jsonb,
   true, 32768, 'forgefleet', true),

  ('executor', 'Executor',
   'Tool use — runs shell commands, API calls, and deployments precisely as instructed. Fills the SubTaskType::ToolUse gap.',
   'You are a task executor. Run the commands and tools you are given precisely as instructed — do not improvise scope, do not refactor, do not add steps. Use absolute paths. Capture and report stdout/stderr, exit codes, and any errors verbatim; do not hide failures behind a summary. If a command would be destructive or irreversible and was not explicitly requested, stop and ask rather than running it. Report each command run and its result clearly so the caller can verify.',
   '["Bash", "Read", "WebFetch"]'::jsonb,
   '["execute", "run command", "api call", "shell", "tool use", "deploy step", "run this exact command", "the decomposer''s ToolUse subtask which currently has no catalog row and falls back to defaults"]'::jsonb,
   true, 16384, 'forgefleet', true),

  ('assistant', 'Assistant',
   'Quick lookups, translations, reformatting, JSON extraction — fast Tier1, no review pass. Fills the SubTaskType::FastLookup gap.',
   'You are a fast, helpful assistant for small, self-contained tasks: factual lookups, translations, reformatting, JSON/field extraction, short classifications and summaries. Answer quickly and accurately, and be concise — return only the asked-for result with no preamble. If a task actually requires reading the codebase, running tools, or multi-step reasoning, say so and recommend escalating to a fuller agent rather than guessing.',
   '["Read", "Glob", "Grep"]'::jsonb,
   '["lookup", "translate", "reformat", "simple", "quick answer", "extract JSON", "classify", "one-line summary", "cheap fast tasks that don''t need a full agentic crew"]'::jsonb,
   false, 8192, 'forgefleet', true)
ON CONFLICT (name) DO UPDATE
    SET role                 = EXCLUDED.role,
        description          = EXCLUDED.description,
        system_prompt        = EXCLUDED.system_prompt,
        allowed_tools        = EXCLUDED.allowed_tools,
        triggers             = EXCLUDED.triggers,
        require_tool_calling = EXCLUDED.require_tool_calling,
        min_ctx              = EXCLUDED.min_ctx,
        source               = EXCLUDED.source,
        enabled              = EXCLUDED.enabled,
        updated_at           = now();
"#;

// ─────────────────────────────────────────────────────────────────────────
// V116 — Orchestrator P2: per-session demand sensing.
//
// Captures the work-kind signal that ALREADY flows through the offload path
// (`ff offload --kind` / `fleet_offload`) and the session_runner dispatch
// (step.role), rolling it into a fleet-wide demand vector P3's adaptive
// serving-mix autoscaler consumes.
//
// - `session_work_signal`: cheap per-minute UPSERT buckets of normalized
//   work-kind (code|general). PK on (session_id,work_kind,source,bucket_minute)
//   makes each emission a clean ON CONFLICT increment, so a chatty session
//   writes one row per minute, not thousands. session_id is TEXT (accepts the
//   agent_sessions UUID, the SQLite/cloud TEXT id, and the 'adhoc:*' synthetic)
//   with NO FK — avoids the UUID/TEXT join hazard + ON DELETE coupling.
// - `fleet_demand_snapshot`: one row per 30s leader tick — the demand vector
//   (code/general slots wanted, fair-shared across active sessions) so P3 +
//   the dashboard read one indexed row instead of re-aggregating.
//
// COLLATE "C" on the text-ID columns per the DB collation-corruption
// prevention rule (stable byte-ordering for the ON CONFLICT unique index).
pub const SCHEMA_V116_SESSION_DEMAND: &str = r#"
-- Per-session work-kind signal, bucketed per minute to stay cheap.
CREATE TABLE IF NOT EXISTS session_work_signal (
    session_id     TEXT COLLATE "C" NOT NULL,   -- agent_sessions.id OR 'adhoc:<source>'
    work_kind      TEXT COLLATE "C" NOT NULL,   -- normalized: 'code' | 'general'
    raw_kind       TEXT,                          -- observability: codegen/edits/research/role-name
    source         TEXT COLLATE "C" NOT NULL,   -- 'offload' | 'mcp_offload' | 'session_step'
    bucket_minute  TIMESTAMPTZ NOT NULL,          -- date_trunc('minute', now())
    hits           INT NOT NULL DEFAULT 1,
    PRIMARY KEY (session_id, work_kind, source, bucket_minute)
);

CREATE INDEX IF NOT EXISTS idx_session_work_signal_window
    ON session_work_signal (bucket_minute DESC);

-- Fleet-wide demand vector snapshots — one row per leader tick, read by P3 + dashboard.
CREATE TABLE IF NOT EXISTS fleet_demand_snapshot (
    id                   BIGSERIAL PRIMARY KEY,
    captured_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    window_secs          INT NOT NULL DEFAULT 300,
    active_sessions      INT NOT NULL DEFAULT 0,
    code_slots_wanted    NUMERIC(8,2) NOT NULL DEFAULT 0,
    general_slots_wanted NUMERIC(8,2) NOT NULL DEFAULT 0,
    per_session          JSONB NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_fleet_demand_snapshot_recent
    ON fleet_demand_snapshot (captured_at DESC);
"#;

// SCHEMA_V117_BRAIN_FACETED_GRAPH — faceted, multi-parent, multi-root graph under Brain.
// INCREMENT 1: structure/faceting only. COLLATE "C" on every internal text-ID column.
pub const SCHEMA_V117_BRAIN_FACETED_GRAPH: &str = r#"
CREATE TABLE IF NOT EXISTS brain_corpora (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug         TEXT COLLATE "C" NOT NULL,
    title        TEXT NOT NULL,
    description  TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_corpora_slug UNIQUE (slug)
);

CREATE TABLE IF NOT EXISTS brain_sources (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    corpus_id    UUID NOT NULL REFERENCES brain_corpora(id) ON DELETE CASCADE,
    root_path    TEXT COLLATE "C" NOT NULL,
    label        TEXT COLLATE "C",
    host         TEXT COLLATE "C",
    scan_status  TEXT COLLATE "C" NOT NULL DEFAULT 'pending',
    last_scanned TIMESTAMPTZ,
    file_count   INT NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_sources_root UNIQUE (root_path)
);
CREATE INDEX IF NOT EXISTS idx_brain_sources_corpus ON brain_sources(corpus_id);

CREATE TABLE IF NOT EXISTS brain_entities (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    corpus_id        UUID NOT NULL REFERENCES brain_corpora(id) ON DELETE CASCADE,
    entity_key       TEXT COLLATE "C" NOT NULL,
    name             TEXT NOT NULL,
    entity_kind      TEXT COLLATE "C" NOT NULL,
    parent_entity_id UUID REFERENCES brain_entities(id) ON DELETE SET NULL,
    primary_path     TEXT COLLATE "C",
    description      TEXT,
    provenance       TEXT COLLATE "C" NOT NULL DEFAULT 'confirmed',
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_entities_corpus_key UNIQUE (corpus_id, entity_key)
);
CREATE INDEX IF NOT EXISTS idx_brain_entities_corpus ON brain_entities(corpus_id);
CREATE INDEX IF NOT EXISTS idx_brain_entities_parent ON brain_entities(parent_entity_id);
CREATE INDEX IF NOT EXISTS idx_brain_entities_kind   ON brain_entities(corpus_id, entity_kind);

CREATE TABLE IF NOT EXISTS brain_facets (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    corpus_id    UUID NOT NULL REFERENCES brain_corpora(id) ON DELETE CASCADE,
    dimension    TEXT COLLATE "C" NOT NULL,
    value        TEXT COLLATE "C" NOT NULL,
    title        TEXT,
    color        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_facets_corpus_dim_value UNIQUE (corpus_id, dimension, value)
);
CREATE INDEX IF NOT EXISTS idx_brain_facets_corpus_dim ON brain_facets(corpus_id, dimension);

CREATE TABLE IF NOT EXISTS brain_memberships (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    corpus_id    UUID NOT NULL REFERENCES brain_corpora(id) ON DELETE CASCADE,
    member_id    UUID NOT NULL,
    member_kind  TEXT COLLATE "C" NOT NULL,
    entity_id    UUID NOT NULL REFERENCES brain_entities(id) ON DELETE CASCADE,
    relation     TEXT COLLATE "C" NOT NULL DEFAULT 'member_of',
    provenance   TEXT COLLATE "C" NOT NULL DEFAULT 'confirmed',
    confidence   REAL NOT NULL DEFAULT 1.0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT brain_membership_kind_chk CHECK (member_kind IN ('content','entity'))
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_brain_membership
    ON brain_memberships(member_id, entity_id, relation);
CREATE INDEX IF NOT EXISTS idx_brain_membership_entity ON brain_memberships(entity_id);
CREATE INDEX IF NOT EXISTS idx_brain_membership_member ON brain_memberships(member_id);
CREATE INDEX IF NOT EXISTS idx_brain_membership_corpus ON brain_memberships(corpus_id);

CREATE TABLE IF NOT EXISTS brain_node_facets (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    corpus_id    UUID NOT NULL REFERENCES brain_corpora(id) ON DELETE CASCADE,
    node_id      UUID NOT NULL,
    node_kind    TEXT COLLATE "C" NOT NULL,
    facet_id     UUID NOT NULL REFERENCES brain_facets(id) ON DELETE CASCADE,
    provenance   TEXT COLLATE "C" NOT NULL DEFAULT 'confirmed',
    confidence   REAL NOT NULL DEFAULT 1.0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT brain_node_facets_kind_chk CHECK (node_kind IN ('content','entity'))
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_brain_node_facet
    ON brain_node_facets(node_id, facet_id);
CREATE INDEX IF NOT EXISTS idx_brain_node_facets_facet  ON brain_node_facets(facet_id);
CREATE INDEX IF NOT EXISTS idx_brain_node_facets_node   ON brain_node_facets(node_id);
CREATE INDEX IF NOT EXISTS idx_brain_node_facets_corpus ON brain_node_facets(corpus_id);

CREATE TABLE IF NOT EXISTS brain_node_sources (
    node_id      UUID NOT NULL REFERENCES brain_vault_nodes(id) ON DELETE CASCADE,
    source_id    UUID NOT NULL REFERENCES brain_sources(id) ON DELETE CASCADE,
    rel_path     TEXT COLLATE "C" NOT NULL,
    PRIMARY KEY (node_id, source_id)
);
CREATE INDEX IF NOT EXISTS idx_brain_node_sources_source ON brain_node_sources(source_id);

CREATE TABLE IF NOT EXISTS brain_corpus_candidates (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    corpus_id    UUID NOT NULL REFERENCES brain_corpora(id) ON DELETE CASCADE,
    kind         TEXT COLLATE "C" NOT NULL,
    title        TEXT,
    payload      JSONB NOT NULL DEFAULT '{}',
    heuristic    TEXT COLLATE "C",
    confidence   REAL NOT NULL DEFAULT 0.5,
    status       TEXT COLLATE "C" NOT NULL DEFAULT 'pending',
    reviewed_at  TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_brain_corpus_candidates_pending
    ON brain_corpus_candidates(corpus_id, status, created_at)
    WHERE status = 'pending';
"#;

// ════════════════════════════════════════════════════════════════════════════
// SCHEMA_V118_DISK_MANAGEMENT
// Active disk management: a leader-gated disk-reconcile tick that turns the
// passive over-quota ALERT into MOVE/DELETE actuation (gated OFF by default via
// fleet_secrets.disk_policy_mode = off|dry-run|active).
//
// Two minimal additions:
//   1. fleet_model_library.pinned BOOLEAN — honors the smart_lru "future:
//      pinned column" comment. A pinned library row is NEVER eligible for
//      eviction (delete OR move source-delete), regardless of age/peer-copies.
//   2. disk_policy_runs — one row per leader disk-reconcile pass for
//      observability (mode, nodes over quota, planned vs actuated deletes/moves).
//   3. disk_move_log — one row per MOVE the active tick performs (source/target
//      node, library, bytes, verified) so a botched transfer is auditable and the
//      future arbiter (#7) can reason about in-flight relocations.
//
// COLLATE "C" on every internal text-ID column (collation-safe; see the
// 2026-05-30 collation-corruption incident). Idempotent: ADD COLUMN IF NOT
// EXISTS + CREATE TABLE IF NOT EXISTS so re-running is a no-op.
// ════════════════════════════════════════════════════════════════════════════
pub const SCHEMA_V118_DISK_MANAGEMENT: &str = r#"
-- 1. Pin flag on the library: a pinned model is never auto-evicted (delete or
--    move-then-delete). Defaults false so existing rows keep current behaviour.
ALTER TABLE fleet_model_library
    ADD COLUMN IF NOT EXISTS pinned BOOLEAN NOT NULL DEFAULT FALSE;

-- 2. One row per leader disk-reconcile pass — observability for the active tick.
CREATE TABLE IF NOT EXISTS disk_policy_runs (
    id                BIGSERIAL PRIMARY KEY,
    ran_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    mode              TEXT COLLATE "C" NOT NULL,        -- off | dry-run | active
    nodes_over_quota  INT NOT NULL DEFAULT 0,
    planned_deletes   INT NOT NULL DEFAULT 0,
    planned_moves     INT NOT NULL DEFAULT 0,
    actuated_deletes  INT NOT NULL DEFAULT 0,
    actuated_moves    INT NOT NULL DEFAULT 0,
    bytes_planned     BIGINT NOT NULL DEFAULT 0,
    bytes_freed       BIGINT NOT NULL DEFAULT 0,
    detail            JSONB NOT NULL DEFAULT '[]'        -- per-candidate classified plan
);

CREATE INDEX IF NOT EXISTS idx_disk_policy_runs_recent
    ON disk_policy_runs (ran_at DESC);

-- 3. One row per MOVE the active tick performs — audit trail for relocations.
CREATE TABLE IF NOT EXISTS disk_move_log (
    id                BIGSERIAL PRIMARY KEY,
    started_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at       TIMESTAMPTZ,
    source_node       TEXT COLLATE "C" NOT NULL,
    target_node       TEXT COLLATE "C" NOT NULL,
    catalog_id        TEXT COLLATE "C" NOT NULL,
    runtime           TEXT COLLATE "C" NOT NULL,
    src_library_id    TEXT COLLATE "C" NOT NULL,
    dst_library_id    TEXT COLLATE "C",                 -- set once the target row exists
    size_bytes        BIGINT NOT NULL DEFAULT 0,
    status            TEXT COLLATE "C" NOT NULL DEFAULT 'started', -- started|verified|source_deleted|failed
    error             TEXT
);

CREATE INDEX IF NOT EXISTS idx_disk_move_log_recent
    ON disk_move_log (started_at DESC);
"#;

// ─────────────────────────────────────────────────────────────────────────
// V119 — Global resource arbiter.
//
// Backlog #7. EXPLICIT-declaration arbiter: a session/operator declares an
// *intent* to reserve a host SET for a span of time; the leader-gated arbiter
// tick grants it all-or-nothing, runs an idempotent prework plan (e.g. offload
// minimax → disk to free GPU), holds a TTL lease, fences general task-claiming
// off the host, and on release runs an idempotent restore plan (reload). This
// is MOSTLY WIRING of existing primitives (V114 reservation CAS + fleet_tasks
// claim queue + ff offload), so the schema only adds the two missing pieces:
//
//   PART A — work_intents: the intent registry (which IS the FIFO grant queue).
//   PART B — owner/lease/expiry columns on the V114 computers reservation.
//
// Conventions: COLLATE "C" on every internal text-ID column; gen_random_uuid
// PK; ADD COLUMN IF NOT EXISTS (idempotent, mirrors V114); partial indexes for
// cheap deterministic scans; no hardcoded fleet data (host sets are
// caller-supplied / dgx-pair expanded from the DB at runtime).
pub const SCHEMA_V119_RESOURCE_ARBITER: &str = r#"
-- PART A — work_intents: the missing intent registry (EXPLICIT-declaration).
CREATE TABLE IF NOT EXISTS work_intents (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    requester       TEXT COLLATE "C" NOT NULL,
    project         TEXT COLLATE "C",
    target_host_set JSONB NOT NULL DEFAULT '[]',
    requires_capability JSONB NOT NULL DEFAULT '[]',
    exclusive       BOOLEAN NOT NULL DEFAULT TRUE,
    requested_secs  INT NOT NULL DEFAULT 3600,
    priority        INT NOT NULL DEFAULT 100,
    state           TEXT COLLATE "C" NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending','granted','active','releasing','done','denied')),
    task_desc       TEXT,
    prework_plan    JSONB NOT NULL DEFAULT '[]',
    restore_plan    JSONB NOT NULL DEFAULT '[]',
    prework_cursor  INT NOT NULL DEFAULT 0,
    restore_cursor  INT NOT NULL DEFAULT 0,
    denied_reason   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    granted_at      TIMESTAMPTZ,
    expires_at      TIMESTAMPTZ,
    released_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_work_intents_state
    ON work_intents(state) WHERE state <> 'done';

CREATE INDEX IF NOT EXISTS idx_work_intents_fifo
    ON work_intents(priority DESC, created_at ASC) WHERE state = 'pending';

-- PART B — lease/owner/expiry on the V114 computers reservation. V114 had
-- reservation_state + reserved_reason + reserved_at only. reserved_reason
-- continues to carry the owner-tag string 'arbiter:<intent_id>' so the existing
-- owner-scoped pg_reap_stale_reservations reaper keeps working unchanged; the
-- new columns add the per-intent lease the arbiter reaper compares to NOW().
ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS reservation_owner   UUID,
    ADD COLUMN IF NOT EXISTS reservation_expires_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_computers_reservation_lease
    ON computers(reservation_expires_at) WHERE reservation_expires_at IS NOT NULL;
"#;
//
// BUILD #9 conformance + #10 AMD ROCm-bind, increment 1.
//
// ROOT GAP: detection treats "version string parsed = ok". Two AMD boxes
// presented the SAME "green pip but GPU never binds" symptom with DIFFERENT
// real causes (both found live on the fleet):
//   - logan:    torch 2.10.0+cu128 (a CUDA wheel) on a Strix Halo AMD box
//               → torch.cuda.is_available()=False forever (wrong backend).
//   - veronica: user NOT in render/video groups → cannot open /dev/kfd →
//               rocminfo as the daemon user enumerates ZERO gpus (as root:
//               gfx1151 fine). DB "2GB VRAM" is misleading — Strix Halo UMA
//               exposes ~62GB GTT backed by 123GB RAM.
//
// V120 (increment 1 — TIGHT scope; full apply-reconciler deferred to a
// follow-up) lands the DESIRED-STATE + VERIFY substrate so conformance can
// say "version parsed" is NOT enough:
//
//   (a) conformance_profiles            — desired state keyed
//       (os_family, hardware_class, role) holding required packages/versions.
//   (b) conformance_profile_packages    — one row per required (software_id,
//       version_constraint / must_contain / must_not_contain) for a profile.
//   (c) conformance_checks              — the VERIFY GATES a profile must
//       pass. check_kind drives which actuator the agent runs:
//         'gpu_bind'   — run a python assert that +rocm is in torch.__version__
//                        AND torch.cuda.is_available() AND a real gfx tensor
//                        op succeeds. Records conformant=bool — NOT a parse.
//         'amd_arch'   — flag non-conformant any AMD host whose torch lacks
//                        +rocm OR carries +cu (wrong-backend wheel, the logan
//                        case).
//         'kfd_access' — assert the daemon user is in render+video groups AND
//                        /dev/kfd is readable (the veronica case).
//         'pkg_version'— the legacy "is this version present" check (kept so
//                        the gate set is complete, NOT the whole story).
//   (d) conformance_results             — latest-wins record of per-host
//       per-check conformant=bool + a SPECIFIC machine-readable reason. The
//       artifact that proves we caught what a version parse misses.
//   (e) software_registry ROCm-training-stack rows (rocm 6.4, the correct
//       +rocm torch for gfx1151, the HSA_OVERRIDE runtime_env entry).
//
// Seeds the `amd-training` role profile for the
// (linux-ubuntu, strix-halo, amd-training) key grounded in the REAL probe:
// gfx1151, ROCm 6.4.0, python 3.12, HSA_OVERRIDE_GFX_VERSION=11.5.1.
//
// SAFETY: the conformance TICK is leader-gated and gated by
// `fleet_secrets.conformance_mode` (off|dry-run|active, DEFAULT off) exactly
// like the autoscaler. In off/dry-run it actuates NOTHING — increment 1 only
// RECORDS conformance, it never remediates a host.
pub const SCHEMA_V120_FLEET_CONFORMANCE: &str = r#"
-- (a) Desired-state profile, keyed (os_family, hardware_class, role).
CREATE TABLE IF NOT EXISTS conformance_profiles (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    profile_key    TEXT COLLATE "C" NOT NULL,   -- "linux-ubuntu/strix-halo/amd-training"
    os_family      TEXT COLLATE "C" NOT NULL,   -- macos|linux-ubuntu|linux-dgx|windows
    hardware_class TEXT COLLATE "C" NOT NULL,   -- strix-halo|apple-silicon|dgx-gb10|generic
    role           TEXT COLLATE "C" NOT NULL,   -- amd-training|inference|leader|generic
    title          TEXT NOT NULL,
    description    TEXT,
    runtime_env    JSONB NOT NULL DEFAULT '{}'::jsonb,  -- env the role REQUIRES
    enabled        BOOLEAN NOT NULL DEFAULT true,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_conformance_profile_key UNIQUE (profile_key)
);
CREATE INDEX IF NOT EXISTS idx_conformance_profiles_match
    ON conformance_profiles(os_family, hardware_class, role) WHERE enabled;

-- (b) Required packages/versions for a profile (→ software_registry).
CREATE TABLE IF NOT EXISTS conformance_profile_packages (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    profile_id         UUID NOT NULL REFERENCES conformance_profiles(id) ON DELETE CASCADE,
    software_id        TEXT COLLATE "C" NOT NULL,   -- references software_registry(id)
    version_constraint TEXT COLLATE "C",            -- ">=6.4.0", NULL = any
    must_contain       TEXT COLLATE "C",            -- substring version MUST carry, e.g. "+rocm"
    must_not_contain   TEXT COLLATE "C",            -- substring it MUST NOT carry, e.g. "+cu"
    required           BOOLEAN NOT NULL DEFAULT true,
    note               TEXT,
    CONSTRAINT uq_conformance_pkg UNIQUE (profile_id, software_id)
);
CREATE INDEX IF NOT EXISTS idx_conformance_pkg_profile
    ON conformance_profile_packages(profile_id);

-- (c) The VERIFY GATES a profile must pass. verify_cmd is the literal command
--     we RUN on the host — the gate is "did this command succeed", NOT "did a
--     version string parse".
CREATE TABLE IF NOT EXISTS conformance_checks (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    profile_id   UUID NOT NULL REFERENCES conformance_profiles(id) ON DELETE CASCADE,
    check_key    TEXT COLLATE "C" NOT NULL,   -- "gpu_bind"|"amd_arch"|"kfd_access"|"rocm_present"
    check_kind   TEXT COLLATE "C" NOT NULL,   -- gpu_bind|amd_arch|kfd_access|pkg_version
    title        TEXT NOT NULL,
    verify_cmd   TEXT COLLATE "C" NOT NULL,   -- literal host command; exit 0 = conformant
    severity     TEXT COLLATE "C" NOT NULL DEFAULT 'blocker', -- blocker|warn
    enabled      BOOLEAN NOT NULL DEFAULT true,
    CONSTRAINT uq_conformance_check UNIQUE (profile_id, check_key),
    CONSTRAINT conformance_check_kind_chk
        CHECK (check_kind IN ('gpu_bind','amd_arch','kfd_access','pkg_version')),
    CONSTRAINT conformance_check_sev_chk
        CHECK (severity IN ('blocker','warn'))
);
CREATE INDEX IF NOT EXISTS idx_conformance_checks_profile
    ON conformance_checks(profile_id) WHERE enabled;

-- (d) Per-host per-check result, latest-wins on (computer_id, profile_id,
--     check_key). conformant is MEASURED; reason carries the SPECIFIC cause.
CREATE TABLE IF NOT EXISTS conformance_results (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    computer_id  UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    profile_id   UUID NOT NULL REFERENCES conformance_profiles(id) ON DELETE CASCADE,
    check_key    TEXT COLLATE "C" NOT NULL,
    check_kind   TEXT COLLATE "C" NOT NULL,
    conformant   BOOLEAN NOT NULL,
    severity     TEXT COLLATE "C" NOT NULL DEFAULT 'blocker',
    reason       TEXT,                         -- machine+human readable cause
    raw_output   TEXT,                         -- captured stdout/stderr (trimmed)
    checked_by   TEXT COLLATE "C",             -- worker_name that ran the check
    checked_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_conformance_result UNIQUE (computer_id, profile_id, check_key)
);
CREATE INDEX IF NOT EXISTS idx_conformance_results_computer
    ON conformance_results(computer_id, checked_at DESC);
CREATE INDEX IF NOT EXISTS idx_conformance_results_nonconformant
    ON conformance_results(profile_id) WHERE NOT conformant;

-- (e) software_registry rows for the ROCm training stack.
INSERT INTO software_registry
    (id, display_name, kind, applies_to_os_family, version_source,
     upgrade_playbook, requires_restart, requires_reboot, detection, metadata)
VALUES
    ('rocm',
     'AMD ROCm platform',
     'runtime',
     'linux-ubuntu',
     '{"method":"manual"}'::jsonb,
     '{}'::jsonb,
     false, false,
     '{"method":"binary_version","binary":"hipconfig","args":["--version"],"regex":"HIP version:\\s*(\\S+)"}'::jsonb,
     '{"required_for":["amd-training"],"target_version":"6.4.0","gfx":"gfx1151"}'::jsonb),
    ('torch-rocm',
     'PyTorch (ROCm build) for gfx1151',
     'runtime',
     'linux-ubuntu',
     '{"method":"manual"}'::jsonb,
     '{}'::jsonb,
     false, false,
     '{"method":"binary_version","binary":"python3","args":["-c","import torch;print(torch.__version__)"],"regex":"(\\S+)"}'::jsonb,
     '{"required_for":["amd-training"],"must_contain":"+rocm","must_not_contain":"+cu","gfx":"gfx1151","index_url":"https://download.pytorch.org/whl/rocm6.4"}'::jsonb),
    ('hsa-override-gfx',
     'HSA_OVERRIDE_GFX_VERSION runtime env (Strix Halo gfx1151)',
     'config',
     'linux-ubuntu',
     '{"method":"manual"}'::jsonb,
     '{}'::jsonb,
     false, false,
     '{"method":"env","var":"HSA_OVERRIDE_GFX_VERSION"}'::jsonb,
     '{"required_for":["amd-training"],"value":"11.5.1","reason":"map gfx1151 onto a ROCm-supported gfx target"}'::jsonb)
ON CONFLICT (id) DO NOTHING;

-- Seed the amd-training profile for (linux-ubuntu, strix-halo, amd-training),
-- grounded in the live probe: gfx1151, ROCm 6.4.0, python 3.12,
-- HSA_OVERRIDE_GFX_VERSION=11.5.1.
WITH p AS (
    INSERT INTO conformance_profiles
        (profile_key, os_family, hardware_class, role, title, description, runtime_env)
    VALUES
        ('linux-ubuntu/strix-halo/amd-training',
         'linux-ubuntu', 'strix-halo', 'amd-training',
         'AMD Strix Halo ROCm training stack',
         'gfx1151 UMA (~62GB GTT over 123GB RAM); ROCm 6.4 + a +rocm torch '
         || 'wheel + HSA_OVERRIDE_GFX_VERSION=11.5.1; daemon user must reach '
         || '/dev/kfd via render+video groups.',
         '{"HSA_OVERRIDE_GFX_VERSION":"11.5.1"}'::jsonb)
    ON CONFLICT (profile_key) DO UPDATE SET updated_at = NOW()
    RETURNING id
)
INSERT INTO conformance_profile_packages
    (profile_id, software_id, version_constraint, must_contain, must_not_contain, note)
SELECT p.id, v.software_id, v.version_constraint, v.must_contain, v.must_not_contain, v.note
FROM p, (VALUES
    ('rocm',             '>=6.4.0', NULL,    NULL,  'ROCm platform 6.4+ for gfx1151'),
    ('torch-rocm',       NULL,      '+rocm', '+cu', 'torch MUST be a +rocm wheel, NEVER a +cu wheel'),
    ('hsa-override-gfx', NULL,      NULL,    NULL,  'HSA_OVERRIDE_GFX_VERSION=11.5.1 in daemon env')
) AS v(software_id, version_constraint, must_contain, must_not_contain, note)
ON CONFLICT (profile_id, software_id) DO NOTHING;

-- Seed the four VERIFY GATES for the amd-training profile.
WITH p AS (
    SELECT id FROM conformance_profiles
    WHERE profile_key = 'linux-ubuntu/strix-halo/amd-training'
)
INSERT INTO conformance_checks
    (profile_id, check_key, check_kind, title, verify_cmd, severity)
SELECT p.id, v.check_key, v.check_kind, v.title, v.verify_cmd, v.severity
FROM p, (VALUES
    -- AMD ARCH GATE: catch the logan case (+cu wheel on an AMD box) BEFORE we
    -- even try to bind. The whole assert lives in ONE python3 -c program (no
    -- nested shell $(...) — SSH-quote-safe): it prints its own NONCONFORMANT
    -- line carrying the REAL torch version and exits non-zero.
    ('amd_arch', 'amd_arch',
     'torch wheel is +rocm (not +cu) on an AMD host',
     'python3 -c ''import sys'' && python3 -c ''
import sys
try:
    import torch
    v = torch.__version__
except Exception as e:
    print("NONCONFORMANT: torch not importable: %s" % e); sys.exit(1)
if "+rocm" not in v or "+cu" in v:
    print("NONCONFORMANT: torch=%s is not a +rocm wheel (wrong backend on an AMD host)" % v)
    sys.exit(1)
print("OK rocm wheel %s" % v)
''',
     'blocker'),
    -- KFD ACCESS GATE: catch the veronica case (user not in render/video →
    -- /dev/kfd unreadable → zero gpus enumerated as the daemon user).
    ('kfd_access', 'kfd_access',
     'daemon user in render+video groups AND /dev/kfd readable',
     'g=$(id -nG); '
     || 'for grp in render video; do echo "$g" | tr " " "\n" | grep -qx "$grp" '
     || '|| { echo "NONCONFORMANT: user not in group $grp (groups: $g)"; exit 1; }; done; '
     || '[ -r /dev/kfd ] || { echo "NONCONFORMANT: /dev/kfd not readable by $(id -un)"; exit 1; }',
     'blocker'),
    -- GPU-BIND VERIFY GATE: the real proof. +rocm AND
    -- torch.cuda.is_available() AND a real gfx tensor op. What a
    -- "version parsed = ok" detector can NEVER tell you.
    ('gpu_bind', 'gpu_bind',
     'torch binds the GPU: +rocm AND is_available() AND a real gfx tensor op',
     'HSA_OVERRIDE_GFX_VERSION=${HSA_OVERRIDE_GFX_VERSION:-11.5.1} python3 -c ''import sys,torch; '
     || 'assert "+rocm" in torch.__version__, "torch is not a +rocm build: "+torch.__version__; '
     || 'assert torch.cuda.is_available(), "torch.cuda.is_available() is False (GPU never bound)"; '
     || 'x=torch.ones(1024,1024,device="cuda"); y=(x@x).sum().item(); '
     || 'assert y==1024.0*1024.0*1024.0, "gfx tensor op produced wrong result: "+str(y); '
     || 'print("BIND_OK", torch.cuda.get_device_name(0))''',
     'blocker'),
    -- ROCm-present pkg_version gate (the legacy "version parsed" check — kept
    -- so the gate set is complete, explicitly NOT the whole story).
    ('rocm_present', 'pkg_version',
     'ROCm platform present (hipconfig reports a version)',
     'hipconfig --version 2>/dev/null | grep -qE "HIP version:\\s*[0-9]" '
     || '|| { echo "NONCONFORMANT: hipconfig reports no HIP version"; exit 1; }',
     'warn')
) AS v(check_key, check_kind, title, verify_cmd, severity)
ON CONFLICT (profile_id, check_key) DO NOTHING;
"#;

// V122 — ff_interactions: the unified interaction log. One row per ff "turn"
// captured at the response boundary (gateway / session_runner / CLI agent loop
// / fleet_run). Serves three jobs from one table: (1) SLM training corpus
// (request_text → response_text with the route+steps in between), (2) the live
// Agent Session Console the UI is missing, (3) routing telemetry for the
// cost/capability router. The error_signature + partial index drive the
// 30-min self-heal tick (novel errors → fleet_self_heal_queue dispatch).
// error_signature is COLLATE "C" — it's a hash ID used for dedup, so byte
// ordering must be locale-independent (see [[db-collation-prevention]]).
// NB: numbered V122 not V121 — 121 was taken by cortex_code_graph in the live
// DB, so this migration was silently skipped until renumbered.
pub const SCHEMA_V122_INTERACTION_LOG: &str = r#"
CREATE TABLE IF NOT EXISTS ff_interactions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id      UUID,
    ts              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    channel         TEXT NOT NULL DEFAULT 'unknown',  -- telegram|web|cli|voice|jarvis|mcp
    user_id         UUID,
    request_text    TEXT NOT NULL DEFAULT '',          -- SLM input
    request_meta    JSONB NOT NULL DEFAULT '{}'::jsonb, -- modality(text|voice|image|video), attachments, source
    route_decision  JSONB NOT NULL DEFAULT '{}'::jsonb, -- engine chosen + why (tier, cost est, local-vs-cloud)
    engine          TEXT,                              -- claude-code|codex|kimi|grok|local:<model>
    steps           JSONB NOT NULL DEFAULT '[]'::jsonb, -- [{type:llm|tool, name, args_summary, result_summary, ms, ok}]
    response_text   TEXT NOT NULL DEFAULT '',          -- SLM target
    tokens_in       INTEGER NOT NULL DEFAULT 0,
    tokens_out      INTEGER NOT NULL DEFAULT 0,
    cost_usd        DOUBLE PRECISION NOT NULL DEFAULT 0,
    latency_ms      INTEGER,
    outcome         TEXT NOT NULL DEFAULT 'ok',         -- ok|error|partial
    error_text      TEXT,
    error_signature TEXT COLLATE "C",                  -- stable hash for self-heal dedup
    ff_build_sha    TEXT,
    model_versions  JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS idx_ff_interactions_ts        ON ff_interactions (ts DESC);
CREATE INDEX IF NOT EXISTS idx_ff_interactions_session   ON ff_interactions (session_id, ts);
CREATE INDEX IF NOT EXISTS idx_ff_interactions_error     ON ff_interactions (error_signature) WHERE outcome = 'error';
CREATE INDEX IF NOT EXISTS idx_ff_interactions_engine    ON ff_interactions (engine, outcome);
"#;

/// Per-file content-hash ledger so Cortex can reindex only changed files.
///
/// `ff cortex index` scans the corpus (updating each `content:file` node's
/// `content_hash`) then re-extracts. The full path rewipes every `code:*` node
/// each run; with this ledger the `--incremental` path compares the corpus
/// scan's current `content_hash` against the hash Cortex last indexed the file
/// at, and only re-extracts files that differ (plus deletes symbols of removed
/// files). `indexed_hash` mirrors the scan's `cheap_hash` (path+size+mtime).
/// Internal text-ID columns use COLLATE "C" so ON CONFLICT stays collation-safe.
pub const SCHEMA_V123_CORTEX_FILE_INDEX: &str = r#"
CREATE TABLE IF NOT EXISTS cortex_file_index (
    corpus_slug   TEXT COLLATE "C" NOT NULL,
    file_path     TEXT COLLATE "C" NOT NULL,
    indexed_hash  TEXT NOT NULL,
    indexed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (corpus_slug, file_path)
);
CREATE INDEX IF NOT EXISTS idx_cortex_file_index_corpus ON cortex_file_index (corpus_slug);
"#;

/// V124: persist 1-based source line spans on Cortex symbol nodes so
/// `ff cortex review` can refine its file-level change map to HUNK level —
/// listing only the symbols whose bodies actually overlap the git-diff line
/// ranges, instead of every symbol a changed file defines. Nullable: only
/// `code:*` nodes set them; pre-existing nodes (and import/extern placeholders)
/// stay NULL, in which case review degrades gracefully to file-level.
pub const SCHEMA_V124_CORTEX_SYMBOL_LINES: &str = r#"
ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS start_line INT;
ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS end_line   INT;
"#;

/// V125: give detected communities a durable registry. `detect_communities`
/// previously wrote each node's `community_id` but never populated the
/// `brain_communities` table, so `ff brain stats`, the `brain_stats` MCP tool,
/// and the gateway brain API all reported 0 communities despite thousands being
/// found — and there was nowhere to attach a community summary. This adds:
///   - `member_hash` — a STABLE identity for a community = hash of its sorted
///     member set. Union-find renumbers `community_id` arbitrarily each run, but
///     an unchanged connected component hashes the same, so a re-detection maps
///     back to the same row and its summary survives (the GraphRAG
///     re-summarize-only-changed lever). UNIQUE so reconciliation can upsert.
///   - `summary` / `summary_model` / `summary_updated_at` — reserved now so the
///     fleet-LLM community-summary pass (cortex roadmap #4) is a pure data fill
///     with no further migration. Nullable until that pass runs.
pub const SCHEMA_V125_BRAIN_COMMUNITY_REGISTRY: &str = r#"
ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS member_hash        TEXT;
ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS summary            TEXT;
ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS summary_model      TEXT;
ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS summary_updated_at TIMESTAMPTZ;
CREATE UNIQUE INDEX IF NOT EXISTS idx_brain_communities_member_hash
    ON brain_communities (member_hash);
"#;

/// V126: make `brain_communities.god_node_id` ON DELETE SET NULL. V125 started
/// populating `god_node_id` (it was always NULL before, so the default NO ACTION
/// FK never bit). Cortex reindex DELETEs `brain_vault_nodes` rows (image-node
/// refresh, incremental GC of removed symbols); if a deleted node was some
/// community's god node, the FK now BLOCKS the delete and the reindex fails
/// partway. The god node is advisory, so nulling it on delete is correct — the
/// next `ff brain communities` run repopulates it from the fresh graph.
pub const SCHEMA_V126_COMMUNITY_GOD_NODE_ONDELETE: &str = r#"
ALTER TABLE brain_communities DROP CONSTRAINT IF EXISTS brain_communities_god_node_id_fkey;
ALTER TABLE brain_communities ADD CONSTRAINT brain_communities_god_node_id_fkey
    FOREIGN KEY (god_node_id) REFERENCES brain_vault_nodes(id) ON DELETE SET NULL;
"#;

/// V127: a CODE-SCOPED community registry, parallel to `brain_communities`.
///
/// `detect_communities` (the brain-KG clusterer) is union-find connected
/// components over ALL `brain_vault_edges` — `contains` (corpus→file→symbol) and
/// `imports` structurally bridge nearly the entire code graph into ONE
/// mega-component (measured live: 44,993 nodes), whose summary is garbage. That's
/// correct for the brain KG (where `contains`/`link` ARE the structure) but
/// useless for Cortex's "explain this subsystem" answers.
///
/// The fix (cortex roadmap #4 blocker) is a SEPARATE clustering — label
/// propagation over the `calls` subgraph among non-extern `code:*` nodes — which
/// *subdivides* the connected graph instead of merging it. Its output can't share
/// `community_id`/`brain_communities` (the brain KG still wants the
/// connected-components view for `ff brain communities`/`stats`), so this adds a
/// parallel column + registry:
///   - `brain_vault_nodes.code_community_id` — the code cluster a symbol belongs
///     to (NULL for non-code / extern / never-called-internally nodes).
///   - `brain_code_communities` — mirror of `brain_communities` (stable
///     `member_hash`, advisory `god_node_id`, reserved `summary*` columns) so the
///     existing summarize pass + `ff cortex explain` repoint here with no further
///     migration. `god_node_id` is ON DELETE SET NULL for the same reindex-GC
///     reason as V126.
pub const SCHEMA_V127_CORTEX_CODE_COMMUNITIES: &str = r#"
ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS code_community_id INT;
CREATE INDEX IF NOT EXISTS idx_vault_nodes_code_community
    ON brain_vault_nodes(code_community_id) WHERE code_community_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS brain_code_communities (
    id                 SERIAL PRIMARY KEY,
    member_hash        TEXT,
    god_node_id        UUID REFERENCES brain_vault_nodes(id) ON DELETE SET NULL,
    member_count       INT NOT NULL DEFAULT 0,
    level              INT NOT NULL DEFAULT 0,
    parent_member_hash TEXT,
    summary            TEXT,
    summary_model      TEXT,
    summary_updated_at TIMESTAMPTZ,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_brain_code_communities_member_hash_level
    ON brain_code_communities (member_hash, level);
CREATE INDEX IF NOT EXISTS idx_brain_code_communities_parent
    ON brain_code_communities (parent_member_hash);
"#;

/// V128: per-file `pub use` re-export ledger so an incremental Cortex reindex can
/// rebuild the corpus-wide facade map.
///
/// `resolve_facade_call` redirects a call through a `pub use` facade
/// (`ff_db::pg_get_brain_user` → real `ff_db::queries::pg_get_brain_user`, or the
/// caller-prefixed `ff_gateway::brain_api::ff_db::pg_get_brain_user`) onto the real
/// internal fn. That redirect needs the corpus-wide re-export map. Before this
/// table the map was rebuilt only from the files being extracted THIS run — fine
/// for a full reindex (every file is in the batch) but on an INCREMENTAL reindex
/// the changed files rarely include the `lib.rs`/`mod.rs` that owns the `pub use`,
/// so every facade call in a changed file silently degraded to a `code:extern`
/// until the next full reindex (the `pg_*` query facade was the single biggest
/// internal mis-resolution source `ff cortex doctor` reported). This ledger
/// persists each file's re-exports (named + glob) so the map is loaded
/// whole-corpus on incremental, exactly like `internal_fns`/`internal_types`.
/// Per-file rows ride the existing incremental lifecycle: cleared on a full
/// reindex, re-recorded per changed file (so a removed `pub use` drops out).
/// Internal text-ID columns use COLLATE "C" so ON CONFLICT stays collation-safe.
pub const SCHEMA_V128_CORTEX_REEXPORTS: &str = r#"
CREATE TABLE IF NOT EXISTS cortex_reexports (
    corpus_slug   TEXT COLLATE "C" NOT NULL,
    file_path     TEXT COLLATE "C" NOT NULL,
    kind          TEXT COLLATE "C" NOT NULL,   -- 'named' | 'glob'
    facade        TEXT COLLATE "C" NOT NULL,   -- named: facade path; glob: base module
    target        TEXT COLLATE "C" NOT NULL,   -- named: real target; glob: target module
    PRIMARY KEY (corpus_slug, file_path, kind, facade, target)
);
CREATE INDEX IF NOT EXISTS idx_cortex_reexports_corpus ON cortex_reexports (corpus_slug);
"#;

// ─── V129: docker upstream tracks tags, not releases ────────────────────────
//
// V47 set docker's version_source to {method:github_release, repo:docker/cli}
// with the default ref_kind=tagged, which probes `/repos/docker/cli/releases/
// latest`. docker/cli publishes git TAGS but never cuts GitHub "Releases", so
// that endpoint 404s on EVERY 6h upstream pass — `ff software check-upstream`
// has reported `errors: 1` (docker) indefinitely. Repoint it to the new
// `latest_tag` ref_kind (lists `/tags`, picks the newest release semver) in
// both catalogs. Idempotent: only rewrites the rows still on the releases form.
pub const SCHEMA_V129_DOCKER_LATEST_TAG: &str = r#"
UPDATE software_registry SET
    version_source = '{"method":"github_release","repo":"docker/cli","ref_kind":"latest_tag"}'::jsonb
 WHERE id = 'docker'
   AND version_source->>'method' = 'github_release'
   AND version_source->>'repo'   = 'docker/cli'
   AND (version_source->>'ref_kind') IS DISTINCT FROM 'latest_tag';

UPDATE external_tools SET
    version_source = '{"method":"github_release","repo":"docker/cli","ref_kind":"latest_tag"}'::jsonb
 WHERE id = 'docker'
   AND version_source->>'method' = 'github_release'
   AND version_source->>'repo'   = 'docker/cli'
   AND (version_source->>'ref_kind') IS DISTINCT FROM 'latest_tag';
"#;

// ─── V130: scheduled backup restore-drill ───────────────────────────────────
//
// Backups (`pg_basebackup -Ft -z`, 4h) were never automatically *test-restored*
// — a backup that has never been proven restorable is the silent 2026-04-18-wipe
// risk. This adds:
//   1. `backup_drills` — one row per automated restore-drill outcome (the
//      leader tick in `ff_agent::ha::restore_drill` decrypts → extracts →
//      validates the newest backup is a structurally complete PGDATA, and
//      records pass/fail + metrics here).
//   2. the `backup_restore_drill_failed` alert policy — fires (critical,
//      telegram, 12h cooldown) when a drill fails OR no successful drill has
//      run inside the staleness window.
// `backup_id` is ON DELETE SET NULL so drill history survives backup pruning.
pub const SCHEMA_V130_BACKUP_RESTORE_DRILL: &str = r#"
CREATE TABLE IF NOT EXISTS backup_drills (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    backup_id        UUID REFERENCES backups(id) ON DELETE SET NULL,
    backup_file      TEXT NOT NULL,
    database_kind    TEXT NOT NULL DEFAULT 'postgres',
    started_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at      TIMESTAMPTZ,
    success          BOOLEAN NOT NULL DEFAULT false,
    stage            TEXT NOT NULL,
    detail           TEXT,
    extracted_bytes  BIGINT,
    file_count       BIGINT,
    pg_version       TEXT,
    verifybackup     BOOLEAN,
    duration_ms      BIGINT,
    drill_node       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_backup_drills_started
    ON backup_drills (started_at DESC);
CREATE INDEX IF NOT EXISTS idx_backup_drills_success_started
    ON backup_drills (success, started_at DESC);

INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('backup_restore_drill_failed',
   'Automated backup restore-drill failed, or no successful drill within the staleness window (silent data-loss risk, cf. 2026-04-18 wipe)',
   'backup_restore_drill_failed', 'leader_only', '> 0',
   0, 'critical', 43200, 'telegram', true)
ON CONFLICT (name) DO NOTHING;
"#;

// ─── V131: fleet-integrity verify tick ──────────────────────────────────────
//
// `verify_computer` (the full post-onboarding check battery: daemon health,
// DB reachability, tool-version reporting, defer-worker, etc.) only ran
// on-demand (`ff fleet verify-node <node>` / the onboard gateway endpoint). So a
// host that enrolled half-configured, OR drifted into a broken state while
// alive, stayed INVISIBLE until an operator manually re-verified it — exactly
// the "9th identical half-configured box" the enrollment self-heal directive
// calls out (PROD_READINESS item 23, GAP).
//
// `revive_scan` already auto-repairs *dead* nodes (ODOWN → restart daemon),
// but an ALIVE-but-misconfigured node fails none of revive's liveness gates.
// This adds the detection layer: a leader-gated tick
// (`ff_agent::fleet_integrity`) runs the verify battery across all *online*
// members on a schedule and fires `fleet_integrity_degraded` when any member
// has failing checks. Gated by `fleet_secrets.fleet_integrity_mode`
// (off|report, default off) — report = detect+alert, never mutates. Per-gap
// auto-repair (active mode) is a tracked follow-up; this closes the detection
// half so drift is never silent again.
pub const SCHEMA_V131_FLEET_INTEGRITY: &str = r#"
INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('fleet_integrity_degraded',
   'One or more ONLINE fleet members failed the verify_computer battery (half-configured enrollment or config drift while alive)',
   'fleet_integrity_degraded', 'leader_only', '> 0',
   0, 'warning', 21600, 'telegram', true)
ON CONFLICT (name) DO NOTHING;
"#;

// ─── V132: persist the evolution backlog ────────────────────────────────────
//
// `ff_evolution::BacklogService` promotes a recurring root-cause to a durable
// backlog item once it has been seen `recurrence_threshold` (default 3) times.
// But it held that state in a pure in-memory `DashMap`, so EVERY daemon restart
// reset all occurrence counters to zero — a root cause at 2/3 occurrences lost
// its history and could never accumulate to promotion. The "durable backlog
// from recurring failures" was, ironically, not durable.
//
// This table persists one row per fingerprint. The `BacklogItem` is stored
// whole as JSONB (it already derives Serialize/Deserialize), so the schema is
// migration-stable as the struct evolves; `durable` is lifted into a column for
// cheap querying. The service hydrates from here on startup and writes through
// after each ingest cycle.
pub const SCHEMA_V132_EVOLUTION_BACKLOG: &str = r#"
CREATE TABLE IF NOT EXISTS evolution_backlog (
    fingerprint  TEXT PRIMARY KEY,
    item         JSONB NOT NULL,
    durable      BOOLEAN NOT NULL DEFAULT false,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_evolution_backlog_durable
    ON evolution_backlog (durable) WHERE durable;
"#;

// ─── V133: HA leader-handoff Phase 2 — maintenance lease ────────────────────
//
// Phase 1 (#224) gave a graceful operator step-down via the `leader_yield_request`
// fleet_secret (`<member>|<until>`): the named leader yields and election picks
// the next-best-priority follower, auto-failing-back when the deadline passes.
// Phase 2 adds a DESIGNATED successor + visibility: two nullable columns on the
// singleton `fleet_leader_state` row record an active maintenance lease — who
// should hold leadership (`standby_member`) and until when (`relinquishing_until`).
// While the lease is live, election PREFERS `standby_member` outright (not merely
// the lowest priority), and it auto-reverts when `relinquishing_until` passes.
// Columns are nullable + inert by default, so the row's existing semantics are
// unchanged when no lease is set. No PG-primary move (that's Phase 3).
pub const SCHEMA_V133_LEADER_MAINTENANCE_LEASE: &str = r#"
ALTER TABLE fleet_leader_state
    ADD COLUMN IF NOT EXISTS standby_member      TEXT,
    ADD COLUMN IF NOT EXISTS relinquishing_until TIMESTAMPTZ;
"#;

// ─── V134: staged upgrade rollouts + auto-halt (PROD_READINESS item 26) ─────
//
// Today a fleet upgrade composes EVERY target into priority-ordered waves and
// inserts them all at once — priority gates ORDER, not SUCCESS, so a bad build
// rolls all 14 non-leader hosts before failures surface (the wave self-kill
// history). This table drives a GATED progression: one row per rollout holds an
// ordered `stages` JSONB list ({stage_idx, target_names[]}); the leader-gated
// `upgrade_rollout` tick advances `current_stage` only when the current stage's
// fleet_tasks all reached a terminal state AND its failure rate is under
// `failure_threshold_pct` (a canary stage halts on the FIRST failure). On a
// breach it sets status='halted' + halted_reason and fires the
// `upgrade_rollout_halted` alert; otherwise it composes ONLY the next stage's
// targets (preserving the V62 one-wave-per-family-in-flight invariant) or marks
// the rollout 'completed' when no stages remain.
//
// `fleet_tasks` gains nullable rollout_id/rollout_stage so the gate can count a
// stage's outcomes; the columns are inert for non-rollout tasks. The whole
// subsystem is gated OFF by default behind `fleet_secrets.staged_rollout_mode`
// (off|dry-run|active) — deploying it is harmless until an operator opts in.
pub const SCHEMA_V134_UPGRADE_ROLLOUTS: &str = r#"
CREATE TABLE IF NOT EXISTS upgrade_rollouts (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    software_id           TEXT,
    started_by            TEXT,
    stages                JSONB,
    current_stage         INT  NOT NULL DEFAULT 0,
    status                TEXT NOT NULL DEFAULT 'in_progress',
    failure_threshold_pct INT  NOT NULL DEFAULT 25,
    halted_reason         TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_upgrade_rollouts_in_progress
    ON upgrade_rollouts (status) WHERE status = 'in_progress';

ALTER TABLE fleet_tasks
    ADD COLUMN IF NOT EXISTS rollout_id    UUID,
    ADD COLUMN IF NOT EXISTS rollout_stage INT;
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_rollout
    ON fleet_tasks (rollout_id, rollout_stage) WHERE rollout_id IS NOT NULL;

INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('upgrade_rollout_halted',
   'A staged upgrade rollout auto-halted because a stage''s task-failure rate crossed its threshold (a bad build was caught before it reached every host)',
   'upgrade_rollout_halted', 'leader_only', '> 0',
   0, 'warning', 3600, 'telegram', true)
ON CONFLICT (name) DO NOTHING;
"#;

// ─── V135: fleet-integrity active-mode repair audit ─────────────────────────
//
// `fleet_integrity` gains an `active` mode (prod-readiness item 23 follow-up):
// after the read-only sweep, the leader may enqueue a SAFE per-gap repair via
// the existing deferred-task queue (today only `revive_member` for a node that
// fails the daemon-health/liveness check). This table is the audit log of every
// such auto-repair the leader enqueued — one row per (node, gap) the active tick
// acted on, with the resulting `deferred_tasks` id when an enqueue happened. It
// is purely observational: nothing reads it back to drive behaviour, so it can
// never widen blast radius. Other (non-liveness) gaps are recorded with
// action='alert_only' and a NULL deferred_task_id — they are detected + alerted
// but not yet auto-mutated.
//
// (V134 is reserved by a sibling branch; this feature uses V135 as assigned.)
pub const SCHEMA_V135_INTEGRITY_ACTIVE_REPAIRS: &str = r#"
CREATE TABLE IF NOT EXISTS integrity_active_repairs (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node              TEXT NOT NULL,
    gap               TEXT NOT NULL,
    action            TEXT NOT NULL,
    deferred_task_id  UUID,
    leader            TEXT NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_integrity_active_repairs_node_created
    ON integrity_active_repairs (node, created_at DESC);
"#;

// ─── V136: HA leader-handoff Phase 3 — DSN of record ────────────────────────
//
// Phase 3 (DB-primary-aware handoff) needs to solve the design's open question
// Q2: after a Postgres primary MOVE, workers hold a STATIC DSN and cannot learn
// the new primary. The "DSN of record" is the single source of truth for the
// current primary connection string. It is ALSO mirrored into the
// `db_dsn_of_record` fleet_secret (the primary mechanism workers read on
// connect-failure); this tiny singleton table is the durable, auditable home of
// record (who repointed it, when, and the prior value for rollback).
//
// Singleton-enforced like fleet_leader_state: one row, `singleton_key='current'`.
// INERT by default — no row exists until an operator runs a Phase-3 handoff with
// `--execute`, so deploying this migration changes nothing on a running fleet.
pub const SCHEMA_V136_DSN_OF_RECORD: &str = r#"
CREATE TABLE IF NOT EXISTS dsn_of_record (
    singleton_key   TEXT PRIMARY KEY DEFAULT 'current'
                        CHECK (singleton_key = 'current'),
    dsn             TEXT NOT NULL,
    primary_member  TEXT,
    previous_dsn    TEXT,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by      TEXT
);
"#;

/// V137: gate-TTL restore. `previous_value` snapshots the value a gate held
/// before a temporary `ff secrets disable-gate`, so TTL-expiry can restore the
/// EXACT prior value — a boolean `on` OR a 3-state mode `active`/`dry-run` —
/// instead of the old boolean-only restore that clobbered mode gates
/// (3-way consensus 2026-06-18; deep review conflict #9). Cleared on a normal
/// non-TTL `ff secrets set` so a stale snapshot is never restored.
pub const SCHEMA_V137_GATE_PREVIOUS_VALUE: &str = r#"
ALTER TABLE fleet_secrets ADD COLUMN IF NOT EXISTS previous_value TEXT;
"#;

/// V138: hybrid-LLM attribution on the interaction log. `worker_name` + `endpoint`
/// record WHICH fleet computer / LLM endpoint actually served a given ff turn, so
/// "how much work did each computer/model do" is answerable from `ff_interactions`
/// (deep review Q1 telemetry gap). Both nullable — channels populate them when the
/// route decision is known.
pub const SCHEMA_V138_INTERACTION_WORKER_ATTRIBUTION: &str = r#"
ALTER TABLE ff_interactions ADD COLUMN IF NOT EXISTS worker_name TEXT;
ALTER TABLE ff_interactions ADD COLUMN IF NOT EXISTS endpoint    TEXT;
CREATE INDEX IF NOT EXISTS idx_ff_interactions_worker ON ff_interactions (worker_name, ts DESC);
"#;

// V139 — Agent working memory ("Scratchpad"): a small, byte-capped, agent-
// self-editable text surface with fixed blocks, layered scope, and
// consolidate-and-forget on overflow. Sits beside session_brain; evicted
// content flows down into Brain candidates. Design: plans/agent-working-memory.md
// (frozen by LLM council 2026-06-19 — codex + kimi + Claude).
pub const SCHEMA_V139_AGENT_SCRATCHPAD: &str = r#"
-- the working set: one row per (scope, block)
CREATE TABLE IF NOT EXISTS agent_memory (
    scope_type   TEXT NOT NULL CHECK (scope_type IN ('session','agent','project')),
    scope_key    TEXT NOT NULL,
    block        TEXT NOT NULL,
    content      TEXT NOT NULL DEFAULT '',
    bytes        INT  NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (scope_type, scope_key, block)
);
CREATE INDEX IF NOT EXISTS idx_agent_memory_scope
    ON agent_memory (scope_type, scope_key);

-- per-scope cap overrides ('' scope_key = default for the scope_type)
CREATE TABLE IF NOT EXISTS agent_memory_caps (
    scope_type TEXT NOT NULL,
    scope_key  TEXT NOT NULL DEFAULT '',
    cap_bytes  INT  NOT NULL DEFAULT 6144,
    PRIMARY KEY (scope_type, scope_key)
);

-- audit trail of every consolidate-and-forget eviction
CREATE TABLE IF NOT EXISTS agent_memory_evictions (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope_type   TEXT NOT NULL,
    scope_key    TEXT NOT NULL,
    block        TEXT NOT NULL,
    prev_hash    TEXT NOT NULL,
    prev_bytes   INT  NOT NULL,
    summary      TEXT NOT NULL,
    summarizer   TEXT NOT NULL,
    brain_ref    TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_agent_memory_evictions_scope
    ON agent_memory_evictions (scope_type, scope_key, created_at DESC);
"#;

// V140 — Pillar 4: distributed concurrent development, on the CANONICAL Postgres
// `work_items` table (LLM council verdict 2026-06-19, codex+kimi+Claude — see
// .forgefleet/plans/DECISION-pillar4-canonical-home.md). Two parts:
//  (a) MATERIALIZE work_items in the migration chain. It exists live (V15 columns)
//      but has no CREATE here (source/live name drift: V15 const says
//      `fleet_work_items`, live name is `work_items`; live `fleet_work_items` is the
//      V75 work-stealing table). `CREATE TABLE IF NOT EXISTS` is a no-op on the live
//      DB and materializes it on a fresh rebuild — fixing the FK/DR gap both council
//      members flagged. Column set verified against live via `ff db query`.
//  (b) Extend work_items with orchestration columns + add leases / worktrees /
//      merge-queue + a work_items→fleet_tasks bridge, all keyed on work_items.id as
//      the single canonical task identity. The DAG uses the existing work_item_relations.
pub const SCHEMA_V140_DISTRIBUTED_DEV: &str = r#"
-- (a) Canonical work_items (no-op on live; materializes on fresh rebuilds).
CREATE TABLE IF NOT EXISTS work_items (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id        TEXT NOT NULL REFERENCES projects(id),
    milestone_id      UUID REFERENCES milestones(id),
    parent_id         UUID REFERENCES work_items(id),
    kind              TEXT NOT NULL,
    title             TEXT NOT NULL,
    description       TEXT,
    labels            JSONB NOT NULL DEFAULT '[]',
    status            TEXT NOT NULL DEFAULT 'idea',
    priority          TEXT NOT NULL DEFAULT 'normal',
    assigned_to       TEXT,
    assigned_computer TEXT,
    branch_name       TEXT,
    pr_url            TEXT,
    brain_node_ids    JSONB NOT NULL DEFAULT '[]',
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by        TEXT NOT NULL DEFAULT 'system',
    started_at        TIMESTAMPTZ,
    completed_at      TIMESTAMPTZ,
    due_date          DATE,
    estimated_hours   DOUBLE PRECISION,
    metadata          JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_work_items_status ON work_items (status);
CREATE INDEX IF NOT EXISTS idx_work_items_parent ON work_items (parent_id);

-- (b) Orchestration columns for fleet-wide concurrent execution.
ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS required_capabilities JSONB   NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS complexity            TEXT    NOT NULL DEFAULT 'mechanical',
    ADD COLUMN IF NOT EXISTS predicted_paths       JSONB   NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS touched_paths         JSONB   NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS base_branch           TEXT,
    ADD COLUMN IF NOT EXISTS base_sha              TEXT,
    ADD COLUMN IF NOT EXISTS integration_branch    TEXT,
    ADD COLUMN IF NOT EXISTS merge_rank            INT,
    ADD COLUMN IF NOT EXISTS risk_score            REAL    NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS reviewer_required     BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN IF NOT EXISTS attempts              INT     NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS last_error            TEXT;

-- one fleet slot leased to a work_item; partial-unique = at most one ACTIVE lease.
CREATE TABLE IF NOT EXISTS work_item_leases (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    work_item_id     UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    sub_agent_id     UUID REFERENCES sub_agents(id),
    computer_id      UUID NOT NULL REFERENCES computers(id),
    session_id       UUID REFERENCES agent_sessions(id),
    endpoint         TEXT,
    lease_state      TEXT NOT NULL DEFAULT 'claimed'
        CHECK (lease_state IN ('claimed','building','reviewing','stale','released','failed')),
    lease_expires_at TIMESTAMPTZ NOT NULL,
    heartbeat_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempt          INT NOT NULL DEFAULT 1,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    released_at      TIMESTAMPTZ,
    release_reason   TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_work_item_leases_active
    ON work_item_leases (work_item_id) WHERE released_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_work_item_leases_heartbeat
    ON work_item_leases (lease_state, heartbeat_at) WHERE released_at IS NULL;

-- a git worktree on a host where a slot does isolated work for a work_item.
CREATE TABLE IF NOT EXISTS work_item_worktrees (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    work_item_id  UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    computer_id   UUID NOT NULL REFERENCES computers(id),
    sub_agent_id  UUID REFERENCES sub_agents(id),
    repo_path     TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    base_branch   TEXT NOT NULL,
    task_branch   TEXT NOT NULL,
    head_sha      TEXT,
    status        TEXT NOT NULL DEFAULT 'creating'
        CHECK (status IN ('creating','active','ready_for_review','merged','failed','cleaned')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    cleaned_at    TIMESTAMPTZ,
    -- NOTE: no UNIQUE (computer_id, worktree_path) — under clone-per-slot many
    -- work_items share one slot clone path; task_branch is the real per-item key.
    UNIQUE (task_branch)
);
CREATE INDEX IF NOT EXISTS idx_work_item_worktrees_item ON work_item_worktrees (work_item_id);

-- serialized, CI-gated merge queue (builds run in parallel; merges land one-by-one).
CREATE TABLE IF NOT EXISTS work_item_merge_queue (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    work_item_id   UUID NOT NULL UNIQUE REFERENCES work_items(id) ON DELETE CASCADE,
    project_id     TEXT NOT NULL REFERENCES projects(id),
    position       BIGSERIAL,
    status         TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued','rebasing','ci_running','mergeable','merged','conflict','failed')),
    ci_run_id      UUID REFERENCES project_ci_runs(id),
    branch_name    TEXT NOT NULL,
    pr_url         TEXT,
    head_sha       TEXT,
    merge_attempts INT NOT NULL DEFAULT 0,
    enqueued_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at     TIMESTAMPTZ,
    merged_at      TIMESTAMPTZ,
    failed_at      TIMESTAMPTZ,
    failure_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_work_item_merge_queue_ready
    ON work_item_merge_queue (project_id, position)
    WHERE status IN ('queued','rebasing','ci_running');

-- bridge: a work_item (PM decomposition) → the fleet_tasks (dispatch queue) it spawned.
CREATE TABLE IF NOT EXISTS work_item_fleet_tasks (
    work_item_id  UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    fleet_task_id UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (work_item_id, fleet_task_id)
);
"#;

/// V141 — Projects-first PM: a project can attach MANY GitHub locations and MANY
/// local folders. The legacy `projects.repo_url` is a single repo and there was
/// no local-folder concept; this is the first stage of the PM consolidation
/// (one Postgres PM model, SQLite removed — see .forgefleet/plans/pm-consolidation.md).
/// Additive only: no existing table/column is dropped, so a rollback is just
/// "ignore the new tables".
pub const SCHEMA_V141_PROJECT_REPOS_FOLDERS: &str = r#"
-- One GitHub location attached to a project. A project may have several
-- (e.g. app repo + infra repo + docs repo). `is_primary` marks the main one.
CREATE TABLE IF NOT EXISTS project_repos (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id     TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    github_url     TEXT NOT NULL,
    name           TEXT,                                   -- short label (e.g. "forge-fleet")
    default_branch TEXT NOT NULL DEFAULT 'main',
    role           TEXT,                                   -- code | infra | docs | ...
    is_primary     BOOLEAN NOT NULL DEFAULT FALSE,
    metadata       JSONB NOT NULL DEFAULT '{}',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (project_id, github_url)
);
CREATE INDEX IF NOT EXISTS idx_project_repos_project ON project_repos (project_id);
-- at most one primary repo per project
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_repos_primary
    ON project_repos (project_id) WHERE is_primary;

-- One local folder attached to a project. `computer_id` NULL = a canonical path
-- that applies to every host; non-NULL = that specific host's checkout (mirrors
-- per-host source_tree_path, so the same project can live at different paths on
-- Taylor vs a Linux node).
CREATE TABLE IF NOT EXISTS project_folders (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    computer_id UUID REFERENCES computers(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    role        TEXT,                                      -- source | data | scratch | ...
    is_primary  BOOLEAN NOT NULL DEFAULT FALSE,
    metadata    JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_project_folders_project ON project_folders (project_id);
-- idempotent sync: a (project, host, path) triple is unique. NULL computer_id
-- rows dedupe on (project, path) via the partial index below.
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_folders_host_path
    ON project_folders (project_id, computer_id, path) WHERE computer_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_folders_canonical_path
    ON project_folders (project_id, path) WHERE computer_id IS NULL;

-- Backfill: promote the legacy single projects.repo_url into project_repos as
-- the primary repo, so existing projects show their repo in the new model.
INSERT INTO project_repos (project_id, github_url, default_branch, is_primary, role)
SELECT p.id, p.repo_url, COALESCE(p.default_branch, 'main'), TRUE, 'code'
  FROM projects p
 WHERE p.repo_url IS NOT NULL AND p.repo_url <> ''
ON CONFLICT (project_id, github_url) DO NOTHING;
"#;

/// V142 — Cortex universal-graph FOUNDATION (P0). The graph (`brain_vault_nodes`
/// and `brain_vault_edges`) is one domain-agnostic knowledge graph: code is one
/// domain (`code:*`, `db:*`, `http:*`, …) alongside non-code (`doc:*`,
/// `project:*`, `person:*`, `decision:*`, …). Two councils converged on this
/// being the ONE thing to land + deploy fleet-wide BEFORE fanning out the
/// per-dimension extractors, so the shared contract never churns under a
/// parallel build. See plans/cortex-feature-knowledge-graph.md.
///
/// Additive only — every column/table is `IF NOT EXISTS`; nothing is dropped, so
/// an older binary keeps working (forward-compatible reads). `brain_vault_edges`
/// already had `confidence`/`provenance`; this adds the rest of the
/// confidence-carrying schema plus the generation/atomic-swap reindex contract:
///   - `method`   — EXTRACTED | INFERRED | HEURISTIC | DYNAMIC | MANUAL
///   - `evidence` — {file, span, snippet_hash, model, prompt_hash} for audit
///   - `generation` — the reindex pass that wrote the row (orphan-sweep + the
///     "in-progress pass is invisible to readers" filter — MVCC alone can't do
///     this across a multi-statement pass)
///   - `cortex_generations` — per-corpus current_generation pointer + the
///     single-writer advisory-lock bookkeeping (one indexer per corpus
///     fleet-wide; commit = one tiny UPDATE that flips the pointer atomically).
pub const SCHEMA_V142_CORTEX_FOUNDATION: &str = r#"
-- Confidence-carrying edges (confidence + provenance already exist from earlier).
ALTER TABLE brain_vault_edges ADD COLUMN IF NOT EXISTS method     TEXT;
ALTER TABLE brain_vault_edges ADD COLUMN IF NOT EXISTS evidence   JSONB;
ALTER TABLE brain_vault_edges ADD COLUMN IF NOT EXISTS generation BIGINT;

-- Nodes get the same generation stamp + provenance (confidence already exists).
ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS generation BIGINT;
ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS provenance TEXT;

-- Per-corpus reindex bookkeeping: the atomic-swap pointer + single-writer lock.
-- A writer takes pg_try_advisory_lock(hashtext('cortex:'||project)), writes its
-- rows stamped generation = current_generation+1, then flips current_generation
-- in ONE update (the atomic publish). Readers always filter to current_generation
-- so a half-written pass is never visible. A crashed writer never flips the
-- pointer, so its rows are simply swept later — no torn state.
CREATE TABLE IF NOT EXISTS cortex_generations (
    project            TEXT PRIMARY KEY,
    current_generation BIGINT      NOT NULL DEFAULT 0,
    indexing_node      TEXT,                              -- who currently holds the writer lock
    indexing_started   TIMESTAMPTZ,                       -- when (stale-lock detection)
    last_swapped        TIMESTAMPTZ,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Read-path indexes (council perf rec): impact/lineage queries filter by
-- (project, node_type) and walk edges by (edge_type, src); generation gates reads.
CREATE INDEX IF NOT EXISTS idx_bvn_project_type   ON brain_vault_nodes (project, node_type);
CREATE INDEX IF NOT EXISTS idx_bve_type_src       ON brain_vault_edges (edge_type, src_id);
CREATE INDEX IF NOT EXISTS idx_bvn_generation     ON brain_vault_nodes (project, generation);
CREATE INDEX IF NOT EXISTS idx_bve_generation     ON brain_vault_edges (generation);
"#;

pub const SCHEMA_V143_PROJECT_GIT_POLICY: &str = r#"
-- Per-project git policy for multi-project build orchestration.
ALTER TABLE projects ADD COLUMN IF NOT EXISTS integration_strategy TEXT NOT NULL DEFAULT 'feature_pr';
ALTER TABLE projects ADD COLUMN IF NOT EXISTS branch_prefix        TEXT NOT NULL DEFAULT 'feat';
ALTER TABLE projects ADD COLUMN IF NOT EXISTS git_remote           TEXT NOT NULL DEFAULT 'origin';

-- HireFlow integrates onto dev, not main.
UPDATE projects SET default_branch = 'dev' WHERE id = 'hireflow360' AND default_branch = 'main';
"#;

pub const SCHEMA_V144_CODE_COMMUNITY_LEVELS: &str = r#"
-- Hierarchical GraphRAG: brain_code_communities gains a community LEVEL.
-- level 0 = finest call clusters (single-level Louvain, the prior behaviour);
-- higher levels = progressively coarser subsystems from multi-level Louvain
-- aggregation. The member_hash uniqueness becomes per-level so the same grouping
-- can be recorded at more than one granularity.
ALTER TABLE brain_code_communities ADD COLUMN IF NOT EXISTS level INT NOT NULL DEFAULT 0;
DROP INDEX IF EXISTS idx_brain_code_communities_member_hash;
CREATE UNIQUE INDEX IF NOT EXISTS idx_brain_code_communities_member_hash_level
    ON brain_code_communities (member_hash, level);
"#;

pub const SCHEMA_V145_CODE_COMMUNITY_PARENT: &str = r#"
-- Hierarchical GraphRAG: each community records its PARENT (the immediate
-- strictly-larger enclosing community up the level hierarchy) by member_hash,
-- making brain_code_communities a navigable tree. NULL = top-level community.
-- Indexed for child lookups (a parent's children = rows WHERE parent_member_hash
-- = the parent's member_hash), which the level>0 map-reduce summary pass uses.
ALTER TABLE brain_code_communities ADD COLUMN IF NOT EXISTS parent_member_hash TEXT;
CREATE INDEX IF NOT EXISTS idx_brain_code_communities_parent
    ON brain_code_communities (parent_member_hash);
"#;

// V146: disable the structurally-dead `computer_offline` alert policy.
//
// The V34 seed fires on `computer_status == 'odown'`, but NOTHING in the live
// system ever produces the status `odown`: the pulse materializer writes only
// `online`/`offline`, and the alert evaluator's `computer_status` metric
// derives `online`/`offline`/`sdown` from beat presence — never `odown`. So the
// policy has fired 0 times since V34 and is structurally INCAPABLE of firing,
// presenting a FALSE sense of coverage (an enabled `critical` "computer
// offline" alert that watches nothing).
//
// Real "computer is down" coverage already exists and works via the numeric,
// duration-gated `beat_age_secs` policies — `member_stale_beat` (>300s warning)
// and `member_beat_dead` (>1800s critical) — which read `computers.last_seen_at`
// and survive Redis TTL expiry. Disabling the dead duplicate loses zero real
// coverage and stops the misleading "enabled" listing.
//
// GUARDED: only the UNMODIFIED V34 default is disabled (condition still
// `== 'odown'` AND currently enabled), so an operator who has since rewired or
// re-enabled the policy is left untouched. To restore a faster critical, rewire
// onto `beat_age_secs` (e.g. `> 600` critical, duration 120) rather than the
// quorum-only `odown` status.
pub const SCHEMA_V146_DISABLE_DEAD_COMPUTER_OFFLINE_ALERT: &str = r#"
UPDATE alert_policies
   SET enabled = false
 WHERE name = 'computer_offline'
   AND metric = 'computer_status'
   AND condition = '== ''odown'''
   AND enabled = true;
"#;

/// V147 — multi-session Telegram router (roadmap E6). One ForgeFleet bot fans
/// out to many coding sessions (this Claude Code session, another, codex, kimi).
/// Each session registers with an INFORMATIVE name so the operator can tell them
/// apart; the operator focuses one and replies route to it; each session polls
/// `telegram_session_inbox` for messages addressed to it.
pub const SCHEMA_V147_TELEGRAM_SESSIONS: &str = r#"
CREATE TABLE IF NOT EXISTS telegram_sessions (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_name     TEXT NOT NULL,
    kind             TEXT NOT NULL DEFAULT 'claude_code',
    project          TEXT,
    chat_id          TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'active',
    focused          BOOLEAN NOT NULL DEFAULT FALSE,
    update_freq_secs INTEGER NOT NULL DEFAULT 1800,
    external_key     TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_active_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_telegram_sessions_status ON telegram_sessions(status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_telegram_sessions_extkey
    ON telegram_sessions(external_key) WHERE external_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS telegram_session_inbox (
    id          BIGSERIAL PRIMARY KEY,
    session_id  UUID NOT NULL REFERENCES telegram_sessions(id) ON DELETE CASCADE,
    text        TEXT NOT NULL,
    from_chat   TEXT,
    delivered   BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_tg_inbox_undelivered
    ON telegram_session_inbox(session_id, delivered, created_at);
"#;

// ─── V148: per-node LLM-CLI backend availability (capability roadmap A2) ──
//
// Records which CLI backends (claude/codex/gemini/kimi/grok) are installed AND
// authenticated on each computer, refreshed by a per-node forgefleetd detector
// tick (`ff_agent::backend_detect::detect_backends`). The dispatch picker reads
// this to route a build to a node+backend that is actually usable — the
// "sub-agents call any available LLM" capability. Purpose-built (not folded into
// `computer_external_tools`) because backend *auth-freshness* has a different
// lifecycle than tool *version-tracking*, and to avoid the FK-to-external_tools
// seeding tangle.
pub const SCHEMA_V148_COMPUTER_BACKENDS: &str = r#"
CREATE TABLE IF NOT EXISTS computer_backends (
    computer_id      UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    backend          TEXT NOT NULL,                       -- claude | codex | gemini | kimi | grok
    installed        BOOLEAN NOT NULL DEFAULT false,
    authenticated    BOOLEAN NOT NULL DEFAULT false,
    version          TEXT,
    last_auth_ok_at  TIMESTAMPTZ,                          -- last time an auth probe passed
    last_checked_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    detail           TEXT,
    PRIMARY KEY (computer_id, backend)
);
-- The picker's hot path: "which backends are dispatchable on this node?"
CREATE INDEX IF NOT EXISTS computer_backends_dispatchable_idx
    ON computer_backends(computer_id)
    WHERE installed AND authenticated;
"#;

// ─── V149: multi-provider routing — cloud error taxonomy + usage + breaker ───
//
// Layers 2/4/5 of the multi-provider routing epic (plans/cloud-error-handling.md).
// EXTENDS the V107 failure-taxonomy/retry/breaker spine rather than forking it:
//   * adds cloud-provider failure categories (= CloudErrorClass::as_str()) so
//     ff_agent::retry_policy::should_retry reuses the existing backoff;
//   * fleet_provider_usage — latest usage headroom per (computer, provider);
//   * fleet_backend_health — durable PROVIDER-level circuit breaker (the V107
//     host breaker is keyed on worker_name; a claude 529 is not a host fault);
//   * fleet_session_dispatch — durable per-headless-session retry/continue
//     state so sessions survive crashes/deploys (ff council item 5).
pub const SCHEMA_V149_PROVIDER_ROUTING: &str = r#"
-- Cloud-provider failure categories (names match CloudErrorClass::as_str()).
INSERT INTO failure_taxonomy (category, description, transient, retryable, notify_threshold)
VALUES
    ('overloaded',       'Cloud provider temporarily overloaded (claude 529 / openai-gemini 503)', true,  true,  5),
    ('rate_limited',     'Provider rate limit hit (RPM/TPM) — honor Retry-After',                  true,  true,  5),
    ('quota_exhausted',  'Subscription quota / credits exhausted — switch provider + alert',       false, false, 1),
    ('unauthenticated',  'Provider auth/token expired (401) — re-auth + switch',                   false, false, 1),
    ('forbidden',        'Provider permission / geo / precondition denied',                        false, false, 1),
    ('context_too_long', 'Prompt exceeds the model context window — compact then continue',        false, true,  0),
    ('transient_5xx',    'Provider generic upstream 5xx (not overload)',                           true,  true,  3),
    ('model_not_found',  'Model id unknown / deprecated / no access (404)',                        false, false, 1),
    ('bad_request',      'Malformed request (400/422) — our bug, do not blind-retry',              false, false, 0),
    ('content_filtered', 'Output blocked by a content/safety filter',                              false, false, 0)
ON CONFLICT (category) DO NOTHING;

-- Latest usage-headroom snapshot per (computer, provider, window).
CREATE TABLE IF NOT EXISTS fleet_provider_usage (
    computer_id     UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    provider        TEXT NOT NULL,                 -- claude|codex|kimi|gemini|grok
    used_pct        DOUBLE PRECISION,              -- 0..100 (NULL if unknown)
    remaining_pct   DOUBLE PRECISION,              -- 100-used, or header-derived
    window_kind     TEXT NOT NULL DEFAULT 'unknown', -- session|5h|weekly|monthly
    resets_at       TIMESTAMPTZ,
    source          TEXT,                          -- ratelimit_header|usage_api|portal
    raw             JSONB,
    sampled_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (computer_id, provider, window_kind)
);
CREATE INDEX IF NOT EXISTS idx_fleet_provider_usage_provider
    ON fleet_provider_usage (provider, sampled_at DESC);

-- Durable PROVIDER-level circuit-breaker state per (computer, provider).
CREATE TABLE IF NOT EXISTS fleet_backend_health (
    computer_id          UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    provider             TEXT NOT NULL,
    breaker_state        TEXT NOT NULL DEFAULT 'closed',   -- closed|open|half_open
    breaker_open_until   TIMESTAMPTZ,
    recent_error_count   INTEGER NOT NULL DEFAULT 0,
    recent_req_count     INTEGER NOT NULL DEFAULT 0,
    window_start         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    half_open_successes  INTEGER NOT NULL DEFAULT 0,
    last_error_class     TEXT,
    last_error_at        TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (computer_id, provider)
);
CREATE INDEX IF NOT EXISTS idx_fleet_backend_health_open
    ON fleet_backend_health (breaker_open_until)
    WHERE breaker_state <> 'closed';

-- Durable per-headless-session retry/continue state (council item 5).
CREATE TABLE IF NOT EXISTS fleet_session_dispatch (
    session_id          TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    attempt_count       INTEGER NOT NULL DEFAULT 0,
    auto_continue_count INTEGER NOT NULL DEFAULT 0,
    last_error_class    TEXT,
    last_error_at       TIMESTAMPTZ,
    last_retry_at       TIMESTAMPTZ,
    resume_token        TEXT,
    context_digest      TEXT,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#;

// ─── V150: register kimi-cli as a tracked external tool ────────────────────
//
// The Kimi CLI (Moonshot's coding agent, PyPI package `kimi-cli`, installed
// fleet-side via `uv tool`) was NEVER in the external_tools / software_registry
// catalog. Every OTHER coding CLI (codex, claude-code, openclaw) is registered
// and auto-version-checked, but kimi had no row — so the auto-upgrade tick
// never queried its upstream, never saw drift, and never upgraded it. The CLI
// nagged "New version available: 1.48.0 — run `uv tool upgrade kimi-cli`" while
// ff sat blind. This closes that blind spot (fleet-fixes-fleet: the bug was
// ff's catalog gap, not kimi being stale).
//
// version_source.method=pip → the hourly `refresh_pypi_latest_versions` tick
// (auto_upgrade.rs) + the 6h ExternalToolsUpstreamChecker query
// https://pypi.org/pypi/kimi-cli/json. Both resolvers already support "pip".
//
// install_method=pip (derive_install_source maps it to install_source "pip",
// so resolve_install_plans picks the `macos`/`linux` playbook keys). The
// playbook uses `uv tool install --force` (install-or-upgrade in one shot;
// `uv tool upgrade` errors when the tool isn't installed yet). macOS prepends
// the homebrew + local bin paths because non-interactive SSH doesn't source
// the shell profile where `uv` lives (same class of fix as the V46 ace bug).
//
// Registering in the catalog does NOT install kimi anywhere — computer_external_tools
// rows only appear on explicit `ff ext install kimi`. This just makes it
// trackable + upgradable like its siblings.
pub const SCHEMA_V150_KIMI_CLI_EXTERNAL_TOOL: &str = r#"
INSERT INTO software_registry
    (id, display_name, kind, version_source, upgrade_playbook)
VALUES
  ('kimi-cli', 'Kimi CLI (Moonshot)', 'binary',
   '{"method":"pip","package":"kimi-cli"}'::jsonb,
   '{"macos":"export PATH=$HOME/.local/bin:/opt/homebrew/bin:$PATH && uv tool install kimi-cli --force","linux":"export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH && uv tool install kimi-cli --force"}'::jsonb)
ON CONFLICT (id) DO UPDATE SET
  version_source   = EXCLUDED.version_source,
  upgrade_playbook = EXCLUDED.upgrade_playbook;

INSERT INTO external_tools
    (id, display_name, github_url, kind, install_method, install_spec,
     cli_entrypoint, register_as_mcp, version_source, upgrade_playbook,
     intake_source, added_by)
VALUES
  ('kimi-cli', 'Kimi CLI (Moonshot)',
   'https://github.com/MoonshotAI/kimi-cli', 'cli', 'pip',
   '{"package":"kimi-cli"}'::jsonb,
   'kimi', false,
   '{"method":"pip","package":"kimi-cli"}'::jsonb,
   '{"macos":"export PATH=$HOME/.local/bin:/opt/homebrew/bin:$PATH && uv tool install kimi-cli --force","linux":"export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH && uv tool install kimi-cli --force"}'::jsonb,
   'migration', 'V150')
ON CONFLICT (id) DO UPDATE SET
  install_method   = EXCLUDED.install_method,
  install_spec     = EXCLUDED.install_spec,
  cli_entrypoint   = EXCLUDED.cli_entrypoint,
  version_source   = EXCLUDED.version_source,
  upgrade_playbook = EXCLUDED.upgrade_playbook;
"#;

// ─── V151: record WHERE each vendor CLI lives per computer ──────────────────
//
// `computer_backends` tracked installed/authenticated/version but NOT the
// resolved absolute path. Combined with `which_on_path` only searching `$PATH`
// (a non-interactive daemon shell drops /opt/homebrew/bin + ~/.local/bin), ff
// re-guessed each CLI by bare name every time and reported "not on PATH" false
// negatives for installed CLIs. The detector now resolves via known install
// dirs and persists the absolute path here so the executor + operator can see
// exactly where codex/claude/kimi live on each node (e.g. macOS
// /opt/homebrew/bin, linux ~/.local/bin).
pub const SCHEMA_V151_COMPUTER_BACKENDS_PATH: &str = r#"
ALTER TABLE computer_backends ADD COLUMN IF NOT EXISTS path TEXT;
"#;

// ─── V152: bind Pillar-4 work_items to the repository they target ──────────
//
// `ff build --cwd <polyrepo-child>` and `ff pm decompose` used to plan against
// the project's primary repo and emitted work_items with no repo binding. In a
// polyrepo project that let the scheduler/sub-agent fall back to the wrong
// checkout. These columns make the target explicit on every generated task:
// a project_repos FK when known, the origin URL for clone/identity, and the
// operator's local repo path when available.
pub const SCHEMA_V152_WORK_ITEM_REPO_BINDING: &str = r#"
ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS repo_id   UUID REFERENCES project_repos(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS repo_url  TEXT,
    ADD COLUMN IF NOT EXISTS repo_path TEXT;

CREATE INDEX IF NOT EXISTS idx_work_items_repo_id ON work_items (repo_id);
CREATE INDEX IF NOT EXISTS idx_work_items_repo_url ON work_items (repo_url);
"#;

// V153 — retire the V75 work-stealing engine. `fleet_work_items` /
// `fleet_work_batches` were the batch/work-stealing tables driven by
// `batch_manager` + `work_stealer` (fed only by a `fleet_tasks` row with
// task_type='decomposed'). That decompose-enqueue path was never wired
// (`pg_enqueue_decomposed_task` had zero callers; zero 'decomposed' rows ever
// existed), so both tables sat permanently at 0 rows while three daemon
// subsystems spun against them every 5s. Superseded by Pillar-4
// (`work_items` + `work_item_leases` + the lease scheduler). Modules + daemon
// wiring removed in the same change; drop the dead tables here. CASCADE clears
// their indexes/constraints. Forward-only; a fresh rebuild creates then drops.
pub const SCHEMA_V153_RETIRE_V75_WORK_STEALING: &str = r#"
DROP TABLE IF EXISTS fleet_work_items  CASCADE;
DROP TABLE IF EXISTS fleet_work_batches CASCADE;
"#;

// V154 — canonicalize the sub-agent slot workspace to the NESTED layout
// `~/.forgefleet/sub-agents/sub-agent-N/`. The provisioner (sub_agents.rs) and
// all docs already use nested, but the scheduler (agent_coordinator) recorded
// FLAT `~/.forgefleet/sub-agent-N/` on existing rows via ON CONFLICT DO NOTHING.
// Rewrite those rows to nested so the scheduler points slots at the same dirs
// the provisioner actually creates. Idempotent: the nested form contains
// `sub-agents/` so the guard skips already-migrated rows.
pub const SCHEMA_V154_NESTED_SUBAGENT_WORKSPACE: &str = r#"
UPDATE sub_agents
   SET workspace_dir = replace(workspace_dir,
                               '/.forgefleet/sub-agent-',
                               '/.forgefleet/sub-agents/sub-agent-')
 WHERE workspace_dir LIKE '%/.forgefleet/sub-agent-%'
   AND workspace_dir NOT LIKE '%/.forgefleet/sub-agents/%';
"#;

pub const SCHEMA_V155_DROP_DEAD_BRIDGE: &str = r#"
DROP TABLE IF EXISTS work_item_fleet_tasks CASCADE;
"#;

pub const SCHEMA_V156_FLEET_TASKS_FOLD_COLUMNS: &str = r#"
ALTER TABLE fleet_tasks
    ADD COLUMN IF NOT EXISTS task_class          TEXT,
    ADD COLUMN IF NOT EXISTS not_before          TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS dedup_signature     TEXT,
    ADD COLUMN IF NOT EXISTS parent_work_item_id UUID;
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_task_class ON fleet_tasks (task_class);
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_not_before ON fleet_tasks (not_before) WHERE not_before IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_fleet_tasks_dedup_signature ON fleet_tasks (dedup_signature) WHERE dedup_signature IS NOT NULL;
"#;

pub const SCHEMA_V157_FOLD_RESEARCH_SUBTASKS: &str = r#"
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '60s';

INSERT INTO fleet_tasks (
    id,
    task_type,
    summary,
    payload,
    priority,
    preferred_computer_id,
    status,
    claimed_by_computer_id,
    started_at,
    completed_at,
    result,
    error,
    created_at,
    created_by_computer_id,
    original_computer_id,
    task_class
)
SELECT
    st.id,
    'research_subtask',
    st.sub_question,
    jsonb_build_object(
        'research_session_id', st.session_id,
        'ordinal', st.ordinal,
        'sub_question', st.sub_question,
        'assigned_computer', st.assigned_computer,
        'assigned_endpoint', st.assigned_endpoint,
        'assigned_model', st.assigned_model,
        'agent_session_id', st.agent_session_id,
        'legacy_status', st.status,
        'metadata', COALESCE(st.metadata, '{}'::jsonb)
    ),
    50,
    c.id,
    CASE st.status
        WHEN 'pending' THEN 'pending'
        WHEN 'running' THEN 'running'
        WHEN 'done' THEN 'completed'
        WHEN 'max_turns' THEN 'completed'
        WHEN 'failed' THEN 'failed'
        WHEN 'cancelled' THEN 'cancelled'
        ELSE 'failed'
    END,
    CASE
        WHEN st.status IN ('running', 'done', 'max_turns', 'failed', 'cancelled') THEN c.id
        ELSE NULL
    END,
    st.started_at,
    st.completed_at,
    jsonb_build_object(
        'output_markdown', st.output_markdown,
        'turn_count', st.turn_count,
        'duration_ms', st.duration_ms,
        'tokens_in', st.tokens_in,
        'tokens_out', st.tokens_out
    ),
    st.error,
    COALESCE(rs.created_at, st.started_at, st.completed_at, NOW()),
    c.id,
    CASE
        WHEN st.status IN ('running', 'done', 'max_turns', 'failed', 'cancelled') THEN c.id
        ELSE NULL
    END,
    'research'
FROM research_subtasks st
LEFT JOIN research_sessions rs ON rs.id = st.session_id
LEFT JOIN computers c ON c.name = st.assigned_computer
ON CONFLICT (id) DO NOTHING;

ALTER TABLE research_subtasks RENAME TO research_subtasks_legacy;

CREATE OR REPLACE VIEW research_subtasks AS
SELECT
    t.id,
    (t.payload->>'research_session_id')::uuid AS session_id,
    COALESCE((t.payload->>'ordinal')::int, 0) AS ordinal,
    COALESCE(t.payload->>'sub_question', t.summary) AS sub_question,
    t.payload->>'assigned_computer' AS assigned_computer,
    t.payload->>'assigned_endpoint' AS assigned_endpoint,
    t.payload->>'assigned_model' AS assigned_model,
    t.payload->>'agent_session_id' AS agent_session_id,
    CASE
        WHEN t.status = 'completed' THEN
            CASE
                WHEN COALESCE(t.payload->>'legacy_status', '') IN ('done', 'max_turns')
                    THEN t.payload->>'legacy_status'
                ELSE 'done'
            END
        WHEN t.status IN ('pending', 'running', 'failed', 'cancelled') THEN t.status
        ELSE COALESCE(NULLIF(t.payload->>'legacy_status', ''), t.status)
    END AS status,
    t.result->>'output_markdown' AS output_markdown,
    (t.result->>'turn_count')::int AS turn_count,
    t.started_at,
    t.completed_at,
    (t.result->>'duration_ms')::bigint AS duration_ms,
    (t.result->>'tokens_in')::bigint AS tokens_in,
    (t.result->>'tokens_out')::bigint AS tokens_out,
    t.error,
    COALESCE(t.payload->'metadata', '{}'::jsonb) AS metadata
FROM fleet_tasks t
WHERE t.task_class = 'research';

CREATE OR REPLACE FUNCTION research_subtasks_view_write()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    effective_id uuid;
    assigned_computer_id uuid;
    existing_created_at timestamptz;
    fleet_status text;
    legacy_status text;
BEGIN
    IF TG_OP = 'DELETE' THEN
        DELETE FROM research_subtasks_legacy WHERE id = OLD.id;
        DELETE FROM fleet_tasks WHERE id = OLD.id AND task_class = 'research';
        RETURN OLD;
    END IF;

    IF TG_OP = 'INSERT' THEN
        effective_id := COALESCE(NEW.id, gen_random_uuid());
        legacy_status := COALESCE(NEW.status, 'pending');
    ELSE
        effective_id := COALESCE(NEW.id, OLD.id);
        legacy_status := COALESCE(NEW.status, OLD.status, 'pending');
    END IF;

    SELECT c.id
      INTO assigned_computer_id
      FROM computers c
     WHERE c.name = NEW.assigned_computer
     LIMIT 1;

    SELECT t.created_at
      INTO existing_created_at
      FROM fleet_tasks t
     WHERE t.id = effective_id
       AND t.task_class = 'research';

    fleet_status := CASE legacy_status
        WHEN 'pending' THEN 'pending'
        WHEN 'running' THEN 'running'
        WHEN 'done' THEN 'completed'
        WHEN 'max_turns' THEN 'completed'
        WHEN 'failed' THEN 'failed'
        WHEN 'cancelled' THEN 'cancelled'
        ELSE 'failed'
    END;

    INSERT INTO research_subtasks_legacy (
        id,
        session_id,
        ordinal,
        sub_question,
        assigned_computer,
        assigned_endpoint,
        assigned_model,
        agent_session_id,
        status,
        output_markdown,
        turn_count,
        started_at,
        completed_at,
        duration_ms,
        tokens_in,
        tokens_out,
        error,
        metadata
    )
    VALUES (
        effective_id,
        NEW.session_id,
        NEW.ordinal,
        NEW.sub_question,
        NEW.assigned_computer,
        NEW.assigned_endpoint,
        NEW.assigned_model,
        NEW.agent_session_id,
        legacy_status,
        NEW.output_markdown,
        NEW.turn_count,
        NEW.started_at,
        NEW.completed_at,
        NEW.duration_ms,
        COALESCE(NEW.tokens_in, 0),
        COALESCE(NEW.tokens_out, 0),
        NEW.error,
        COALESCE(NEW.metadata, '{}'::jsonb)
    )
    ON CONFLICT (id) DO UPDATE
    SET session_id = EXCLUDED.session_id,
        ordinal = EXCLUDED.ordinal,
        sub_question = EXCLUDED.sub_question,
        assigned_computer = EXCLUDED.assigned_computer,
        assigned_endpoint = EXCLUDED.assigned_endpoint,
        assigned_model = EXCLUDED.assigned_model,
        agent_session_id = EXCLUDED.agent_session_id,
        status = EXCLUDED.status,
        output_markdown = EXCLUDED.output_markdown,
        turn_count = EXCLUDED.turn_count,
        started_at = EXCLUDED.started_at,
        completed_at = EXCLUDED.completed_at,
        duration_ms = EXCLUDED.duration_ms,
        tokens_in = EXCLUDED.tokens_in,
        tokens_out = EXCLUDED.tokens_out,
        error = EXCLUDED.error,
        metadata = EXCLUDED.metadata;

    INSERT INTO fleet_tasks (
        id,
        task_type,
        summary,
        payload,
        priority,
        preferred_computer_id,
        status,
        claimed_by_computer_id,
        started_at,
        completed_at,
        result,
        error,
        created_at,
        created_by_computer_id,
        original_computer_id,
        task_class
    )
    VALUES (
        effective_id,
        'research_subtask',
        NEW.sub_question,
        jsonb_build_object(
            'research_session_id', NEW.session_id,
            'ordinal', NEW.ordinal,
            'sub_question', NEW.sub_question,
            'assigned_computer', NEW.assigned_computer,
            'assigned_endpoint', NEW.assigned_endpoint,
            'assigned_model', NEW.assigned_model,
            'agent_session_id', NEW.agent_session_id,
            'legacy_status', legacy_status,
            'metadata', COALESCE(NEW.metadata, '{}'::jsonb)
        ),
        50,
        assigned_computer_id,
        fleet_status,
        CASE
            WHEN legacy_status IN ('running', 'done', 'max_turns', 'failed', 'cancelled')
                THEN assigned_computer_id
            ELSE NULL
        END,
        NEW.started_at,
        NEW.completed_at,
        jsonb_build_object(
            'output_markdown', NEW.output_markdown,
            'turn_count', NEW.turn_count,
            'duration_ms', NEW.duration_ms,
            'tokens_in', COALESCE(NEW.tokens_in, 0),
            'tokens_out', COALESCE(NEW.tokens_out, 0)
        ),
        NEW.error,
        COALESCE(existing_created_at, NEW.started_at, NEW.completed_at, NOW()),
        assigned_computer_id,
        CASE
            WHEN legacy_status IN ('running', 'done', 'max_turns', 'failed', 'cancelled')
                THEN assigned_computer_id
            ELSE NULL
        END,
        'research'
    )
    ON CONFLICT (id) DO UPDATE
    SET summary = EXCLUDED.summary,
        payload = EXCLUDED.payload,
        preferred_computer_id = EXCLUDED.preferred_computer_id,
        status = EXCLUDED.status,
        claimed_by_computer_id = EXCLUDED.claimed_by_computer_id,
        started_at = EXCLUDED.started_at,
        completed_at = EXCLUDED.completed_at,
        result = EXCLUDED.result,
        error = EXCLUDED.error,
        created_by_computer_id = EXCLUDED.created_by_computer_id,
        original_computer_id = EXCLUDED.original_computer_id,
        task_class = EXCLUDED.task_class;

    NEW.id := effective_id;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS research_subtasks_view_write_trg ON research_subtasks;
CREATE TRIGGER research_subtasks_view_write_trg
INSTEAD OF INSERT OR UPDATE OR DELETE ON research_subtasks
FOR EACH ROW
EXECUTE FUNCTION research_subtasks_view_write();
"#;

pub const SCHEMA_V158_FOLD_SELF_HEAL_QUEUE: &str = r#"
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '60s';

INSERT INTO fleet_tasks (
    id,
    task_type,
    summary,
    payload,
    priority,
    status,
    created_at,
    task_class,
    dedup_signature
)
SELECT
    gen_random_uuid(),
    'self_heal_writer',
    format('self_heal_writer: %s', q.bug_signature),
    jsonb_build_object(
        'bug_signature', q.bug_signature,
        'tier', q.tier,
        'status', q.status,
        'writer_computer_id', q.writer_computer_id,
        'writer_model', q.writer_model,
        'reviewer_computer_id', q.reviewer_computer_id,
        'reviewer_model', q.reviewer_model,
        'reviewer_confidence', q.reviewer_confidence,
        'pr_number', q.pr_number,
        'branch_name', q.branch_name,
        'fix_commit_sha', q.fix_commit_sha,
        'fixed_tag', q.fixed_tag,
        'attempts', q.attempts,
        'last_attempt_at', q.last_attempt_at,
        'escalated_to_operator_at', q.escalated_to_operator_at,
        'report_count', q.report_count
    ),
    CASE q.tier
        WHEN 'T1' THEN 100
        WHEN 'T0' THEN 90
        WHEN 'T2' THEN 80
        ELSE 70
    END,
    CASE q.status
        WHEN 'detected' THEN 'pending'
        WHEN 'fixing' THEN 'running'
        WHEN 'reviewing' THEN 'running'
        WHEN 'pr_open' THEN 'running'
        WHEN 'merged' THEN 'running'
        WHEN 'rolled_out' THEN 'running'
        WHEN 'verified' THEN 'completed'
        WHEN 'paused' THEN 'paused'
        WHEN 'reverted' THEN 'cancelled'
        ELSE 'failed'
    END,
    q.created_at,
    'self_heal',
    q.bug_signature
FROM fleet_self_heal_queue q
ON CONFLICT (id) DO NOTHING;

ALTER TABLE fleet_self_heal_queue RENAME TO fleet_self_heal_queue_legacy;

CREATE OR REPLACE VIEW fleet_self_heal_queue AS
SELECT
    t.dedup_signature AS bug_signature,
    COALESCE(t.payload->>'tier', 'T2') AS tier,
    COALESCE(
        NULLIF(t.payload->>'status', ''),
        CASE t.status
            WHEN 'pending' THEN 'detected'
            WHEN 'claimed' THEN 'fixing'
            WHEN 'running' THEN 'fixing'
            WHEN 'completed' THEN 'verified'
            WHEN 'paused' THEN 'paused'
            WHEN 'cancelled' THEN 'reverted'
            ELSE 'failed'
        END
    ) AS status,
    NULLIF(t.payload->>'writer_computer_id', '')::uuid AS writer_computer_id,
    t.payload->>'writer_model' AS writer_model,
    NULLIF(t.payload->>'reviewer_computer_id', '')::uuid AS reviewer_computer_id,
    t.payload->>'reviewer_model' AS reviewer_model,
    (t.payload->>'reviewer_confidence')::double precision AS reviewer_confidence,
    (t.payload->>'pr_number')::int AS pr_number,
    t.payload->>'branch_name' AS branch_name,
    t.payload->>'fix_commit_sha' AS fix_commit_sha,
    t.payload->>'fixed_tag' AS fixed_tag,
    COALESCE((t.payload->>'attempts')::int, 0) AS attempts,
    NULLIF(t.payload->>'last_attempt_at', '')::timestamptz AS last_attempt_at,
    NULLIF(t.payload->>'escalated_to_operator_at', '')::timestamptz AS escalated_to_operator_at,
    COALESCE((t.payload->>'report_count')::int, 0) AS report_count,
    t.created_at
FROM fleet_tasks t
WHERE t.task_class = 'self_heal';
"#;

pub const SCHEMA_V159_FOLD_DEFERRED_TASKS: &str = r#"
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '60s';

INSERT INTO fleet_tasks (
    id,
    task_type,
    summary,
    payload,
    priority,
    requires_capability,
    status,
    claimed_at,
    completed_at,
    result,
    error,
    created_at,
    task_class,
    not_before
)
SELECT
    d.id,
    d.kind,
    d.title,
    jsonb_strip_nulls(
        jsonb_build_object(
            'deferred_payload', COALESCE(d.payload, '{}'::jsonb),
            'created_by', d.created_by,
            'kind', d.kind,
            'trigger_type', d.trigger_type,
            'trigger_spec', COALESCE(d.trigger_spec, '{}'::jsonb),
            'preferred_node', d.preferred_node,
            'required_caps', COALESCE(d.required_caps, '[]'::jsonb),
            'attempts', d.attempts,
            'max_attempts', d.max_attempts,
            'claimed_by', d.claimed_by
        )
    ),
    50,
    COALESCE(d.required_caps, '[]'::jsonb),
    d.status,
    d.claimed_at,
    d.completed_at,
    d.result,
    d.last_error,
    d.created_at,
    'deferred',
    d.next_attempt_at
FROM deferred_tasks d
ON CONFLICT (id) DO NOTHING;

ALTER TABLE deferred_tasks RENAME TO deferred_tasks_legacy;

CREATE VIEW deferred_tasks AS
SELECT
    t.id,
    t.created_at,
    NULLIF(t.payload->>'created_by', '') AS created_by,
    t.summary AS title,
    COALESCE(NULLIF(t.payload->>'kind', ''), t.task_type) AS kind,
    COALESCE(t.payload->'deferred_payload', '{}'::jsonb) AS payload,
    COALESCE(t.payload->>'trigger_type', 'now') AS trigger_type,
    COALESCE(t.payload->'trigger_spec', '{}'::jsonb) AS trigger_spec,
    NULLIF(t.payload->>'preferred_node', '') AS preferred_node,
    COALESCE(t.payload->'required_caps', '[]'::jsonb) AS required_caps,
    t.status,
    COALESCE((t.payload->>'attempts')::int, 0) AS attempts,
    COALESCE((t.payload->>'max_attempts')::int, 5) AS max_attempts,
    t.not_before AS next_attempt_at,
    NULLIF(t.payload->>'claimed_by', '') AS claimed_by,
    t.claimed_at,
    t.error AS last_error,
    t.result,
    t.completed_at
FROM fleet_tasks t
WHERE t.task_class = 'deferred';
"#;

// V160 — operator failure-alert dedup/throttle. `notify_operator_task_failed`
// fired a Telegram alert on EVERY terminal work_item failure; during an incident
// (e.g. the 2026-07-04 restart loop that failed dozens of builds with the same
// "no dispatchable backend" error) this floods the operator. This tiny table lets
// the notifier collapse a burst of same-signature failures into one alert/hour.
pub const SCHEMA_V160_NOTIFY_DEDUP: &str = r#"
CREATE TABLE IF NOT EXISTS operator_notify_dedup (
    signature  TEXT PRIMARY KEY,
    last_sent  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#;

// ─── V161: mark the canonical GitHub SSH alias ──────────────────────────────
//
// The Pillar-4 dispatcher clones project repos with the BARE `git@github.com:`
// URL, which resolves (via each node's ~/.ssh/config) to a per-node default
// key — `id_rsa` — that is NOT authorized on the venkatyarl account on most
// fleet nodes. Measured 2026-07-06: bare `git@github.com:` fails with
// "Permission denied (publickey)" on 9 of 14 worker nodes (aura, logan, lily,
// veronica, duncan, sia, adele, rihanna, beyonce), so EVERY dispatch on those
// nodes dies at the clone step before any build runs. The canonical alias
// `github.com-venkat` (→ `id_venkat`, synced fleet-wide by `ff github sync` at
// enrollment) authenticates on 14/14.
//
// Flag the canonical alias in the DB so the dispatcher can look it up and clone
// via the authorized identity instead of hardcoding an account name. A future
// account migration just moves the flag; no code change.
pub const SCHEMA_V161_CANONICAL_GITHUB_ALIAS: &str = r#"
ALTER TABLE github_ssh_aliases
    ADD COLUMN IF NOT EXISTS is_canonical boolean NOT NULL DEFAULT false;

-- venkatyarl is the canonical account post-migration (taylor-oclaw retired).
UPDATE github_ssh_aliases SET is_canonical = true  WHERE alias_name = 'github.com-venkat';
UPDATE github_ssh_aliases SET is_canonical = false WHERE alias_name <> 'github.com-venkat';

-- At most one canonical alias per hostname.
CREATE UNIQUE INDEX IF NOT EXISTS github_ssh_aliases_one_canonical_per_host
    ON github_ssh_aliases (hostname) WHERE is_canonical;
"#;

/// V162 — drop the stale `UNIQUE (computer_id, worktree_path)` on
/// `work_item_worktrees`. Under clone-per-slot (operator decision 2026-07-17)
/// every build for a given slot runs in that slot's ONE clone, so many
/// work_items legitimately share the same `(computer_id, worktree_path)`. The
/// real per-item key is `task_branch` (still UNIQUE), which the dispatch INSERT
/// already conflicts on. The old pair-unique now spuriously rejects every
/// second build on a slot with `duplicate key ... work_item_worktrees_
/// computer_id_worktree_path_key`, failing the item. Forward-only drop.
pub const SCHEMA_V162_DROP_WORKTREE_PATH_UNIQUE: &str = r#"
ALTER TABLE work_item_worktrees
    DROP CONSTRAINT IF EXISTS work_item_worktrees_computer_id_worktree_path_key;
"#;

// ─── V163: fleet-wide backup policy table ───────────────────────────────────
//
// Operator req 2026-07-18: every backup setting (who produces a kind, where the
// offsite copies go, cadence, retention, encryption) must live in the DATABASE
// and be read by the code — not hardcoded. One row per backup kind; the
// on-disk layout is `~/.forgefleet/backups/<KIND>/` on every node.
//
// `source_host` NULL means "the current fleet leader" (postgres/redis run on
// the leader); a concrete name pins production to the host that actually runs
// that datastore (FalkorDB's container lives on priya). Empty `dest_hosts`
// means "auto-pick 2 recently-seen peers excluding the source" — the
// offsite-2-nodes rule; a non-empty array pins the destinations.
pub const SCHEMA_V163_FLEET_BACKUP_CONFIG: &str = r#"
CREATE TABLE IF NOT EXISTS fleet_backup_config (
    kind            TEXT PRIMARY KEY,          -- postgres|redis|falkordb|brain|obsidian
    source_host     TEXT,                      -- NULL = current fleet leader
    dest_hosts      TEXT[] NOT NULL DEFAULT '{}', -- empty = auto-pick 2 non-source peers
    interval_secs   BIGINT NOT NULL DEFAULT 14400,
    retention_count INT NOT NULL DEFAULT 14,   -- newest generations kept on the source
    retention_days  INT,                       -- NULL = no age-based pruning
    encrypt         BOOLEAN NOT NULL DEFAULT TRUE,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed the kinds the fleet backs up today. postgres/redis mirror the previous
-- hardcoded cadences/retention; falkordb is NEW (BGSAVE + AOF tar off priya —
-- it had no offsite dump job before this); brain/obsidian are policy
-- placeholders for their folders under ~/.forgefleet/backups/.
INSERT INTO fleet_backup_config
    (kind, source_host, interval_secs, retention_count, retention_days, encrypt)
VALUES
    ('postgres', NULL,    14400, 14, 30,   TRUE),
    ('redis',    NULL,     7200, 60, 14,   TRUE),
    ('falkordb', 'priya', 21600, 14, 30,   TRUE),
    ('brain',    NULL,    86400, 14, 60,   TRUE),
    ('obsidian', NULL,    86400, 14, 60,   TRUE)
ON CONFLICT (kind) DO NOTHING;
"#;

// ─── V165: inference-server-per-hardware decision table ─────────────────────
//
// Operator req 2026-07-18: the hardware → inference-server mapping must live
// in the DATABASE and be read by the code (onboarding hardware-detection),
// not hardcoded in the bootstrap script / detector heuristics. Rows with
// kind='server_policy' are keyed on (arch, gpu_kind, has_discrete_vram,
// ram_tier), each key column accepting the literal 'any' as a wildcard. The
// resolver (ff-pulse materializer) picks the row matching the most concrete
// key columns and self-heals fleet_workers.runtime to the row's runtime,
// then seeds the row's model downloads through the deferred task queue.
//
// Operator override baked into the seed: AMD GTT-unified boxes (Strix Halo:
// duncan/lily/logan/veronica) serve via ROCm llama-server (Lemonade as the
// fallback stack), NOT the Vulkan default the runtime detector recommends.
pub const SCHEMA_V165_SERVER_POLICY: &str = r#"
CREATE TABLE IF NOT EXISTS fleet_server_policies (
    id                BIGSERIAL PRIMARY KEY,
    kind              TEXT NOT NULL DEFAULT 'server_policy',
    arch              TEXT NOT NULL DEFAULT 'any',   -- aarch64|x86_64|any
    gpu_kind          TEXT NOT NULL DEFAULT 'any',   -- nvidia_cuda|amd_rocm|apple_silicon|none|any
    has_discrete_vram TEXT NOT NULL DEFAULT 'any',   -- yes|no|any ('no' = unified/shared RAM)
    ram_tier          TEXT NOT NULL DEFAULT 'any',   -- tiny (<=8GB)|standard|any
    runtime           TEXT NOT NULL,                 -- fleet_workers.runtime value: vllm|llama.cpp|mlx
    primary_server    TEXT NOT NULL,                 -- concrete server + backend to run
    fallback_server   TEXT,
    seed_model_ids    JSONB NOT NULL DEFAULT '[]'::jsonb, -- fleet_model_catalog ids to download on onboard
    notes             TEXT,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (kind, arch, gpu_kind, has_discrete_vram, ram_tier)
);

INSERT INTO fleet_server_policies
    (kind, arch, gpu_kind, has_discrete_vram, ram_tier,
     runtime, primary_server, fallback_server, seed_model_ids, notes)
VALUES
    ('server_policy', 'aarch64', 'nvidia_cuda',   'no',  'any',
     'vllm',      'vllm (aarch64/sm_121)',             'llama-server (CUDA)',
     '["qwen35-9b"]'::jsonb, 'DGX Spark GB10 — CUDA unified memory'),
    ('server_policy', 'x86_64',  'nvidia_cuda',   'yes', 'any',
     'vllm',      'vllm (CUDA)',                       'llama-server (CUDA)',
     '["qwen35-9b"]'::jsonb, 'x86 NVIDIA with discrete VRAM'),
    ('server_policy', 'x86_64',  'amd_rocm',      'yes', 'any',
     'vllm',      'vllm (ROCm)',                       'llama-server (ROCm)',
     '["qwen35-9b"]'::jsonb, 'x86 AMD with discrete VRAM'),
    ('server_policy', 'x86_64',  'amd_rocm',      'no',  'any',
     'llama.cpp', 'llama-server (ROCm)',               'lemonade (ROCm)',
     '["qwen35-9b"]'::jsonb, 'AMD GTT-unified (Strix Halo: duncan/lily/logan/veronica) — operator 2026-07-18: ROCm, NOT Vulkan'),
    ('server_policy', 'aarch64', 'apple_silicon', 'no',  'any',
     'mlx',       'mlx_lm.server',                     'llama-server (Metal)',
     '["qwen35-9b"]'::jsonb, 'Apple Silicon unified memory'),
    ('server_policy', 'x86_64',  'none',          'any', 'standard',
     'llama.cpp', 'llama-server (CPU: OpenVINO/AVX2)', NULL,
     '["qwen35-9b"]'::jsonb, 'Intel/AMD CPU-only boxes'),
    ('server_policy', 'any',     'any',           'any', 'tiny',
     'llama.cpp', 'llama-server (CPU)',                NULL,
     '[]'::jsonb,            '<=8GB RAM — CPU only, no model seed'),
    ('server_policy', 'any',     'any',           'any', 'any',
     'llama.cpp', 'llama-server (CPU)',                NULL,
     '[]'::jsonb,            'catch-all fallback')
ON CONFLICT (kind, arch, gpu_kind, has_discrete_vram, ram_tier) DO NOTHING;
"#;

// ─── V166: task notification outbox ─────────────────────────────────────────
//
// Transactional outbox for fleet_tasks lifecycle events. A trigger captures
// new tasks and status changes so a background relay can publish notifications
// without coupling the task writer to external brokers.
pub const SCHEMA_V166_TASK_NOTIFICATION_OUTBOX: &str = r#"
CREATE TABLE IF NOT EXISTS task_notification_outbox (
    id           BIGSERIAL PRIMARY KEY,
    task_id      UUID NOT NULL,
    event_type   TEXT NOT NULL,            -- 'created' | 'status_changed'
    payload      JSONB NOT NULL DEFAULT '{}'::jsonb,
    processed_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_task_notification_outbox_unprocessed
    ON task_notification_outbox (created_at ASC)
    WHERE processed_at IS NULL;

CREATE OR REPLACE FUNCTION enqueue_task_notification()
RETURNS TRIGGER AS $$
DECLARE
    event_type TEXT;
    payload    JSONB;
BEGIN
    IF TG_OP = 'INSERT' THEN
        event_type := 'created';
    ELSIF TG_OP = 'UPDATE' AND OLD.status IS DISTINCT FROM NEW.status THEN
        event_type := 'status_changed';
    ELSE
        RETURN NEW;
    END IF;

    payload := jsonb_build_object(
        'task_id',          NEW.id,
        'event_type',       event_type,
        'status',           NEW.status,
        'previous_status',  CASE WHEN TG_OP = 'UPDATE' THEN OLD.status ELSE NULL END,
        'changed_at',       NOW()
    );

    INSERT INTO task_notification_outbox (task_id, event_type, payload)
    VALUES (NEW.id, event_type, payload);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_task_notification_outbox ON fleet_tasks;
CREATE TRIGGER trg_task_notification_outbox
    AFTER INSERT OR UPDATE OF status ON fleet_tasks
    FOR EACH ROW
    EXECUTE FUNCTION enqueue_task_notification();
"#;

/// V167 — Telegram send/reply routing (operator request 2026-07-19: "if I
/// reply to a Telegram message, ff routes the reply to the session that sent
/// it"). `telegram_messages` records every recorded outbound send with the
/// originating session; the leader's reply poller matches incoming
/// `reply_to_message` ids against it and files rows in `telegram_replies`,
/// which sessions consume (claim) via `ff notify replies`. `telegram_poll_state`
/// is the single-row getUpdates offset so updates are consumed exactly once
/// even across leader failover.
pub const SCHEMA_V167_TELEGRAM_REPLY_ROUTING: &str = r#"
CREATE TABLE IF NOT EXISTS telegram_messages (
    id            BIGSERIAL PRIMARY KEY,
    chat_id       TEXT NOT NULL,
    tg_message_id BIGINT NOT NULL,
    session_id    TEXT,
    title         TEXT,
    sent_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (chat_id, tg_message_id)
);

CREATE TABLE IF NOT EXISTS telegram_replies (
    id                     BIGSERIAL PRIMARY KEY,
    tg_update_id           BIGINT NOT NULL UNIQUE,
    chat_id                TEXT NOT NULL,
    reply_to_tg_message_id BIGINT,
    session_id             TEXT,
    from_name              TEXT,
    body                   TEXT NOT NULL,
    received_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    claimed_at             TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_telegram_replies_unclaimed
    ON telegram_replies (session_id) WHERE claimed_at IS NULL;

CREATE TABLE IF NOT EXISTS telegram_poll_state (
    singleton      BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    last_update_id BIGINT NOT NULL DEFAULT 0,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#;

/// V168 — Add free-form structured `context` to canonical work_items.
///
/// The `context` JSONB holds task-specific metadata (e.g. source-system
/// identifiers, dispatch hints, or precomputed Cortex packs) without requiring
/// a new column for every ad-hoc key. It defaults to an empty object so
/// existing rows remain valid and fresh rebuilds get the column too.
pub const SCHEMA_V168_WORK_ITEM_CONTEXT: &str = r#"
ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS context JSONB NOT NULL DEFAULT '{}';
"#;

/// V169 — Peer-mount inventory.
///
/// Records which fleet computers mount which peers (autofs or manual NFS
/// mounts).  The mesh check and `ff doctor` correlate these rows with
/// `fleet_mesh_status` to flag stale mounts while a peer is unreachable.
pub const SCHEMA_V169_PEER_MOUNT_INVENTORY: &str = r#"
CREATE TABLE IF NOT EXISTS node_peer_mounts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    computer_id     UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    peer_name       TEXT NOT NULL,
    source          TEXT NOT NULL,
    mount_path      TEXT NOT NULL,
    fs_type         TEXT NOT NULL,
    mount_options   TEXT,
    detected_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_check_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (computer_id, mount_path)
);

CREATE INDEX IF NOT EXISTS idx_node_peer_mounts_peer
    ON node_peer_mounts(peer_name);
CREATE INDEX IF NOT EXISTS idx_node_peer_mounts_computer
    ON node_peer_mounts(computer_id);
"#;

/// V170 — Work queue persistence.
///
/// Generic durable work queue with priority and status. Items are claimed by
/// workers, retried up to `max_attempts`, and completed with an optional result
/// or failure reason. Indexed for the common "next pending by priority" claim
/// pattern and for status scans.
pub const SCHEMA_V170_WORK_QUEUE: &str = r#"
CREATE TABLE IF NOT EXISTS work_queue (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    queue_name      TEXT NOT NULL DEFAULT 'default',
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb,
    priority        INT NOT NULL DEFAULT 0,
    status          TEXT NOT NULL DEFAULT 'pending', -- pending | claimed | running | completed | failed | cancelled
    worker_id       TEXT,
    attempts        INT NOT NULL DEFAULT 0,
    max_attempts    INT NOT NULL DEFAULT 3,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    scheduled_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    last_error      TEXT,
    result          JSONB
);

CREATE INDEX IF NOT EXISTS idx_work_queue_claim
    ON work_queue (queue_name, status, priority DESC, scheduled_at, id);
CREATE INDEX IF NOT EXISTS idx_work_queue_worker
    ON work_queue (worker_id) WHERE worker_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_work_queue_status_created
    ON work_queue (status, created_at DESC);
"#;

/// V171 — Fleet artifact cache index.
///
/// Tracks each immutable artifact version by digest and the fleet nodes that
/// currently hold it in their local cache.
pub const SCHEMA_V171_ARTIFACT_INDEX: &str = r#"
CREATE TABLE IF NOT EXISTS artifact_index (
    artifact        TEXT NOT NULL,
    version         TEXT NOT NULL,
    sha256          TEXT NOT NULL,
    holder_nodes    TEXT[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (artifact, version)
);

CREATE INDEX IF NOT EXISTS idx_artifact_index_sha256
    ON artifact_index (sha256);
"#;

/// V173 — Atomic `primary_ip` / `total_ram_gb` updates on `computers`.
///
/// Every legitimate writer of a computer's network identity carries its
/// hardware profile in the SAME statement (materializer Q4, self-enroll
/// upsert), so a row where one half moved without the other is always a
/// partial update — e.g. an IP rewrite that leaves RAM from a different
/// enrollment, which mis-sizes autoscaler placement. Postgres triggers can't
/// see an UPDATE's SET list, so the enforceable invariant is value-level:
/// a `primary_ip` change must leave `total_ram_gb` populated, and a
/// `total_ram_gb` change must leave `primary_ip` non-blank. The trigger's
/// WHEN clause keeps the hot last_seen_at-only heartbeat UPDATE from ever
/// invoking the function.
pub const SCHEMA_V173_COMPUTERS_IP_RAM_ATOMIC: &str = r#"
CREATE OR REPLACE FUNCTION computers_ip_ram_paired_update_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $guard$
BEGIN
    IF NEW.primary_ip IS DISTINCT FROM OLD.primary_ip
       AND NEW.total_ram_gb IS NULL
    THEN
        RAISE EXCEPTION
            'computers: partial update rejected — primary_ip changed (% -> %) with total_ram_gb NULL; update primary_ip and total_ram_gb together in one statement',
            OLD.primary_ip, NEW.primary_ip
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.total_ram_gb IS DISTINCT FROM OLD.total_ram_gb
       AND btrim(NEW.primary_ip) = ''
    THEN
        RAISE EXCEPTION
            'computers: partial update rejected — total_ram_gb changed (% -> %) with primary_ip blank; update primary_ip and total_ram_gb together in one statement',
            OLD.total_ram_gb, NEW.total_ram_gb
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END;
$guard$;

DROP TRIGGER IF EXISTS trg_computers_ip_ram_paired_update ON computers;
CREATE TRIGGER trg_computers_ip_ram_paired_update
    BEFORE UPDATE ON computers
    FOR EACH ROW
    WHEN (OLD.primary_ip IS DISTINCT FROM NEW.primary_ip
          OR OLD.total_ram_gb IS DISTINCT FROM NEW.total_ram_gb)
    EXECUTE FUNCTION computers_ip_ram_paired_update_guard();
"#;

/// V174 — Dispatch tick tracking for active work_item leases.
///
/// `heartbeat_at` tracks the in-build process, but a host's dispatch loop can
/// stall while the build keeps heartbeating (e.g. the outer tokio task wedges).
/// `dispatch_tick_at` is bumped by the host's dispatch loop every tick so the
/// stale-lease reaper can also reclaim leases whose host stopped dispatching.
pub const SCHEMA_V174_DISPATCH_TICK_AT: &str = r#"
ALTER TABLE work_item_leases
    ADD COLUMN IF NOT EXISTS dispatch_tick_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

CREATE INDEX IF NOT EXISTS idx_work_item_leases_dispatch_tick
    ON work_item_leases (lease_state, dispatch_tick_at) WHERE released_at IS NULL;
"#;

/// V175 — Per-deployment `/metrics` scrape samples.
///
/// The ff-agent metrics scraper polls each local inference server's
/// Prometheus `/metrics` endpoint every 30s and appends one row per reachable
/// deployment per pass. Stale-record lifecycle: `deployment_id` cascades so
/// samples vanish with their deployment row, and rows older than the
/// retention window are pruned by the scraper on each pass.
pub const SCHEMA_V175_DEPLOYMENT_METRICS_SCRAPES: &str = r#"
CREATE TABLE IF NOT EXISTS deployment_metrics_scrapes (
    id               BIGSERIAL PRIMARY KEY,
    deployment_id    UUID NOT NULL REFERENCES fleet_model_deployments(id) ON DELETE CASCADE,
    worker_name      TEXT NOT NULL,
    port             INT NOT NULL,
    runtime          TEXT,
    tokens_per_sec   DOUBLE PRECISION,
    queue_depth      INT,
    active_requests  INT,
    metric_count     INT NOT NULL DEFAULT 0,
    scraped_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_deployment_metrics_scrapes_by_time
    ON deployment_metrics_scrapes (scraped_at DESC);
CREATE INDEX IF NOT EXISTS idx_deployment_metrics_scrapes_by_deployment
    ON deployment_metrics_scrapes (deployment_id, scraped_at DESC);
"#;

/// V176 — Merge train status tracking.
///
/// A merge train groups PRs/branches targeting the same base branch so they can
/// be built and merged as an ordered unit. `merge_trains` tracks the composite
/// train state and outcome; `merge_train_members` tracks PR membership and
/// per-member merge outcomes. Existing per-PR `work_item_merge_queue` rows are
/// linked to their parent train via `train_id`.
pub const SCHEMA_V176_MERGE_TRAINS: &str = r#"
CREATE TABLE IF NOT EXISTS merge_trains (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id     TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    base_branch    TEXT NOT NULL,
    base_sha       TEXT,
    head_sha       TEXT,
    status         TEXT NOT NULL DEFAULT 'assembling'
        CHECK (status IN ('assembling','running','merged','failed','cancelled')),
    outcome        TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at     TIMESTAMPTZ,
    completed_at   TIMESTAMPTZ,
    failure_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_merge_trains_project_status
    ON merge_trains (project_id, status);
CREATE INDEX IF NOT EXISTS idx_merge_trains_status_created
    ON merge_trains (status, created_at DESC);

CREATE TABLE IF NOT EXISTS merge_train_members (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    train_id       UUID NOT NULL REFERENCES merge_trains(id) ON DELETE CASCADE,
    work_item_id   UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    queue_id       UUID REFERENCES work_item_merge_queue(id) ON DELETE SET NULL,
    position       INT NOT NULL,
    branch_name    TEXT NOT NULL,
    pr_url         TEXT,
    head_sha       TEXT,
    status         TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','running','merged','failed','skipped')),
    joined_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    merged_at      TIMESTAMPTZ,
    failed_at      TIMESTAMPTZ,
    failure_reason TEXT,
    UNIQUE (train_id, work_item_id),
    UNIQUE (train_id, position)
);
CREATE INDEX IF NOT EXISTS idx_merge_train_members_train
    ON merge_train_members (train_id, position);
CREATE INDEX IF NOT EXISTS idx_merge_train_members_work_item
    ON merge_train_members (work_item_id);

ALTER TABLE work_item_merge_queue
    ADD COLUMN IF NOT EXISTS train_id UUID REFERENCES merge_trains(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_work_item_merge_queue_train
    ON work_item_merge_queue (train_id) WHERE train_id IS NOT NULL;
"#;

/// V177 — Tiered fleet metrics storage (partitioned).
///
/// Three retention tiers for fleet-wide time-series metrics, all natively
/// range-partitioned on their time column so retention is a cheap partition
/// DROP instead of a bloating DELETE:
///
/// - `fleet_metrics_raw`    — every sample as written; DAILY partitions kept 7 days.
/// - `fleet_metrics_1min`   — 1-minute rollups; DAILY partitions kept 30 days.
/// - `fleet_metrics_hourly` — hourly rollups; MONTHLY partitions kept forever.
///
/// The migration creates only the partitioned parents (plus partitioned
/// indexes, which cascade to every child). Dated child partitions are created
/// ahead of time and expired ones dropped by the partition-maintenance tick
/// (`ff_db::metrics_partitions`, driven leader-gated from forgefleetd) — a
/// parent with no children rejects inserts, so writers depend on that tick
/// having run at least once.
pub const SCHEMA_V177_FLEET_METRICS: &str = r#"
CREATE TABLE IF NOT EXISTS fleet_metrics_raw (
    worker_name  TEXT NOT NULL,
    metric       TEXT NOT NULL,
    value        DOUBLE PRECISION NOT NULL,
    labels       JSONB NOT NULL DEFAULT '{}'::jsonb,
    recorded_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
) PARTITION BY RANGE (recorded_at);
CREATE INDEX IF NOT EXISTS idx_fleet_metrics_raw_metric_time
    ON fleet_metrics_raw (metric, recorded_at DESC);
CREATE INDEX IF NOT EXISTS idx_fleet_metrics_raw_worker_time
    ON fleet_metrics_raw (worker_name, recorded_at DESC);

CREATE TABLE IF NOT EXISTS fleet_metrics_1min (
    worker_name  TEXT NOT NULL,
    metric       TEXT NOT NULL,
    bucket_start TIMESTAMPTZ NOT NULL,
    sample_count BIGINT NOT NULL DEFAULT 0,
    value_min    DOUBLE PRECISION NOT NULL,
    value_max    DOUBLE PRECISION NOT NULL,
    value_avg    DOUBLE PRECISION NOT NULL,
    value_last   DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (worker_name, metric, bucket_start)
) PARTITION BY RANGE (bucket_start);
CREATE INDEX IF NOT EXISTS idx_fleet_metrics_1min_metric_time
    ON fleet_metrics_1min (metric, bucket_start DESC);

CREATE TABLE IF NOT EXISTS fleet_metrics_hourly (
    worker_name  TEXT NOT NULL,
    metric       TEXT NOT NULL,
    bucket_start TIMESTAMPTZ NOT NULL,
    sample_count BIGINT NOT NULL DEFAULT 0,
    value_min    DOUBLE PRECISION NOT NULL,
    value_max    DOUBLE PRECISION NOT NULL,
    value_avg    DOUBLE PRECISION NOT NULL,
    value_last   DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (worker_name, metric, bucket_start)
) PARTITION BY RANGE (bucket_start);
CREATE INDEX IF NOT EXISTS idx_fleet_metrics_hourly_metric_time
    ON fleet_metrics_hourly (metric, bucket_start DESC);
"#;

/// V178 — Classified model-server error events.
///
/// Persists startup/load/crash/OOM failures from ff-agent's model runtime so
/// dashboards and the autoscaler can reason about failure patterns per model,
/// node, and runtime without re-parsing server log files.
pub const SCHEMA_V178_ERROR_EVENTS: &str = r#"
CREATE TABLE IF NOT EXISTS error_events (
    id              BIGSERIAL PRIMARY KEY,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    worker_name     TEXT NOT NULL,
    deployment_id   UUID REFERENCES fleet_model_deployments(id) ON DELETE SET NULL,
    library_id      UUID REFERENCES fleet_model_library(id) ON DELETE SET NULL,
    catalog_id      TEXT,
    runtime         TEXT NOT NULL,
    error_kind      TEXT NOT NULL CHECK (error_kind IN ('startup', 'load', 'crash', 'oom')),
    summary         TEXT NOT NULL,
    details         JSONB NOT NULL DEFAULT '{}'::jsonb,
    stderr_tail     TEXT,
    resolved_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_error_events_worker_time
    ON error_events (worker_name, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_error_events_kind_time
    ON error_events (error_kind, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_error_events_deployment
    ON error_events (deployment_id, occurred_at DESC) WHERE deployment_id IS NOT NULL;
"#;

/// V179 — Work-item status-transition events.
///
/// Append-only audit trail of PM work-item status changes (from_status →
/// to_status), recording which computer drove the transition and the attempt
/// number, so dashboards can reconstruct a work item's lifecycle without
/// parsing scheduler logs.
pub const SCHEMA_V179_WORK_ITEM_EVENTS: &str = r#"
CREATE TABLE IF NOT EXISTS work_item_events (
    id            BIGSERIAL PRIMARY KEY,
    work_item_id  UUID NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    from_status   TEXT,
    to_status     TEXT NOT NULL,
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    computer      TEXT,
    attempt       INTEGER,
    detail        JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS idx_work_item_events_item_time
    ON work_item_events (work_item_id, occurred_at DESC);
"#;

/// V180 — `model_capacity` view over scraped deployment metrics.
///
/// Per (computer, deployment) capacity/utilization snapshot: each running
/// deployment joined with the newest `deployment_metrics_scrapes` sample for
/// it. Freshness gate per the council observability design: when the newest
/// sample is older than 90 seconds (or no sample exists yet) `status` is
/// 'unknown' — otherwise it passes through the deployment's `health_status`.
/// Read-only observability surface; no routing logic lives here.
pub const SCHEMA_V180_MODEL_CAPACITY_VIEW: &str = r#"
CREATE OR REPLACE VIEW model_capacity AS
SELECT
    d.worker_name       AS computer,
    d.id                AS deployment_id,
    d.catalog_id,
    d.runtime,
    d.port,
    d.context_window,
    s.tokens_per_sec,
    s.queue_depth,
    s.active_requests,
    s.scraped_at        AS last_scraped_at,
    CASE
        WHEN s.scraped_at IS NULL
          OR s.scraped_at < NOW() - INTERVAL '90 seconds'
        THEN 'unknown'
        ELSE d.health_status
    END                 AS status
FROM fleet_model_deployments d
LEFT JOIN LATERAL (
    SELECT m.tokens_per_sec, m.queue_depth, m.active_requests, m.scraped_at
    FROM deployment_metrics_scrapes m
    WHERE m.deployment_id = d.id
    ORDER BY m.scraped_at DESC
    LIMIT 1
) s ON TRUE;
"#;

/// V181 — Fleet work-item velocity views.
///
/// Aggregates the V179 transition log into stable hourly and daily reporting
/// surfaces consumed by the nightly fleet digest. Successful completions are
/// the PM terminal states `done` and `merged`; retries remain visible through
/// the event attempt number.
pub const SCHEMA_V181_FLEET_VELOCITY_VIEWS: &str = r#"
CREATE OR REPLACE VIEW v_throughput_hourly AS
SELECT
    date_trunc('hour', occurred_at) AS hour_bucket,
    COUNT(*) FILTER (WHERE to_status IN ('done', 'merged'))::BIGINT AS completed_count,
    COUNT(*) FILTER (WHERE to_status = 'failed')::BIGINT AS failed_count
FROM work_item_events
WHERE to_status IN ('done', 'merged', 'failed')
GROUP BY 1;

CREATE OR REPLACE VIEW v_lead_time_daily AS
SELECT
    date_trunc('day', e.occurred_at) AS day_bucket,
    COUNT(*)::BIGINT AS completed_count,
    AVG(EXTRACT(EPOCH FROM (e.occurred_at - w.created_at)))::DOUBLE PRECISION
        AS avg_lead_time_seconds
FROM work_item_events e
JOIN work_items w ON w.id = e.work_item_id
WHERE e.to_status IN ('done', 'merged')
GROUP BY 1;

CREATE OR REPLACE VIEW v_computer_builds_daily AS
SELECT
    date_trunc('day', e.occurred_at) AS day_bucket,
    COALESCE(e.computer, w.assigned_computer, 'unknown') AS computer_name,
    COUNT(*) FILTER (WHERE e.to_status = 'building')::BIGINT AS builds_started,
    COUNT(*) FILTER (WHERE e.to_status IN ('done', 'merged'))::BIGINT AS builds_succeeded,
    COUNT(*) FILTER (WHERE e.to_status = 'failed')::BIGINT AS builds_failed
FROM work_item_events e
JOIN work_items w ON w.id = e.work_item_id
WHERE e.to_status IN ('building', 'done', 'merged', 'failed')
GROUP BY 1, 2;

CREATE OR REPLACE VIEW v_first_pass_rate_daily AS
SELECT
    date_trunc('day', occurred_at) AS day_bucket,
    COUNT(*)::BIGINT AS completed_count,
    COUNT(*) FILTER (WHERE COALESCE(attempt, 1) <= 1)::BIGINT AS first_pass_count,
    (COUNT(*) FILTER (WHERE COALESCE(attempt, 1) <= 1)::DOUBLE PRECISION
        / NULLIF(COUNT(*), 0))::DOUBLE PRECISION AS first_pass_rate
FROM work_item_events
WHERE to_status IN ('done', 'merged')
GROUP BY 1;
"#;

/// V182 — trigger that records every `work_items.status` transition into
/// `work_item_events`. A DB-level trigger (not app-side inserts) is deliberate:
/// it captures ALL writers, including manual `psql` heals and out-of-band
/// updates, so the V181 velocity views see the complete journey. Fires only
/// when status actually changes; `detail` is left as the table default.
pub const SCHEMA_V182_WORK_ITEM_EVENTS_TRIGGER: &str = r#"
CREATE OR REPLACE FUNCTION log_work_item_status_change() RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO work_item_events
        (work_item_id, from_status, to_status, computer, attempt)
    VALUES
        (NEW.id, OLD.status, NEW.status, NEW.assigned_computer, NEW.attempts);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_work_item_status_change ON work_items;
CREATE TRIGGER trg_work_item_status_change
    AFTER UPDATE OF status ON work_items
    FOR EACH ROW
    WHEN (OLD.status IS DISTINCT FROM NEW.status)
    EXECUTE FUNCTION log_work_item_status_change();
"#;

/// V183 — Artifact cache index.
///
/// Tracks which computer holds a local copy of a cached build artifact, for
/// the download-once-distribute-peer-to-peer cache (parent 468a7dc9): before
/// re-downloading an artifact from its origin, callers check this table for
/// a fleet peer that already has it.
pub const SCHEMA_V183_ARTIFACT_CACHE_INDEX: &str = r#"
CREATE TABLE IF NOT EXISTS artifact_cache_index (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    artifact_key   TEXT NOT NULL,
    computer       TEXT NOT NULL,
    file_path      TEXT NOT NULL,
    size_bytes     BIGINT NOT NULL DEFAULT 0,
    checksum       TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at   TIMESTAMPTZ,
    UNIQUE (artifact_key, computer)
);

CREATE INDEX IF NOT EXISTS idx_artifact_cache_index_artifact_key
    ON artifact_cache_index (artifact_key);
"#;

/// V184 — Postgres replica-death alert policy.
///
/// Seeds the imperative `postgres_replica_dead` alert policy used by
/// `ff_agent::ha::replica_monitor`. The tick probes every registered Postgres
/// replica via TCP and fires this alert when one or more replicas are
/// unreachable, closing the "both replicas silently dead while hosts stay up"
/// monitoring gap.
pub const SCHEMA_V184_POSTGRES_REPLICA_DEAD_ALERT: &str = r#"
INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('postgres_replica_dead',
   'One or more Postgres replicas are unreachable (TCP probe failed)',
   'postgres_replica_dead', 'leader_only', '> 0',
   0, 'critical', 3600, 'telegram', true)
ON CONFLICT (name) DO NOTHING;
"#;

// V185 — canonical sub-agent slot kind
//
// Distinguishes regular sub-agent slots (kind='sub_agent') from the
// canonical per-computer project checkout slot (kind='canonical', slot=99,
// workspace_dir=~/projects/{project}). The scheduler prefers regular slots
// and only falls back to canonical slots when all regular slots are busy.
pub const SCHEMA_V185_SUB_AGENTS_KIND: &str = r#"
ALTER TABLE sub_agents ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'sub_agent';
CREATE INDEX IF NOT EXISTS idx_sub_agents_kind ON sub_agents(kind);
"#;

/// V186 — bounded retention tiers for typed per-computer metrics history.
pub const SCHEMA_V186_COMPUTER_METRICS_ROLLUPS: &str = r#"
CREATE TABLE IF NOT EXISTS computer_metrics_history_hourly (
    computer_id UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    recorded_at TIMESTAMPTZ NOT NULL,
    sample_count BIGINT NOT NULL,
    cpu_pct DOUBLE PRECISION,
    ram_pct DOUBLE PRECISION,
    ram_used_gb DOUBLE PRECISION,
    disk_free_gb DOUBLE PRECISION,
    gpu_pct DOUBLE PRECISION,
    llm_ram_allocated_gb DOUBLE PRECISION,
    llm_queue_depth DOUBLE PRECISION,
    llm_active_requests DOUBLE PRECISION,
    llm_tokens_per_sec DOUBLE PRECISION,
    PRIMARY KEY (computer_id, recorded_at)
);
CREATE INDEX IF NOT EXISTS idx_computer_metrics_hourly_time
    ON computer_metrics_history_hourly (recorded_at DESC);

CREATE TABLE IF NOT EXISTS computer_metrics_history_daily
    (LIKE computer_metrics_history_hourly INCLUDING ALL);
CREATE INDEX IF NOT EXISTS idx_computer_metrics_daily_time
    ON computer_metrics_history_daily (recorded_at DESC);
"#;

/// V187 — Seed the ssh_mesh_degraded alert policy for the SSH mesh check.
pub const SCHEMA_V187_SSH_MESH_DEGRADED_ALERT: &str = r#"
INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('ssh_mesh_degraded',
   'One or more SSH mesh pairs are failed or asymmetric',
   'ssh_mesh_degraded', 'leader_only', '> 0',
   0, 'warning', 21600, 'telegram', true)
ON CONFLICT (name) DO NOTHING;
"#;

/// V188 — Align sub-agent paths to the consolidated nested full-clone-per-slot layout.
///
/// The runtime now keeps each sub-agent slot under
/// `~/.forgefleet/sub-agents/sub-agent-{N}/` with a full clone at
/// `~/.forgefleet/sub-agents/sub-agent-{N}/{repo-slug}/`. Historical seeds and
/// backfills still used the old flat `~/.forgefleet/sub-agent-0/...` spelling,
/// and the Phase-15e `fleet_workspaces` / `subagent_cleanup_log` tables tracked
/// a workspace layout (`~/.forgefleet/agents/agent-{id}/...`) that was never
/// adopted. This migration rewrites the paths and drops the orphaned tables.
pub const SCHEMA_V188_ALIGN_SUBAGENT_PATHS: &str = r#"
-- 1. Auto-upgrade playbooks for ForgeFleet's own repo now clone/build in the
--    nested slot-0 directory that dispatch actually uses.
UPDATE software_registry
   SET upgrade_playbook = replace(
       upgrade_playbook::text,
       '$HOME/.forgefleet/sub-agent-0/forge-fleet',
       '$HOME/.forgefleet/sub-agents/sub-agent-0/forge-fleet'
   )::jsonb
 WHERE id IN ('ff_git', 'forgefleetd_git')
   AND upgrade_playbook::text LIKE '%$HOME/.forgefleet/sub-agent-0/forge-fleet%';

-- 2. open-design skills repo now clones into the nested slot-0 directory.
UPDATE software_registry
   SET upgrade_playbook = replace(
       upgrade_playbook::text,
       '$HOME/.forgefleet/sub-agent-0/open-design',
       '$HOME/.forgefleet/sub-agents/sub-agent-0/open-design'
   )::jsonb
 WHERE id = 'open_design_git'
   AND upgrade_playbook::text LIKE '%$HOME/.forgefleet/sub-agent-0/open-design%';

-- 3. Skill catalog root for fleet-installed open-design skills.
UPDATE skill_sources
   SET path = '$HOME/.forgefleet/sub-agents/sub-agent-0/open-design/skills'
 WHERE id = 'fleet-open-design'
   AND path = '$HOME/.forgefleet/sub-agent-0/open-design/skills';

-- 4. Backfill worker source_tree_path from the old flat layout.
UPDATE computers
   SET source_tree_path = '~/.forgefleet/sub-agents/sub-agent-0/forge-fleet'
 WHERE source_tree_path = '~/.forgefleet/sub-agent-0/forge-fleet';

-- 5. Drop Phase-15e workspace tables that implemented a conflicting
--    `~/.forgefleet/agents/agent-{id}/...` layout and are no longer referenced.
DROP TABLE IF EXISTS subagent_cleanup_log;
DROP TABLE IF EXISTS fleet_workspaces;
"#;

/// V189 — Fleet capacity registry.
///
/// Adds `cloud_budget_buckets` and a read-only `v_fleet_capacity` view that
/// unions inference deployments, available build slots per computer, and cloud
/// budget buckets. This is the first slice of the router capacity epic; later
/// consumers will query this view instead of scraping tables ad-hoc.
pub const SCHEMA_V189_FLEET_CAPACITY_REGISTRY: &str = r#"
-- ─── V189: Fleet capacity registry ───────────────────────────────────────────

-- Cloud provider budget buckets. Tokens-per-minute is optional; when set the
-- view marks the bucket `degraded` once spent_today crosses it.
CREATE TABLE IF NOT EXISTS cloud_budget_buckets (
    provider        TEXT PRIMARY KEY,
    max_concurrent  INT NOT NULL,
    tokens_per_min  BIGINT,
    spent_today     NUMERIC NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed conservative defaults for the three cloud CLI backends.
INSERT INTO cloud_budget_buckets (provider, max_concurrent, tokens_per_min)
VALUES
    ('claude', 3, 10000),
    ('codex',  3, 10000),
    ('kimi',   3, 10000)
ON CONFLICT (provider) DO NOTHING;

-- Read-only capacity view over inference pools, build slots, and cloud buckets.
-- Each branch exposes a common shape so consumers can reason about capacity in
-- one place.
CREATE OR REPLACE VIEW v_fleet_capacity AS
SELECT
    'inference'::TEXT AS kind,
    dep.catalog_id,
    dep.worker_name AS worker,
    dep.port,
    dep.parallel_slots,
    dep.health_status AS health,
    NULL::INT AS max_concurrent,
    NULL::BIGINT AS tokens_per_min,
    NULL::NUMERIC AS spent_today
FROM fleet_model_deployments dep
LEFT JOIN fleet_model_catalog cat ON cat.id = dep.catalog_id

UNION ALL

SELECT
    'build'::TEXT AS kind,
    NULL::TEXT AS catalog_id,
    c.name AS worker,
    NULL::INT AS port,
    GREATEST(0, fw.sub_agent_count - COALESCE(active_leases.lease_count, 0)) AS parallel_slots,
    c.status AS health,
    fw.sub_agent_count AS max_concurrent,
    NULL::BIGINT AS tokens_per_min,
    NULL::NUMERIC AS spent_today
FROM fleet_workers fw
JOIN computers c ON c.name = fw.name
LEFT JOIN (
    SELECT computer_id, COUNT(*) AS lease_count
    FROM work_item_leases
    WHERE released_at IS NULL
    GROUP BY computer_id
) active_leases ON active_leases.computer_id = c.id

UNION ALL

SELECT
    'cloud'::TEXT AS kind,
    NULL::TEXT AS catalog_id,
    provider AS worker,
    NULL::INT AS port,
    max_concurrent AS parallel_slots,
    CASE
        WHEN tokens_per_min IS NULL OR tokens_per_min <= 0 THEN 'healthy'
        WHEN spent_today < tokens_per_min THEN 'healthy'
        ELSE 'degraded'
    END AS health,
    max_concurrent,
    tokens_per_min,
    spent_today
FROM cloud_budget_buckets;
"#;

/// V190 — In-place dispatch review (Pillar-4 v2).
///
/// The dispatch reviews a built change IN the warm build workspace before it
/// enters the merge queue. These columns record who built and who reviewed
/// (rule: never the same model), the verdict + rationale, and the review's
/// start/complete timestamps — the latency signal that makes reviewer routing
/// data-driven via `v_reviewer_stats` (folds into the fleet velocity views:
/// per-reviewer volume, verdict quality vs downstream merge outcome, and
/// average latency, which weights the cloud-trio round-robin).
pub const SCHEMA_V190_MERGE_QUEUE_INPLACE_REVIEW: &str = r#"
ALTER TABLE work_item_merge_queue
    ADD COLUMN IF NOT EXISTS builder             TEXT,
    ADD COLUMN IF NOT EXISTS reviewer            TEXT,
    ADD COLUMN IF NOT EXISTS review_verdict      TEXT,
    ADD COLUMN IF NOT EXISTS review_reason       TEXT,
    ADD COLUMN IF NOT EXISTS review_started_at   TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS review_completed_at TIMESTAMPTZ;

CREATE OR REPLACE VIEW v_reviewer_stats AS
SELECT
    reviewer,
    COUNT(*)                                                           AS reviews,
    COUNT(*) FILTER (WHERE review_verdict = 'approve')                 AS approvals,
    COUNT(*) FILTER (WHERE review_verdict = 'reject')                  AS rejections,
    COUNT(*) FILTER (WHERE review_verdict = 'approve'
                       AND status = 'merged')                          AS approved_then_merged,
    COUNT(*) FILTER (WHERE review_verdict = 'approve'
                       AND status = 'failed')                          AS approved_then_failed,
    AVG(EXTRACT(EPOCH FROM (review_completed_at - review_started_at))) AS avg_latency_secs,
    MAX(review_completed_at)                                           AS last_review_at
FROM work_item_merge_queue
WHERE reviewer IS NOT NULL
  AND review_started_at IS NOT NULL
  AND review_completed_at IS NOT NULL
GROUP BY reviewer;
"#;

/// V191 — Cloud budget bucket status seeds.
///
/// Operator-provided provider budget windows for cloud LLM routing. This
/// extends the V189 capacity table with provider budget-window status and
/// seeds the current operator-provided values.
pub const SCHEMA_V191_CLOUD_BUDGET_BUCKETS: &str = r#"
CREATE TABLE IF NOT EXISTS cloud_budget_buckets (
    provider                TEXT PRIMARY KEY,
    max_concurrent          INT NOT NULL DEFAULT 3,
    tokens_per_min          BIGINT,
    spent_today             NUMERIC NOT NULL DEFAULT 0,
    window_exhausted_until  TIMESTAMPTZ,
    weekly_pct              SMALLINT,
    weekly_reset_at         TIMESTAMPTZ,
    monthly_pct             SMALLINT,
    monthly_reset_at        TIMESTAMPTZ,
    credit_pool_spent_usd   NUMERIC DEFAULT 0,
    last_error_at           TIMESTAMPTZ,
    last_success_at         TIMESTAMPTZ,
    source                  TEXT,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE cloud_budget_buckets
    ADD COLUMN IF NOT EXISTS window_exhausted_until TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS weekly_pct SMALLINT,
    ADD COLUMN IF NOT EXISTS weekly_reset_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS monthly_pct SMALLINT,
    ADD COLUMN IF NOT EXISTS monthly_reset_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS credit_pool_spent_usd NUMERIC DEFAULT 0,
    ADD COLUMN IF NOT EXISTS last_error_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_success_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS source TEXT;

-- ensure max_concurrent has a default even if the table pre-existed without one
-- (older cloud_budget_buckets had max_concurrent INT NOT NULL with no default; the
-- CREATE IF NOT EXISTS above is a no-op there, so the seed INSERT below would 401 on NOT NULL)
ALTER TABLE cloud_budget_buckets ALTER COLUMN max_concurrent SET DEFAULT 3;
UPDATE cloud_budget_buckets SET max_concurrent = 3 WHERE max_concurrent IS NULL;

INSERT INTO cloud_budget_buckets (
    provider,
    window_exhausted_until,
    weekly_pct,
    weekly_reset_at,
    monthly_pct,
    monthly_reset_at,
    credit_pool_spent_usd,
    source,
    updated_at
) VALUES
    (
        'claude',
        NULL,
        64,
        '2026-07-23 01:59:00+00'::timestamptz,
        NULL,
        NULL,
        0,
        'fable-tier exhausted; credits on',
        NOW()
    ),
    (
        'codex',
        NULL,
        12,
        '2026-07-24 23:30:00+00'::timestamptz,
        NULL,
        NULL,
        0,
        'weekly budget used',
        NOW()
    ),
    (
        'kimi',
        '2026-07-20 04:20:00+00'::timestamptz,
        64,
        '2026-07-21 16:23:00+00'::timestamptz,
        19,
        '2026-08-03 00:00:00+00'::timestamptz,
        0,
        '5h window exhausted; 7day and monthly buckets',
        NOW()
    )
ON CONFLICT (provider) DO UPDATE SET
    window_exhausted_until = EXCLUDED.window_exhausted_until,
    weekly_pct = EXCLUDED.weekly_pct,
    weekly_reset_at = EXCLUDED.weekly_reset_at,
    monthly_pct = EXCLUDED.monthly_pct,
    monthly_reset_at = EXCLUDED.monthly_reset_at,
    credit_pool_spent_usd = EXCLUDED.credit_pool_spent_usd,
    source = EXCLUDED.source,
    updated_at = NOW();
"#;

/// V192 — Postgres WAL archiving policy.
///
/// Base backups still run every 4h, but PITR needs the intervening WAL
/// segments. Postgres now archives completed WAL into
/// `~/.forgefleet/backups/postgres-wal/` via deploy/docker-compose.yml; the
/// backup orchestrator rsyncs that directory to the same off-fleet destinations
/// and prunes it to 7 days. Seven days keeps recovery decoupled from the
/// immediate 4h snapshot cadence while bounding the archive generated by the
/// 5-minute `archive_timeout`.
pub const SCHEMA_V192_POSTGRES_WAL_ARCHIVING_CONFIG: &str = r#"
INSERT INTO fleet_backup_config
    (kind, source_host, dest_hosts, interval_secs, retention_count,
     retention_days, encrypt, enabled)
VALUES
    ('postgres_wal', NULL, '{}'::text[], 14400, 10000, 7, false, true)
ON CONFLICT (kind) DO UPDATE SET
    interval_secs = EXCLUDED.interval_secs,
    retention_count = EXCLUDED.retention_count,
    retention_days = COALESCE(fleet_backup_config.retention_days, EXCLUDED.retention_days),
    encrypt = false,
    enabled = fleet_backup_config.enabled,
    updated_at = NOW();

"#;

/// V193 — Per-node stale local backup alert policy.
///
/// The backup orchestrator evaluates this imperatively on every daemon because
/// the signal is each node's local `~/.forgefleet/backups/<kind>/` directory,
/// not a leader-visible catalog row.
pub const SCHEMA_V193_STALE_LOCAL_BACKUP_ALERT: &str = r#"
INSERT INTO alert_policies
    (name, description, metric, scope, condition, duration_secs, severity,
     cooldown_secs, channel, enabled, metadata)
VALUES
    (
        'stale_local_backup',
        'A node expected to hold a local HA backup has no fresh local artifact; newest local backup is older than 2x the configured backup interval.',
        'stale_local_backup_age_secs',
        'any_computer',
        '> 0',
        0,
        'critical',
        3600,
        'telegram',
        true,
        '{"source":"backup_orchestrator","imperative":true}'::jsonb
    )
ON CONFLICT (name) DO NOTHING;
"#;

/// V194 — Add review tracking columns to the work-item merge queue.
///
/// These fields support a lightweight human/LLM review gate before an item is
/// allowed to land: who claimed the review, when, and the verdict + rationale.
pub const SCHEMA_V194_MERGE_QUEUE_REVIEW_FIELDS: &str = r#"
ALTER TABLE work_item_merge_queue
    ADD COLUMN IF NOT EXISTS reviewer_computer TEXT,
    ADD COLUMN IF NOT EXISTS review_claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS review_verdict    TEXT,
    ADD COLUMN IF NOT EXISTS review_reason     TEXT;
"#;

/// V195 — Record the squashed v161 bootstrap as baseline generation v1.
pub const SCHEMA_V195_BOOTSTRAP_V161_V1_BASELINE: &str = r#"
CREATE TABLE IF NOT EXISTS _migration_baselines (
    generation       INTEGER PRIMARY KEY,
    migration_version INTEGER NOT NULL,
    name             TEXT NOT NULL
);

INSERT INTO _migration_baselines (generation, migration_version, name)
VALUES (1, 161, 'bootstrap_v161')
ON CONFLICT (generation) DO NOTHING;
"#;

/// V196 — Per-host work-item dispatch loop liveness.
///
/// Pulse materializes the most recent dispatch tick here so the leader's
/// work-item scheduler can avoid leasing work to a daemon whose general
/// heartbeat is still fresh while its dispatch subsystem is wedged.
pub const SCHEMA_V196_COMPUTER_DISPATCH_TICK: &str = r#"
ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS dispatch_tick_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_computers_dispatch_tick
    ON computers (dispatch_tick_at);
"#;

// V197 — persist suppressed operator-alert counts so Telegram throttling is
// shared by every leader process and the next delivered alert can summarize
// the identical (metric, node) occurrences collapsed during the window.
pub const SCHEMA_V197_OPERATOR_ALERT_DEDUP_COUNTS: &str = r#"
ALTER TABLE operator_notify_dedup
    ADD COLUMN IF NOT EXISTS suppressed_count BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS send_count BIGINT NOT NULL DEFAULT 1;
"#;

/// V198 — Persist explicit backlog parking and seed the autonomous feeder off.
pub const SCHEMA_V198_AUTO_BACKLOG_FEEDER: &str = r#"
ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS parked BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_work_items_feedable_ideas
    ON work_items (priority, created_at)
    WHERE status = 'idea' AND parked = FALSE;

INSERT INTO fleet_secrets (key, value)
VALUES ('auto_feeder_mode', 'off')
ON CONFLICT (key) DO NOTHING;
"#;

/// V199 — Continuous daemon rollout safety gate and provenance.
///
/// The operator-visible gate deliberately has only two values: `manual` keeps
/// the existing explicit rollout workflow, while `auto` allows the leader tick
/// to create a canary rollout after merge/time drift crosses its threshold.
pub const SCHEMA_V199_CONTINUOUS_ROLLOUT: &str = r#"
INSERT INTO fleet_secrets (key, value)
VALUES ('rollout_mode', 'manual')
ON CONFLICT (key) DO NOTHING;

ALTER TABLE upgrade_rollouts
    ADD COLUMN IF NOT EXISTS target_version TEXT,
    ADD COLUMN IF NOT EXISTS automatic BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS canary_bake_started_at TIMESTAMPTZ;

CREATE UNIQUE INDEX IF NOT EXISTS idx_upgrade_rollouts_one_auto_inflight
    ON upgrade_rollouts (software_id)
    WHERE automatic = TRUE AND status = 'in_progress';
"#;

/// V200 — Enable the cost-optimal local-first PR review ladder.
pub const SCHEMA_V200_REVIEW_LADDER_MODE: &str = r#"
INSERT INTO fleet_secrets (key, value, description)
VALUES (
    'review_ladder_mode',
    'cost_optimal',
    'Local review first; cloud only confirms weak local approvals; local coders repair rejects'
)
ON CONFLICT (key) DO NOTHING;
"#;

/// V201 — Make folder-owned, in-place review the merge-queue contract.
pub const SCHEMA_V201_FOLDER_OWNED_PR_REVIEW: &str = r#"
INSERT INTO fleet_secrets (key, value, description)
VALUES (
    'distributed_review_mode',
    'on',
    'Each build folder cross-model reviews its own warm tree before enqueue'
)
ON CONFLICT (key) DO UPDATE SET
    value = EXCLUDED.value,
    description = EXCLUDED.description,
    updated_at = NOW();

CREATE INDEX IF NOT EXISTS idx_work_item_merge_queue_approved
    ON work_item_merge_queue (project_id, position)
    WHERE status IN ('queued', 'ci_running', 'mergeable')
      AND review_verdict = 'approve';
"#;

/// V202 — Make the per-holder artifact cache index safe for verified LAN pulls.
pub const SCHEMA_V202_ARTIFACT_CACHE_HOLDERS: &str = r#"
ALTER TABLE artifact_cache_index ALTER COLUMN checksum SET NOT NULL;
ALTER TABLE artifact_cache_index
    ADD CONSTRAINT artifact_cache_index_sha256_check
    CHECK (checksum ~ '^[0-9a-fA-F]{64}$') NOT VALID;
CREATE INDEX IF NOT EXISTS idx_artifact_cache_index_lookup
    ON artifact_cache_index (artifact_key, last_used_at DESC NULLS LAST, created_at DESC);
"#;

/// V203 — One queryable attribution record for the complete work-item lifecycle.
pub const SCHEMA_V203_WORK_ITEM_PROVENANCE: &str = r#"
CREATE TABLE work_item_provenance (
    work_item_id UUID PRIMARY KEY REFERENCES work_items(id) ON DELETE CASCADE,
    builder_model TEXT,
    builder_computer TEXT,
    builder_port INTEGER,
    builder_lane TEXT CHECK (builder_lane IS NULL OR builder_lane IN ('local', 'cloud')),
    reviewer_model TEXT,
    reviewer_computer TEXT,
    reviewer_port INTEGER,
    reviewer_lane TEXT CHECK (reviewer_lane IS NULL OR reviewer_lane IN ('local', 'cloud')),
    confirmer_model TEXT,
    pr_url TEXT,
    pr_created_at TIMESTAMPTZ,
    pr_created_by TEXT,
    merged_by TEXT,
    merged_at TIMESTAMPTZ,
    cleanup_complete BOOLEAN NOT NULL DEFAULT FALSE,
    cleanup_at TIMESTAMPTZ,
    cleanup_detail JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_work_item_provenance_merged_at
    ON work_item_provenance (merged_at DESC) WHERE merged_at IS NOT NULL;
"#;

/// V204 — Correct work-item velocity instrumentation to use the authoritative
/// merge queue and lease lifecycle sources. V179–V182 introduced the event
/// log and initial views; this forward-only migration aligns them with the
/// operator-approved KPI definitions without rewriting those migrations.
pub const SCHEMA_V204_WORK_ITEM_VELOCITY_INSTRUMENTATION: &str = r#"
ALTER TABLE work_item_events
    ALTER COLUMN detail DROP DEFAULT,
    ALTER COLUMN detail DROP NOT NULL,
    ALTER COLUMN detail TYPE TEXT USING detail::text;

DROP VIEW IF EXISTS v_throughput_hourly;
DROP VIEW IF EXISTS v_lead_time_daily;
DROP VIEW IF EXISTS v_computer_builds_daily;
DROP VIEW IF EXISTS v_first_pass_rate_daily;

CREATE VIEW v_throughput_hourly AS
SELECT
    date_trunc('hour', merged_at) AS hour_bucket,
    COUNT(*)::BIGINT AS merge_count
FROM work_item_merge_queue
WHERE merged_at IS NOT NULL
GROUP BY 1;

CREATE VIEW v_lead_time_daily AS
SELECT
    date_trunc('day', merged_at) AS day_bucket,
    COUNT(*)::BIGINT AS merge_count,
    AVG(EXTRACT(EPOCH FROM (merged_at - enqueued_at)))::DOUBLE PRECISION
        AS avg_lead_time_seconds,
    percentile_cont(0.5) WITHIN GROUP (
        ORDER BY EXTRACT(EPOCH FROM (merged_at - enqueued_at))
    )::DOUBLE PRECISION AS p50_lead_time_seconds,
    percentile_cont(0.9) WITHIN GROUP (
        ORDER BY EXTRACT(EPOCH FROM (merged_at - enqueued_at))
    )::DOUBLE PRECISION AS p90_lead_time_seconds
FROM work_item_merge_queue
WHERE merged_at IS NOT NULL
GROUP BY 1;

CREATE VIEW v_computer_builds_daily AS
SELECT
    date_trunc('day', l.released_at) AS day_bucket,
    COALESCE(c.name, 'unknown') AS computer_name,
    COUNT(*)::BIGINT AS build_count,
    AVG(EXTRACT(EPOCH FROM (l.released_at - l.created_at)) / 60.0)::DOUBLE PRECISION
        AS avg_build_minutes
FROM work_item_leases l
LEFT JOIN computers c ON c.id = l.computer_id
WHERE l.released_at IS NOT NULL
GROUP BY 1, 2;

CREATE VIEW v_first_pass_rate_daily AS
SELECT
    date_trunc('day', q.merged_at) AS day_bucket,
    COUNT(*)::BIGINT AS merged_count,
    COUNT(*) FILTER (WHERE NOT EXISTS (
        SELECT 1
        FROM work_item_events e
        WHERE e.work_item_id = q.work_item_id
          AND (
              (e.to_status = 'ready' AND e.from_status NOT IN ('idea', 'ready'))
              OR COALESCE(e.detail, '') ~* '(heal|reset)'
          )
    ))::BIGINT AS first_pass_count,
    (COUNT(*) FILTER (WHERE NOT EXISTS (
        SELECT 1
        FROM work_item_events e
        WHERE e.work_item_id = q.work_item_id
          AND (
              (e.to_status = 'ready' AND e.from_status NOT IN ('idea', 'ready'))
              OR COALESCE(e.detail, '') ~* '(heal|reset)'
          )
    ))::DOUBLE PRECISION / NULLIF(COUNT(*), 0))::DOUBLE PRECISION AS first_pass_rate
FROM work_item_merge_queue q
WHERE q.merged_at IS NOT NULL
GROUP BY 1;
"#;

/// V205 — Desired generation for the DB-independent MCP bootstrap contract.
/// Installers can compare this value when reconciling client configurations.
pub const SCHEMA_V205_MCP_BOOTSTRAP_GENERATION: &str = r#"
INSERT INTO fleet_secrets (key, value, description)
VALUES (
    'mcp_bootstrap_generation',
    '2',
    'MCP clients use the DB-independent forgefleetd mcp --stdio bootstrap'
)
ON CONFLICT (key) DO UPDATE SET
    value = EXCLUDED.value,
    description = EXCLUDED.description,
    updated_at = NOW();
"#;

/// V206 — Request-level model endpoint metrics converted from llama-server logs.
pub const SCHEMA_V206_MODEL_ENDPOINT_METRICS: &str = r#"
ALTER TABLE deployment_metrics_scrapes
    ADD COLUMN IF NOT EXISTS endpoint TEXT NOT NULL DEFAULT 'all',
    ADD COLUMN IF NOT EXISTS requests_per_sec DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS batch_occupancy DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS avg_latency_ms DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS prompt_tokens_total DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS predicted_tokens_total DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS inference_seconds_total DOUBLE PRECISION;

CREATE INDEX IF NOT EXISTS idx_deployment_metrics_endpoint_time
    ON deployment_metrics_scrapes (deployment_id, endpoint, scraped_at DESC);

CREATE OR REPLACE VIEW model_capacity AS
SELECT
    d.worker_name AS computer, d.id AS deployment_id, d.catalog_id, d.runtime,
    d.port, d.context_window, s.tokens_per_sec, s.queue_depth, s.active_requests,
    s.scraped_at AS last_scraped_at,
    CASE WHEN s.scraped_at IS NULL OR s.scraped_at < NOW() - INTERVAL '90 seconds'
         THEN 'unknown' ELSE d.health_status END AS status
FROM fleet_model_deployments d
LEFT JOIN LATERAL (
    SELECT m.tokens_per_sec, m.queue_depth, m.active_requests, m.scraped_at
    FROM deployment_metrics_scrapes m
    WHERE m.deployment_id = d.id AND m.endpoint = 'all'
    ORDER BY m.scraped_at DESC, m.id DESC LIMIT 1
) s ON TRUE;
"#;

/// V207 — Ensure merge-queue review ownership, outcome, and latency fields.
pub const SCHEMA_V207_MERGE_QUEUE_REVIEW_TRACKING: &str = r#"
ALTER TABLE work_item_merge_queue
    ADD COLUMN IF NOT EXISTS reviewer_computer   TEXT,
    ADD COLUMN IF NOT EXISTS review_claimed_at   TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS review_verdict      TEXT,
    ADD COLUMN IF NOT EXISTS review_reason       TEXT,
    ADD COLUMN IF NOT EXISTS review_started_at   TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS review_completed_at TIMESTAMPTZ;
"#;

/// V208 — Store parked work-item state explicitly.
pub const SCHEMA_V208_WORK_ITEMS_PARKED: &str = r#"
ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS parked BOOLEAN NOT NULL DEFAULT false;
"#;

/// V209 — Pollable iCalendar feeds and exactly-once event actions.
pub const SCHEMA_V209_CALENDAR_MONITORING: &str = r#"
CREATE TABLE IF NOT EXISTS calendar_monitors (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id            TEXT NOT NULL,
    name                  TEXT NOT NULL,
    feed_url              TEXT NOT NULL,
    task_template         JSONB NOT NULL DEFAULT '{}'::jsonb,
    lead_time_minutes     INTEGER NOT NULL DEFAULT 15 CHECK (lead_time_minutes >= 0),
    poll_interval_minutes INTEGER NOT NULL DEFAULT 5 CHECK (poll_interval_minutes > 0),
    enabled               BOOLEAN NOT NULL DEFAULT true,
    next_poll_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_polled_at        TIMESTAMPTZ,
    last_error            TEXT,
    etag                  TEXT,
    last_modified         TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (project_id, name)
);

CREATE INDEX IF NOT EXISTS idx_calendar_monitors_due
    ON calendar_monitors (next_poll_at) WHERE enabled;

CREATE TABLE IF NOT EXISTS calendar_event_actions (
    monitor_id UUID NOT NULL REFERENCES calendar_monitors(id) ON DELETE CASCADE,
    event_uid  TEXT NOT NULL,
    event_start TIMESTAMPTZ NOT NULL,
    task_id    UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (monitor_id, event_uid, event_start)
);
"#;

/// V210 — Unified capacity registry over inference, build, and cloud pools.
pub const SCHEMA_V210_FLEET_CAPACITY_REGISTRY_VIEW: &str = r#"
CREATE TABLE IF NOT EXISTS cloud_budget_buckets (
    provider       TEXT PRIMARY KEY,
    max_concurrent INT NOT NULL,
    tokens_per_min BIGINT,
    spent_today    NUMERIC NOT NULL DEFAULT 0,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO cloud_budget_buckets (provider, max_concurrent, tokens_per_min)
VALUES
    ('claude', 3, 10000),
    ('codex',  3, 10000),
    ('kimi',   3, 10000)
ON CONFLICT (provider) DO NOTHING;

CREATE OR REPLACE VIEW v_fleet_capacity AS
SELECT
    'inference'::TEXT AS kind,
    d.catalog_id,
    d.worker_name AS worker,
    d.port,
    d.parallel_slots,
    d.health_status AS health,
    NULL::INT AS max_concurrent,
    NULL::BIGINT AS tokens_per_min,
    NULL::NUMERIC AS spent_today
FROM fleet_model_deployments d
JOIN fleet_model_catalog c ON c.id = d.catalog_id

UNION ALL

SELECT
    'build'::TEXT AS kind,
    NULL::TEXT AS catalog_id,
    c.name AS worker,
    NULL::INT AS port,
    GREATEST(0, fw.sub_agent_count - COALESCE(l.active_count, 0)) AS parallel_slots,
    fw.status AS health,
    fw.sub_agent_count AS max_concurrent,
    NULL::BIGINT AS tokens_per_min,
    NULL::NUMERIC AS spent_today
FROM fleet_workers fw
JOIN computers c ON c.name = fw.name
LEFT JOIN (
    SELECT computer_id, COUNT(*) AS active_count
    FROM work_item_leases
    WHERE released_at IS NULL
    GROUP BY computer_id
) l ON l.computer_id = c.id

UNION ALL

SELECT
    'cloud'::TEXT AS kind,
    NULL::TEXT AS catalog_id,
    b.provider AS worker,
    NULL::INT AS port,
    b.max_concurrent AS parallel_slots,
    CASE
        WHEN b.tokens_per_min IS NULL OR b.tokens_per_min <= 0 THEN 'healthy'
        WHEN b.spent_today < b.tokens_per_min THEN 'healthy'
        ELSE 'degraded'
    END AS health,
    b.max_concurrent,
    b.tokens_per_min,
    b.spent_today
FROM cloud_budget_buckets b;
"#;

/// V211 — Make venkatyarl the sole permanent fleet GitHub identity and remove
/// the retired taylor-oclaw credentials from the fleet-wide registry.
pub const SCHEMA_V211_DECOMMISSION_TAYLOR_GITHUB_IDENTITY: &str = r#"
UPDATE github_ssh_aliases
SET is_canonical = (alias_name = 'github.com-venkat'),
    description = CASE
        WHEN alias_name = 'github.com-venkat'
            THEN 'Permanent venkatyarl identity — canonical fleet GitHub account'
        ELSE description
    END,
    updated_at = NOW()
WHERE hostname = 'github.com';

DELETE FROM github_ssh_aliases
WHERE alias_name = 'github.com-taylor';

DELETE FROM fleet_secrets
WHERE key IN ('github_ssh_id_taylor_priv', 'github_ssh_id_taylor_pub');
"#;

/// V212 — One bounded read interface across raw, hourly, and daily computer
/// metrics. Consumers can use retained history without knowing tier cutovers.
pub const SCHEMA_V212_COMPUTER_METRICS_RETAINED_VIEW: &str = r#"
CREATE OR REPLACE VIEW computer_metrics_history_retained AS
SELECT computer_id, recorded_at, 'raw'::TEXT AS resolution, 1::BIGINT AS sample_count,
       cpu_pct, ram_pct, ram_used_gb, disk_free_gb, gpu_pct,
       llm_ram_allocated_gb, llm_queue_depth::DOUBLE PRECISION AS llm_queue_depth,
       llm_active_requests::DOUBLE PRECISION AS llm_active_requests,
       llm_tokens_per_sec
  FROM computer_metrics_history
UNION ALL
SELECT computer_id, recorded_at, 'hourly'::TEXT, sample_count,
       cpu_pct, ram_pct, ram_used_gb, disk_free_gb, gpu_pct,
       llm_ram_allocated_gb, llm_queue_depth, llm_active_requests,
       llm_tokens_per_sec
  FROM computer_metrics_history_hourly
UNION ALL
SELECT computer_id, recorded_at, 'daily'::TEXT, sample_count,
       cpu_pct, ram_pct, ram_used_gb, disk_free_gb, gpu_pct,
       llm_ram_allocated_gb, llm_queue_depth, llm_active_requests,
       llm_tokens_per_sec
  FROM computer_metrics_history_daily;
"#;

/// V213 — Canonically configure v161 as the fresh-database bootstrap baseline.
pub const SCHEMA_V213_BOOTSTRAP_V161_BASELINE: &str = r#"
INSERT INTO _migration_baselines (generation, migration_version, name)
VALUES (1, 161, 'bootstrap_v161')
ON CONFLICT (generation) DO UPDATE
SET migration_version = EXCLUDED.migration_version,
    name = EXCLUDED.name;
"#;

/// V214 — Retain self-heal bug identities after ephemeral task pruning.
pub const SCHEMA_V214_SELF_HEAL_BUG_HISTORY: &str = r#"
CREATE TABLE IF NOT EXISTS self_heal_bug_history (
    bug_signature  TEXT PRIMARY KEY,
    last_task_id   UUID NOT NULL,
    last_status    TEXT NOT NULL,
    completed_at   TIMESTAMPTZ,
    last_seen_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_self_heal_bug_history_completed
    ON self_heal_bug_history(completed_at DESC);
"#;

// V215 — make daemon-computed sub-agent capacity an invariant at release.
// Capacity reconciliation never disables a busy excess slot. Once that slot
// finishes, this trigger converts the normal `idle` release into `disabled`,
// preventing it from immediately claiming another build. Growing capacity is
// unaffected: the daemon raises fleet_workers.sub_agent_count before it
// re-enables the newly in-range rows.
pub const SCHEMA_V215_SUB_AGENT_CAPACITY_BOUNDARY: &str = r#"
CREATE OR REPLACE FUNCTION enforce_sub_agent_capacity_boundary()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status = 'idle' AND EXISTS (
        SELECT 1
          FROM computers c
          JOIN fleet_workers fw ON LOWER(fw.name) = LOWER(c.name)
         WHERE c.id = NEW.computer_id
           AND NEW.slot >= GREATEST(COALESCE(fw.sub_agent_count, 1), 1)
    ) THEN
        NEW.status := 'disabled';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_sub_agent_capacity_boundary ON sub_agents;
CREATE TRIGGER trg_sub_agent_capacity_boundary
BEFORE INSERT OR UPDATE OF status, slot, computer_id ON sub_agents
FOR EACH ROW EXECUTE FUNCTION enforce_sub_agent_capacity_boundary();

UPDATE sub_agents sa
   SET status = 'disabled'
  FROM computers c
  JOIN fleet_workers fw ON LOWER(fw.name) = LOWER(c.name)
 WHERE sa.computer_id = c.id
   AND sa.slot >= GREATEST(COALESCE(fw.sub_agent_count, 1), 1)
   AND sa.status = 'idle'
   AND sa.current_work_item_id IS NULL;
"#;

/// V216 — retain both diagnostics produced by the full-mesh probe. SSH remains
/// authoritative in `status`; ICMP is diagnostic because healthy hosts may drop it.
pub const SCHEMA_V216_MESH_PROBE_DIAGNOSTICS: &str = r#"
ALTER TABLE fleet_mesh_status
    ADD COLUMN IF NOT EXISTS ping_ok BOOLEAN,
    ADD COLUMN IF NOT EXISTS ssh_ok BOOLEAN;
"#;

/// V217 — durable, lease-coordinated Jira monitor state.
pub const SCHEMA_V217_JIRA_MONITORING: &str = r#"
CREATE TABLE IF NOT EXISTS jira_rulesets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    version INTEGER NOT NULL,
    rules_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    content_hash TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    UNIQUE (name, version)
);

CREATE TABLE IF NOT EXISTS jira_configs (
    name TEXT PRIMARY KEY,
    project_key TEXT NOT NULL,
    owner_account_id TEXT NOT NULL,
    jira_secret_ref TEXT NOT NULL,
    poll_interval_s INTEGER NOT NULL DEFAULT 300 CHECK (poll_interval_s > 0),
    retag_after_s INTEGER NOT NULL DEFAULT 86400 CHECK (retag_after_s > 0),
    queue_jql TEXT NOT NULL,
    ruleset_id TEXT NOT NULL REFERENCES jira_rulesets(id),
    label_policy_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    transition_policy_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    repo_map_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    cwd_path_globs TEXT[] NOT NULL DEFAULT '{}',
    version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0)
);

CREATE TABLE IF NOT EXISTS jira_monitor_leases (
    config_id TEXT PRIMARY KEY REFERENCES jira_configs(name) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    lease_token UUID NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL,
    lease_until TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS jira_issue_leases (
    config_id TEXT NOT NULL REFERENCES jira_configs(name) ON DELETE CASCADE,
    issue_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    lease_token UUID NOT NULL,
    branch TEXT,
    repo TEXT,
    heartbeat_at TIMESTAMPTZ NOT NULL,
    lease_until TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (config_id, issue_id)
);

CREATE TABLE IF NOT EXISTS jira_watch_state (
    config_id TEXT NOT NULL REFERENCES jira_configs(name) ON DELETE CASCADE,
    issue_id TEXT NOT NULL,
    last_seen_comment_id TEXT,
    last_seen_comment_created_at TIMESTAMPTZ,
    last_seen_status TEXT,
    last_seen_assignee_id TEXT,
    awaiting_party TEXT,
    awaiting_since TIMESTAMPTZ,
    last_retag_at TIMESTAMPTZ,
    next_action_at TIMESTAMPTZ,
    active_work_lease_id UUID,
    state_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (config_id, issue_id)
);

CREATE TABLE IF NOT EXISTS jira_action_log (
    id BIGSERIAL PRIMARY KEY,
    event_key TEXT NOT NULL UNIQUE,
    config_id TEXT NOT NULL REFERENCES jira_configs(name) ON DELETE CASCADE,
    issue_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_jira_watch_due ON jira_watch_state(config_id, next_action_at);
CREATE INDEX IF NOT EXISTS idx_jira_issue_leases_until ON jira_issue_leases(config_id, lease_until);

INSERT INTO jira_rulesets (id, name, version, rules_json, content_hash, active)
VALUES (
    'hireflow360-v1', 'hireflow360', 1,
    jsonb_build_object(
      'instructions_secret_ref', 'jira.hireflow360.instructions',
      'queue_policy', ARRAY['Blocker summary','Priority summary','reopen','reporter reply','Jira priority','oldest assigned bug','other'],
      'scope', 'assignee=currentUser() AND statusCategory != Done',
      'repo_name_labels', ARRAY['hireflow360','hireflow360-api','hireflow360-web']
    ),
    md5('jira.hireflow360.instructions:v1'), true
)
ON CONFLICT (id) DO NOTHING;

INSERT INTO jira_configs (
    name, project_key, owner_account_id, jira_secret_ref, poll_interval_s,
    retag_after_s, queue_jql, ruleset_id, label_policy_json,
    transition_policy_json, repo_map_json, cwd_path_globs, version
) VALUES (
    'hireflow360', 'HFPROD', 'venkat@hireflow360.com',
    'hireflow360_jira_api_token', 300, 86400,
    'project = HFPROD AND assignee = currentUser() AND statusCategory != Done',
    'hireflow360-v1',
    '{"product_label":"hireflow360","labels_from_repos":true}'::jsonb,
    '{"todo":"11","in_progress":"21","review":"31","done":"41"}'::jsonb,
    '{"hireflow360":"hireflow360","hireflow360-api":"hireflow360-api","hireflow360-web":"hireflow360-web"}'::jsonb,
    ARRAY['**/projects/HireFlow360/**'], 1
)
ON CONFLICT (name) DO NOTHING;
"#;

/// Model-facing fabric-pair fields derived from the existing topology rows.
pub const SCHEMA_V218_FABRIC_PAIR_MODEL_COLUMNS: &str = r#"
ALTER TABLE fabric_pairs
    ADD COLUMN IF NOT EXISTS source_node TEXT
        GENERATED ALWAYS AS (split_part(pair_name, '-', 1)) STORED,
    ADD COLUMN IF NOT EXISTS target_node TEXT
        GENERATED ALWAYS AS (split_part(pair_name, '-', 2)) STORED,
    ADD COLUMN IF NOT EXISTS cidr TEXT
        GENERATED ALWAYS AS (set_masklen(a_ip::inet, 30)::cidr::text) STORED,
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'pending',
    ADD COLUMN IF NOT EXISTS verified BOOLEAN NOT NULL DEFAULT FALSE;
"#;

/// Latest health result for each local SLM endpoint.
pub const SCHEMA_V219_SLM_HEALTH_MONITOR: &str = r#"
CREATE TABLE IF NOT EXISTS slm_health_status (
    computer_id UUID NOT NULL,
    endpoint TEXT NOT NULL,
    healthy BOOLEAN NOT NULL,
    checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    latency_ms BIGINT NOT NULL,
    error TEXT,
    PRIMARY KEY (computer_id, endpoint)
);

CREATE INDEX IF NOT EXISTS idx_slm_health_status_checked_at
    ON slm_health_status (checked_at DESC);
"#;

/// Structured lifecycle and verification state for autonomously detected work.
pub const SCHEMA_V220_AUTONOMOUS_WORK_ITEM_LOOP: &str = r#"
ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS pre_work JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN IF NOT EXISTS work JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN IF NOT EXISTS post_work JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN IF NOT EXISTS cleanup_complete BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS original_signal JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN IF NOT EXISTS signal_cleared BOOLEAN,
    ADD COLUMN IF NOT EXISTS signal_verified_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS refiled_from UUID REFERENCES work_items(id) ON DELETE SET NULL;

ALTER TABLE work_items
    ADD CONSTRAINT work_items_pre_work_array CHECK (jsonb_typeof(pre_work) = 'array'),
    ADD CONSTRAINT work_items_work_array CHECK (jsonb_typeof(work) = 'array'),
    ADD CONSTRAINT work_items_post_work_array CHECK (jsonb_typeof(post_work) = 'array'),
    ADD CONSTRAINT work_items_original_signal_object CHECK (jsonb_typeof(original_signal) = 'object');

CREATE INDEX IF NOT EXISTS idx_work_items_open_signal
    ON work_items ((original_signal->>'signature'))
    WHERE signal_cleared IS NOT TRUE;
"#;

/// Latest bounded dependency probe reported by each fleet computer.
pub const SCHEMA_V221_SERVICE_CONNECTIVITY_STATUS: &str = r#"
CREATE TABLE IF NOT EXISTS service_connectivity_status (
    computer_id UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('healthy', 'unavailable', 'unconfigured')),
    latency_ms BIGINT,
    checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (computer_id, service)
);

CREATE INDEX IF NOT EXISTS idx_service_connectivity_status_checked_at
    ON service_connectivity_status (checked_at DESC);
"#;

// ─── V222: retire code-review-graph after the Cortex migration ─────────────
//
// Repository hooks and MCP configuration now use Cortex exclusively. Remove
// the legacy catalog entry so fleet-wide tool drift and install workflows do
// not continue provisioning code-review-graph. The external-tools foreign key
// cascades this deletion to per-computer install state.
pub const SCHEMA_V222_RETIRE_CODE_REVIEW_GRAPH: &str = r#"
DELETE FROM external_tools WHERE id = 'code-review-graph';
"#;

/// Real, sized local model choices and explicit placement constraints for
/// models that cannot fit on a single 122-123 GB unified-memory node.
pub const SCHEMA_V223_REAL_SIZED_MODEL_CATALOG: &str = r#"
INSERT INTO fleet_model_catalog
    (id, name, family, parameters, tier, description, gated,
     preferred_workloads, variants, tool_calling)
VALUES
  ('qwen3-4b-instruct-2507', 'Qwen3-4B-Instruct-2507', 'qwen', '4B', 1,
   '2.5 GB SLM floor for fast local instruction following.', false,
   '["chat","slm","tool_calling"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"Q4_K_M","hf_repo":"Qwen/Qwen3-4B-Instruct-2507-GGUF","size_gb":2.5}]'::jsonb, true),
  ('qwen3-coder-30b', 'Qwen3-Coder-30B-A3B-Instruct', 'qwen', '30B-A3B', 2,
   '17.7 GB local coding workhorse (3B active MoE).', false,
   '["code","coding","reasoning","tool_calling"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"Q4_K_M","hf_repo":"Qwen/Qwen3-Coder-30B-A3B-Instruct-GGUF","size_gb":17.7}]'::jsonb, true),
  ('gpt-oss-20b', 'gpt-oss-20b', 'gpt-oss', '20B', 2,
   '14 GB agentic floor for local tool-using workloads.', false,
   '["agentic","code","reasoning","tool_calling"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"MXFP4","hf_repo":"openai/gpt-oss-20b","size_gb":14.0}]'::jsonb, true),
  ('gpt-oss-120b', 'gpt-oss-120b', 'gpt-oss', '120B', 3,
   '58 GB large local agentic model.', false,
   '["agentic","code","reasoning","tool_calling"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"MXFP4","hf_repo":"openai/gpt-oss-120b","size_gb":58.0}]'::jsonb, true),
  ('glm-4.5-air', 'GLM-4.5-Air', 'glm', '106B-A12B', 3,
   'Approximately 65 GB; local sweet spot for 122 GB unified-memory nodes.', false,
   '["agentic","code","reasoning","tool_calling"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"Q4_K_M","hf_repo":"zai-org/GLM-4.5-Air-GGUF","size_gb":65.0}]'::jsonb, true),
  ('glm-5.2', 'GLM-5.2', 'glm', 'watch', 4,
   'WATCH/ADOPT candidate; size and deployable artifact must be verified before placement.', true,
   '["watch","adopt","reasoning","code"]'::jsonb, '[]'::jsonb, true),
  ('deepseek-v4-flash', 'DeepSeek-V4-Flash', 'deepseek', 'watch', 4,
   'WATCH/ADOPT candidate; size and deployable artifact must be verified before placement.', true,
   '["watch","adopt","reasoning","code"]'::jsonb, '[]'::jsonb, true),
  ('kimi-k2-thinking', 'Kimi-K2-Thinking', 'kimi', '1T-A32B', 4,
   'OFFLOAD-ONLY or multi-node-ring-only: 247 GB at 1.8-bit exceeds every single 122-123 GB node.', false,
   '["reasoning","architecture","offload_only","multi_node_ring_only"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"UD-TQ1_0","hf_repo":"unsloth/Kimi-K2-Thinking-GGUF","size_gb":247.0,"placement":"offload_or_multi_node_ring"}]'::jsonb, true),
  ('kimi-k3', 'Kimi-K3', 'kimi', '2.8T', 4,
   'OFFLOAD-ONLY or multi-node-ring-only: 2.8T parameters cannot fit on a single fleet node.', true,
   '["reasoning","architecture","offload_only","multi_node_ring_only","watch","adopt"]'::jsonb,
   '[]'::jsonb, true),
  ('qwen3-coder-480b', 'Qwen3-Coder-480B-A35B-Instruct', 'qwen', '480B-A35B', 4,
   'OFFLOAD-ONLY or multi-node-ring-only: 180 GB Q2 exceeds every single 122-123 GB node.', false,
   '["coding","agentic","tool_calling","offload_only","multi_node_ring_only"]'::jsonb,
   '[{"runtime":"llama.cpp","quant":"Q2_K","hf_repo":"unsloth/Qwen3-Coder-480B-A35B-Instruct-GGUF","size_gb":180.0,"placement":"offload_or_multi_node_ring"}]'::jsonb, true)
ON CONFLICT (id) DO UPDATE SET
  name = EXCLUDED.name,
  family = EXCLUDED.family,
  parameters = EXCLUDED.parameters,
  tier = EXCLUDED.tier,
  description = EXCLUDED.description,
  gated = EXCLUDED.gated,
  preferred_workloads = EXCLUDED.preferred_workloads,
  variants = EXCLUDED.variants,
  tool_calling = EXCLUDED.tool_calling,
  updated_at = NOW();
"#;

/// Current operator-console cloud budget windows and hard-stop policy.
pub const SCHEMA_V224_CLOUD_BUDGET_BUCKET_SEEDS: &str = r#"
CREATE TABLE IF NOT EXISTS cloud_budget_buckets (
    provider                TEXT PRIMARY KEY,
    window_exhausted_until  TIMESTAMPTZ,
    weekly_pct              SMALLINT,
    weekly_reset_at         TIMESTAMPTZ,
    monthly_pct             SMALLINT,
    monthly_reset_at        TIMESTAMPTZ,
    credit_pool_spent_usd   NUMERIC DEFAULT 0,
    last_error_at           TIMESTAMPTZ,
    last_success_at         TIMESTAMPTZ,
    source                  TEXT,
    updated_at              TIMESTAMPTZ DEFAULT NOW()
);

INSERT INTO cloud_budget_buckets (
    provider,
    window_exhausted_until,
    weekly_pct,
    weekly_reset_at,
    monthly_pct,
    monthly_reset_at,
    credit_pool_spent_usd,
    last_success_at,
    source,
    updated_at
) VALUES
    (
        'kimi',
        NULL,
        64,
        '2026-07-21 16:23:00-04'::timestamptz,
        19,
        '2026-08-03 00:00:00-04'::timestamptz,
        0,
        '2026-07-20 05:23:00-04'::timestamptz,
        'operator console 2026-07-20 05:23 ET; 5h 0.97% used, just reset and healthy; 7day 64.12% used; monthly 19.18% used; extra-usage disabled (hard stop)',
        NOW()
    ),
    (
        'codex',
        NULL,
        16,
        '2026-07-24 23:30:00-04'::timestamptz,
        NULL,
        NULL,
        0,
        '2026-07-20 05:23:00-04'::timestamptz,
        'operator console 2026-07-20 05:2x ET; OpenAI shared Codex/Work/Agents weekly 16% used (84% remaining); rolling 5h window recovered',
        NOW()
    ),
    (
        'claude',
        NULL,
        66,
        '2026-07-23 02:00:00-04'::timestamptz,
        NULL,
        NULL,
        0,
        NULL,
        'operator console 2026-07-20 05:2x ET; Anthropic session 20% used, resets in ~2h; all-models weekly 66% used, resets Thu 02:00 ET; automation uses Sonnet; Fable tier 100% in this operator session on credits; Claude Code +50% boost through 2026-08-19; usage credits on (overflow bills, no hard block)',
        NOW()
    )
ON CONFLICT (provider) DO UPDATE SET
    window_exhausted_until = EXCLUDED.window_exhausted_until,
    weekly_pct = EXCLUDED.weekly_pct,
    weekly_reset_at = EXCLUDED.weekly_reset_at,
    monthly_pct = EXCLUDED.monthly_pct,
    monthly_reset_at = EXCLUDED.monthly_reset_at,
    credit_pool_spent_usd = EXCLUDED.credit_pool_spent_usd,
    last_success_at = EXCLUDED.last_success_at,
    source = EXCLUDED.source,
    updated_at = NOW();
"#;

/// Make fleet leadership a movable control-plane lease.  Redis and NATS move
/// with the leader and are intentionally stored on the lease rather than on
/// the Postgres-primary record.
pub const SCHEMA_V225_MOVABLE_LEADER_LEASE: &str = r#"
ALTER TABLE fleet_leader_state
    ADD COLUMN IF NOT EXISTS redis_url TEXT,
    ADD COLUMN IF NOT EXISTS nats_url TEXT;
"#;

/// Keep the legacy fleet roster projection aligned with its canonical sources.
/// `computers` owns connection identity; enabled `sub_agents` rows own worker
/// capacity. Slot 99 is the per-host manager and is not dispatch capacity.
pub const SCHEMA_V226_REGISTRY_HYGIENE: &str = r#"
UPDATE fleet_workers fw
   SET ip = c.primary_ip,
       ssh_user = c.ssh_user,
       updated_at = NOW()
  FROM computers c
 WHERE LOWER(c.name) = LOWER(fw.name)
   AND (fw.ip IS DISTINCT FROM c.primary_ip
        OR fw.ssh_user IS DISTINCT FROM c.ssh_user);

CREATE OR REPLACE FUNCTION sync_fleet_worker_connection_identity()
RETURNS TRIGGER AS $$
BEGIN
    UPDATE fleet_workers
       SET ip = NEW.primary_ip,
           ssh_user = NEW.ssh_user,
           updated_at = NOW()
     WHERE LOWER(name) = LOWER(NEW.name)
       AND (ip IS DISTINCT FROM NEW.primary_ip
            OR ssh_user IS DISTINCT FROM NEW.ssh_user);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_sync_fleet_worker_connection_identity ON computers;
CREATE TRIGGER trg_sync_fleet_worker_connection_identity
AFTER INSERT OR UPDATE OF name, primary_ip, ssh_user ON computers
FOR EACH ROW EXECUTE FUNCTION sync_fleet_worker_connection_identity();

UPDATE fleet_workers fw
   SET sub_agent_count = (
       SELECT COUNT(*)::integer
         FROM computers c
         JOIN sub_agents sa ON sa.computer_id = c.id
        WHERE LOWER(c.name) = LOWER(fw.name)
          AND sa.slot BETWEEN 0 AND 98
          AND sa.status <> 'disabled'
   ),
       updated_at = NOW();

CREATE OR REPLACE FUNCTION sync_fleet_worker_sub_agent_count()
RETURNS TRIGGER AS $$
DECLARE
    old_delta integer := 0;
    new_delta integer := 0;
BEGIN
    IF TG_OP <> 'INSERT'
       AND OLD.slot BETWEEN 0 AND 98
       AND OLD.status <> 'disabled' THEN
        old_delta := 1;
    END IF;
    IF TG_OP <> 'DELETE'
       AND NEW.slot BETWEEN 0 AND 98
       AND NEW.status <> 'disabled' THEN
        new_delta := 1;
    END IF;

    IF TG_OP <> 'INSERT' AND old_delta <> 0 THEN
        UPDATE fleet_workers fw
           SET sub_agent_count = GREATEST(fw.sub_agent_count - old_delta, 0),
               updated_at = NOW()
          FROM computers c
         WHERE c.id = OLD.computer_id
           AND LOWER(fw.name) = LOWER(c.name);
    END IF;
    IF TG_OP <> 'DELETE' AND new_delta <> 0 THEN
        UPDATE fleet_workers fw
           SET sub_agent_count = fw.sub_agent_count + new_delta,
               updated_at = NOW()
          FROM computers c
         WHERE c.id = NEW.computer_id
           AND LOWER(fw.name) = LOWER(c.name);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_sync_fleet_worker_sub_agent_count ON sub_agents;
CREATE TRIGGER trg_sync_fleet_worker_sub_agent_count
AFTER INSERT OR DELETE OR UPDATE OF computer_id, slot, status ON sub_agents
FOR EACH ROW EXECUTE FUNCTION sync_fleet_worker_sub_agent_count();
"#;

/// Squashed Postgres bootstrap through migration v161.
///
/// The incremental 7→161 migration chain cannot replay cleanly on a fresh empty
/// Postgres due to accumulated rename/renumber drift. On a brand-new DB the
/// migration runner applies this single idempotent baseline instead of the
/// legacy chain, then continues with any migrations after v161.
pub const BOOTSTRAP_V161_SQL: &str = include_str!("migrations/v161_bootstrap_baseline.sql");
