-- ForgeFleet Postgres bootstrap squashed through migration v161.
-- Creates the v161 schema on an empty database and marks v7..v161 applied.

CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS amcheck;


-- V7: fleet_config_tables

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


-- V8: task_provenance_schema

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


-- V9: fleet_secrets

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


-- V10: deferred_tasks

-- ─── Deferred Task Queue ──────────────────────────────────────────────────
-- Persistent queue for work that can't run right now (offline node, future time,
-- event trigger). Leader schedules, any daemon can worker-claim via SKIP LOCKED.
CREATE TABLE IF NOT EXISTS deferred_tasks_legacy (
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
    ON deferred_tasks_legacy (status, next_attempt_at);
CREATE INDEX IF NOT EXISTS idx_deferred_tasks_preferred_node
    ON deferred_tasks_legacy (preferred_node) WHERE status IN ('pending', 'dispatchable');
CREATE INDEX IF NOT EXISTS idx_deferred_tasks_trigger
    ON deferred_tasks_legacy (trigger_type) WHERE status = 'pending';


-- V11: model_lifecycle

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


-- V12: onboarding_foundation

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


-- V13: virtual_brain

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


-- V14: computers_and_portfolio

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


-- V15: project_management

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
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_repos_primary
    ON project_repos (project_id) WHERE is_primary;


-- Canonical project-management/distributed-build work items (V15 + V140 + V152).
CREATE TABLE IF NOT EXISTS work_items (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id            TEXT NOT NULL REFERENCES projects(id),
    milestone_id          UUID REFERENCES milestones(id),
    parent_id             UUID REFERENCES work_items(id),
    kind                  TEXT NOT NULL,
    title                 TEXT NOT NULL,
    description           TEXT,
    labels                JSONB NOT NULL DEFAULT '[]'::jsonb,
    status                TEXT NOT NULL DEFAULT 'idea',
    priority              TEXT NOT NULL DEFAULT 'normal',
    assigned_to           TEXT,
    assigned_computer     TEXT,
    branch_name           TEXT,
    pr_url                TEXT,
    brain_node_ids        JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by            TEXT NOT NULL DEFAULT 'system',
    started_at            TIMESTAMPTZ,
    completed_at          TIMESTAMPTZ,
    due_date              DATE,
    estimated_hours       DOUBLE PRECISION,
    metadata              JSONB NOT NULL DEFAULT '{}'::jsonb,
    required_capabilities JSONB NOT NULL DEFAULT '[]'::jsonb,
    complexity            TEXT NOT NULL DEFAULT 'mechanical',
    predicted_paths       JSONB NOT NULL DEFAULT '[]'::jsonb,
    touched_paths         JSONB NOT NULL DEFAULT '[]'::jsonb,
    base_branch           TEXT,
    base_sha              TEXT,
    integration_branch    TEXT,
    merge_rank            INT,
    risk_score            REAL NOT NULL DEFAULT 0,
    reviewer_required     BOOLEAN NOT NULL DEFAULT TRUE,
    attempts              INT NOT NULL DEFAULT 0,
    last_error            TEXT,
    repo_id               UUID REFERENCES project_repos(id) ON DELETE SET NULL,
    repo_url              TEXT,
    repo_path             TEXT
);
CREATE INDEX IF NOT EXISTS idx_work_items_status ON work_items (status);
CREATE INDEX IF NOT EXISTS idx_work_items_parent ON work_items (parent_id);
CREATE INDEX IF NOT EXISTS idx_work_items_repo_id ON work_items (repo_id);
CREATE INDEX IF NOT EXISTS idx_work_items_repo_url ON work_items (repo_url);

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


-- V16: observability

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


-- V17: security_hardening

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


-- V18: network_scope

ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS network_scope TEXT NOT NULL DEFAULT 'lan';


-- V19: storage_power_training

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


-- V20: port_registry

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


-- V21: drop_deployment_model_fk

ALTER TABLE computer_model_deployments
    DROP CONSTRAINT IF EXISTS computer_model_deployments_model_id_fkey;


-- V22: drop_model_presence_fk

ALTER TABLE computer_models
    DROP CONSTRAINT IF EXISTS computer_models_model_id_fkey;


-- V23: sub_agents

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


-- V24: external_tools

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


-- V25: social_media_ingest

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


-- V26: cloud_llm_providers

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


-- V27: pool_aliases

ALTER TABLE fleet_task_coverage
    ADD COLUMN IF NOT EXISTS alias TEXT UNIQUE;

CREATE INDEX IF NOT EXISTS fleet_task_coverage_alias_idx
    ON fleet_task_coverage(alias);


-- V28: software_registry_seed

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


-- V29: fix_ff_git_linux_playbook

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


-- V30: playbook_self_heal_repo

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


-- V31: source_tree_path

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


-- V32: playbook_bugfixes

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


-- V33: cli_aliases

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


-- V34: retire_alert_policies_toml

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


-- V35: retire_cloud_llm_providers_toml

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


-- V36: retire_task_coverage_toml

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


-- V37: retire_ports_toml

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


-- V38: retire_external_tools_toml

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


-- V39: retire_model_catalog_toml

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


-- V40: agent_session_on_work_outputs

ALTER TABLE work_outputs
    ADD COLUMN IF NOT EXISTS agent_session_id TEXT,
    ADD COLUMN IF NOT EXISTS modified_files JSONB NOT NULL DEFAULT '[]';

CREATE INDEX IF NOT EXISTS idx_work_outputs_by_session
    ON work_outputs(agent_session_id)
    WHERE agent_session_id IS NOT NULL;


-- V41: per_arch_build_leader

ALTER TABLE computers ADD COLUMN IF NOT EXISTS build_archs JSONB NOT NULL DEFAULT '[]'::jsonb;

UPDATE computers SET build_archs = '["darwin-aarch64"]'::jsonb
 WHERE LOWER(name) = 'taylor' AND build_archs = '[]'::jsonb;

UPDATE computers SET build_archs = '["linux-x86_64"]'::jsonb
 WHERE LOWER(name) = 'sophie' AND build_archs = '[]'::jsonb;

UPDATE computers SET build_archs = '["linux-aarch64"]'::jsonb
 WHERE LOWER(name) = 'sia' AND build_archs = '[]'::jsonb;

CREATE INDEX IF NOT EXISTS computers_build_archs_idx
  ON computers USING GIN (build_archs);


-- V42: research_subsystem

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

CREATE TABLE IF NOT EXISTS research_subtasks_legacy (
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
    ON research_subtasks_legacy(session_id, ordinal);
CREATE INDEX IF NOT EXISTS idx_research_subtasks_by_computer
    ON research_subtasks_legacy(assigned_computer, status);

CREATE TABLE IF NOT EXISTS research_findings (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id         UUID NOT NULL REFERENCES research_sessions(id) ON DELETE CASCADE,
    subtask_id         UUID REFERENCES research_subtasks_legacy(id) ON DELETE SET NULL,
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


-- V43: multi_host_and_self_heal

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

CREATE TABLE IF NOT EXISTS fleet_self_heal_queue_legacy (
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
    ON fleet_self_heal_queue_legacy(status, tier, created_at);

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
    bug_signature            TEXT NOT NULL REFERENCES fleet_self_heal_queue_legacy(bug_signature) ON DELETE CASCADE,
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


-- V44: fleet_tasks

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


-- V45: beat_age_alerts

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


-- V46: npm_cli_catalog

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


-- V47: fabric_measurements_and_docker

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


-- V48: upgrade_playbook_restart_fix

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


-- V49: connectivity_mode_and_eligibility

ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS connectivity_mode TEXT,
    ADD COLUMN IF NOT EXISTS election_eligibility TEXT NOT NULL DEFAULT 'eligible';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'computers_connectivity_mode_check'
    ) THEN
        ALTER TABLE computers
            ADD CONSTRAINT computers_connectivity_mode_check
            CHECK (connectivity_mode IS NULL
                   OR connectivity_mode IN ('lan_attached', 'roaming', 'island'));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'computers_eligibility_check'
    ) THEN
        ALTER TABLE computers
            ADD CONSTRAINT computers_eligibility_check
            CHECK (election_eligibility IN ('eligible', 'prefer_skip', 'never_leader'));
    END IF;
END $$;

-- Aura is a laptop today. Until we have heartscale support to track
-- which laptops are LAN-attached vs. away, never let any laptop hold
-- the leader role.
UPDATE computers
   SET election_eligibility = 'never_leader'
 WHERE name = 'aura';


-- V50: seed_canonical_ports

INSERT INTO fleet_secrets (key, value, description, updated_by)
VALUES
    ('port.gateway',  '51002', 'ForgeFleet HTTP gateway / dashboard / onboard.sh', 'migration-V50'),
    ('port.openclaw', '50000', 'OpenClaw WebSocket gateway',                       'migration-V50'),
    ('port.postgres', '55432', 'Postgres on the leader',                            'migration-V50'),
    ('port.redis',    '6380',  'Redis on the leader',                               'migration-V50'),
    ('port.nats',     '4222',  'NATS pub/sub on every member',                      'migration-V50'),
    ('port.mcp',      '50001', 'MCP HTTP server on every member',                   'migration-V50')
ON CONFLICT (key) DO NOTHING;


-- V51: idempotent_upgrade_playbook

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


-- V52: wait_for_siblings_barrier

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


-- V53: oauth_subscription_providers

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


-- V54: agent_orchestration

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


-- V55: session_brain

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


-- V56: retire_last_tomls_and_cli_build

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


-- V57: macos_ff_git_parity

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


-- V58: kill_switch_ttl

ALTER TABLE fleet_secrets ADD COLUMN IF NOT EXISTS disabled_reason TEXT;


-- V59: openclaw_macos_sudo

UPDATE software_registry
   SET upgrade_playbook = jsonb_set(
           upgrade_playbook,
           '{macos}',
           to_jsonb('export PATH=/opt/homebrew/bin:$PATH && sudo -n npm install -g openclaw@latest'::text)
       )
 WHERE id = 'openclaw';


-- V60: auto_upgrade_memory

ALTER TABLE computer_software
    ADD COLUMN IF NOT EXISTS consecutive_failures INTEGER NOT NULL DEFAULT 0;


-- V61: peer_driven_upgrades

ALTER TABLE fleet_tasks
    ADD COLUMN IF NOT EXISTS excludes_computer_ids JSONB NOT NULL DEFAULT '[]'::jsonb;

CREATE INDEX IF NOT EXISTS idx_fleet_tasks_excludes
    ON fleet_tasks USING GIN (excludes_computer_ids);


-- V63: drop_need_build_shortcut

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


-- V64: register_ff_forgefleetd

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


-- V65: register_open_design

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


-- V66: data_driven_detection

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


-- V67: auto_install_agent_hint

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


-- V69: skill_sources

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


-- V70: fleet_model_catalog_qwen36

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


-- V71: backfill_fleet_model_catalog

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


-- V72: sqlite_consolidation

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


-- V73: fleet_tool_registry

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


-- V74: routing_mode

-- Routing strategy for each task. Affects claim ordering in TaskRunner.
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS routing_mode TEXT NOT NULL DEFAULT 'fleet_first'
    CHECK (routing_mode IN ('local_first', 'fleet_first', 'local_only', 'balanced'));

-- Index to speed up fleet-first claim queries (deprioritize own tasks).
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_routing ON fleet_tasks(routing_mode, created_by_computer_id, status)
    WHERE status = 'pending';


-- V75: work_items

-- ─── Work Items ─────────────────────────────────────────────────────────────
-- Individual units of work within a decomposed task.







-- ─── Work Batches ───────────────────────────────────────────────────────────
-- A batch is a group of work_items assigned to one node.


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


-- V76: vault_sync

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


-- V77: fleet_task_notify

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


-- V78: pgvector_embeddings

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


-- V79: project_schedules

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


-- V80: agent_procedures

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


-- V81: security_hardening

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


-- V82: rename_fleet_node_ssh_keys

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


-- V83: rename_fleet_nodes

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


-- V86: drop_fleet_members

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


-- V87: rename_node_name_columns

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


-- V88: rename_fleet_node_runtime

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


-- V89: github_ssh_aliases

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


-- V90: deployment_desired_state

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


-- V91: task_models_seed

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


-- V92: ff_git_linux_parity

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


-- V93: backfill_fleet_worker_runtime

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


-- V94: bge_quant_fix

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


-- V95: bge_embedding_dim_1024

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


-- V96: register_pipeline_llm_alias

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


-- V97: redis_nats_5digit_remap

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


-- V98: gemma4_repo_fix

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


-- V99: default_pool_alias

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


-- V100: retire_qwen25

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


-- V101: upgrade_playbook_refresh

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


-- V102: wave_self_kill_fix

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


-- V103: retire_qwen2_vl

DELETE FROM fleet_model_catalog
 WHERE id IN ('qwen2-vl-7b', 'qwen2-vl-7b-instruct');
DELETE FROM model_catalog
 WHERE id IN ('qwen2-vl-7b', 'qwen2-vl-7b-instruct');


-- V104: wave_disown_fix

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


-- V105: skills_v1

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


-- V106: model_library_state

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


-- V107: dispatcher_foundation

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


-- V108: task_depends_on

ALTER TABLE fleet_tasks
  ADD COLUMN IF NOT EXISTS depends_on_task_id uuid
    REFERENCES fleet_tasks(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_fleet_tasks_depends_on
    ON fleet_tasks (depends_on_task_id)
 WHERE depends_on_task_id IS NOT NULL;


-- V109: open_design_corepack_fix

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


-- V110: amcheck_integrity

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


-- V111: agent_swarm_data_plane

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


-- V112: fleet_agents

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


-- V113: coder_tool_calling

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


-- V114: node_reservation

ALTER TABLE computers
    ADD COLUMN IF NOT EXISTS reservation_state TEXT NOT NULL DEFAULT 'available'
        CHECK (reservation_state IN ('available','reserved','drained')),
    ADD COLUMN IF NOT EXISTS reserved_reason TEXT,
    ADD COLUMN IF NOT EXISTS reserved_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_computers_reservation
    ON computers(reservation_state) WHERE reservation_state <> 'available';


-- V115: agent_catalog

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


-- V116: session_demand

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


-- V117: brain_faceted_graph

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


-- V118: disk_management

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


-- V119: resource_arbiter

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


-- V120: fleet_conformance

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


-- V122: interaction_log

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


-- V123: cortex_file_index

CREATE TABLE IF NOT EXISTS cortex_file_index (
    corpus_slug   TEXT COLLATE "C" NOT NULL,
    file_path     TEXT COLLATE "C" NOT NULL,
    indexed_hash  TEXT NOT NULL,
    indexed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (corpus_slug, file_path)
);
CREATE INDEX IF NOT EXISTS idx_cortex_file_index_corpus ON cortex_file_index (corpus_slug);


-- V124: cortex_symbol_lines

ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS start_line INT;
ALTER TABLE brain_vault_nodes ADD COLUMN IF NOT EXISTS end_line   INT;


-- V125: brain_community_registry

ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS member_hash        TEXT;
ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS summary            TEXT;
ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS summary_model      TEXT;
ALTER TABLE brain_communities ADD COLUMN IF NOT EXISTS summary_updated_at TIMESTAMPTZ;
CREATE UNIQUE INDEX IF NOT EXISTS idx_brain_communities_member_hash
    ON brain_communities (member_hash);


-- V126: community_god_node_ondelete

ALTER TABLE brain_communities DROP CONSTRAINT IF EXISTS brain_communities_god_node_id_fkey;
ALTER TABLE brain_communities ADD CONSTRAINT brain_communities_god_node_id_fkey
    FOREIGN KEY (god_node_id) REFERENCES brain_vault_nodes(id) ON DELETE SET NULL;


-- V127: cortex_code_communities

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


-- V128: cortex_reexports

CREATE TABLE IF NOT EXISTS cortex_reexports (
    corpus_slug   TEXT COLLATE "C" NOT NULL,
    file_path     TEXT COLLATE "C" NOT NULL,
    kind          TEXT COLLATE "C" NOT NULL,   -- 'named' | 'glob'
    facade        TEXT COLLATE "C" NOT NULL,   -- named: facade path; glob: base module
    target        TEXT COLLATE "C" NOT NULL,   -- named: real target; glob: target module
    PRIMARY KEY (corpus_slug, file_path, kind, facade, target)
);
CREATE INDEX IF NOT EXISTS idx_cortex_reexports_corpus ON cortex_reexports (corpus_slug);


-- V129: docker_latest_tag

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


-- V130: backup_restore_drill

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


-- V131: fleet_integrity

INSERT INTO alert_policies
    (name, description, metric, scope, condition,
     duration_secs, severity, cooldown_secs, channel, enabled)
VALUES
  ('fleet_integrity_degraded',
   'One or more ONLINE fleet members failed the verify_computer battery (half-configured enrollment or config drift while alive)',
   'fleet_integrity_degraded', 'leader_only', '> 0',
   0, 'warning', 21600, 'telegram', true)
ON CONFLICT (name) DO NOTHING;


-- V132: evolution_backlog

CREATE TABLE IF NOT EXISTS evolution_backlog (
    fingerprint  TEXT PRIMARY KEY,
    item         JSONB NOT NULL,
    durable      BOOLEAN NOT NULL DEFAULT false,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_evolution_backlog_durable
    ON evolution_backlog (durable) WHERE durable;


-- V133: leader_maintenance_lease

ALTER TABLE fleet_leader_state
    ADD COLUMN IF NOT EXISTS standby_member      TEXT,
    ADD COLUMN IF NOT EXISTS relinquishing_until TIMESTAMPTZ;


-- V134: upgrade_rollouts

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


-- V135: integrity_active_repairs

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


-- V136: dsn_of_record

CREATE TABLE IF NOT EXISTS dsn_of_record (
    singleton_key   TEXT PRIMARY KEY DEFAULT 'current'
                        CHECK (singleton_key = 'current'),
    dsn             TEXT NOT NULL,
    primary_member  TEXT,
    previous_dsn    TEXT,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by      TEXT
);


-- V137: gate_previous_value

ALTER TABLE fleet_secrets ADD COLUMN IF NOT EXISTS previous_value TEXT;


-- V138: interaction_worker_attribution

ALTER TABLE ff_interactions ADD COLUMN IF NOT EXISTS worker_name TEXT;
ALTER TABLE ff_interactions ADD COLUMN IF NOT EXISTS endpoint    TEXT;
CREATE INDEX IF NOT EXISTS idx_ff_interactions_worker ON ff_interactions (worker_name, ts DESC);


-- V139: agent_scratchpad

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


-- V140: distributed_dev_workitems

-- (a) Canonical work_items (no-op on live; materializes on fresh rebuilds).

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


-- V141: project_repos_folders

-- One GitHub location attached to a project. A project may have several
-- (e.g. app repo + infra repo + docs repo). `is_primary` marks the main one.


-- at most one primary repo per project


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


-- V142: cortex_universal_foundation

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


-- V143: project_git_policy

-- Per-project git policy for multi-project build orchestration.
ALTER TABLE projects ADD COLUMN IF NOT EXISTS integration_strategy TEXT NOT NULL DEFAULT 'feature_pr';
ALTER TABLE projects ADD COLUMN IF NOT EXISTS branch_prefix        TEXT NOT NULL DEFAULT 'feat';
ALTER TABLE projects ADD COLUMN IF NOT EXISTS git_remote           TEXT NOT NULL DEFAULT 'origin';

-- HireFlow integrates onto dev, not main.
UPDATE projects SET default_branch = 'dev' WHERE id = 'hireflow360' AND default_branch = 'main';


-- V144: code_community_levels

-- Hierarchical GraphRAG: brain_code_communities gains a community LEVEL.
-- level 0 = finest call clusters (single-level Louvain, the prior behaviour);
-- higher levels = progressively coarser subsystems from multi-level Louvain
-- aggregation. The member_hash uniqueness becomes per-level so the same grouping
-- can be recorded at more than one granularity.
ALTER TABLE brain_code_communities ADD COLUMN IF NOT EXISTS level INT NOT NULL DEFAULT 0;
DROP INDEX IF EXISTS idx_brain_code_communities_member_hash;
CREATE UNIQUE INDEX IF NOT EXISTS idx_brain_code_communities_member_hash_level
    ON brain_code_communities (member_hash, level);


-- V145: code_community_parent

-- Hierarchical GraphRAG: each community records its PARENT (the immediate
-- strictly-larger enclosing community up the level hierarchy) by member_hash,
-- making brain_code_communities a navigable tree. NULL = top-level community.
-- Indexed for child lookups (a parent's children = rows WHERE parent_member_hash
-- = the parent's member_hash), which the level>0 map-reduce summary pass uses.
ALTER TABLE brain_code_communities ADD COLUMN IF NOT EXISTS parent_member_hash TEXT;
CREATE INDEX IF NOT EXISTS idx_brain_code_communities_parent
    ON brain_code_communities (parent_member_hash);


-- V146: disable_dead_computer_offline_alert

UPDATE alert_policies
   SET enabled = false
 WHERE name = 'computer_offline'
   AND metric = 'computer_status'
   AND condition = '== ''odown'''
   AND enabled = true;


-- V147: telegram_sessions

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


-- V148: computer_backends

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


-- V149: provider_routing

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


-- V150: kimi_cli_external_tool

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


-- V151: computer_backends_path

ALTER TABLE computer_backends ADD COLUMN IF NOT EXISTS path TEXT;


-- V152: work_item_repo_binding

ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS repo_id   UUID REFERENCES project_repos(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS repo_url  TEXT,
    ADD COLUMN IF NOT EXISTS repo_path TEXT;

CREATE INDEX IF NOT EXISTS idx_work_items_repo_id ON work_items (repo_id);
CREATE INDEX IF NOT EXISTS idx_work_items_repo_url ON work_items (repo_url);


-- V156: fleet_tasks_fold_columns

ALTER TABLE fleet_tasks
    ADD COLUMN IF NOT EXISTS task_class          TEXT,
    ADD COLUMN IF NOT EXISTS not_before          TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS dedup_signature     TEXT,
    ADD COLUMN IF NOT EXISTS parent_work_item_id UUID;
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_task_class ON fleet_tasks (task_class);
CREATE INDEX IF NOT EXISTS idx_fleet_tasks_not_before ON fleet_tasks (not_before) WHERE not_before IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_fleet_tasks_dedup_signature ON fleet_tasks (dedup_signature) WHERE dedup_signature IS NOT NULL;


-- V157: fold_research_subtasks

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


-- V158: fold_self_heal_queue

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


-- V159: fold_deferred_tasks

CREATE OR REPLACE VIEW deferred_tasks AS
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


-- V160: notify_dedup

CREATE TABLE IF NOT EXISTS operator_notify_dedup (
    signature  TEXT PRIMARY KEY,
    last_sent  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);


-- V161: canonical_github_alias

ALTER TABLE github_ssh_aliases
    ADD COLUMN IF NOT EXISTS is_canonical boolean NOT NULL DEFAULT false;

-- venkatyarl is the canonical account post-migration (taylor-oclaw retired).
UPDATE github_ssh_aliases SET is_canonical = true  WHERE alias_name = 'github.com-venkat';
UPDATE github_ssh_aliases SET is_canonical = false WHERE alias_name <> 'github.com-venkat';

-- At most one canonical alias per hostname.
CREATE UNIQUE INDEX IF NOT EXISTS github_ssh_aliases_one_canonical_per_host
    ON github_ssh_aliases (hostname) WHERE is_canonical;


-- Migration runner metadata: mark every Postgres migration version through v161 applied.
CREATE TABLE IF NOT EXISTS _migrations (
    version     INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    applied_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
INSERT INTO _migrations (version, name)
VALUES

    (7, 'fleet_config_tables'),
    (8, 'task_provenance_schema'),
    (9, 'fleet_secrets'),
    (10, 'deferred_tasks'),
    (11, 'model_lifecycle'),
    (12, 'onboarding_foundation'),
    (13, 'virtual_brain'),
    (14, 'computers_and_portfolio'),
    (15, 'project_management'),
    (16, 'observability'),
    (17, 'security_hardening'),
    (18, 'network_scope'),
    (19, 'storage_power_training'),
    (20, 'port_registry'),
    (21, 'drop_deployment_model_fk'),
    (22, 'drop_model_presence_fk'),
    (23, 'sub_agents'),
    (24, 'external_tools'),
    (25, 'social_media_ingest'),
    (26, 'cloud_llm_providers'),
    (27, 'pool_aliases'),
    (28, 'software_registry_seed'),
    (29, 'fix_ff_git_linux_playbook'),
    (30, 'playbook_self_heal_repo'),
    (31, 'source_tree_path'),
    (32, 'playbook_bugfixes'),
    (33, 'cli_aliases'),
    (34, 'retire_alert_policies_toml'),
    (35, 'retire_cloud_llm_providers_toml'),
    (36, 'retire_task_coverage_toml'),
    (37, 'retire_ports_toml'),
    (38, 'retire_external_tools_toml'),
    (39, 'retire_model_catalog_toml'),
    (40, 'agent_session_on_work_outputs'),
    (41, 'per_arch_build_leader'),
    (42, 'research_subsystem'),
    (43, 'multi_host_and_self_heal'),
    (44, 'fleet_tasks'),
    (45, 'beat_age_alerts'),
    (46, 'npm_cli_catalog'),
    (47, 'fabric_measurements_and_docker'),
    (48, 'upgrade_playbook_restart_fix'),
    (49, 'connectivity_mode_and_eligibility'),
    (50, 'seed_canonical_ports'),
    (51, 'idempotent_upgrade_playbook'),
    (52, 'wait_for_siblings_barrier'),
    (53, 'oauth_subscription_providers'),
    (54, 'agent_orchestration'),
    (55, 'session_brain'),
    (56, 'retire_last_tomls_and_cli_build'),
    (57, 'macos_ff_git_parity'),
    (58, 'kill_switch_ttl'),
    (59, 'openclaw_macos_sudo'),
    (60, 'auto_upgrade_memory'),
    (61, 'peer_driven_upgrades'),
    (62, 'reserved_gap'),
    (63, 'drop_need_build_shortcut'),
    (64, 'register_ff_forgefleetd'),
    (65, 'register_open_design'),
    (66, 'data_driven_detection'),
    (67, 'auto_install_agent_hint'),
    (68, 'reserved_gap'),
    (69, 'skill_sources'),
    (70, 'fleet_model_catalog_qwen36'),
    (71, 'backfill_fleet_model_catalog'),
    (72, 'sqlite_consolidation'),
    (73, 'fleet_tool_registry'),
    (74, 'routing_mode'),
    (75, 'work_items'),
    (76, 'vault_sync'),
    (77, 'fleet_task_notify'),
    (78, 'pgvector_embeddings'),
    (79, 'project_schedules'),
    (80, 'agent_procedures'),
    (81, 'security_hardening'),
    (82, 'rename_fleet_node_ssh_keys'),
    (83, 'rename_fleet_nodes'),
    (84, 'rename_node_name_column'),
    (85, 'drop_compat_views'),
    (86, 'drop_fleet_members'),
    (87, 'rename_node_name_columns'),
    (88, 'rename_fleet_node_runtime'),
    (89, 'github_ssh_aliases'),
    (90, 'deployment_desired_state'),
    (91, 'task_models_seed'),
    (92, 'ff_git_linux_parity'),
    (93, 'backfill_fleet_worker_runtime'),
    (94, 'bge_quant_fix'),
    (95, 'bge_embedding_dim_1024'),
    (96, 'register_pipeline_llm_alias'),
    (97, 'redis_nats_5digit_remap'),
    (98, 'gemma4_repo_fix'),
    (99, 'default_pool_alias'),
    (100, 'retire_qwen25'),
    (101, 'upgrade_playbook_refresh'),
    (102, 'wave_self_kill_fix'),
    (103, 'retire_qwen2_vl'),
    (104, 'wave_disown_fix'),
    (105, 'skills_v1'),
    (106, 'model_library_state'),
    (107, 'dispatcher_foundation'),
    (108, 'task_depends_on'),
    (109, 'open_design_corepack_fix'),
    (110, 'amcheck_integrity'),
    (111, 'agent_swarm_data_plane'),
    (112, 'fleet_agents'),
    (113, 'coder_tool_calling'),
    (114, 'node_reservation'),
    (115, 'agent_catalog'),
    (116, 'session_demand'),
    (117, 'brain_faceted_graph'),
    (118, 'disk_management'),
    (119, 'resource_arbiter'),
    (120, 'fleet_conformance'),
    (121, 'reserved_gap'),
    (122, 'interaction_log'),
    (123, 'cortex_file_index'),
    (124, 'cortex_symbol_lines'),
    (125, 'brain_community_registry'),
    (126, 'community_god_node_ondelete'),
    (127, 'cortex_code_communities'),
    (128, 'cortex_reexports'),
    (129, 'docker_latest_tag'),
    (130, 'backup_restore_drill'),
    (131, 'fleet_integrity'),
    (132, 'evolution_backlog'),
    (133, 'leader_maintenance_lease'),
    (134, 'upgrade_rollouts'),
    (135, 'integrity_active_repairs'),
    (136, 'dsn_of_record'),
    (137, 'gate_previous_value'),
    (138, 'interaction_worker_attribution'),
    (139, 'agent_scratchpad'),
    (140, 'distributed_dev_workitems'),
    (141, 'project_repos_folders'),
    (142, 'cortex_universal_foundation'),
    (143, 'project_git_policy'),
    (144, 'code_community_levels'),
    (145, 'code_community_parent'),
    (146, 'disable_dead_computer_offline_alert'),
    (147, 'telegram_sessions'),
    (148, 'computer_backends'),
    (149, 'provider_routing'),
    (150, 'kimi_cli_external_tool'),
    (151, 'computer_backends_path'),
    (152, 'work_item_repo_binding'),
    (153, 'retire_v75_work_stealing'),
    (154, 'nested_subagent_workspace'),
    (155, 'drop_dead_bridge'),
    (156, 'fleet_tasks_fold_columns'),
    (157, 'fold_research_subtasks'),
    (158, 'fold_self_heal_queue'),
    (159, 'fold_deferred_tasks'),
    (160, 'notify_dedup'),
    (161, 'canonical_github_alias')
ON CONFLICT (version) DO UPDATE SET name = EXCLUDED.name;
