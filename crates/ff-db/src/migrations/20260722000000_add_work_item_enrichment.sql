ALTER TABLE work_items
    ADD COLUMN eisenhower_quadrant INTEGER,
    ADD COLUMN numeric_priority INTEGER,
    ADD COLUMN pick_score DECIMAL,
    ADD COLUMN blocked_by_count INTEGER NOT NULL DEFAULT 0;

ALTER TABLE sub_agents
    ADD COLUMN capabilities JSONB NOT NULL DEFAULT '[]'::jsonb;
