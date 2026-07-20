---
name: Debug Issue
description: Systematically debug issues using graph-powered code navigation
---

## Debug Issue

Use the Cortex code graph to systematically trace and debug issues.

### Steps

1. Use `cortex_find` or `cortex_search` to find code related to the issue.
2. Use `cortex_callers` and `cortex_callees` to trace call chains.
3. Use `cortex_path` to see the call chain through suspected areas.
4. Run `cortex_review` on the working tree to check if recent changes caused the issue.
5. Use `cortex_impact` on suspected symbols to see what else is affected.

### Tips

- Check both callers and callees to understand the full context.
- Look at `cortex_path` results to find the entry point that triggers the bug.
- Recent changes are the most common source of new issues.

## Token Efficiency Rules
- ALWAYS start with `cortex_find` or `cortex_search` to locate the relevant symbol(s) before reading files.
- Use `cortex_context` for focused symbol context instead of reading whole files.
- Target: complete any review/debug/refactor task in ≤5 tool calls and ≤800 total output tokens.
