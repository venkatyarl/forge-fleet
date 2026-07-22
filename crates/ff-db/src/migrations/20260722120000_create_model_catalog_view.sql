-- V245: model_catalog becomes a compatibility view over fleet_model_catalog.
--
-- fleet_model_catalog (see V70/V71/V243) is now the actively-synced,
-- richly-fielded catalog; the standalone model_catalog table has gone stale.
-- Replace it with a view exposing all fleet_model_catalog columns under the
-- old name so existing call sites keep resolving.
--
-- Idempotent: only drops model_catalog when it is still a real table — a
-- second run finds it already converted to a view and skips straight to the
-- (idempotent) CREATE OR REPLACE VIEW.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM pg_catalog.pg_tables
        WHERE schemaname = 'public' AND tablename = 'model_catalog'
    ) THEN
        EXECUTE 'DROP TABLE model_catalog CASCADE';
    END IF;
END $$;

-- Single-table views are auto-updatable in Postgres >= 9.3, so
-- INSERT / UPDATE / DELETE against model_catalog continue to work.
CREATE OR REPLACE VIEW model_catalog AS
    SELECT * FROM fleet_model_catalog;
