---
name: Refactor Safely
description: Plan and execute safe refactoring using dependency analysis
---

## Refactor Safely

Use the Cortex code graph to plan refactoring with confidence.

### Steps

1. Use `cortex_find` / `cortex_search` to locate the symbol(s) you want to refactor.
2. Use `cortex_callers` and `cortex_callees` to map all references.
3. Use `cortex_impact` to preview the full blast radius before applying changes.
4. After changes, run `cortex_review` to verify the refactoring impact.

### Safety Checks

- Always preview impact with `cortex_impact` before major refactors.
- Check `cortex_path` between affected symbols to ensure no critical paths are broken.
- Use `cortex_outline` to understand the structure of files you are changing.
- Use `cortex_tests` to identify tests that should be updated or added.

## Token Efficiency Rules
- ALWAYS start with `cortex_find` or `cortex_search` to locate the relevant symbol(s) before reading files.
- Use `cortex_context` for focused symbol context instead of reading whole files.
- Target: complete any review/debug/refactor task in ≤5 tool calls and ≤800 total output tokens.
