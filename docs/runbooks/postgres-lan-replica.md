# PostgreSQL LAN replica bootstrap

ForgeFleet supports an idempotent physical PostgreSQL streaming replica on an
enrolled LAN node. Planning is read-only; applying is dispatched to the named
target through the fleet deferred-task worker.

Prerequisites:

- primary and target are enrolled nodes with stable LAN addresses;
- the explicitly named primary agrees with any registered DB authority;
- a checksummed, restore-verified PostgreSQL backup is less than 24 hours old;
- Docker Compose and this checkout's `deploy/` directory exist on the target;
- `POSTGRES_REPLICATION_PASSWORD` is in the target daemon environment;
- primary TCP port 55432 accepts the `replicator` role from the target;
- target free space is at least twice the current database size.

```sh
ff fleet db replica plan --to <target> --primary <primary>
ff fleet db replica apply --to <target> --primary <primary> --plan-id <id> --yes
```

Review the plan's target, primary, deterministic physical slot, PostgreSQL
major, backup evidence, and plan ID before applying. Retries reuse the slot and
healthy standby PGDATA. Partial PGDATA or primary PGDATA is refused and never
deleted; this workflow intentionally has no implicit wipe/reseed operation.

Apply succeeds only after recovery mode, read-only mode, a streaming WAL
receiver, a replay LSN, an active primary slot, and lag below 1 GiB are proven.
Only then is `database_replicas` updated. Inspect ongoing health via `ff top`,
`pg_stat_replication`, and `pg_replication_slots`.

Forced backups are dispatched to `fleet_backup_config.source_host`; when it is
unset they run on the current PostgreSQL authority, never merely the fleet
leader. Existing per-node freshness alerts surface missing, stale, or
undistributed backup copies and should be resolved before recovery work.

Rollback is non-destructive: stop the follower Compose service. Do not drop its
slot until the node is intentionally decommissioned and PGDATA is preserved or
removed through a separately approved reseed procedure.
