CREATE TABLE task_outbox (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL,
    attempt_metadata JSONB NOT NULL,
    status TEXT DEFAULT 'PENDING',
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX idx_task_outbox_task_id_status ON task_outbox (task_id, status);
