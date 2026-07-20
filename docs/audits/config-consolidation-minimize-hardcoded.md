# Config Consolidation: Minimize Hardcoded Files → DB

**Date:** 2026-07-20
**Status:** Audit complete. Decomposed into leaf tasks below — ready for individual dispatch.
**Retry context:** This work item (`config-consolidation-minimize-hardcoded`) failed twice before
under a single dispatch: "repeated no-diff across lanes — too large/vague for one harness pass."
Root cause: the item asked a harness to both *discover* the scope of hardcoded config across a
~200k-LOC, 31-crate workspace *and* migrate it, in one pass, with no named files or acceptance
criteria — an unbounded research task masquerading as a code change, so most lanes produced no
diff. This report fixes that by doing the discovery once, here, and splitting the remaining work
into named, independently-verifiable leaf tasks. Do not re-dispatch the original item as-is;
dispatch the leaf tasks in section 4 instead.

## 1. Where things already stand

Config consolidation is **already substantially underway** — it is not a greenfield problem:

- `fleet_settings` (`crates/ff-db/src/schema.rs:333`, `key`/`value JSONB`/`updated_at`) was built
  specifically to replace `[general]`, `[scheduling]`, `[ports]`, `[llm]`, `[enrollment]` TOML
  sections. It exists but is under-populated relative to what still lives in Rust consts.
- `config/model_catalog.toml` was **already retired** in migration `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML`
  (`crates/ff-db/src/migrations.rs:189-190`) — the catalog now seeds directly into the `model_catalog`
  table (`crates/ff-db/src/schema.rs:954`). `crates/ff-agent/src/model_catalog.rs` and
  `model_catalog_seed.rs` are now dead weight: `sync_catalog()` is a no-op, `load_catalog_file()`
  returns `Ok(vec![])` since the file doesn't exist, and `DEFAULT_CATALOG_PATH` still points at a
  stale absolute dev path (`/Users/venkat/projects/forge-fleet/config/model_catalog.toml`).
- `fleet_secrets` (`crates/ff-db/src/schema.rs:736`) already doubles as a general config-flag store
  (e.g. `port.gateway`, `autoscaler_mode`, `disk_policy_mode`, `conformance_mode`).
- `config_kv` (`crates/ff-db/src/schema.rs:151`) is a **legacy, pre-`fleet_settings`** KV table —
  likely a consolidation target itself (merge into `fleet_settings` or confirm dead and drop).

**Correction to CLAUDE.md:** it currently states `fleet_model_catalog` is "populated from
`config/model_catalog.toml`" — that's stale, describing the superseded V14 flow. It should
reference the V39 retirement and the current `model_catalog` table instead (leaf task 4.7).

## 2. Inventory of remaining hardcoded config

### 2a. Rust `const`/`static` tables worth moving to DB (human-edited, not build-time)

| Table | Location | What it holds |
|---|---|---|
| `OAUTH_PROVIDERS` | `crates/ff-agent/src/oauth_distributor.rs:62` | Per-CLI OAuth cred-file paths/secret keys (claude/codex/gemini/kimi) — comment flags it "best-guess, verify and update" |
| `BACKENDS` | `crates/ff-agent/src/cli_executor.rs:89` | CLI backend table: binary name, port, flags, cwd mode |
| `MIRROR_SOURCES` | `crates/ff-agent/src/brain_mirror.rs:44` | Per-CLI memory-mirror source dirs |
| `KIMI_CONFIG_FILES` | `crates/ff-agent/src/config_distributor.rs:32` | List of Kimi config file paths to distribute |
| `ALLOWED_LICENSES` | `crates/ff-agent/src/model_scout.rs:41` | License allowlist for model discovery |
| `EVALUATOR_METRICS`, `COMPUTER_STATUS_VALUES`, `IMPERATIVE_METRICS` | `crates/ff-agent/src/alert_evaluator.rs:168,186,193` | Metric/threshold routing tables |
| `EXCLUDE_HOSTS` | `crates/ff-agent/src/autoscaler.rs:74` | Node exclusion list for autoscale churn (`&["taylor"]`) |
| `VISION_MODEL_PREFS` | `crates/ff-agent/src/social_ingest/mod.rs:40` | Preferred vision-model list |
| `DAEMON_GATES` | `crates/ff-terminal/src/fleet_cmd.rs:5848` | Every daemon subsystem gate key + default + description |
| `COMMANDS` | `crates/ff-gateway/src/telegram_commands.rs:24` | Full Telegram bot command table |
| `TASK_DEFINITIONS` | `crates/ff-gateway/src/tasks.rs:60` | Task→capabilities→system-prompt routing table |
| `DEFAULT_PATHS`, `DEFAULT_PATTERNS` | `crates/ff-agent/src/log_analysis_worker.rs:34-35` | Log-scan glob/pattern defaults |

Tables intentionally **excluded** from consolidation (keep as code, not data):
- `LLM_MODEL_PORTS`/`LLAMA_CPP_PORTS`/`OLLAMA_PORT` (`crates/ff-discovery/src/ports.rs`) — protocol
  conventions, not operator-tunable.
- `VALID_TAGS`, `BLOCKED_PATHS`, `INTERPRETERS`, `BLOCK_PREFIXES`, `PUBLIC_ROUTES` — security
  allow/denylists. Moving security-relevant lists to a runtime-editable DB row is a security
  regression (an attacker or bad automation could widen the allowlist at runtime); leave in code
  and add DB config only if there's a real operational need to tune them without a redeploy.
- `TABLES` (`crates/ff-db/src/schema.rs:778`) — already stale/legacy; candidate for **deletion**,
  not migration (leaf task 4.8).
- `MEMORY_BLOCKS`, `WORKLOAD_SYNONYM_CLUSTERS` (`crates/ff-db/src/queries.rs`) — taxonomy fixed by
  code that pattern-matches on them; moving to DB without also making the matching logic
  data-driven would just add a network round trip for no flexibility gain. Out of scope here.

### 2b. Env vars with scattered hardcoded defaults (candidates for `fleet_settings` fallback rows)

Roughly 110 distinct `FORGEFLEET_*`/`FF_*` vars across ~291 call sites. Highest-value targets:
`FF_API_PORT`, `FF_API_HOST`, `FF_GATEWAY_URL`, `FF_LEADER_URL`, `FORGEFLEET_LEADER_HOST/PORT`,
`FF_EMBEDDING_ENDPOINT`, `FF_EMBEDDING_MODEL`, `FF_RERANK_ENDPOINT`,
`FORGEFLEET_TELEGRAM_BOT_TOKEN/OWNER/ALLOWED_CHATS`. These are read in many files with inline
defaults (`unwrap_or_else(|| "8080".into())`-style) rather than one lookup table — worth a
follow-up audit of its own once `fleet_settings` has a stable read helper (see leaf task 4.4),
but is **not** included as a leaf task below because it needs its own file-by-file inventory.

### 2c. Must stay file-based (bootstrap-order constraint — do not touch)

- `FORGEFLEET_DATABASE_URL` / `FORGEFLEET_POSTGRES_URL` env vars (chicken-and-egg: needed to reach
  the DB at all).
- `config/patroni.yaml` — Postgres HA bootstrap, runs before Postgres itself is up.
- `rust-toolchain.toml`, root and per-crate `Cargo.toml` — build-time only.
- `deploy/docker-compose*.yml` — provisions the containers the DB runs in.
- `FORGEFLEET_ENROLLMENT_TOKEN`, `FORGEFLEET_PGCAT_ADMIN_USER/PASSWORD` — bootstrap credentials to
  reach the DB/pooler.
- `FORGEFLEET_NODE_NAME` env override — first step in `resolve_this_worker_name()`
  (`crates/ff-agent/src/fleet_info.rs:184`) before a `fleet_workers` DB lookup is even possible.
- `.mcp.json`, `.opencode.json`, `deploy/claude-code-mcp-config.json`, `.github/workflows/*.yml` —
  local dev-tool/CI bootstrapping, precede any app DB connection.

## 3. Why the previous dispatch attempts produced no diff

The item as originally written had no file list and no "done" signal a single-pass harness could
check — "minimize hardcoded files to DB" is a direction, not a task. Each of the tables in 2a
requires: (a) a new/extended DB table or `fleet_settings` key schema, (b) a migration, (c) a Rust
loader replacing the const with a DB read + cached fallback, and (d) call-site updates — that's
real per-table work, not a mechanical rename, so a harness given the whole list at once had no
single coherent diff to produce and defaulted to a no-op. Fixing this means dispatching one table
(or a tightly related pair) per leaf task, each naming its exact file(s) and a verifiable output.

## 4. Leaf tasks (dispatch individually, in this order)

1. **Delete dead model-catalog-file code path.** Remove `DEFAULT_CATALOG_PATH`,
   `load_catalog_file()`, and the now-inert `model_catalog_seed.rs` from
   `crates/ff-agent/src/model_catalog.rs` (or fold `sync_catalog()` down to a documented no-op
   stub if external callers still invoke `ff model sync-catalog`). Verifiable output: `cargo check`
   clean, no references to the retired TOML path remain (`grep -r model_catalog.toml crates/`
   returns nothing outside migration history/docs).
2. **Migrate `EXCLUDE_HOSTS` autoscaler denylist to `fleet_settings`.** Smallest table (single
   `&["taylor"]`), good pilot for the read-path pattern. Add a `fleet_settings` row
   (`key = 'autoscaler.exclude_hosts'`, JSONB array), a loader in
   `crates/ff-agent/src/autoscaler.rs` that reads it with the current array as the fallback
   default, migration in `crates/ff-db/src/migrations.rs` (next version int). Verifiable output:
   new migration compiles, unit test confirms the loader returns the const default when the row is
   absent and the DB value when present (guarded per the DB-test rule — early return without
   `FORGEFLEET_POSTGRES_URL`/`FORGEFLEET_DATABASE_URL`).
3. **Migrate `DAEMON_GATES` to `fleet_settings`.** `crates/ff-terminal/src/fleet_cmd.rs:5848` — one
   row per gate key already resembles a KV table; this is close to a direct port. Verifiable
   output: `ff daemon` gate lookups read `fleet_settings` first, fall back to the const table,
   migration seeds current defaults so behavior is unchanged post-migration.
4. **Build one shared `fleet_settings` typed-read helper** in `crates/ff-db` (e.g.
   `get_setting_json<T>(key, default) -> T`) so tasks 2, 3, and future migrations don't each
   hand-roll JSONB parsing. Verifiable output: helper function + unit test, used by tasks 2 and 3
   (do this one first if sequencing allows, since 2 and 3 depend on it existing).
5. **Migrate `TASK_DEFINITIONS`** (`crates/ff-gateway/src/tasks.rs:60`) to DB. Larger/nested
   structure (task→capabilities→prompt) — needs its own table, not a `fleet_settings` JSONB blob,
   since it's queried/filtered, not just read whole. Verifiable output: new
   `gateway_task_definitions` table + migration + loader + one integration test round-tripping a
   row.
6. **Migrate `COMMANDS` Telegram bot table** (`crates/ff-gateway/src/telegram_commands.rs:24`) to
   DB, same shape as task 5.
7. **Fix stale CLAUDE.md model-catalog description** — replace the "populated from
   `config/model_catalog.toml`" line with a reference to `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML`
   and the current `model_catalog` table. Verifiable output: doc line updated, no code change.
8. **Delete or repopulate stale `TABLES` const** (`crates/ff-db/src/schema.rs:778`, 16 of 200+
   actual tables) and decide fate of legacy `config_kv` (`schema.rs:151`) — confirm via
   `ff db query` whether anything still reads `config_kv` live; if dead, add a migration dropping
   it, if live, migrate its rows into `fleet_settings` and then drop it. Verifiable output: `ff db
   query "SELECT count(*) FROM config_kv"` documented in the migration commit message, plus the
   drop migration itself.

Tasks 2-3-4 are the recommended next dispatch batch: task 4 is a small shared-infra PR, and 2/3
are the two most self-contained, lowest-risk consolidations that exercise it end-to-end. Tasks 5-6
are larger (need new tables, not just `fleet_settings` rows) and should follow once the pattern
from 2-4 is proven. Tasks 1, 7, 8 are independent cleanup and can be dispatched any time.

## 5. Explicitly out of scope

- Env var consolidation (2b) — needs its own file-by-file audit before it can be split into leaf
  tasks; flagged here, not decomposed.
- Security allow/denylists (`BLOCKED_PATHS`, `INTERPRETERS`, `PUBLIC_ROUTES`, etc.) — stay in code
  per 2a rationale.
- Anything in section 2c (bootstrap-order constraint).
