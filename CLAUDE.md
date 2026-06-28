<!-- code-review-graph MCP tools -->
## MCP Tools: code-review-graph

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
code-review-graph MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `semantic_search_nodes` or `query_graph` instead of Grep
- **Understanding impact**: `get_impact_radius` instead of manually tracing imports
- **Code review**: `detect_changes` + `get_review_context` instead of reading entire files
- **Finding relationships**: `query_graph` with callers_of/callees_of/imports_of/tests_for
- **Architecture questions**: `get_architecture_overview` + `list_communities`

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### ⛔ Discovery-first — search before you build (hard rule)
Before writing ANY new table/module/feature, inventory what ALREADY exists — the #1 waste here
is rebuilding a capability the fleet already has (e.g. a whole PM system already lives in `ff-mc`;
a separate work-stealing system lives in `ff-agent`). Order is non-negotiable:
1. **Cortex / CRG first** — `cortex_find` / `semantic_search_nodes` ("what handles work_items?"
   → instantly points to the owning crate). Cheaper + faster than grep.
2. **`ff db query "<read-only SQL>"`** — confirm the LIVE schema. Source `CREATE TABLE` strings
   can DRIFT from the live DB (see schema caveat below); never `ALTER` a table you haven't
   confirmed live.
3. **`brain_search`** for prior decisions; **grep/Read last**.
Then reuse/extend what exists instead of forking. If Cortex can't answer "do we already have
this?", that's a Cortex gap to fix — improve it, don't route around it.

### Key Tools

| Tool | Use when |
|------|----------|
| `detect_changes` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context` | Need source snippets for review — token-efficient |
| `get_impact_radius` | Understanding blast radius of a change |
| `get_affected_flows` | Finding which execution paths are impacted |
| `query_graph` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes` | Finding functions/classes by name or keyword |
| `get_architecture_overview` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `detect_changes` for code review.
3. Use `get_affected_flows` to understand impact.
4. Use `query_graph` pattern="tests_for" to check coverage.

---

## Working through ff (dogfood + key verbs)

**Dogfood ff for real work.** Route work through ff (`ff run`, `ff supervise`, `ff offload`,
`ff research`, `fleet_crew` MCP) rather than raw `codex`/`kimi`/`claude -p`. Two reasons: (1) it
surfaces ff's own bugs — fix them in source, don't work around them; (2) every call routed through
ff is logged to `ff_interactions` (req + resp + worker + endpoint + tokens) — the training corpus
for ForgeFleet's own LLM. A raw `codex exec` does the work but the data is LOST. Prefer the ff
wrapper even when the underlying model is a cloud CLI.

**Verbs worth reaching for:**
| Verb | Use |
|------|-----|
| `ff db query "<sql>"` | Read-only SQL against live Postgres (READ ONLY txn) — the source-of-truth for what tables/columns actually exist. Use during discovery-first. |
| `ff memory get/add/replace/remove` | Agent **Scratchpad** — bounded (6 KB/scope) self-curating working memory with fixed blocks (task/decisions/findings/state/scratch), layered scope (session/agent/project), consolidate-and-forget. Also exposed as MCP `memory_*` tools. |
| `ff mcp install --for all` | Wire the forgefleet MCP server into Claude Code / Codex / Kimi / Cursor / etc. Every MCP tool ff adds (memory_*, brain_*, cortex_*, fleet_*) reaches those CLIs automatically on any project. |
| `ff cortex index / query` | Build/query the code graph for this (or any) repo. |
| `ff fleet versions` / `ff fleet deploy --all` | Drift matrix (installed_version = source HEAD, not the running binary) / propagate binaries fleet-wide. |
| `ff run`/`supervise`/`offload`/`research` | Dispatch work to fleet LLMs (local) or wrapped cloud CLIs — logged as training data. |

**⚠️ Two schema systems — source can drift from live.** `ff-db` owns the main `PG_MIGRATIONS`
(`crates/ff-db/src/{schema.rs,migrations.rs}`), but **`ff-mc` (mission control) bootstraps its
OWN schema in `crates/ff-mc/src/db.rs`** (e.g. the PM `work_items` / `projects` / `milestones`
tables live there, NOT in ff-db migrations). So a `CREATE TABLE` in ff-db source may not match the
live DB, and a fresh ff-db-only rebuild won't create ff-mc's tables. **Always `ff db query` to
confirm a table's real name + columns before extending it.** New ff-db migrations go at the end of
`PG_MIGRATIONS` (forward-only, never edit existing entries); check the highest version across all
branches first to avoid a version collision.

## Key subsystems (added 2026-04-14)

### Fleet Secrets
Stored in Postgres table `fleet_secrets` (schema V9). Read via
`ff_agent::fleet_info::fetch_secret("key")` or `get_hf_token()` — with
env-var fallback (`HF_TOKEN` etc). Managed via `ff secrets set/get/list/delete`.
Never write secrets to local files.

### Deferred Task Queue
Schema V10 `deferred_tasks`. Used for work that can't run now (node offline,
future time, manual retry). Trigger types: `node_online`, `at_time`,
`manual`, `now`. Atomic multi-worker claim via `FOR UPDATE SKIP LOCKED`.
CLI: `ff defer add-shell / list / get / cancel / retry`.
Worker: `ff defer-worker --scheduler --as-node <name>`.

### Model Lifecycle (Schema V11)
- `fleet_model_catalog`    — what ForgeFleet can download (from `config/model_catalog.toml`)
- `fleet_model_library`    — what's on disk per node (one row per file_path)
- `fleet_model_deployments` — what's running per node (llama-server / mlx_lm.server / vllm)
- `fleet_model_jobs`        — in-flight downloads/deletes/loads/swaps with progress
- `fleet_disk_usage`        — periodic disk snapshots for quota monitoring
- `fleet_workers` extended with `runtime`, `models_dir`, `disk_quota_pct`

Modules in `ff-agent`:
- `model_catalog` — load + sync TOML to DB
- `model_library_scanner` — walk `~/models`, classify files/dirs, upsert rows
- `hf_download` — stream HF repo files with progress/resume/token auth
- `model_runtime` — launch llama-server / mlx_lm.server / vllm (+ health wait, unload, ps)
- `disk_sampler` — `df -Pk` + recursive size walk; writes to `fleet_disk_usage`
- `deployment_reconciler` — sync DB with real processes (adopt + evict + refresh)

Key CLI (`ff model <sub>`):
- `sync-catalog` — load TOML → DB
- `catalog / search / library / deployments / jobs / disk`
- `scan` — rebuild library from `~/models` on this node
- `download <id>` — fetch from HF (local or cross-node via defer queue)
- `download-batch --node <n> <id>...` — many downloads → defer queue
- `delete <lib-id> --yes`
- `load <lib-id> [--port 51001 --ctx 32768 --parallel 4]` — start inference server
- `unload <deployment-id>`
- `ps` — running inference processes
- `ping <deployment-id>` — health check
- `disk-sample` — one snapshot

Daemon:
- `ff daemon` — bundled scheduler + worker + disk sampler + reconciler
  (runs defer-worker scheduling every 15s, disk sampling every 5min,
  reconciliation every 60s; `--once` for single-pass/cron mode)

### Node naming
`ff_agent::fleet_info::resolve_this_worker_name()` picks in order:
1. `$FORGEFLEET_NODE_NAME` env
2. Postgres `fleet_workers` row matching a local IPv4 address
3. `hostname` short-name fallback

### macOS code-signing gotcha
When updating the `ff` binary, `cp` breaks the macOS code signature and
subsequent runs get SIGKILL'd (Exit 137). ALWAYS use:
```
install -m 755 target/release/ff ~/.local/bin/ff
codesign --force --sign - ~/.local/bin/ff
```
Same for `~/.cargo/bin/ff`.

<!-- ff-build-methodology -->
## ForgeFleet build methodology (DEFAULT for every terminal: Claude Code, Codex, Kimi)

For any substantive build/code work in a ForgeFleet-related project, the DEFAULT
is to build **with the whole fleet and all its LLMs**, not solo on one machine:

1. **Build with all the computers + their sub-agents.** Route real work through
   ForgeFleet's distributed build (Pillar-4: `ff pm` work_items → scheduler →
   sub-agent worktrees on every computer, under `~/.forgefleet/sub-agents/sub-agent-N/`).
   Every computer runs sub-agent slots; the scheduler fans work across all of them.
   Don't build everything on one box when the fleet can parallelize it.
2. **Use ALL the LLMs (Hybrid LLM Architecture).** A sub-agent is an orchestrator,
   not bound to its host's RAM — it can call ANY available LLM: a local model on
   another fleet node (tiered cascade 9B→30B→70B→235B via `fleet_run` / capability
   router / `ff offload`), OR a cloud CLI (claude / codex / kimi) on its own machine.
   Pick the cheapest capable LLM per task; escalate only when needed.
3. **Use the LLM Council for hard decisions.** For non-trivial design/architecture/
   tradeoff calls, run `ff council --members codex,kimi` (multi-LLM consensus) before
   committing to an approach.
4. **Use ALL of ff's resources.** Prefer the `forgefleet` MCP tools + ff skills +
   ff agents over generic primitives: `fleet_run`/`fleet_crew`, `cortex_*` (code graph),
   `brain_*` (memory), `ff offload`/`ff supervise`/`ff research`, `ff db query` (live
   schema). `ff mcp install --for all` wires these into every CLI.
5. **Dogfood ff.** Route work through `ff` verbs (logged to `ff_interactions` =
   training data), not raw codex/kimi/ssh. If ff lacks a verb, add it — don't route
   around it. Solo/inline is only for trivial edits or conversational turns.
<!-- /ff-build-methodology -->
