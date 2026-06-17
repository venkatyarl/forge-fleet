# Design Doc #1 — Bounded Self-Curating Working Memory

Status: **DRAFT — awaiting operator review** (no code until approved)
Priority: HIGH (first of the 3 Hermes-inspired capabilities)
Inspiration: Hermes Agent / MemGPT-Letta "core memory" blocks — built ForgeFleet-native.

## 1. Problem

Long-running `ff` agents have no disciplined, capped working-memory surface. Context
either bloats (everything stays in the window until compaction throws it away
lossily) or gets lost (no deliberate "what's worth keeping" step). There is no
*consolidate-and-forget*: the agent can't curate a small, durable working set while
pushing detail down to long-term stores.

## 2. What already exists (prior art in-repo)

- **`session_brain`** (table: `session_id uuid`, `key`, `value jsonb`, `written_by_role`,
  `written_by_step`) — a per-session KV scratchpad **shared by the multi-LLM team** and
  written by roles/steps. It is **uncapped**, not agent-self-editable as a text surface,
  and scoped to `agent_sessions` (the planner/coder/reviewer DAG), not to a single
  long-running agent or a project. → related, but not the capped self-curating surface
  we need; we will not extend it (different lifecycle + scope).
- **brain / Cortex / vault** (`ff-brain`): the **unbounded backing store**. `brain_propose_node`
  (stage to vault Inbox), `procedural_memory::consolidate`, `vault.rs` indexing,
  `cortex.rs` retrieval. Working memory sits *in front* of these; they stay the source of
  truth for evicted detail.
- **MCP dispatch** (`ff-mcp/src/handlers.rs::dispatch`), **migrations** (`ff-db`, next = **V137**),
  **session context assembly** (`ff-sessions/src/context.rs::ContextManager::inject_system_prompt`),
  **CLI** (`ff-terminal`, `*_cmd.rs` + `Command` enum).

## 3. Design overview

A **small, hard-capped, agent-editable text surface** per scope, frozen into the system
prompt at session start, backed by the unbounded brain/vault. Modeled as a handful of
named **blocks** (not free-form KV) so "replace(substring)" is well-defined.

```
            ┌─────────────────────────────────────────────┐
   agent ──▶│  WORKING MEMORY  (capped, frozen-at-start)   │  ← memory_* MCP tools
            │  blocks: task / findings / decisions / scratch│
            └───────────────┬─────────────────────────────┘
                            │ consolidate-and-forget on overflow
                            ▼  (summarize + push detail down)
            ┌─────────────────────────────────────────────┐
            │  brain / Cortex / vault  (UNBOUNDED backing)  │  ← retrievable later
            └─────────────────────────────────────────────┘
```

- **Scope**: `(scope_type, scope_key)` where `scope_type ∈ {session, agent, project}`.
  A session loads its `project` + `agent` + `session` blocks at start (per the
  "per-agent and/or per-project" requirement). Session scope is ephemeral; agent/project
  scope persists across sessions (this is what gives "curate memory across sessions").
- **Cap**: a per-scope **byte budget** (default 6 KB ≈ ~1.5–2k tokens, configurable).
  Enforced server-side on every write — the store *provably* never exceeds the cap.
- **Blocks**: named text blocks within a scope (e.g. `task`, `findings`, `decisions`,
  `scratch`). Total bytes across a scope's blocks ≤ cap.

## 4. Schema (V137, Postgres — not flat files)

```sql
-- SCHEMA_V137_AGENT_WORKING_MEMORY
CREATE TABLE IF NOT EXISTS agent_memory (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope_type      TEXT NOT NULL CHECK (scope_type IN ('session','agent','project')),
    scope_key       TEXT NOT NULL,                 -- agent_sessions.id / fleet_agents.id / projects.id
    block           TEXT NOT NULL,                 -- 'task' | 'findings' | 'decisions' | 'scratch' | ...
    content         TEXT NOT NULL DEFAULT '',
    bytes           INT  NOT NULL DEFAULT 0,        -- maintained = octet_length(content)
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (scope_type, scope_key, block)
);

CREATE TABLE IF NOT EXISTS agent_memory_caps (
    scope_type      TEXT NOT NULL,
    scope_key       TEXT NOT NULL,                 -- '' = default for this scope_type
    cap_bytes       INT  NOT NULL DEFAULT 6144,
    PRIMARY KEY (scope_type, scope_key)
);

-- Audit trail of every consolidate-and-forget (detail is in brain/vault; this is the pointer).
CREATE TABLE IF NOT EXISTS agent_memory_evictions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope_type      TEXT NOT NULL,
    scope_key       TEXT NOT NULL,
    block           TEXT NOT NULL,
    summary         TEXT NOT NULL,                 -- the kept summary
    vault_ref       TEXT,                          -- brain node / vault path where detail landed
    evicted_bytes   INT  NOT NULL,
    evicted_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_agent_memory_scope ON agent_memory(scope_type, scope_key);
```

Cap is enforced in code (transactional read-modify-write), not by a DB trigger, so the
consolidation pass can run as part of the same write path.

## 5. Memory tool surface (the agent-editable API)

All four are transactional and **cap-checked**; a write that would exceed the cap triggers
consolidation (§6) *before* committing, so a tool call never returns an over-cap state.

| tool | effect |
|---|---|
| `memory_add(scope, block, text)` | append `text` to `block` (creates block if absent) |
| `memory_replace(scope, block, old, new)` | substring replace `old`→`new` within `block` (exact, unique-match like Edit) |
| `memory_remove(scope, block, [substring])` | remove a substring, or clear the whole block if omitted |
| `memory_get(scope?, block?)` | read current working set (all blocks, or one) |

`scope` defaults to the current session's `(agent, project)` pair; explicit scope allowed.

## 6. Consolidate-and-forget (the "forget" that makes the cap real)

Triggered when a write would push a scope over `cap_bytes`:

1. Pick the eviction target: the **largest** block over a soft floor (keep `task`/`decisions`
   stickier via a per-block weight; `scratch` evicts first).
2. Call a **fleet LLM** (routed via the agent-capable router — tool_calling endpoint) to
   produce a short **summary** of that block (target ≤ N bytes).
3. Push the **full pre-summary content** down to the backing store:
   `brain_propose_node` (staged to vault Inbox) and/or `procedural_memory::consolidate`,
   capturing a `vault_ref`.
4. Replace the block's content with the summary; record a row in `agent_memory_evictions`
   (summary + `vault_ref` + `evicted_bytes`).
5. Re-check cap; repeat until under budget. Deterministic + bounded (max K passes, then
   hard-truncate `scratch` as a backstop so the cap is *always* honored even if the LLM is
   unavailable).

Evicted detail is later retrievable via `brain_search` / `cortex_*` / `brain_vault_read`
using the `vault_ref` — closing the "evicted detail is retrievable" acceptance loop.

## 7. Frozen-at-start load (prompt caching)

At session start, `ContextManager::inject_system_prompt()` injects ONE rendered block:

```
## Working memory (curated; edit via memory_* tools)
### task
<content>
### decisions
<content>
...
```

Rendered **once at start** and held constant for the session's lifetime → preserves prompt
caching. Mid-session edits via `memory_*` change the DB (and the *next* session's frozen
load) but do **not** mutate the current frozen block (no cache invalidation). This is the
Letta/MemGPT discipline: the in-context surface is a stable snapshot; the tools edit the
durable store that the next freeze reads.

## 8. MCP + CLI + fleet-awareness

- **MCP**: new `ff-mcp/src/memory_tools.rs` with `memory_add/replace/remove/get`, registered
  in `handlers.rs::dispatch`. Exposed on every fleet computer's MCP server (port 50001),
  so any agent on any node curates the same Postgres-backed store → **fleet-aware** by
  construction (shared DB).
- **CLI**: new `ff-terminal/src/memory_cmd.rs` → `ff memory list|show|export|set-cap|purge`
  for operator inspection (`ff memory show --scope project:forge-fleet`).
- **Never auto-apply**: this layer is read/written by agents, not the upgrade pipeline; no
  destructive fleet op. The only LLM-actuated step is summarization, which is local + cheap.

## 9. Acceptance criteria (from the directive)

1. An agent curates its memory **across sessions** → agent/project-scoped blocks persist and
   re-load at the next session start. *(integration test: write in session A, read in B)*
2. The store **provably never exceeds its cap** → property test: random add/replace
   sequences, assert `Σ bytes ≤ cap` after every op; assert consolidation fired.
3. **Evicted detail is retrievable** from brain/Cortex/vault → test: overflow a block, then
   fetch the `vault_ref` and recover the original detail.
4. **Unit + integration tests**: cap math, substring replace semantics (Edit-like unique
   match), consolidation target selection, frozen-load rendering, MCP round-trip.

## 10. Phasing (implementation, after approval)

- **P1**: V137 schema + cap-enforced store + `memory_*` CRUD (no LLM) + unit tests.
- **P2**: consolidate-and-forget (fleet-LLM summary + push to brain/vault) + the hard-truncate
  backstop + eviction audit.
- **P3**: frozen-at-start injection in `ff-sessions` + MCP tools + `ff memory` CLI + integration tests.

## 11. Open questions for operator review

1. **Default cap**: 6 KB/scope (~1.5–2k tokens) a good starting budget, or larger/smaller?
2. **Block set**: fixed (`task`/`findings`/`decisions`/`scratch`) vs agent-defined block names?
   (Fixed is simpler + more predictable in the system prompt; free-form is more flexible.)
3. **Primary scope for "long-running ff agents"**: agent-id, project-id, or both layered?
   (Proposal: load **project + agent** blocks at start; session scope optional/ephemeral.)
4. **Summarizer model**: route to the agent-capable tier (qwen3-coder/qwen36) or a fixed
   synthesis model (taylor qwen36-35b)? (Proposal: router, fall back to taylor.)
