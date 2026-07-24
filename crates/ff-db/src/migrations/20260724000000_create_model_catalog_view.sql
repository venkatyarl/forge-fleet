-- `model_catalog` (the old comprehensive HF-metadata table, seeded by V39)
-- is stale: nothing keeps it in sync any more, while `fleet_model_catalog`
-- (V11+) is the table `ff model sync-catalog` actually maintains. Rename
-- the stale table out of the way (existing FKs from `work_outputs`,
-- `training_jobs`, and its own self-referential `replaced_by` column keep
-- working — Postgres follows renamed tables by OID) and reuse the
-- `model_catalog` name for a view over the canonical table, so existing
-- readers of `model_catalog` transparently see the live data.
ALTER TABLE model_catalog RENAME TO model_catalog_legacy;

CREATE OR REPLACE VIEW model_catalog AS
SELECT * FROM fleet_model_catalog;
