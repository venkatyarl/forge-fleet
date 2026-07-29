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
