# TB.3 — Postgres streaming hot standby on James over Thunderbolt

**Status:** blocked on prereq (no `replicator` role exists yet —
backup_orchestrator is spamming the err log).

## Goal

James becomes a live read-replica of Taylor's Postgres so:
1. Reads can fan out to James for the brain-search / dashboard load
2. Backups stream from James (no impact on Taylor's WAL)
3. If Taylor dies hard, James can be promoted in <30s (operator manual)

Network: Postgres replication runs over Thunderbolt (10.44.0.x) so
WAL traffic doesn't compete with LAN.

## Prereq (fix existing pg_basebackup errors)

The `backup_orchestrator` is already attempting basebackup but failing:

```
pg_basebackup: error: connection to server at "127.0.0.1", port 5432
failed: FATAL: role "replicator" does not exist
```

Step 0: create the role.

```sql
CREATE ROLE replicator WITH REPLICATION LOGIN PASSWORD 'CHANGE_ME_strong_pw';
ALTER SYSTEM SET wal_level = 'replica';
ALTER SYSTEM SET max_wal_senders = 5;
ALTER SYSTEM SET max_replication_slots = 5;
SELECT pg_reload_conf();
```

```sql
-- pg_hba.conf entry:
host  replication  replicator  10.44.0.2/32  scram-sha-256
```

Restart Taylor's Postgres (in Docker) to pick up wal_level change:
`docker compose restart postgres`.

Store the password in fleet_secrets:
```bash
ff secrets set pg.replicator_password 'CHANGE_ME_strong_pw' \
  --description "Postgres streaming replication credential — replicator role"
```

## James side

1. **Install Postgres 18 in Docker on James**

```bash
ssh james "mkdir -p ~/.forgefleet/postgres-standby"
# Reuse deploy/docker-compose.yml's postgres image; mount volume locally.
```

2. **Take base backup over TB**

```bash
ssh james "PGPASSWORD=... pg_basebackup \
  -h 10.44.0.1 -p 55432 \
  -D /var/lib/postgresql/data \
  -U replicator -X stream -P -R"
```

3. **Start in standby mode** (the `-R` above writes `standby.signal`
   automatically).

4. **Verify replication lag**

```sql
-- on Taylor:
SELECT client_addr, state, sync_state,
       pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn) AS bytes_behind
FROM pg_stat_replication;
```

Target: <1 MB lag in steady state.

## Routing reads to James

ff has `FleetResolver` — add a read-only DSN for the brain layer:

```toml
[postgres.read_replica]
host = "10.44.0.2"
port = 5432
user = "forgefleet_read"   # create as RO role
```

Only the embedding search + dashboard queries should target the replica
initially — anything that writes must hit Taylor.

## Failover (manual, not automatic)

```bash
# On James:
pg_ctl promote -D /var/lib/postgresql/data
# Then re-point Taylor's clients via ff fleet leader-override.
```

We do NOT want automatic failover until we have a quorum cluster
(future: 3-node Patroni? PostgreSQL-on-Raft?).

## Risks

- **Wal lag bursts during heavy schema migration** — pause replication
  before V-series migrations.
- **TB cable disconnect = no WAL streaming** — Postgres will switch to
  WAL archive mode if `archive_mode=on`; otherwise James will fall
  behind until reconnect. Set `archive_mode=on` and archive to NFS
  (TB.4).
