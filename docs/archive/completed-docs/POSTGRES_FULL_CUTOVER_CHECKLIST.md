# Postgres Full Cutover Checklist (Safety-First)

> Scope: Safe activation path for `database.mode = "postgres_full"`.
>
> Non-goal: Deleting SQLite files during cutover execution.

---

## 0) Preconditions (must be true before cutover)

- [ ] Transitional mode (`postgres_runtime`) is already running cleanly.
- [ ] Postgres backup/restore process has been tested.
- [ ] Fleet health baseline captured (API + dashboard + enrollment + heartbeat).
- [ ] On-call owner and rollback owner identified.
- [ ] Maintenance window and communication plan approved.

Record:
- Change/Ticket ID: `________________`
- Cutover operator: `________________`
- Date/time (local + UTC): `________________`

---

## 1) Evidence package (required)

Create a cutover evidence folder (example):

```bash
mkdir -p ~/.forgefleet/cutover-evidence/$(date +%Y%m%d-%H%M%S)-postgres-full
```

Set `EVIDENCE_DIR` to that folder and capture all artifacts below.

- [ ] Runbook/checklist copy saved.
- [ ] Command transcript captured.
- [ ] Pre/post verification output captured.
- [ ] Rollback command bundle prepared.

Final evidence identifier to write into config:
`[database].cutover_evidence = "________________"`

---

## 2) SQLite backup (mandatory, non-destructive)

1. Locate active SQLite DB path (from current config/logs).
2. Stop writer services or enter maintenance mode.
3. Create immutable backup copy + checksum.

Example commands:

```bash
# Replace with actual DB path from your current mode
SQLITE_DB="~/.forgefleet/forgefleet.db"

cp "$SQLITE_DB" "$EVIDENCE_DIR/forgefleet-precutover.db"
shasum -a 256 "$EVIDENCE_DIR/forgefleet-precutover.db" > "$EVIDENCE_DIR/forgefleet-precutover.db.sha256"
```

- [ ] Backup file exists.
- [ ] SHA256 checksum captured.
- [ ] Backup path stored in ticket/evidence log.

---

## 3) Data migration + validation gates

If any SQLite-resident data still matters for production behavior:

- [ ] Run migration/export/import tooling as applicable.
- [ ] Verify row counts for critical tables.
- [ ] Verify key functional queries against Postgres.
- [ ] Capture before/after diffs and signoff.

Minimum validation examples:

- [ ] runtime registry rows present and updating
- [ ] enrollment event writes + reads working
- [ ] mission-critical historical data (if required) is available in target store

Store outputs under `$EVIDENCE_DIR/validation/`.

---

## 4) Config switch (controlled)

Update `fleet.toml`:

```toml
[database]
mode = "postgres_full"
url = "postgresql://forgefleet:forgefleet@127.0.0.1:55432/forgefleet"
max_connections = 10
cutover_evidence = "<ticket-or-artifact-reference>"
```

- [ ] `cutover_evidence` is non-empty and points to real artifacts.
- [ ] Config committed/audited according to team process.

> Note: ForgeFleet startup preflight in `postgres_full` now enforces explicit safety gates
> (`[database].url` and `[database].cutover_evidence`).
> ff-db replication/backup helpers are intentionally `embedded_sqlite`-only and are disabled in
> Postgres-backed modes; use Postgres-native backup/replication tooling for DR/cutover evidence.
> `ff-cron` persistence and `ff-mc` mission-control runtime routes are OperationalStore-backed in Postgres modes.

---

## 5) Post-switch verification (API/UI/runtime)

After restart attempt, verify:

- [ ] `/health` returns OK
- [ ] `/api/fleet/status` returns expected node data
- [ ] heartbeat endpoint persists and reads expected runtime values
- [ ] enrollment endpoint writes and lists events
- [ ] dashboard fleet status renders correctly
- [ ] no hidden SQLite-write dependency errors in logs

Capture:
- [ ] API responses
- [ ] key log excerpts
- [ ] dashboard screenshots (if used for signoff)

---

## 6) Rollback plan (must be ready before cutover)

Rollback trigger examples:
- startup preflight fails (missing/invalid cutover evidence or Postgres connectivity/config)
- data mismatch / missing critical state
- error-rate or latency regression beyond agreed threshold

Rollback steps:

1. Set config back to transitional mode:

```toml
[database]
mode = "postgres_runtime"
```

2. Restart ForgeFleet services.
3. Re-verify health + endpoints + dashboard.
4. If required, restore SQLite backup copy from `$EVIDENCE_DIR`.

Rollback completion checks:
- [ ] Service healthy in previous known-good mode
- [ ] Critical write/read paths restored
- [ ] Incident note posted with failure details

---

## 7) SQLite deletion policy (explicitly prohibited here)

- [ ] Do **not** delete SQLite DB/WAL/SHM files during this cutover.
- [ ] Keep backups until post-cutover stability period is complete.
- [ ] Deletion requires separate approved procedure + signoff.

---

## 8) Signoff

- [ ] Ops signoff
- [ ] Data signoff
- [ ] Runtime/platform signoff
- [ ] Rollback readiness signoff

Final outcome:
- [ ] GO (full cutover active)
- [ ] NO-GO (reverted to transitional)

Notes:
`____________________________________________________________________`
