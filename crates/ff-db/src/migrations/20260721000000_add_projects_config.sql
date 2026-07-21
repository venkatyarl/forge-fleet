-- V235 — Structured project config on the `projects` table.
--
-- Stores a project's declarative configuration (paths[], repos[], targets[],
-- vault_realm, status) as one structured JSONB document. A companion
-- `config_json` TEXT column holds the same document for callers that persist a
-- serialized JSON string instead of relying on Postgres JSONB coercion.
--
-- Idempotent (ADD COLUMN IF NOT EXISTS) so re-running anywhere is safe. The
-- runner is forward-only and never applies a down step; the rollback DDL lives
-- in the companion `20260721000000_add_projects_config.down.sql` for manual use.

ALTER TABLE projects
    ADD COLUMN IF NOT EXISTS config      JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN IF NOT EXISTS config_json TEXT;
