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
