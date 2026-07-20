<!-- cortex MCP tools -->
## MCP Tools: Cortex

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
ForgeFleet Cortex MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `cortex_find`, `cortex_show`, or `cortex_outline` instead of Grep
- **Understanding impact**: `cortex_impact` instead of manually tracing imports
- **Code review**: `cortex_review` + `cortex_show` instead of reading entire files
- **Finding relationships**: `cortex_callers` / `cortex_callees` instead of manual tracing
- **Architecture questions**: `cortex_explain` for subsystem-level context

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### ⛔ Discovery-first — search before you build (hard rule)
The #1 waste here is rebuilding a capability the fleet already has. Before writing any
new table/module/feature, inventory what exists, in this order:
1. **Cortex / code graph** — `cortex_find` / `cortex_show` ("what handles X?")
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
| `cortex_review` | Reviewing code changes — gives risk-scored analysis |
| `cortex_show` | Need a symbol definition/snippet — token-efficient |
| `cortex_impact` | Understanding blast radius of a change |
| `cortex_callers` | Finding who depends on a symbol |
| `cortex_callees` | Finding what a symbol calls |
| `cortex_find` | Finding functions/types by name or intent |
| `cortex_explain` | Understanding the owning subsystem |
| `cortex_outline` | Orienting within a file without reading all of it |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `cortex_review` for code review.
3. Use `cortex_impact` to understand blast radius.
4. Use `cortex_callers` / `cortex_callees` before falling back to file scans.

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
