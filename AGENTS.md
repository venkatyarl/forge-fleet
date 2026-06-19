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
The #1 waste here is rebuilding a capability the fleet already has. Before writing any
new table/module/feature, inventory what exists, in this order:
1. **Cortex / code graph** — `cortex_find` / `semantic_search_nodes` ("what handles X?")
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
