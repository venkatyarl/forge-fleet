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
- `fleet_nodes` extended with `runtime`, `models_dir`, `disk_quota_pct`

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
`ff_agent::fleet_info::resolve_this_node_name()` picks in order:
1. `$FORGEFLEET_NODE_NAME` env
2. Postgres `fleet_nodes` row matching a local IPv4 address
3. `hostname` short-name fallback

### macOS code-signing gotcha
When updating the `ff` binary, `cp` breaks the macOS code signature and
subsequent runs get SIGKILL'd (Exit 137). ALWAYS use:
```
install -m 755 target/release/ff ~/.local/bin/ff
codesign --force --sign - ~/.local/bin/ff
```
Same for `~/.cargo/bin/ff`.
