---
name: fleet-research
description: |
  Parallel multi-LLM research across fleet computers. Decomposes a
  query into N sub-questions, dispatches each to a different fleet
  LLM, then synthesizes a cited markdown report.
when-to-invoke: |
  When the user asks "research X" or "look at how others do Y" or
  needs a comparative review. Prefer this over a single web-fetch
  because it runs N searches in parallel and produces a synthesis.
family: research
source: forgefleet
version: 1.0.0
tools:
  - Bash
---

# Fleet research

`ff research` decomposes a research prompt into sub-questions, fans
out across fleet LLMs, and synthesizes the results into a cited
markdown report.

## Invocation

```bash
ff research "<question>" \
  --depth shallow|medium|deep \
  --output report.md
```

Defaults to medium depth (≈8 parallel sub-queries).

## When this beats inline research

- The question has multiple facets (compare A vs B vs C).
- The answer is not in your repo.
- You want a fresh web sweep, not just memory recall.
- The user is in an autonomous build window (they want everything
  done; the fleet should do the work).

## When this is NOT a good fit

- The answer is in the local repo — use `mcp__forgefleet__brain_search`.
- The user wants a single short answer — use `ff run`.
- The user is iterating on a specific file — solve inline.
