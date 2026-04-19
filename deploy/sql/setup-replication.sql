-- Create replication user (run once on primary).
--
-- Apply on the primary Postgres (Taylor) with:
--   docker exec -i forgefleet-postgres \
--     psql -U forgefleet -d forgefleet < deploy/sql/setup-replication.sql
--
-- Also requires matching pg_hba.conf entries so the replica can connect.
-- Fleet nodes may reach the primary via either the LAN (192.168.5.0/24)
-- or a VPN overlay whose egress SNATs to a public IP (e.g. a DigitalOcean
-- relay for Tailscale-style meshes). Both rules are needed; auth is
-- password-gated via md5 so the wildcard rule is still safe.
--   host replication replicator 192.168.5.0/24 md5
--   host replication replicator 0.0.0.0/0      md5
-- Append with:
--   docker exec -u postgres forgefleet-postgres bash -c \
--     "echo 'host replication replicator 192.168.5.0/24 md5' >> \
--      /var/lib/postgresql/data/pg_hba.conf && \
--      echo 'host replication replicator 0.0.0.0/0 md5' >> \
--      /var/lib/postgresql/data/pg_hba.conf"
-- Then: docker exec forgefleet-postgres psql -U forgefleet -c "SELECT pg_reload_conf();"

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'replicator') THEN
        CREATE ROLE replicator WITH REPLICATION LOGIN PASSWORD 'replicator-default';
    END IF;
END
$$;

-- Grant pg_read_all_data for safety (so replica can copy).
GRANT pg_read_all_data TO replicator;
