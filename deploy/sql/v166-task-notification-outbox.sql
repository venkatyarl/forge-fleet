-- V166: task notification outbox
--
-- Transactional outbox for fleet_tasks lifecycle events. A trigger captures
-- new tasks and status changes so a background relay can publish notifications
-- without coupling the task writer to external brokers.

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
