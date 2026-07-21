-- Down migration for V235 (20260721000000_add_projects_config.sql).
--
-- The embedded runner is forward-only and never executes this file; it is kept
-- for manual rollback of the structured project config columns.

ALTER TABLE projects
    DROP COLUMN IF EXISTS config_json,
    DROP COLUMN IF EXISTS config;
