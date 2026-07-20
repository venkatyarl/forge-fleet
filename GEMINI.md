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

### Key Cortex Tools

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
