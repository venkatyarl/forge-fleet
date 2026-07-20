<!-- forgefleet MCP tools -->
## MCP Tools: forgefleet

**IMPORTANT: This project has a ForgeFleet MCP server. ALWAYS use the
`forgefleet` MCP tools BEFORE using Grep/Glob/Read to explore the codebase.**
The `cortex_*` code-graph tools are faster, cheaper (fewer tokens), and give
you structural context (callers, dependents, test coverage) that file scanning
cannot.

### When to use forgefleet tools FIRST

- **Exploring code**: `cortex_find`, `cortex_search`, or `cortex_context` instead of Grep
- **Understanding impact**: `cortex_impact` or `cortex_affected_flows` instead of manually tracing imports
- **Code review**: `cortex_review` instead of reading entire files
- **Finding relationships**: `cortex_callers`, `cortex_callees`, `cortex_path`
- **Architecture questions**: `cortex_explain`, `cortex_corpora`
- **Fleet work**: `fleet_run`, `fleet_crew`, `fleet_status`, `fleet_pulse`
- **Memory/state**: `memory_get`/`memory_add`, `brain_search`

Fall back to Grep/Glob/Read **only** when the forgefleet tools don't cover what you need.

### ⛔ Discovery-first — search before you build (hard rule)
The #1 waste here is rebuilding a capability the fleet already has. Before writing any
new table/module/feature, inventory what exists, in this order:
1. **Cortex / code graph** — `cortex_find` / `cortex_search` / `cortex_context` ("what handles X?")
   to find the owning crate. Faster + cheaper than grep.
2. **`ff db query "<read-only SQL>"`** — confirm the LIVE Postgres schema. Source
   `CREATE TABLE` strings can DRIFT from the live DB; never extend a table you haven't
   confirmed live. (Note: `ff-mc` bootstraps its OWN schema in `crates/ff-mc/src/db.rs` —
   e.g. PM `work_items`/`projects`/`milestones` — separately from ff-db migrations.)
3. **`brain_search`** for prior decisions; grep/read files LAST.
Then reuse/extend what exists instead of forking.

### Working through ff (dogfood + key verbs)
Prefer routing work through ff over raw cloud calls — it surfaces ff bugs and every call
is logged to `ff_interactions` (the training corpus for ff's own LLM):
- `fleet_run` (single-turn LLM), `fleet_crew` (writer→reviewer), `ff offload` / `ff research`.
- `memory_*` — the Scratchpad: bounded self-curating working memory (blocks
  task/decisions/findings/state/scratch; scopes session/agent/project). Read at start, record as you go.
- `ff db query "<sql>"` — read-only live schema. `ff mcp install --for all` — wire ff into all CLIs.
- Build the CLI: `cargo build --release -p ff-terminal --bin ff`; on macOS `codesign --force --sign -`
  after copying the binary (cp breaks the signature → SIGKILL). New ff-db migrations go at the END
  of `PG_MIGRATIONS`, forward-only.

### Key Tools

| Tool | Use when |
|------|----------|
| `cortex_find` | Find symbols by name fragment or semantic intent |
| `cortex_search` | Natural-language hybrid code search |
| `cortex_context` | One-call orientation for a symbol (def + callers + callees + impact + community) |
| `cortex_show` | Return a symbol's source definition |
| `cortex_callers` / `cortex_callees` | Direct call-graph neighbors |
| `cortex_impact` | Transitive blast radius |
| `cortex_affected_flows` | Execution flows through a symbol (callers + callees + impact + tests) |
| `cortex_path` | Shortest call chain between two symbols |
| `cortex_tests` | Tests covering a symbol |
| `cortex_review` | Risk-scored git diff review |
| `cortex_explain` | Subsystem/community summary |
| `cortex_outline` | File-level symbol outline |

### Workflow

1. Start with `cortex_corpora` to discover indexed repos.
2. Use `cortex_find` or `cortex_search` to resolve intent into symbol names.
3. Use `cortex_context` or `cortex_affected_flows` to orient on a symbol.
4. Use `cortex_review` for diff review.

<!-- ff-build-methodology -->
## ForgeFleet build methodology (DEFAULT for every terminal: Claude Code, Codex, Kimi)

For any substantive build/code work in a ForgeFleet-related project, the DEFAULT
is to build **with the whole fleet and all its LLMs**, not solo on one machine:

1. **Build with all the computers + their sub-agents.** Route real work through
   ForgeFleet's distributed build (Pillar-4: `ff pm` work_items → scheduler →
   per-node sub-agent worktrees on every computer, under `~/.forgefleet/sub-agents/sub-agent-N/`).
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
