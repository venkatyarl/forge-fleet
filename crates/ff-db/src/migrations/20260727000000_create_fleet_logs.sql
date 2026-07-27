-- Append-only store for fleet node log lines.
CREATE TABLE IF NOT EXISTS fleet_logs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    ts          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    node_id     TEXT NOT NULL,
    log_level   TEXT NOT NULL,
    message     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fleet_logs_node_ts
    ON fleet_logs (node_id, ts);
