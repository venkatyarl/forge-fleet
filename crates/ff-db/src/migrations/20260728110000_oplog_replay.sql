CREATE TABLE IF NOT EXISTS isolated_node_oplog (
    node_id TEXT NOT NULL,
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    operation_id UUID NOT NULL DEFAULT gen_random_uuid(),
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    field_name TEXT NOT NULL,
    merge_strategy TEXT NOT NULL CHECK (merge_strategy IN ('LWW', 'UNION')),
    value JSONB NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    writer_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (node_id, sequence)
);

CREATE INDEX IF NOT EXISTS idx_isolated_node_oplog_pending
    ON isolated_node_oplog (node_id, sequence);

CREATE TABLE IF NOT EXISTS oplog_shared_state (
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    field_name TEXT NOT NULL,
    value JSONB NOT NULL,
    merge_strategy TEXT NOT NULL CHECK (merge_strategy IN ('LWW', 'UNION')),
    lww_observed_at TIMESTAMPTZ,
    lww_writer_id TEXT,
    lww_sequence BIGINT,
    version BIGINT NOT NULL DEFAULT 1 CHECK (version > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (entity_type, entity_id, field_name)
);

CREATE TABLE IF NOT EXISTS oplog_replay_checkpoints (
    node_id TEXT PRIMARY KEY,
    last_sequence BIGINT NOT NULL DEFAULT 0 CHECK (last_sequence >= 0),
    state TEXT NOT NULL DEFAULT 'idle' CHECK (state IN ('idle', 'replaying', 'failed')),
    state_version BIGINT NOT NULL DEFAULT 0 CHECK (state_version >= 0),
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS oplog_replay_applied (
    node_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    operation_id UUID NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (node_id, sequence),
    UNIQUE (operation_id)
);
