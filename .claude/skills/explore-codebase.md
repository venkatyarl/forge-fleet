---
name: Explore Codebase
description: Navigate and understand codebase structure using the Cortex code graph
---

## Explore Codebase

Use the Cortex (`cortex_*`) MCP tools to explore and understand the codebase.

### Steps

1. Run `cortex_corpora` to see indexed repos and pick a `corpus` slug.
2. Run `cortex_explain` on a known symbol for high-level community structure.
3. Use `cortex_find` (by name) or `cortex_search` (by intent) to locate specific functions, structs, or classes.
4. Use `cortex_callers` and `cortex_callees` to trace relationships.
5. Use `cortex_path` to understand execution paths between two symbols.
6. Use `cortex_outline` to see every symbol defined in a file.

### Tips

- Start broad (`cortex_corpora`, `cortex_explain`) then narrow down to specific areas.
- Use `cortex_context` to get a symbol's source, callers, callees, impact count, and community summary in one call.
- Use `cortex_tests` to check coverage for a symbol.

## Token Efficiency Rules
- ALWAYS start with `cortex_find` or `cortex_search` to locate the relevant symbol(s) before reading files.
- Use `cortex_context` for focused symbol context instead of reading whole files.
- Target: complete any review/debug/refactor task in ≤5 tool calls and ≤800 total output tokens.
