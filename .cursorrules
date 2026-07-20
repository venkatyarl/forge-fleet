<!-- Cortex code-graph MCP tools -->
## MCP Tools: Cortex code graph

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
Cortex (`cortex_*`) MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `cortex_find` / `cortex_search` instead of Grep
- **Understanding impact**: `cortex_impact` instead of manually tracing imports
- **Code review**: `cortex_review` + `cortex_show` instead of reading entire files
- **Finding relationships**: `cortex_callers` / `cortex_callees` / `cortex_path` / `cortex_tests`
- **Architecture questions**: `cortex_explain` / `cortex_corpora`

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### Key Tools

| Tool | Use when |
|------|----------|
| `cortex_review` | Reviewing code changes — gives risk-scored analysis |
| `cortex_show` | Need source snippets for review — token-efficient |
| `cortex_impact` | Understanding blast radius of a change |
| `cortex_path` | Finding execution paths between two symbols |
| `cortex_callers` / `cortex_callees` | Tracing who calls what |
| `cortex_find` / `cortex_search` | Finding functions/classes by name or intent |
| `cortex_explain` | Understanding subsystem/community structure |
| `cortex_outline` | File-level table of contents |

### Workflow

1. The graph is updated by `ff cortex index` (and post-commit hooks on enrolled fleet computers).
2. Use `cortex_review` for code review.
3. Use `cortex_impact` and `cortex_path` to understand impact.
4. Use `cortex_tests` to check coverage.
