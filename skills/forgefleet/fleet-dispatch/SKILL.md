---
name: fleet-dispatch
description: |
  Dispatch a well-scoped task to ForgeFleet's tiered LLM cascade
  (9B local → 30B local → 70B local → cloud) via `ff run` /
  `ff supervise` / `mcp__forgefleet__fleet_run` / `mcp__forgefleet__fleet_crew`.
when-to-invoke: |
  When the user has a single self-contained task that does NOT need
  to read or modify many files — definitions, summaries, classifications,
  rewrites, JSON extraction, one-shot refactors — and you are about
  to do it inline with cloud Claude. Dispatching to fleet is cheaper
  and exercises the fleet you are dogfooding.
family: fleet
source: forgefleet
version: 1.0.0
tools:
  - Bash
---

# Fleet dispatch

ForgeFleet runs 15 LAN-attached computers with local LLMs that idle
98% of the time. Sending well-scoped work there is free; sending it
to Anthropic is metered.

## Decision rule

```
Is the task self-contained (no multi-file context needed)?
├─ Yes → `ff run "<prompt>"` or `mcp__forgefleet__fleet_run`
└─ No  → Is it a coding task with reviewable output?
         ├─ Yes → `ff supervise "<prompt>" --max-attempts 3`
         │       or `mcp__forgefleet__fleet_crew`
         └─ No  → Solve inline with cloud Claude
```

## Concrete tools

- **`ff run "<task>" --output json`** — one-shot LLM call against the
  cascade. Returns full JSON. Use for definitions, classifications,
  one-off rewrites.
- **`ff supervise "<task>" --max-attempts 3`** — like `ff run` but
  retries on failure with auto-diagnosis. Use for coding tasks where
  the LLM might need a second pass.
- **`ff research "<question>" --depth shallow|medium|deep`** —
  parallel multi-source research. Use for research questions, not
  for code-writing.
- **`mcp__forgefleet__fleet_run`** — same as `ff run` but available
  as an MCP tool to other coding agents.
- **`mcp__forgefleet__fleet_crew`** — 3-agent pipeline (Context →
  Writer → Reviewer). Use for multi-file refactors.

## When dispatching, route by shape

| Task | Best target |
|------|-------------|
| One-shot text generation | `ff run` (9B tier) |
| Code edit, simple | `ff run --llm http://192.168.5.102:55000` (Marcus Qwen3-Coder) |
| Code edit, edge-case-heavy | `ff supervise` |
| Multi-file refactor | `ff supervise` or `fleet_crew` |
| Research / synthesis | `ff research` |
| Definition / classification / JSON | `ff run` (cheapest tier) |

## Avoid these failure modes

- **Gemma-4 can't tool-call**: don't dispatch coding work to Taylor's
  mlx default — use `--llm http://192.168.5.102:55000` (Marcus) or
  Sophie's Qwen3-Coder.
- **Loop detection false positives**: if `ff supervise` reports a
  loop on N identical Edit outputs, that's likely a bug, not real
  looping — operator has it on the backlog.
- **Silent false-success**: `ff supervise` declares "done" when the
  LLM says so; it does not stat artifact files. Verify the artifact
  exists before believing success.
