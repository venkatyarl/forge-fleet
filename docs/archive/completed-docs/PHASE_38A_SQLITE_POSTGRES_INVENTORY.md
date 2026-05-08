# Phase 38A — SQLite → Postgres Inventory (Current State)

Last updated: 2026-04-05

## Purpose

This is the implementation-level inventory for the SQLite→Postgres migration plan.
It captures what is:

- SQLite-only
- dual-backed in code but not yet wired
- actively wired to Postgres today

Use this with:
- `docs/POSTGRES_RUNTIME_MODE.md`
- `docs/checklists/POSTGRES_FULL_CUTOVER_CHECKLIST.md`

---

## 1) Mode selection and guardrails

### Database mode selector

Defined in `crates/ff-core/src/config.rs`:
- `embedded_sqlite` (default)
- `postgres_runtime` (runtime registry/enrollment only)
- `postgres_full` (target final mode, guarded)

Environment overrides:
- `FORGEFLEET_DATABASE_MODE`
- `FORGEFLEET_DATABASE_URL`
- `FORGEFLEET_DATABASE_MAX_CONNECTIONS`
- `FORGEFLEET_DATABASE_SQLITE_PATH`
- `FORGEFLEET_DATABASE_CUTOVER_EVIDENCE`

### Startup preflight (postgres_full)

`src/main.rs` enforces a hard preflight for `postgres_full` with explicit cutover safety gates.

Current preflight requirements in code:
- non-empty `[database].url`
- non-empty `[database].cutover_evidence`

Update (post-Phase 38A follow-up): `ff-cron` persistence and `ff-mc` runtime mission-control paths now have OperationalStore-backed modes for Postgres-compatible runtimes, and ff-db snapshot backup/replication helpers are explicitly embedded-only (not a `postgres_full` startup blocker).

---

## 2) Storage abstractions and actual wiring

## `ff-db::RuntimeRegistryStore` (wired)

File: `crates/ff-db/src/runtime_registry.rs`

Dual backend support:
- SQLite
- Postgres

Scope in use today:
- `fleet_node_runtime`
- `fleet_enrollment_events`

Wired from:
- `src/main.rs` (`initialize_runtime_registry`)
- `crates/ff-gateway/src/server.rs` (enroll/heartbeat + fleet snapshot read path)

Status: **ACTIVE Postgres path in `postgres_runtime` mode**.

## `ff-db::OperationalStore` (wired)

File: `crates/ff-db/src/operational_store.rs`

Provides dual-backend operational persistence for tasks, ownership leases, audit/config, autonomy events, telegram ingest, and node metadata.

Runtime wiring now includes:
- `src/main.rs` startup initialization for `embedded_sqlite` vs `postgres_runtime`/`postgres_full`
- `ff-agent` autonomous task/ownership/audit/autonomy paths
- `ff-gateway` and telegram transport operational writes
- `ff-mcp` audit + config KV + project profile persistence (stored in `config_kv`)

Status: **ACTIVE runtime integration (SQLite and Postgres backends)**.

---

## 3) SQLite pragmas / engine-specific behavior

SQLite pragmas explicitly set in:

- `crates/ff-db/src/connection.rs`
  - `journal_mode=WAL`
  - `synchronous=NORMAL`
  - `busy_timeout`
  - `cache_size`
  - `mmap_size`
  - `foreign_keys=ON`
  - `temp_store=MEMORY`
  - `page_size=4096`

- `crates/ff-cron/src/persistence.rs`
  - `foreign_keys=ON`
  - `busy_timeout=5000ms`

- `crates/ff-mc/src/db.rs`
  - `PRAGMA journal_mode=WAL`
  - `PRAGMA foreign_keys=ON`

Replication/backup are SQLite-backup-API based:
- `crates/ff-db/src/replication.rs`
- `crates/ff-db/src/backup.rs`
- `crates/ff-db/src/sync.rs`

---

## 4) ff-db table inventory (core schema)

Canonical table list from `crates/ff-db/src/schema.rs`.

| Table | SQLite implementation | Postgres implementation | Runtime wiring status |
|---|---|---|---|
| `fleet_node_runtime` | `queries.rs` + sqlite migrations | `RuntimeRegistryStore` Postgres | **Wired** (gateway enroll/heartbeat/fleet status) |
| `fleet_enrollment_events` | `queries.rs` + sqlite migrations | `RuntimeRegistryStore` Postgres | **Wired** |
| `nodes` | `queries.rs` | `OperationalStore` (`upsert_node`, `list_nodes`) | Not wired |
| `tasks` | `queries.rs` | `OperationalStore` (`insert/get/list/claim/set`) | Not wired |
| `task_results` | `queries.rs` | `OperationalStore` (`record_task_result`) | Not wired |
| `task_ownership` | `queries.rs` | `OperationalStore` (`ownership_claim/release`) | Not wired |
| `ownership_events` | `queries.rs` | Postgres DDL in `OperationalStore`; no dedicated methods exposed yet | Not wired |
| `autonomy_events` | `queries.rs` | `OperationalStore` (`insert/list_recent`) | Not wired |
| `telegram_media_ingest` | `queries.rs` | `OperationalStore` (`insert`) | Not wired |
| `audit_log` | `queries.rs` | `OperationalStore` (`audit_log`, `recent_audit_log`) | Not wired |
| `config_kv` | `queries.rs` | `OperationalStore` (`config_set/get/delete/list_prefix`) | Wired (daemon + ff-mcp) |
| `models` | SQLite schema + migration tool SQL | Postgres DDL in `OperationalStore`; no store methods | Not wired |
| `memories` | `queries.rs` | Postgres DDL in `OperationalStore`; no store methods | Not wired |
| `sessions` | `queries.rs` | Postgres DDL in `OperationalStore`; no store methods | Not wired |
| `cron_jobs` | `queries.rs` | Postgres DDL in `OperationalStore`; no store methods | Not wired |
| `cron_runs` | `queries.rs` | Postgres DDL in `OperationalStore`; no store methods | Not wired |

Notes:
- `ff_db::queries` has no non-test callsites for session APIs (`insert_session/get_session/find_active_sessions/touch_session/close_session`) yet.
- `ff_db::queries` memory read/search/purge functions currently have no non-test runtime callsites (only migration tooling writes `memories`).

---

## 5) Remaining direct SQLite-oriented callsites (runtime + tooling)

After OperationalStore wiring, the high-volume runtime/operational paths now route through store abstractions.

Remaining direct SQLite-oriented usage is concentrated in:

- `tools/migrate_from_postgres.rs`
  - explicit migration utility targeting SQLite outputs (`forgefleet.db` / optional MC DB)
- sqlite-native helper modules intentionally scoped to embedded mode (`ff-db` replication/backup helpers)

This narrows runtime blocker scope and keeps Postgres cutover preflight focused on evidence/safety gates.

---

## 6) Other sqlite-only domains outside ff-db core

## ff-cron

`crates/ff-cron/src/persistence.rs` routes persistence through `ff_db::OperationalStore`
while retaining `embedded_sqlite` compatibility.

Status: **Postgres-compatible via OperationalStore (no longer sqlite-only blocker)**.

## ff-mc (Mission Control)

`crates/ff-mc/src/db.rs` and legacy domain modules use `rusqlite` directly.
Tables include:
- `work_items`, `review_items`, `work_item_dependencies`, `task_groups`
- `epics`, `sprints`
- `legal_entities`, `compliance_obligations`, `filings`
- `companies`, `projects`, `project_repos`, `project_environments`

New runtime path:
- `crates/ff-mc/src/operational_api.rs` persists core mission-control workflows into
  `OperationalStore::config_kv` keys (`ff_mc.work_item.*`, `ff_mc.review_item.*`,
  `ff_mc.dependency.*`) for Postgres-backed modes.
- `crates/ff-gateway/src/server.rs` mounts this OperationalStore router when Postgres is active
  (or when no sqlite mission-control DB path is configured).

Status: **Hybrid (legacy sqlite + operational-store-backed runtime path for Postgres modes)**.

## ff-mcp handlers

`crates/ff-mcp/src/handlers.rs` now uses `ff_db::OperationalStore` for persistence.

Current behavior:
- respects configured database mode (`embedded_sqlite`, `postgres_runtime`, `postgres_full`)
- persists `audit_log` via OperationalStore
- persists quality snapshots in `config_kv`
- persists project profiles as `config_kv` records with `ff_mcp.project_profile.*` keys (no sqlite-only side table)
- retains sqlite candidate fallback only when fleet config cannot be loaded

Status: **Dual-backed via OperationalStore (no direct rusqlite handler path)**.

---

## 7) Postgres-only memory domain (separate from ff-db memories)

`crates/ff-memory/src/store.rs` is `sqlx::PgPool` based and manages a distinct Postgres `memories` schema (`workspace_id`, tags as `TEXT[]`, etc.).

This is **not** a drop-in backend replacement for `ff_db::queries::memories` shape.

---

## 8) TODO/FIXME migration markers

Repository scan found **no TODO/FIXME/XXX markers** explicitly tracking sqlite→postgres cutover work.

Recommendation: continue tracking operational cutover evidence and rollback readiness explicitly in checklist docs (and optionally CI checks), not only through startup preflight.

---

## 9) Recommended location for this inventory

Primary home:
- `docs/PHASE_38A_SQLITE_POSTGRES_INVENTORY.md` (this file)

Cross-links:
- `docs/POSTGRES_RUNTIME_MODE.md` (mode semantics + operational runbook)
- `docs/INDEX.md` (canonical docs entrypoint)

This keeps implementation inventory separate from runbook/checklist docs while making it discoverable.
