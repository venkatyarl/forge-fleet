---
name: offload
description: |
  Credit-saver. Before doing heavy, high-token / low-architectural-subtlety
  work yourself (bulk codegen, mechanical multi-file edits, research,
  summarization, test/doc generation, data extraction), offload it to a WARM
  tool-capable local LLM on the fleet via `fleet_offload` / `ff offload`. Use
  and review its result; if it returns `do_in_cloud`, do the work yourself.
  Keep architectural / load-bearing decisions in the cloud.
when-to-invoke: |
  When you (the cloud orchestrator) are about to spend many tokens generating
  bulk output whose SHAPE is already clear and that you'll review anyway —
  scaffolding, repetitive edits, summaries, research digests, test/doc stubs,
  parsing/extraction. NOT for one-or-two-line edits, and NOT for decisions
  that need real architectural judgment.
triggers:
  - "offload"
  - "save credits"
  - "save tokens"
  - "save cloud tokens"
  - "don't burn tokens"
  - "use the fleet for the bulk"
  - "let the fleet do this"
  - "delegate to local LLM"
  - "bulk codegen"
  - "generate all the boilerplate"
  - "mechanical edits across files"
  - "summarize this for me"
  - "extract the data from"
  - "write the tests for"
  - "write docs for"
family: fleet
source: forgefleet
version: 1.0.0
tools:
  - Bash
---

# Offload — the credit-saver

You (Claude Code / Codex / Kimi) are the **orchestrator**. The fleet's 15
LAN computers run local LLMs that idle most of the time. Sending the *bulk*
of high-token work there is free; sending it to the cloud is metered.

**The pattern: you delegate, the fleet generates, you review.** You stay in
control of architecture and correctness; the fleet does the typing.

## When to offload (call `fleet_offload` FIRST)

Offload when the task is **high-token but low-architectural-subtlety** and
you will review the result anyway:

- **Bulk code generation** — scaffolding, boilerplate, repetitive handlers,
  serializers, fixtures.
- **Multi-file mechanical edits** — rename-and-update, apply the same change
  across N files, format/lint-style rewrites.
- **Research** — gather/digest facts on a well-scoped question.
- **Summarization** — condense logs, diffs, docs, threads.
- **Test + doc generation** — unit tests for an existing function, docstrings,
  README sections.
- **Data extraction** — parse text/JSON/CSV into a target shape.

## When to keep it in the cloud (do NOT offload)

- **Architectural / load-bearing decisions** — schema design, API contracts,
  concurrency models, security boundaries, anything where "works but wrong"
  is expensive.
- **One-or-two-line edits** — the round-trip costs more than doing it.
- **Tasks needing your full conversation context** — the local model only
  sees the `task` string you send; it does not see this chat.

## How to call it

### MCP (preferred for agents)

```
fleet_offload({
  "task": "<self-contained task — include ALL context the local model needs>",
  "kind": "codegen" | "edits" | "research" | "summarize" | "tests" | "docs" | "extract",
  "est_output_tokens": 4096,   // optional — caps the local model's budget
  "min_ctx": 16384             // optional — usable per-slot ctx floor
})
```

### CLI (direct use / measurement)

```
ff offload "<task>"
ff offload "<task>" --kind codegen --est-output-tokens 6000 --min-ctx 32768 --output json
```

## What you get back, and what to do with it

- **`{ "offloaded": true, "endpoint": ..., "model": ..., "result": ... }`**
  → The fleet did the work on a warm tool-capable local model. **Read and
  review `result`.** Use it if it's correct. If it's wrong or it turns out
  the task needed architectural judgment, redo it yourself — you only spent
  cloud tokens on the review, not the generation.

- **`{ "offloaded": false, "decision": "do_in_cloud", "reason": ... }`**
  → No warm tool-capable endpoint was available right now. **Proceed and do
  the work yourself.** (v1 never cold-loads a model; warming is a v2
  concern, not yours.)

## Why this exists

`fleet_offload` picks the best **warm** tool-capable deployment via the V111
capability router (`pg_pick_agent_endpoint` — requires `tool_calling=true`
and enough usable per-slot context), then dispatches over the standard
OpenAI-compatible API — the same selector and dispatch path the rest of ff
uses. It is the explicit "spend fleet capacity instead of cloud credits"
verb: reach for it before generating bulk output inline.
