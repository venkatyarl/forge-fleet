# Postgres Migrations

## How migrations work

ForgeFleet's Postgres schema is managed by an embedded migration runner in
`ff-db` (`crates/ff-db/src/migrations.rs`, SQL constants in
`crates/ff-db/src/schema.rs`):

- Migrations are SQL strings embedded in Rust, registered in the
  `PG_MIGRATIONS` list with a strictly increasing integer version.
- Applied versions are tracked in the `_migrations` meta-table
  (`version`, `name`, `applied_at`); the runner applies only versions
  greater than `MAX(version)`.
- Concurrent runners (daemon startup racing an `ff` subcommand) are
  serialized via a session-level Postgres advisory lock, so they can never
  collide on the `_migrations` primary key.
- Migrations are **forward-only**: never edit an existing migration const;
  add ONE new const and register it at the end of `PG_MIGRATIONS` with the
  next free integer version. Check the highest version claimed across all
  in-flight branches first — versions get reserved (e.g. V164), and a
  collision is only caught at merge time by the
  `migration_versions_are_strictly_increasing` unit test.

> ⚠️ **Two schema systems.** `ff-mc` (mission control) bootstraps its OWN
> schema in `crates/ff-mc/src/db.rs` (the PM `work_items` / `projects` /
> `milestones` tables live there, NOT in ff-db migrations). Always
> `ff db query` to confirm a table's live name + columns before extending it.

---

## v161 is the baseline for fresh DB bootstrap

The legacy v7 → v161 migration chain **cannot replay on a fresh empty
Postgres**: it accumulated rename/renumber drift and only ever ran as
in-place forward migrations on the original primary. As of DR 2026-07-16,
fresh databases bootstrap from a squashed baseline instead:

- **`deploy/sql/bootstrap-v161.sql`** — a single idempotent script that
  builds the final v161 schema (158 tables) in one pass on an empty
  database, then seeds `_migrations` with every version v7..v161 marked
  applied (`ON CONFLICT (version) DO UPDATE`, so re-running is safe).
- After the bootstrap, the embedded runner takes over and applies only the
  post-baseline migrations (v162+) forward, exactly as before.

### Bootstrapping a fresh database

```bash
# 1. Apply the squashed v161 baseline to the empty database.
psql "$FORGEFLEET_POSTGRES_URL" -f deploy/sql/bootstrap-v161.sql

# 2. Start any ff binary (daemon or CLI) — the embedded runner applies
#    everything after v161 automatically.
ff db query "SELECT MAX(version) FROM _migrations"
```

### Rules that follow from the baseline

| Rule | Why |
|------|-----|
| New migrations still go at the end of `PG_MIGRATIONS` (v162+), never into the baseline. | The baseline is a snapshot of v161, not a living schema file. |
| A fix to schema state ≤ v161 must be mirrored in `bootstrap-v161.sql` if a later migration patches it. | Otherwise a fresh-DB rebuild from the baseline recreates the stale state (see the v162 `drop_worktree_path_unique` mirror). |
| Existing databases already at ≥ v161 never run the bootstrap. | Their `_migrations` table already records v7..v161; the runner only looks forward. |
| The legacy v7..v161 consts remain in `schema.rs` / `PG_MIGRATIONS` for history but are effectively retired. | Fresh installs never execute them; removal is tracked separately. |
