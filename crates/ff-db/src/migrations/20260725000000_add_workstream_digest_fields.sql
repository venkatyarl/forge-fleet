-- Add optional project metadata used to attach projects to workstreams and
-- create their initial digest configuration.
ALTER TABLE projects
    ADD COLUMN IF NOT EXISTS workstream_id TEXT,
    ADD COLUMN IF NOT EXISTS digest_template_id JSONB,
    ADD COLUMN IF NOT EXISTS logo_url TEXT;

CREATE INDEX IF NOT EXISTS idx_projects_workstream_id
    ON projects (workstream_id)
    WHERE workstream_id IS NOT NULL;

-- Rollback:
-- DROP INDEX IF EXISTS idx_projects_workstream_id;
-- ALTER TABLE projects
--     DROP COLUMN IF EXISTS logo_url,
--     DROP COLUMN IF EXISTS digest_template_id,
--     DROP COLUMN IF EXISTS workstream_id;
