---
name: brain-recall
description: |
  Pull operator memory, architecture decisions, and prior incidents
  from the ForgeFleet Virtual Brain (Postgres + Obsidian vault + pgvector)
  BEFORE doing your own analysis. Persistent memory across agent
  switches — the #1 cross-session pain point from Reddit's 2026 surveys.
when-to-invoke: |
  At the START of any non-trivial task. The brain knows: who said what
  when, what was decided and why, which past attempts failed, what is
  scheduled for which date. Skipping this means re-discovering the
  repo every session.
family: memory
source: forgefleet
version: 1.0.0
tools:
  - mcp__forgefleet__brain_search
  - mcp__forgefleet__brain_vault_read
---

# Brain recall

Your project's brain belongs to you, not Anthropic. Use it.

## Tools

- **`brain_search`** — semantic search over operator memory + vault
  notes + decisions + prior incidents. Returns ranked snippets with
  source paths.
- **`brain_vault_read`** — read a specific markdown note from the
  vault. Use the path returned by `brain_search`.
- **`brain_graph_neighbors`** — explore what's linked to a memory
  node. Useful when you want context around a decision.

## When to invoke

- "How did we decide X?" → `brain_search`
- "Refactor Y" → `brain_search Y` FIRST so you don't undo a recent
  decision.
- "Is this related to incident Z?" → `brain_search Z`
- "What's the current architecture of W?" → `brain_search W`

## When NOT to invoke

- Trivial well-defined task (rename a variable).
- Question that's clearly answerable by reading one file.
- The user explicitly says "ignore prior context."
