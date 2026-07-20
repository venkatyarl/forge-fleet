# Memory Systems Providers + Hermes User-Modeling — Research (2nd pass)

Source: work_item `5e397287-1c3b-4b2e-8c39-00f75623b86d` ("RESEARCH: memory systems
providers + Hermes user-modeling (2nd pass)", migrated from DR session backlog
2026-07-17). Web research 2026-07-19; fleet research session
`46e8d590-f93d-4748-8a23-5404f305d16e` (`ff research --show <id>` for the
fleet-side report). Feeds sibling backlog item `3bb7ffd5` — "Memory architecture:
realm-scoped subgraphs + GLOBAL user-model + cross-realm operates_on edges".

## Verdict (TL;DR)

The field has converged on exactly the shape the `3bb7ffd5` idea sketches, and ff
already owns most of the substrate:

1. **Namespaced subgraphs are the industry pattern** — Graphiti `group_id`,
   Honcho workspaces, Supermemory containers, Hermes per-profile isolation. ff's
   `realm` frontmatter field (`ff-core/src/schema/basic_memory.rs`) + brain vault
   graph is the same idea; what's missing is realm-scoped *retrieval* (query one
   realm's subgraph, not the whole vault).
2. **A GLOBAL user-model separate from task memory is the single clearest
   convergent feature** — Hermes `USER.md` (bounded, ~500 tokens), Honcho "global
   representation" (aggregates everything a peer says about themself, distinct
   from per-observer "local representations"), Mem0/Supermemory/RetainDB profile
   objects. ff has NO user-model store today — the Scratchpad blocks
   (task/decisions/findings/state/scratch) are all task-scoped. This is the gap
   to fill first.
3. **Cross-partition typed edges are how graph-memory systems relate the user to
   domains** — Graphiti custom edge ontologies (Pydantic-defined edge types) and
   Hindsight's entity graph do this; `operates_on` edges from a global user-model
   node into realm subgraphs is a straightforward extension of the brain vault's
   existing typed-edge model (`RELATION_TYPES` in basic_memory.rs already ships
   `relates_to/part_of/implements/derived_from/supersedes/depends_on` — add
   `operates_on`).
4. **Bounded + curated beats append-only.** Hermes's strongest design choice:
   memory that ERRORS at the cap instead of silently compacting, forcing the
   agent to consolidate. ff's Scratchpad (6 KB/scope, consolidate-and-forget)
   independently landed on the same design — keep it; don't adopt append-only
   vector memory for the working set.
5. **Don't adopt an external provider wholesale.** Every capability the nine
   Hermes providers sell (fact extraction, temporal KG, tiered retrieval, trust
   scoring) maps onto substrate ff already runs (Postgres + pgvector-style
   embedding column on `brain_vault_nodes`, vault edges, cortex, fleet LLMs for
   extraction). The valuable imports are *patterns*, itemized below.

## 1. ff's current memory stack (discovery-first inventory)

| Layer | Where | Nature |
|---|---|---|
| Scratchpad (`ff memory` / `memory_*` MCP) | Postgres, scoped session/agent/project | Bounded 6 KB/scope, fixed blocks (task/decisions/findings/state/scratch), self-curating |
| Scoped file memory | `ff-agent/src/scoped_memory.rs` → `~/.forgefleet/memory/` | Global/Project/Folder/Temp/FleetBrain/HiveMind scopes, date-organized |
| FORGEFLEET.md discovery | `ff-agent/src/memory.rs` | CLAUDE.md-style hierarchical context injection |
| Basic-memory notes | `ff-core/src/schema/basic_memory.rs` (merged 2026-07-19) | Markdown + YAML frontmatter (incl. `realm`) + `[[wikilink]]` typed relations |
| Brain vault graph | `brain_vault_nodes` / `brain_vault_edges` (Postgres, embedding column) | Cross-project knowledge graph, communities, provenance |
| Cortex | code-symbol graph | Code lobe, calls/imports edges |
| Session/thread store | `brain_messages`, `session_brain`, `research_*` | Raw episodic record |

Missing vs the field: (a) a user-model store, (b) realm-scoped subgraph
retrieval, (c) temporal validity on vault facts, (d) an automatic
extract-at-session-end write path from episodic → semantic memory.

## 2. Hermes Agent memory architecture (Nous Research)

[hermes-agent](https://github.com/nousresearch/hermes-agent) ("the agent that
grows with you") is the most instructive reference because its *shape* matches
ff: a local agent CLI with built-in bounded memory plus a plugin bus of external
providers.

**Built-in layer** (always on):
- Two files in `~/.hermes/memories/`: `MEMORY.md` (~800 tokens / 2,200 chars —
  environment, conventions, lessons) and `USER.md` (~500 tokens / 1,375 chars —
  identity, preferences, communication style). Entries `§`-delimited.
- Injected as a **frozen snapshot at session start** — mid-session writes appear
  next session. Deliberate: preserves the LLM prefix cache.
- **No auto-compaction**: a write past the cap returns an ERROR; the agent must
  consolidate (at 80%+ it merges entries into denser versions) or delete first.
  `replace` uses substring matching for surgical edits.
- Episodic recall via `session_search` (SQLite FTS5 over full conversation
  history + LLM summarization) — unbounded history stays OUT of the prompt and
  is pulled on demand.

**Provider plugin layer** (exactly one active alongside built-in, selected via
`hermes memory setup`): the plugin bus (1) injects provider context into the
system prompt, (2) prefetches memories per turn non-blocking, (3) syncs turns
after responses, (4) extracts memories at session end, (5) mirrors built-in
writes to the provider, (6) registers provider-specific tools. Per-profile
isolation throughout (local providers get per-profile dirs; cloud providers get
profile-scoped project names).

## 3. Hermes user-modeling = Honcho dialectic (Plastic Labs)

The "user-modeling" half of the task title is Hermes's Honcho integration —
AI-native cross-session user modeling:

- **Peer-centric model**: workspaces → peers (humans AND agents are both peers)
  → sessions → messages. In Hermes: ONE global user peer shared across all
  profiles + one AI peer per profile, sharing a workspace. Each AI peer builds
  its own view of the same user (coder profile stays code-oriented, writer
  profile editorial).
- **Deriver (reasoning layer)**: continuously processes messages → fact
  derivation, conversation summarization, peer-card generation, and "dreaming"
  (offline reflective consolidation). Tasks touching the same peer
  representation serialize; everything else parallelizes.
- **Global vs local representations** — the key idea for `3bb7ffd5`:
  - *Global representation*: aggregation of everything a peer has ever said
    about themself — observer-independent. THE global user-model.
  - *Local representation*: how peer B models peer A from only the messages B
    actually observed (off by default, per-peer/per-session opt-in).
- **Dialectic API**: natural-language queries answered by "an expert on the
  peer" — prompt hydration asks Honcho about the user rather than grepping raw
  memory. Hermes exposes `honcho_profile/search/context/reasoning/conclude`
  tools with cost knobs (`contextCadence`, `dialecticCadence`, `dialecticDepth`).
- Context endpoint auto-maintains session summaries (short every 20 messages,
  long every 60).

## 4. Provider survey (the nine Hermes plugins + the big standalone frameworks)

| Provider | Architecture | Hosting | Standout pattern for ff |
|---|---|---|---|
| **Honcho** | Peer graph + deriver + dialectic LLM reasoning | cloud/self-host | Global-vs-local user representations; query-the-expert API |
| **OpenViking** | Filesystem-style hierarchy, tiered retrieval L0 (~100 tok) → L1 (~2 k) → L2 (full), `viking://` URIs | self-host (AGPL) | Tiered context loading — cheap index first, hydrate on demand |
| **Mem0** | Server-side LLM fact extraction + semantic search + dedup; vector+graph+KV | cloud/docker/in-process | Hands-off extraction pipeline; 49.0% LongMemEval (GPT-4o) |
| **Hindsight** (vectorize-io) | Postgres-native KG: retain/recall/reflect; World/Experience/Mental-Model banks; 4 parallel retrieval strategies (semantic, BM25, graph, temporal) + cross-encoder rerank | cloud/local-Postgres | Closest stack match to ff (Postgres); `reflect` = cross-memory synthesis; claims #1 LongMemEval, 50–500 ms recall |
| **Holographic** | Local SQLite FTS5 fact store + trust scoring + HRR algebra | local only | Asymmetric trust feedback (+0.05 helpful / −0.10 unhelpful) on memories |
| **RetainDB** | Hybrid vector+BM25+rerank, 7 memory types, delta compression | cloud ($20/mo) | Typed memory taxonomy |
| **ByteRover** | Hierarchical knowledge tree, fuzzy → LLM-driven tiered search | local-first + sync | **Pre-compression extraction** — salvage insights before context compaction discards them |
| **Supermemory** | Semantic LTM + profile + session-end conversation-graph ingestion | cloud | **Context fencing** — strips recalled memories from captured turns so recall never re-ingests itself |
| **Memori** | Structured LTM, background turn capture, tool-aware context | cloud | Tool-aware episodic capture |
| **Zep / Graphiti** | Bi-temporal knowledge graph: episodes → entity/edge extraction, validity windows, edge invalidation (supersede, never delete), `group_id` subgraph namespacing, custom Pydantic entity/edge ontologies, hybrid semantic+BM25+traversal retrieval, sub-second, backends Neo4j/FalkorDB/Kuzu/Neptune | OSS + cloud | The reference design for realm-scoped subgraphs + typed cross-realm edges; 63.8% LongMemEval |
| **Letta (MemGPT)** | OS-style memory hierarchy — agent self-manages core blocks vs archival storage via tools | OSS + cloud | Agent-managed paging; ff's Scratchpad already does the bounded-core half |
| **LangMem / Cognee** | In-framework memory (LangGraph) / OSS document→KG ("memify") pipelines | OSS | Cognee's ECL (extract-cognify-load) pipeline shape for episodic→semantic promotion |

Notes:
- FalkorDB as a Graphiti backend is convenient: `plans/cortex-falkordb-backend.md`
  already evaluates FalkorDB for cortex, so one graph engine could serve both if
  ff ever outgrows Postgres edges.
- Benchmarks (LongMemEval) consistently show graph/temporal systems (Zep,
  Hindsight) beating extraction-only vector memory (Mem0) on multi-session and
  temporal-reasoning questions — relevant because fleet work is long-horizon and
  temporal ("what did we decide last month, and is it still true?").

## 5. Convergent patterns worth importing (2nd-pass synthesis)

1. **Two-speed memory**: bounded curated core injected every prompt (Hermes
   MEMORY.md/USER.md, Letta core blocks, ff Scratchpad ✓) + unbounded episodic
   store searched on demand (Hermes session_search; ff `brain_messages` — ✓ data,
   ✗ no agent-facing FTS recall verb).
2. **Global user-model as a first-class object**, observer-independent, bounded,
   injected everywhere, updated by a background deriver — NOT mined ad hoc from
   episodic memory at question time.
3. **Subgraph namespacing with typed cross-namespace edges** (Graphiti
   `group_id` + custom edge types; Honcho workspaces). Realms should partition
   retrieval by default and be bridged explicitly by edges like `operates_on`.
4. **Bi-temporal facts + supersede-don't-delete.** Graphiti/Hindsight invalidate
   facts with validity windows. basic_memory frontmatter already has
   `valid_from`/`valid_until`/`superseded_by` columns on `brain_vault_nodes` —
   the schema anticipated this; the write path just needs to use it.
5. **Session-end extraction hook** (all nine providers): episodic → semantic
   promotion runs once at commit, not inline per turn. ff analog: a daemon tick
   or dispatch-harness hook that runs a fleet LLM over the session transcript
   and upserts basic-memory notes.
6. **Pre-compression salvage** (ByteRover): before any context compaction,
   extract durable insights. ff analog: hook wherever sub-agent transcripts get
   truncated/summarized.
7. **Context fencing** (Supermemory): tag recalled-memory spans so the
   session-end extractor skips them — prevents recursive memory pollution.
   Cheap to add to any extractor prompt; easy to forget; do it from day one.
8. **Trust/feedback scoring** (Holographic): memories carry a trust score
   adjusted asymmetrically on helpful/unhelpful outcomes; retrieval ranks by it.
   ff analog: `brain_vault_nodes.confidence` + `hits` already exist — wire
   outcome feedback from `ff_interactions`.
9. **Frozen-snapshot injection** for prefix-cache preservation: memory changes
   apply at the NEXT session, not mid-prompt.

## 6. Mapping onto work item `3bb7ffd5` (recommendations)

- **Realm-scoped subgraphs**: treat `realm` as the Graphiti `group_id` — add it
  to vault node/edge queries as a default retrieval filter (realm of the calling
  scope), with an explicit `--all-realms` escape hatch. No new tables; the
  `realm` frontmatter + `brain_vault_nodes.project` columns carry it.
- **GLOBAL user-model**: one bounded basic-memory note per human peer (operator)
  with `realm: global`, Hermes-USER.md-sized (~500 tokens), plus a deriver-style
  updater (daemon tick, fleet LLM) that consolidates evidence from
  `brain_messages`/`ff_interactions` into it. Inject it in every agent system
  prompt next to FORGEFLEET.md. Honcho's split says: keep it
  observer-independent — per-agent impressions are a later, opt-in layer.
- **Cross-realm `operates_on` edges**: add `operates_on` to `RELATION_TYPES` in
  `basic_memory.rs` and emit edges user-model-node → realm subgraph roots
  (projects, machines, domains). Retrieval: hydrate the user model, follow
  `operates_on` into the active realm, THEN do scoped semantic/FTS search —
  i.e., Hindsight's graph+semantic+BM25+temporal quad, all of which ff's
  Postgres can serve.
- **Sequencing** (each step useful alone): (1) `operates_on` relation type +
  global user-model note + injection; (2) session-end extractor with context
  fencing; (3) realm-filtered retrieval; (4) temporal validity in the write
  path; (5) trust feedback from `ff_interactions`.
- **Buy vs build**: build on ff substrate. If a dependency is ever wanted for
  the temporal KG layer, Graphiti (OSS, FalkorDB backend) is the only candidate
  that fits the realm/edge model; Hindsight is the design reference for
  Postgres-native retrieval.

## Sources

- [Hermes Agent repo](https://github.com/nousresearch/hermes-agent) · [Persistent Memory docs](https://hermes-agent.nousresearch.com/docs/user-guide/features/memory) · [Memory Providers docs](https://hermes-agent.nousresearch.com/docs/user-guide/features/memory-providers)
- [Honcho architecture](https://honcho.dev/docs/v2/documentation/core-concepts/architecture) (Plastic Labs)
- [Graphiti (Zep)](https://github.com/getzep/graphiti)
- [Hindsight (vectorize-io)](https://github.com/vectorize-io/hindsight) · [Hindsight FAQ](https://hindsight.vectorize.io/faq) · [arXiv 2512.12818 "Hindsight is 20/20"](https://arxiv.org/html/2512.12818v1)
- Landscape comparisons: [Graphlit survey](https://www.graphlit.com/blog/survey-of-ai-agent-memory-frameworks) · [particula.tech Mem0/Zep/Letta/Cognee test](https://particula.tech/blog/agent-memory-frameworks-tested-mem0-zep-letta-cognee-2026) · [vectorize.io 8-framework comparison](https://vectorize.io/articles/best-ai-agent-memory-systems)
- Fleet pass: research session `46e8d590-f93d-4748-8a23-5404f305d16e`
