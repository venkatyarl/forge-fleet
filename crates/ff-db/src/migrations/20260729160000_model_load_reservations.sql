CREATE TABLE IF NOT EXISTS model_load_reservations (
    host_name TEXT NOT NULL,
    resource_kind TEXT NOT NULL CHECK (resource_kind IN ('port', 'library', 'operation')),
    resource_value TEXT NOT NULL,
    owner_token UUID NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (host_name, resource_kind, resource_value)
);

CREATE INDEX IF NOT EXISTS model_load_reservations_expiry_idx
    ON model_load_reservations (expires_at);

-- PID alone is not an identity: kernels recycle it. Model lifecycle actions
-- persist the OS-specific process incarnation marker captured at launch and
-- require an exact match before refresh, replacement, or termination.
ALTER TABLE fleet_model_deployments
    ADD COLUMN IF NOT EXISTS process_start_marker TEXT,
    ADD COLUMN IF NOT EXISTS agent_profile_verified_at TIMESTAMPTZ;
