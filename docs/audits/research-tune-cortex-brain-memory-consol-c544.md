# Cortex / Brain / Memory consolidation cadence — research + tuning decisions

**Work item:** c5447940 (`research-tune-cortex-brain-memory-consol-c544`) · **Date:** 2026-07-19/20
**Pairs with:** FalkorDB memory design (492c9612, status `idea`, `plans/cortex-falkordb-backend.md`) and Session→Obsidian export daemon (3a13244e, `done`).

## Executive summary — the decisions

| Loop | Today | Decision |
|---|---|---|
| Cortex structural reindex | 10-min leader tick + per-commit git hook + optional `watch` | **Keep 10-min reconciliation tick; add a session-end event trigger; expose the interval as a `fleet_secrets` knob.** |
| Cortex embed refresh | Independent hourly tick (up to ~70 min semantic-search staleness) | **Chain an embed pass immediately after any reindex that changed files; keep hourly tick as reconciliation only.** |
| Cortex community summaries | Hourly, 20/tick | Keep hourly incremental; **add nightly full community rebuild** (Graphiti-style periodic re-clustering). |
| Brain "dreamer" consolidation | **Does not exist** — `brain_knowledge_candidates` has 635 pending / 0 ever processed (live DB, 2026-07-19) | **New: hourly bounded candidate-triage tick + nightly deep pass at 03:00 local.** Registered in the daemon tick registry, NOT as recurring `deferred_tasks` (which are one-shot); the nightly pass *enqueues* `deferred_tasks` for heavy offloadable work. |
| Scratchpad consolidate-and-forget | Inline on any write exceeding the 6 KB scope cap | **Keep write-triggered eviction (it works: 608 evictions live); add a sleep-time rewrite of `project`-scope pads inside the nightly dreamer pass.** |
| Procedural memory consolidation | Every 6 h, 7-day lookback | Keep; fold its schedule under the same tick registry for observability. |

The single biggest finding: ForgeFleet already has **write-side** memory machinery from every school (blocks + eviction like Letta, episodic candidates like Graphiti, temporal fields like Zep) — but **no read-side/background consolidator**. Evictions and extractors push into `brain_knowledge_candidates` and nothing ever drains it. The "dreamer" is the missing organ, not a tuning knob.

---

## 1. Current state (as-is inventory)

### 1.1 Cortex code graph

Engine in `crates/ff-brain/src/cortex.rs` (full `index_langs` :300, incremental `index_langs_incremental` :400). Incremental is the default and self-sufficient: per-file `content_hash` diffed against the `cortex_file_index` ledger (`schema.rs:8448`, V123), changed files re-extracted, deleted symbols demoted to `code:extern` placeholders, orphans GC'd (`cortex.rs:581`). Generation lock via `cortex_generations` + `CORTEX_EDIT_LOCK` (`cortex.rs:305,405`).

Triggers today:

| Trigger | Where | Cadence |
|---|---|---|
| `ff cortex index` CLI | `ff-terminal/src/top_cortex_cmd.rs:817-856` | manual |
| git post-commit hook (`ff cortex hook install`) | `top_cortex_cmd.rs:2847-2888` | every commit, `--incremental` |
| `ff cortex watch` (notify watcher) | `top_cortex_cmd.rs:552-564, 2968` | live, 3 s debounce |
| Daemon reindex tick | `ff-brain/src/cortex_reindex.rs:229`, wired `src/main.rs:851` | **600 s**, leader-gated |
| Embed refresh (bge-m3, 1024-dim, batch 64, ≤5000/tick) | `cortex_embed.rs:361`, `main.rs:871` | **3600 s** |
| Community summary (LLM, ≤20/tick) | `community_summary.rs:711`, `main.rs:895` | **3600 s** |

Knobs: kill-switches `cortex_index_mode` / `cortex_embed_mode` / `cortex_summary_mode` and corpus list `cortex_reindex_corpora` in `fleet_secrets`; per-tick caps `FORGEFLEET_CORTEX_EMBED_MAX_PER_TICK` (5000) and `FORGEFLEET_CORTEX_SUMMARY_MAX_PER_TICK` (20). **The intervals themselves are hardcoded** in `src/main.rs` (600/3600/3600) — the one set of numbers with no knob. (Stale comments at `main.rs:837` say "every hour" for the 10-min reindex tick.)

Session-end trigger: impossible today. `ff-sessions/src/session.rs:242` `end()` is an in-memory DashMap state flip — nothing persisted, no event. The closest durable signal is `ff_interactions` (one row per turn, `idx_ff_interactions_session (session_id, ts)`), from which "no turn for N minutes" is derivable but currently unconsumed.

Live state (2026-07-19): only `forge-fleet` is registered for reindex; `cortex_generations.last_swapped` = 2026-07-17 — **2 days stale on this actively-built repo**, i.e. the daemon tick is not running against this checkout's DB or the leader isn't executing it. This repeats the 2026-06-19 incident documented at `cortex_reindex.rs:1-18` (corpus 4 days behind HEAD, `cortex_find` returning 0 hits). Cadence design must assume the tick can silently die → observability requirement (§4, tick registry).

### 1.2 Scratchpad (working memory)

`crates/ff-agent/src/scratchpad.rs`. Five fixed blocks (`task/decisions/findings/state/scratch`), scopes `session|agent|project`, 6144-byte cap per scope (`queries.rs:7405`, `agent_memory_caps`). Consolidation is **inline and write-triggered only**: any write pushing the scope over cap runs `consolidate_and_forget` (`scratchpad.rs:205-285`) — evict in order `scratch→findings→state→task→decisions`, LLM-summarize the block to ~half, **push the full pre-summary content into `brain_knowledge_candidates`** (`kind=working-memory-eviction`), audit to `agent_memory_evictions`, hard-trim backstop if the summarizer is down. No timer, no background pass.

Live state: healthy on the write side — 608 eviction rows, most recent 2026-07-20; one project scope at 6395 bytes (right at cap, mid-consolidation churn).

### 1.3 Brain graph + candidates

`brain_vault_nodes` (485 k nodes live) carries per-node temporal validity (`valid_from`/`valid_until`/`superseded_by`), hits/last_accessed, `community_id`, `embedding vector(1024)`. `brain_vault_edges` (873 k live) is typed but **has no temporal validity columns** — edges are only ever inserted or deleted. `brain_communities`: 21 k clusters with LLM summaries. `brain_knowledge_candidates` (`schema.rs:499`) is the ingestion gate: extractors and scratchpad evictions land there as `status='pending'` awaiting review.

**Live state: 635 pending, 0 in any other status.** The episodic→semantic pipeline has a front half and no back half.

### 1.4 Schedulers available for a "dreamer"

- `deferred_tasks` (`schema.rs:698`): trigger types `node_online|at_time|manual|now` (+`operator`), one-shot status machine, **no recurrence**. Worker: 10 s poll / 4 concurrent in forgefleetd (`main.rs:911`), 2 h per-task cap.
- `project_schedules` (V79): real cron expressions, but project-scoped task templates.
- **Daemon tick registry** (`ff-agent/src/daemon.rs:44-98`): named ticks, per-tick interval, LeaderOnly/EveryNode scope, watchdog (30 s check / 300 s timeout), 5 s leader cache. Council verdict 2026-07-01 (`plans/daemon-tick-registry.md`): all periodic work migrates here; no new ad-hoc `tokio::interval` loops.
- Wall-clock-gated daily pattern already proven: `nightly_telegram_digest` (`ha/periodic.rs:185`) = 60 s due-check tick + fire once/day ≥ 08:00 local, deduped by deterministic session id.

Existing consolidation-adjacent loops: procedural-memory consolidation every 6 h (7-day lookback, ≥3 sessions/≥80 % success → `agent_procedures`; `procedural_memory.rs:263`, `main.rs:829`); brain mirror (CLI memory dirs → Obsidian `Inbox/`, filesystem watch, every node).

---

## 2. What the other memory systems do (research)

### Zep — bi-temporal knowledge graph
Zep ([arXiv:2501.13956](https://arxiv.org/html/2501.13956v1)) builds on Graphiti and tracks **two timelines per fact**: valid time (when true in the world: `t_valid`/`t_invalid` on every edge) and transaction time (when ingested). On ingestion it compares new edges against semantically related existing edges with an LLM; on contradiction it **invalidates rather than deletes** — sets the old edge's `t_invalid` to the new edge's `t_valid`. Consolidation is therefore **continuous and ingestion-driven**, not batch: no nightly re-derivation of the whole graph, just point-invalidation at write time. ([Zep temporal KG overview](https://www.getzep.com/ai-agents/temporal-knowledge-graph/), [Beyond static graphs](https://blog.getzep.com/beyond-static-knowledge-graphs/))

**Takeaway for ff:** nodes already have validity fields; edges don't. Supersede-don't-delete at edge level is the cheapest way to make `brain_search` time-aware and to let the dreamer resolve contradictions without destructive rewrites.

### Letta / MemGPT — self-editing blocks + sleep-time compute
Letta's core memory is labeled, size-capped blocks the agent edits with tools (`memory_replace`, `memory_rethink` for whole-block rewrites) — ff's Scratchpad is already this shape. The consolidation innovation is **sleep-time agents** ([docs](https://docs.letta.com/guides/agents/architectures/sleeptime/), [memory blocks](https://www.letta.com/blog/memory-blocks/)): a background agent sharing the primary agent's memory, triggered **every N steps (default 5)** and during idle periods, that consolidates fragmented entries, dedups/reorganizes blocks, and archives stale content — moving memory quality work off the hot path ([sleep-time compute, arXiv:2504.13171](https://arxiv.org/html/2504.13171v1)).

**Takeaway for ff:** cap-triggered inline eviction (current) is Letta's *lazy* path; the missing half is the *sleep-time* rewrite pass over long-lived (`project`/`agent`-scope) pads while nothing is running. Cadence: idle/nightly, not per-write.

### Graphiti — episodic → semantic → community, incremental
Graphiti ([Neo4j blog](https://neo4j.com/blog/developer/graphiti-knowledge-graph-memory/), [docs](https://help.getzep.com/graphiti/getting-started/overview)) maintains three tiers: **episode subgraph** (raw messages/events, provenance), **semantic entity subgraph** (entities/facts extracted and resolved against existing nodes *at ingestion time*), and **community subgraph** (label-propagation clusters with summaries). Communities update incrementally as nodes arrive, but the docs recommend **periodically rebuilding communities** (`build_communities`) for optimal grouping ([communities doc](https://help.getzep.com/graphiti/core-concepts/communities)).

**Takeaway for ff:** ff's tiers map cleanly — episodes = `ff_interactions`/`brain_threads`/eviction payloads; semantic = `brain_vault_nodes`+`edges` gated by `brain_knowledge_candidates`; communities = `brain_communities`. Graphiti resolves candidates **at ingestion**; ff queues them and never resolves. Match Graphiti by draining the queue continuously (hourly, bounded) and doing the full community rebuild periodically (nightly) — the hourly summary tick already only touches changed communities via `member_hash`, which is the incremental half.

### basic-memory — markdown as the durable substrate
basic-memory keeps knowledge as human-editable markdown (entities/observations/relations in frontmatter + wikilinks) with the DB as a derived index; sync is file-watch driven, no consolidation daemon — the human editing loop *is* consolidation.

**Takeaway for ff:** this is the Obsidian-export lane ff already built (vault parser `ff-brain/src/vault.rs`, brain mirror → `Inbox/`, export daemon 3a13244e). The dreamer should close the loop: promote accepted candidates into vault markdown (Inbox → curated note) so operator-visible memory and graph memory stay one system, and vault edits flow back via the existing watcher.

---

## 3. Tuned cadences (the decisions, with rationale)

### D1 — Cortex structural reindex: event-driven first, 10-min reconciliation second
Keep the **600 s leader tick** purely as reconciliation (catches non-commit writes, hook-less clones, other corpora). Add the **session-end event trigger** the work item asks for:

1. Persist session end. Cheapest durable form: an `outcome`-less terminal row or an `agent_sessions`/`sessions` status update at `SessionState::end()` (`ff-sessions/src/session.rs:242`) — plus NOTIFY `cortex_reindex` per `plans/daemon-tick-registry.md` step 6 (LISTEN/NOTIFY for hot queues, polling stays as reconciliation).
2. Sub-agent harness sessions already commit per work item, so the git post-commit hook covers them; the session-end trigger matters for interactive CLI sessions that edit without committing. Debounce: coalesce to at most one reindex per corpus per 60 s (reuse the leader-gate window, `cortex_reindex.rs:246`).
3. Expose intervals as `fleet_secrets` knobs with the current values as defaults: `cortex_reindex_interval_secs=600`, `cortex_embed_interval_secs=3600`, `cortex_summary_interval_secs=3600`. Fix the stale "every hour" comments at `src/main.rs:837,859,879` while touching this.

Why not shorten 600 s: an incremental pass on an unchanged tree is cheap but not free (corpus scan + hash diff over the tree, 50 k-dir walk bound); the hook + session-end events already give near-real-time freshness where it matters. Why not lengthen: the module's own incident history (`cortex_reindex.rs:14`) shows staleness is the failure mode that actually bites.

### D2 — Chain embedding after reindex; hourly tick becomes reconciliation
Today a symbol re-indexed at t+0 can sit with NULL embedding until the hourly embed tick — `cortex_embed.rs:287-296` documents exactly this staleness. Since `IncrementalReport` (`cortex.rs:337`) knows whether files changed, the reindex tick should invoke the embed drain (same bounded `EMBED_BATCH`/`MAX_ITERATIONS` machinery) immediately when `files_changed > 0`. Hourly tick stays as the catch-all (endpoint was down, cap hit). No new knobs needed beyond D1's.

### D3 — Community maintenance: hourly incremental + nightly rebuild
Keep the hourly changed-only summary pass (`member_hash` gating, 20/tick). Add a **nightly full community re-detection** into the dreamer deep pass (D4): Graphiti's own guidance is that incrementally-updated label-propagation clusters drift and need periodic rebuild. Nightly is proportionate at 485 k nodes / 21 k communities; the 2026-06-19 "5,406 of 5,414 communities had no summary" incident (`community_summary.rs:608`) shows what un-scheduled maintenance decays into.

### D4 — The dreamer: two-speed consolidation, registered as ticks
**Not** a recurring `deferred_tasks` row — the table is one-shot by design and the council verdict routes all periodic work through the tick registry. The dreamer is two registry ticks (both LeaderOnly):

**`brain_candidate_triage` — hourly, bounded.** Drain `brain_knowledge_candidates` oldest-first, ≤50/tick (knob `FORGEFLEET_DREAMER_MAX_CANDIDATES_PER_TICK`), via a cheap fleet LLM (tiered cascade — this is classic `fleet_run` work). Per candidate, Graphiti-style ingestion resolution: extract/resolve entities against existing `brain_vault_nodes`, dedup, then `promote` (create/update node + edges, mark `accepted`), `merge` (fold into an existing node), or `reject` (mark, keep for audit). Zep-style contradiction handling: if a promoted fact contradicts an existing edge, **invalidate the old edge (set `invalid_at`), never delete** (needs D6). At 50/tick the current 635-row backlog clears in ~13 h, and steady-state (evictions + extractors) stays near zero.

**`brain_dreamer_nightly` — 60 s due-check, fires once/day at 03:00 local** (clone the `nightly_telegram_digest` wall-clock-gate + deterministic-dedup pattern, `ha/periodic.rs:31-60`; knob `FORGEFLEET_DREAMER_HOUR_LOCAL=3`, well clear of the 08:00 digest that reads its outputs). Deep pass, in order:
1. Full community re-detection + summary refresh for changed communities (D3).
2. **Sleep-time scratchpad rewrite** (Letta): for each `project`/`agent`-scope pad idle >24 h, one LLM pass to dedup/reorganize blocks *below* cap — quality rewrite, not eviction; audit to `agent_memory_evictions` with a distinct `summarizer` tag.
3. Decay/validity sweep: nodes with `valid_until < now()` or superseded get demoted from search defaults; recompute staleness from `last_accessed`/`hits` (fields already on `brain_vault_nodes`).
4. Promote the day's accepted candidates into Obsidian vault markdown (basic-memory lane): write curated notes via the existing vault writer so `Inbox/` → curated graduation is automated, operator-reviewable in git/Obsidian.
5. Heavy offloadable jobs (e.g. corpus-wide re-embedding after a model change) are **enqueued as `deferred_tasks`** (`at_time`/`node_online`) targeting capable nodes — this is the legitimate deferred_tasks role in the dreamer: dispatch, not scheduling.

**Cost note:** the triage tick is the only new recurring LLM load: ≤50 small calls/h worst-case, tiered-local first, and logged to `ff_interactions` (training corpus — dogfooding requirement). Nightly pass is bounded by changed-community count + idle-pad count.

### D5 — Keep what already works
- Inline cap-triggered `consolidate_and_forget`: unchanged. It is the Letta lazy path and live data shows it functioning.
- Procedural-memory 6 h consolidation: unchanged cadence; migrate its loop into the tick registry for watchdog coverage.
- Git hook + `ff cortex watch`: unchanged.

### D6 — Bi-temporal edges (schema change, one migration)
New forward-only migration (next free version at the end of `PG_MIGRATIONS` — check the highest version across branches first): add to `brain_vault_edges` nullable `valid_from timestamptz`, `invalid_at timestamptz`, `superseded_by uuid` mirroring the node fields; partial index on `invalid_at IS NULL`. Read paths (`brain_search`, `brain_graph_neighbors`, cortex queries) default to `invalid_at IS NULL`. Writers: only the dreamer triage sets `invalid_at` (contradiction) — cortex structural indexing keeps its delete/GC semantics for *code* edges, which are ground-truthed by the filesystem, not by assertions; bi-temporality applies to knowledge/fact edges. This is deliberately FalkorDB-forward: `plans/cortex-falkordb-backend.md` keeps Postgres as production until parity, and validity intervals port directly to graph properties later.

---

## 4. Implementation plan (phased, each phase independently shippable)

| Phase | Work | Size |
|---|---|---|
| **P1** | Interval knobs (`cortex_*_interval_secs` secrets) + D2 embed chaining + fix stale comments. No schema change. | S |
| **P2** | `brain_candidate_triage` hourly tick (dreamer v0): drain queue with promote/merge/reject via `fleet_run`. Clears the 635-row backlog. | M |
| **P3** | Persist session end + LISTEN/NOTIFY reindex trigger with 60 s coalescing (D1). | M |
| **P4** | `brain_dreamer_nightly` tick: community rebuild, sleep-time pad rewrite, decay sweep, vault promotion, deferred_tasks dispatch for heavy jobs (D4). | M/L |
| **P5** | Bi-temporal edge migration + read-path filters + triage-side invalidation (D6). | M |

All new loops register in the daemon tick registry (`ff-agent/src/daemon.rs`) with names, LeaderOnly scope, and watchdog coverage — per the council verdict, no new ad-hoc interval loops. Every dreamer LLM call routes through fleet dispatch so it lands in `ff_interactions`.

**Operational precondition surfaced by this audit:** verify why `cortex_generations.last_swapped` for `forge-fleet` is 2 days old despite a 600 s tick — either the leader daemon isn't running the tick or `cortex_index_mode` is off. Tuning cadence is moot while the tick is silently dead; `ff daemon ticks` observability (tick registry item 2) is the durable fix.

## Sources

- [Zep: A Temporal Knowledge Graph Architecture for Agent Memory (arXiv:2501.13956)](https://arxiv.org/html/2501.13956v1)
- [Zep — What is a temporal knowledge graph](https://www.getzep.com/ai-agents/temporal-knowledge-graph/) · [Beyond static knowledge graphs](https://blog.getzep.com/beyond-static-knowledge-graphs/)
- [Graphiti overview (Zep docs)](https://help.getzep.com/graphiti/getting-started/overview) · [Communities](https://help.getzep.com/graphiti/core-concepts/communities) · [Neo4j: Graphiti knowledge graph memory](https://neo4j.com/blog/developer/graphiti-knowledge-graph-memory/)
- [Letta sleep-time agents (docs)](https://docs.letta.com/guides/agents/architectures/sleeptime/) · [Memory blocks](https://www.letta.com/blog/memory-blocks/) · [Agent memory](https://www.letta.com/blog/agent-memory/) · [Sleeptime best-practices (forum)](https://forum.letta.com/t/sleeptime-agents-for-memory-consolidation-best-practices-guide/154)
- [Sleep-time Compute: Beyond Inference Scaling at Test-time (arXiv:2504.13171)](https://arxiv.org/html/2504.13171v1)
- Internal: `plans/daemon-tick-registry.md` (council 2026-07-01), `plans/cortex-falkordb-backend.md`, `plans/cortex-improvement-research.md`, `crates/ff-brain/src/{cortex_reindex.rs,cortex_embed.rs,community_summary.rs,procedural_memory.rs}`, `crates/ff-agent/src/{scratchpad.rs,daemon.rs,ha/periodic.rs}`, live DB via `ff db query` (2026-07-19).
