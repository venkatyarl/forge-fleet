---
name: Review Changes
description: Perform a structured code review using change detection and impact
---

## Review Changes

Perform a thorough, risk-aware code review using the Cortex code graph.

### Steps

1. Run `cortex_review` to get a risk-scored change analysis.
2. Run `cortex_impact` on changed symbols to find the blast radius.
3. For each high-risk symbol, run `cortex_tests` to check test coverage.
4. Run `cortex_path` between changed symbols and their callers to understand affected execution paths.
5. For any untested changes, suggest specific test cases.

### Output Format

Provide findings grouped by risk level (high/medium/low) with:
- What changed and why it matters
- Test coverage status
- Suggested improvements
- Overall merge recommendation

## Token Efficiency Rules
- ALWAYS start with `cortex_find` or `cortex_search` to locate the relevant symbol(s) before reading files.
- Use `cortex_context` for focused symbol context instead of reading whole files.
- Target: complete any review/debug/refactor task in ≤5 tool calls and ≤800 total output tokens.
