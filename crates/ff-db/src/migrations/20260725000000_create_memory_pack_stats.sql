CREATE TABLE IF NOT EXISTS memory_pack_stats (
    work_item_id UUID PRIMARY KEY REFERENCES work_items(id) ON DELETE CASCADE,
    predicted_paths JSONB NOT NULL DEFAULT '[]'::jsonb,
    touched_paths JSONB NOT NULL DEFAULT '[]'::jsonb,
    hit_rate REAL NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_memory_pack_stats_created_at
    ON memory_pack_stats (created_at DESC);
