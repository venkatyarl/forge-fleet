# WAN / Off-site Replication

Goal: maintain a read-only Postgres replica off-site for disaster recovery.
This replica is NOT part of leader election (latency too high); it's purely
a cold standby.

## Setup

1. Join the off-site machine to Tailscale (`tailscale up`).
2. Confirm it can reach Taylor's Tailscale IP:
   `nc -vz 100.64.5.100 55432` (or `curl -s http://100.64.5.100:55432`).
3. Add the replication rule to Taylor's `pg_hba.conf`:
   `host replication replicator 100.64.0.0/10 md5`
   and reload Postgres (`docker exec forgefleet-postgres pg_ctl reload`
   or `SELECT pg_reload_conf();`).
4. Copy `deploy/docker-compose.follower-remote.yml` + `deploy/docker/replica-bootstrap.sh`
   onto the off-site machine (rsync over Tailscale works fine).
5. Start the follower:
   `POSTGRES_PRIMARY_HOST=100.64.5.100 POSTGRES_REPLICATION_PASSWORD=<same as primary> \
       docker compose -f docker-compose.follower-remote.yml up -d`

Verify on the primary:
`docker exec forgefleet-postgres psql -U forgefleet -c "SELECT client_addr, state, write_lag FROM pg_stat_replication;"`

You should see the replica's Tailscale IP with `state='streaming'`.

## Backups to off-site

Use the existing backup distribution logic (Phase 6B). The off-site computer
is just another target for rsync over SSH via Tailscale. Add it to
`database_replicas` with `role='wan_replica'` and the distributor will
include it in the nightly fan-out.

## Failover

The WAN replica does NOT participate in leader election. If Taylor fails,
Marcus takes over (LAN quorum). Off-site is for DR only — if Taylor's
building burns down, promote off-site manually:

```bash
# On the off-site machine
docker exec forgefleet-postgres-replica-remote \
  su postgres -c "pg_ctl promote -D /var/lib/postgresql/data/pgdata"
```

Then update `~/.forgefleet/fleet.toml` on every surviving machine to point
`database.url` at the off-site Tailscale IP. ForgeFleet will pick up the
new primary on next daemon restart.

## Security notes

- Replication password is still plaintext in env vars. Keep them in
  `~/.forgefleet/fleet.toml` (mode 600) or use `op read` / Docker secrets.
- `md5` hashing in `pg_hba` is fine over Tailscale (end-to-end encrypted).
  For even belt-and-suspenders, switch the rule to `scram-sha-256`.
- The Tailscale ACL should be restricted so only the off-site machine can
  reach port 55432 on Taylor — `acls` in your tailnet policy:
  ```jsonc
  {
    "acls": [
      { "action": "accept", "src": ["tag:wan-replica"], "dst": ["tag:primary:55432"] }
    ]
  }
  ```

## Operational checklist

| Step | Who | Command |
|------|-----|---------|
| Tailscale installed + up | off-site | `tailscale up` |
| Primary reachable | off-site | `nc -vz 100.64.5.100 55432` |
| pg_hba rule added to primary | primary | edit `pg_hba.conf` + reload |
| Start replica | off-site | `docker compose -f ... up -d` |
| Verify streaming | primary | `SELECT * FROM pg_stat_replication` |
| Register in `database_replicas` | any leader | `ff fleet db add-remote-replica --computer <n> --via tailscale` |

## Known limits (v1)

- Promotion is manual. Automated DR failover (e.g. Patroni-style) is out
  of scope — the LAN fleet already covers auto-failover for normal outages.
- Only Postgres is replicated. Redis/NATS are not WAN-replicated — they
  are ephemeral coordination layers and re-bootstrap on failover.
- Backup encryption at rest on the off-site volume is not handled here
  (plan on LUKS / FileVault at the host level).
