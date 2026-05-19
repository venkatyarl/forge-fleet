---
name: fleet-consensus
description: |
  Before any destructive change (commit on main, schema migration,
  large refactor, force-push), dispatch the proposal to N≥3 disparate
  LLMs and require quorum. Disagreements bubble up to the operator.
when-to-invoke: |
  Before a commit/push/migration/refactor lands on `main` (or any
  branch the operator considers production). Skip for local
  experiments and feature branches.
family: governance
source: forgefleet
version: 1.0.0
tools:
  - Bash
---

# Fleet consensus (commit gate)

Three different model families have to agree before `main` changes.

## How it works

```bash
ff session start \
  --goal "Review the change in <branch>" \
  --team "judge=qwen-coder-30b,judge=kimi-k2,judge=claude-opus,judge=gemma-4-judge" \
  --quorum 3
```

Each judge sees:
1. The diff
2. Recent project memory (`brain_recall`)
3. The risk classification of the touched files

Each emits {approve, request-changes, block} with a one-line reason.
The session ends when ≥quorum approvals OR any block fires.

## Why this matters for ForgeFleet

The cloud agents can't do this — Claude won't call OpenAI; OpenAI
won't call Anthropic. Only a vendor-neutral runtime can debate across
model families. ForgeFleet's pitch on this is in `plans/`.

## What this is NOT

- Not a CI gate. (CI runs tests; this runs *judgment*.)
- Not an audit log. (Use `audit_logger` for that.)
- Not a replacement for the operator's review. The operator still
  gets the final word; consensus is signal, not authority.
