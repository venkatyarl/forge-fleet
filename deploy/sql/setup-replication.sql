-- Create replication user (run once on primary).
--
-- Apply on the primary Postgres (Taylor) with:
--   docker exec -i forgefleet-postgres \
--     psql -U forgefleet -d forgefleet < deploy/sql/setup-replication.sql
--
-- Also requires a matching pg_hba.conf entry so the replica can connect:
--   host replication replicator 192.168.5.0/24 md5
-- Append with:
--   docker exec -u postgres forgefleet-postgres bash -c \
--     "echo 'host replication replicator 192.168.5.0/24 md5' >> \
--      /var/lib/postgresql/data/pg_hba.conf"
-- Then: docker restart forgefleet-postgres

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'replicator') THEN
        CREATE ROLE replicator WITH REPLICATION LOGIN PASSWORD 'replicator-default';
    END IF;
END
$$;

-- Grant pg_read_all_data for safety (so replica can copy).
GRANT pg_read_all_data TO replicator;
