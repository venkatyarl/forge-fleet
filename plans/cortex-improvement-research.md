# Cortex vs graphify/CRG + Improvement Research

Source: kimi analysis + online SOTA research (2026-06-17). Full transcript: `/tmp/ff_cortex_research.md`.

## Verdict: which is superior?
**For ForgeFleet, Cortex is the superior foundation.** It has caught up on the CRG features that matter most (MCP surface, incremental indexing, `tests_for`, `detect_changes`-style review, community summaries, confidence tiers) while staying deterministic, cost-efficient, and natively integrated into the Brain graph.
- **CRG (code-review-graph)** = capable external fallback but duplicates indexing and fragments context.
- **graphify** = wins only for cross-domain sensemaking over docs+media+code; wrong engine for precise large-scale code navigation.

**Cortex's current limits:** narrower language coverage (5 vs 19+), no built-in visualization, heuristic tree-sitter (no LSP/compiler-accurate resolution), partial cross-corpus querying, single-level Louvain communities (not hierarchical GraphRAG).

## Prioritized improvement ideas (from 2025–26 research: CodeGraph, RepoGraph, GraphRAG, Sourcegraph/SCIP, Serena/LSP-MCP, RANGER/SpIDER, LocAgent)

### P0 — highest impact
1. **Unified `cortex_context` MCP tool** — given a symbol or NL query, return in ONE call: definition + callers/callees + siblings + imports + community summary + snippets. Biggest agent token-saver. *(This also directly answers the "is cortex consulted in the loop" gap — make it the one call the agent loop always makes.)*
2. **Hybrid retrieval: vector + graph + rerank** — embeddings + graph-neighborhood expansion + cross-encoder reranker. Graph-aware beats pure embeddings for repo-level tasks.
3. **Hierarchical GraphRAG communities** — multi-level Leiden/Louvain + LLM summaries per level + map-reduce over summaries for broad "explain this subsystem" queries; incremental re-summarization of changed communities only.
4. **Data-flow / type / control edges** — beyond calls/imports: `implements`/`extends`/`trait_bounds`, `reads`/`writes`/`mutates`, `branches_to`/`loops_over` (RepoGraph/LocAgent localization).

### P1 — strong additions
5. **SCIP ingestion or LSP bridge** — compiler-accurate symbol IDs where tree-sitter heuristics fall short (Serena/agent-lsp pattern).
6. **Line/statement-level graph** — fine-grained def/ref nodes (RepoGraph improves SWE-bench) with ego-subgraph retrieval for bug localization.
7. **Cross-repo merged corpus view** — unified namespace across corpora (monorepo/workspace nav; multi-computer relevance). ✅ PARTIAL: `ff cortex find --all-corpora <q>` (`find_symbols_all_corpora`) searches EVERY indexed corpus at once, fan-in-ranked, each hit tagged with its repo. Cross-repo *relationship* traversal (calls spanning repos) is the remaining piece.
8. **Visualization layer** — interactive HTML/SVG/GraphML + Obsidian export (graphify-style).
9. **Framework-aware routing edges** — URL/HTTP-method → handler (Axum/Actix/Express/FastAPI/…).
10. **Runtime test-coverage edges** — augment static `tests_for` with coverage-weighted `test_touches_code` for trustworthy change-risk.

### P2 — future
11. Agentic graph traversal (MCTS/ReAct, RANGER/LocAgent). 12. Broader languages (Go/C/C++/C#/Ruby/Swift/Kotlin/…). 13. Build/config graph (`Cargo.toml`/`package.json` → `depends_on`/`builds_into`). 14. NL→Cypher/SQL query generation. 15. Token-budget subgraph selection (PageRank/learned relevance). 16. Static dead-code / god-symbol / circular-import alerts.

## Suggested sequencing
1. Ship `cortex_context` (P0.1) — immediate agent productivity win **and** the hook to make cortex auto-consulted in the loop.
2. Data-flow/type edges (P0.4) + hybrid retrieval (P0.2) — biggest accuracy gains.
3. Hierarchical GraphRAG (P0.3) — "explain this subsystem" at scale.
4. SCIP/LSP (P1.5) — close the precision gap.
5. Visualization + cross-repo (P1.7–8) — usability + scaling.

## Build status (audited 2026-06-24)
- ✅ **P0.1 `cortex_context`** — BUILT (#539): one MCP call = definition + callers/callees +
  impact + community summary + snippet. The agent loop's default Cortex call.
- ✅ **P0.4 data-flow / type / trait edges** — ALREADY BUILT by the universal-graph work:
  `reads`/`writes` (dataflow), `has_field` (types), `implements`/`extends` (trait relationships)
  are all live in the graph. Only intra-function control-flow (`branches_to`/`loops_over`) is
  unbuilt — low value, skipped.
- ✅ **P0.3 GraphRAG hierarchical** — COMPLETE (#551/#552/#553/slice-3): multi-level
  Louvain hierarchy + parent tree + per-level map-reduce summaries, surfaced in
  cortex_explain as a symbol's subsystem chain. Slices:
  - ✅ slice 1 (this PR): full multi-level Louvain (`cluster_calls_graph_levels` —
    local-move → aggregate condensed graph → repeat; unit-tested: level-0 match,
    no-edge singletons, monotonic coarsening). V144 adds `level` to
    `brain_code_communities` (per-level unique index); `detect_code_communities`
    stores every level's communities (each distinct grouping once, at its finest
    level). Summarizer scoped to `level = 0` (it resolves members via the level-0
    `code_community_id`, so coarse rows wait for slice 2).
  - ✅ slice 2a (this PR): parent links — `community_parents` (pure, unit-tested)
    finds each community's immediate strictly-larger enclosing community; V145 adds
    `parent_member_hash` (indexed) to `brain_code_communities`; detection stores it.
    The hierarchy is now a navigable tree (children of P = rows WHERE
    parent_member_hash = P.member_hash).
  - ✅ slice 2b (this PR): per-level map-reduce summaries. After the level-0 pass,
    `summarize_levels_above_zero` processes levels ASCENDING (1,2,…): each level-N
    community's summary is synthesized from its CHILDREN's summaries (rows whose
    parent_member_hash points at it) via `build_hierarchy_summary_prompt` (pure,
    unit-tested) — map-reduce, not raw code. Removed the `eligible==0` early-return
    so the hierarchy pass runs even when level 0 is fully summarized. No migration
    (reuses summary*/parent/level columns).
  - ✅ slice 3 (this PR): `explain_community` walks `parent_member_hash` (recursive
    CTE) from a symbol's finest community up the tree, returning a `subsystem_chain`
    (each ancestor's level + member_count + summary + god symbol). Surfaced in the
    `cortex_explain` MCP tool JSON and `ff cortex explain` (JSON + a human-readable
    "subsystem hierarchy" section). Answers "what larger subsystem is this part of,
    and what does it do?" at increasing scope.
  - ⏳ slice 3: wire the hierarchy into `cortex_explain` so "explain this
    subsystem" traverses levels.
- 🔨 **P0.2 hybrid retrieval** — vector (`find_symbols_semantic`) + graph-neighborhood
  expansion + bge-reranker rescoring, combined into one `cortex_search`.
- (OPS.1 — auto-refresh the Cortex SQLite mirror — removed: the mirror was deleted in #531;
  Cortex is Postgres-only now.)
