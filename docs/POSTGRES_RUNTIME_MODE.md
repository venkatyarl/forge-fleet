# Postgres Runtime Registry Mode (Phase 37A)

## What this adds

ForgeFleet now supports a **config-driven runtime persistence mode**:

- `embedded_sqlite` (default): everything remains in local SQLite
- `postgres_runtime`: runtime registry + enrollment events are written to Postgres
- `postgres_full`: target final mode for full Postgres cutover (preflight-guarded; startup requires explicit cutover evidence and a valid Postgres URL)

`postgres_runtime` remains the explicit **transitional mode** to make the running Docker `forgefleet-postgres` instance usable as primary runtime persistence for live node state.

---

## Current persistence split (explicit)

### Postgres-backed in this phase (`postgres_runtime`)

- Runtime registry tables:
  - `fleet_node_runtime` (live node runtime / heartbeat state)
  - `fleet_enrollment_events` (enrollment accept/reject history)
- OperationalStore-backed domains used by daemon and MCP handlers:
  - tasks + task results + task ownership
  - audit log + config KV
  - autonomy events + telegram media ingest
  - node/task operational views used by gateway/agent paths

### Embedded-only helper domains (non-blocking for `postgres_full`)

- ff-db replication/backup helpers remain SQLite-backup-API based **by design** and are now
  explicitly scoped to `embedded_sqlite` mode only.
- In `postgres_runtime`/`postgres_full`, these helpers are disabled; use Postgres-native
  backup/replication controls for DR and cutover operations.

`ff-cron` persistence is now OperationalStore-backed (SQLite + Postgres compatible), so it is no longer a `postgres_full` blocker.

`ff-mc` now has an OperationalStore-backed runtime path for core mission-control workflows
(work item lifecycle + review/dependency + board/dashboard endpoints) when Postgres-backed
modes are active.

This keeps `postgres_runtime` as a transitional mode for rollout safety, not because of remaining hard preflight blockers.

---

## Configuration

In `~/.forgefleet/fleet.toml`:

```toml
[database]
mode = "postgres_runtime"
url = "postgresql://forgefleet:forgefleet@127.0.0.1:55432/forgefleet"
max_connections = 10

# Optional: override embedded SQLite file for legacy/core tables
# sqlite_path = "./forgefleet.db"
```

To force SQLite-only mode:

```toml
[database]
mode = "embedded_sqlite"
```

Target full cutover mode (guarded):

```toml
[database]
mode = "postgres_full"
url = "postgresql://forgefleet:forgefleet@127.0.0.1:55432/forgefleet"
cutover_evidence = "CHANGE-12345" # required: backup + validation proof reference
```

`postgres_full` startup preflight enforces cutover safety gates (`[database].url` +
`[database].cutover_evidence`).
Do **not** delete SQLite files before checklist completion and rollback readiness.

### Environment overrides

- `FORGEFLEET_DATABASE_MODE` (`embedded_sqlite` | `postgres_runtime` | `postgres_full`)
- `FORGEFLEET_DATABASE_URL`
- `FORGEFLEET_DATABASE_MAX_CONNECTIONS`
- `FORGEFLEET_DATABASE_SQLITE_PATH`
- `FORGEFLEET_DATABASE_CUTOVER_EVIDENCE`

---

## Startup logging

On boot, `forgefleetd` logs the active database mode clearly:

- `mode=embedded_sqlite` when SQLite-only
- `mode=postgres_runtime` when runtime tables are Postgres-backed
- `mode=postgres_full` when full cutover mode is active

For `postgres_runtime`, logs call out Postgres-backed runtime/operational persistence (including mission-control runtime routes) and explicitly report that ff-db sqlite snapshot helpers are disabled in Postgres-backed modes.

For `postgres_full`, startup runs a mandatory preflight:

- requires non-empty Postgres URL
- requires `cutover_evidence`

---

## Full cutover checklist (required before `postgres_full`)

Use: `docs/checklists/POSTGRES_FULL_CUTOVER_CHECKLIST.md`

Implementation inventory (Phase 38A):
- `docs/PHASE_38A_SQLITE_POSTGRES_INVENTORY.md`

The checklist defines:
- SQLite backup + checksum capture
- migration/validation evidence gates
- config switch + endpoint/UI verification
- rollback procedure

No SQLite file deletion is allowed without completed evidence.

---

## Verify against Docker `forgefleet-postgres`

## 1) Confirm container is running

```bash
docker ps --format '{{.Names}}\t{{.Ports}}' | grep forgefleet-postgres
```

## 2) Confirm Postgres connectivity

```bash
docker exec -it forgefleet-postgres \
  psql -U forgefleet -d forgefleet -c 'select now();'
```

## 3) Start ForgeFleet with `mode = "postgres_runtime"`

```bash
cd ~/taylorProjects/forge-fleet
cargo run --bin forgefleetd -- --config ~/.forgefleet/fleet.toml start
```

Watch startup logs for `mode=postgres_runtime`.

## 4) Trigger runtime writes

- send `POST /api/fleet/heartbeat`, or
- enroll a node via `POST /api/fleet/enroll`

## 5) Inspect Postgres tables

```bash
docker exec -it forgefleet-postgres \
  psql -U forgefleet -d forgefleet -c '\dt fleet_*'

docker exec -it forgefleet-postgres \
  psql -U forgefleet -d forgefleet -c 'select node_id, hostname, reported_status, last_heartbeat from fleet_node_runtime order by updated_at desc limit 20;'

docker exec -it forgefleet-postgres \
  psql -U forgefleet -d forgefleet -c 'select id, node_id, outcome, created_at from fleet_enrollment_events order by id desc limit 20;'
```

---

## Implementation notes

- Runtime registry persistence is abstracted by `ff_db::RuntimeRegistryStore`.
- Gateway runtime endpoints (`/api/fleet/enroll`, `/api/fleet/heartbeat`) use this store.
- Fleet status assembly reads runtime rows from this store, so Postgres-backed runtime visibility appears in `/api/fleet/status`.

No placeholder behavior: Postgres schema is created automatically on startup when `postgres_runtime` is enabled.
