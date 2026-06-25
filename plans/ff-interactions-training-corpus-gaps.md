# ff_interactions training-corpus logging gaps (research, 2026-06-25)

`ff_interactions` is ForgeFleet's own LLM training corpus — per CLAUDE.md, every
`ff run/supervise/offload/research`/`fleet_crew`/council call should log a
req+resp+worker+endpoint+tokens row. Audited the live table; it's
under-populated and several columns are systematically null.

## Live state (2026-06-25, fleet @ 8028a6b2)
- **60 rows total, 0 in the last 24h.** Mostly a *behavioral* gap: work has been
  routed as direct Claude code edits, not through `ff run`/`ff research`. Routing
  more real work through `ff` is the first fix (operator/loop behavior, not code).
- **Null columns by interaction type:**
  | channel / engine | rows | null worker | null tokens |
  |---|---|---|---|
  | research_subtask / qwen36 (fleet) | 10 | 0 | **10** |
  | work_item_dispatch / codex | 6 | 0 | 6 |
  | council_member / kimi · codex | 11 | **11** | 11 |
  | offload / qwen36 (fleet) | 5 | 0 | **0 ✓** |
  | council_chairman / kimi · claude | 5 | 5 | 5 |
  | research / thinking | 5 | 5 | 5 |
  | cli / various | 6 | 6 | 6 |

## Findings (root-caused)

### 1. `research_subtask` tokens are ALWAYS 0 — a dead stub (clean bug, multi-site fix)
Chain (all in `crates/ff-agent/src/research.rs` + helpers):
- `extract_token_counts(_events)` (research.rs ~1973) is a **stub returning `(0,0)`**;
  the `_events` arg is unused.
- It's called on `result.events`, but the research sub-agent result is built with
  `events: Vec::new()` (research.rs ~574) — empty by construction.
- The sub-agent runner `openai_single_completion(endpoint, model, prompt, …)`
  (research.rs ~548) returns only the completion **string** and **discards
  `response.usage`** (OpenAI-compatible fleet servers DO return
  `usage.prompt_tokens`/`completion_tokens`).
- `AgentTaskResult` (multi_agent.rs:35) has **no token fields** to carry usage.

**Fix plan (do in a focused iteration with an end-to-end `ff research` validation):**
1. Add `tokens_in: u64, tokens_out: u64` to `AgentTaskResult`; default 0 at its
   ~5 construction sites (multi_agent.rs:109/221, research.rs:570/827).
2. `openai_single_completion` (or a `_with_usage` variant to avoid touching other
   callers) returns `(text, prompt_tokens, completion_tokens)` parsed from
   `response.usage`.
3. Thread tokens through the research sub-agent spawn closure → `AgentTaskResult`.
4. In `run_agent_task` (multi_agent.rs ~221) capture `session.usage.total_input_tokens`
   / `total_output_tokens` BEFORE `drop(session)` (line 201) — the agent loop DOES
   populate `session.usage` from `response.usage` (agent_loop.rs ~1008:
   `record_turn(prompt_tokens, completion_tokens)`), so this is reliable for the
   live-tool agent path too.
5. Replace the `extract_token_counts` stub call with `(result.tokens_in, result.tokens_out)`;
   delete the stub.
**Validate:** run `ff research "<trivial question>" --parallel 1` against a fleet
model, then `ff db query "SELECT tokens_in, tokens_out FROM ff_interactions WHERE
channel='research_subtask' ORDER BY ts DESC LIMIT 1"` — expect non-zero.

### 2. Cloud-CLI interactions (`council_member`/`cli` via kimi/codex/claude): null worker + null tokens
- **worker_name null is arguably CORRECT** (a cloud CLI doesn't run on a fleet
  worker) but a sentinel like `cloud:<cli>` or the invoking host would be more
  useful for corpus filtering than NULL.
- **tokens null is a real gap** but requires per-CLI usage parsing (codex/claude
  print a usage line; kimi differs) — higher-effort, lower-priority than #1.

## Recommendation
Fix #1 first (clean, self-contained, high-signal — research subtasks are exactly
the "fleet model + computer → grounded answer" pairs the corpus wants). Treat #2
as a follow-up. The *volume* problem (0/24h) is solved by routing real work
through `ff`, not by code.
