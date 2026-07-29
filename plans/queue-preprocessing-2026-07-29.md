# Queue preprocessing & slot gating design (2026-07-29)

Operator directive: when an item enters the queue it must be PREPROCESSED —
context, research, memory, project parsing — BEFORE it can hold a slot. Only
preprocessed items get slots. When a slot assigns a computer + LLM, the task
must already carry everything it needs.

## Why (everything below was observed live this week)

- Items with no repo binding burn leases then fail closed at dispatch
  (canary-3, 4 attempts lost).
- Prompt context is computed AT BUILD TIME on the builder node: the 16K
  region cap silently dropped the tests module → GLM hallucinated
  `test_utils::env_with` (F6 fix shipped, but the class remains).
- Cloud rescues cost 26 min when the local lane fails because the prompt was
  thin; a prebuilt rich prompt makes the local lane succeed first-try
  (qwen3-coder canary-4: 3 rounds, 13 min).
- Zombie claims: slots whose supervisor died kept items 'claimed' forever;
  the scheduler had no preprocess/health gate to catch it upstream.
- Memory/research never reach the builder prompt at all today.

## Current flow
`idea → ready → scheduler claims free slot → dispatch builds prompt at build
time (repo structure + identifier regions, 16K cap) → codegen → review → PR`.

## New flow
`idea → ready → **preprocess** → preprocessed → scheduler claims slot →
dispatch consumes the context pack (prompt mostly pre-built) → codegen →
review → PR`.

### Preprocess stages (leader-gated tick, reuses derive_working_summaries shape;
repo reads happen on a node holding the canonical checkout)

1. **validate** — repo binding present (repo_id/url/path), title/description
   sane, not a duplicate of an open item. Failures → `blocked` with a precise
   reason, never a slot.
2. **resolve-context** — predicted_paths (already produced by decompose),
   repo structure, identifier list, and the F6-grounded region extraction
   (test-module-first, named truncation) computed ONCE and stored in
   `work_items.context` (jsonb, already exists — the Dreamer context-pack
   evidence already lands there).
3. **memory lookup** — brain_search + project memory blocks for the touched
   files/symbols ("who broke/fixed this file before and why") →
   `context.memory_notes[]`.
4. **research** (opt-in per item kind) — one Lucy-lane pass for unfamiliar
   domains → `context.research_notes[]`; citations verified (F5).
5. **lane plan** — capability tags, complexity score, min_ctx estimate,
   prefers_cloud decided HERE and stored (`context.lane_plan`), so dispatch
   reads a decision instead of recomputing one per attempt.
6. **prompt assembly** — the final codegen prompt (system contract + task +
   regions + memory/research notes) rendered and stored; dispatch sends it
   as-is. Build-time prompt building becomes a fallback for legacy items.

### Slot gating
- `work_items.preprocess_status`: `pending | running | complete | failed`
  (+`preprocess_error`). Scheduler's eligible query adds
  `preprocess_status = 'complete'` — **only preprocessed items get slots.**
- Slot assignment records the pairing: which computer AND which LLM lane
  (from lane_plan) — visible on `ff pm board`.
- Zombie guard: a claimed slot with no build activity (no lease heartbeat
  progression, no LLM interaction) within N minutes is auto-freed and the
  item re-queued with the note — the reaper learns from the beyonce/duncan
  incidents instead of me freeing slots by hand.

### Migration
One forward-only migration: preprocess_status + preprocessed_at +
preprocess_error on work_items (context jsonb reused). Default 'complete'
for existing rows (grandfathered, no behavior change for in-flight work).

### Failure modes handled
- No repo binding → blocked at validate (no lease wasted).
- Region truncation → computed once, named in the pack, never re-truncated
  differently per attempt (today each attempt can get a DIFFERENT prompt!).
- GLM/local failure → repair feedback is appended to the SAME stored pack,
  so retries converge instead of re-grounding from scratch.

## Implementation order
P5-1 migration + preprocess tick (validate + resolve-context) → P5-2 slot
gating + lane plan → P5-3 memory/research enrichment → P5-4 prompt assembly
+ zombie guard. Each shippable independently; P5-1 already kills the two
most expensive failure classes (binding waste, per-attempt prompt drift).
