# Cortex roadmap — gap matrix vs CRG/graphify + research takeaways

> 2026-06-12. Durable record of the CRG-vs-graphify-vs-Cortex comparison and the
> code-graph research pass (the overnight workflow's synthesis agents returned
> empty, so no document survived; this replaces it). Multi-language Cortex
> (TS/TSX/JS + Java) landed on main `bb2def42b`, detection floor fix `0f77a9e75`,
> live-verified against HireFlow360 (498 Java files, 4,583 symbols, 13,626 call
> sites, `ff cortex callers <JavaClass>` resolves).

## Where Cortex stands (honest gap matrix)

| Capability | CRG (code-review-graph MCP) | graphify | Cortex |
|---|---|---|---|
| Multi-language code | ✅ tree-sitter, many langs | ✅ | ✅ rust/ts/tsx/js/java (python: extractor design exists in a superseded stash, not landed) |
| Change-aware review (detect_changes, risk-scored diff) | ✅ | – | ❌ **biggest gap** |
| Test-coverage mapping (tests_for) | ✅ | – | ❌ |
| Execution flows / affected-flows | ✅ | – | ❌ |
| Provenance + confidence tiers (EXTRACTED/INFERRED) | – | ✅ | ❌ (all edges equal) |
| Visualization (HTML graph) | – | ✅ | ❌ |
| Budgeted GraphRAG answers (query --budget) | partial | ✅ | partial (semantic search, no answer synthesis) |
| Cross-repo merge | ✅ | ✅ | partial (multi-corpus, no merged view) |
| Wiki generation | ✅ | ✅ | ❌ |
| Incremental (changed-files-only) reindex | ✅ | ✅ --update | ❌ full rewipe each run |

## Research takeaways (2026-06-12 pass)

1. **CodeGraph** (<https://github.com/colbymchenry/codegraph>; overview
   <https://toknow.ai/posts/codegraph-knowledge-graph-ai-coding-agents-fewer-tokens/>;
   Big Hat writeup <https://www.bighatgroup.com/blog/codegraph-2026-05-26/>):
   the hot-2026 pattern — pre-indexed local graph served over **MCP**, ~47%
   token / 58% tool-call reduction for agents. Cortex has the graph but **no MCP
   query surface** — agents can't consume it the way they consume CRG. Arguably
   the highest-value next build: expose cortex_search/callers/callees/impact as
   fleet MCP tools.
2. **Microsoft GraphRAG** (<https://arxiv.org/pdf/2404.16130>; incremental-indexing
   issue <https://github.com/microsoft/graphrag/issues/741>): hierarchical Leiden
   communities + **community summaries** for query-time synthesis; incremental
   updates re-summarize only changed communities. Cortex has communities
   (`ff cortex embed`) but no LLM summaries per community — adding them (fleet
   LLMs are idle!) unlocks "explain this subsystem" answers.
3. **SCIP** (<https://sourcegraph.com/blog/announcing-scip>): Sourcegraph's
   now-independent index format — language-agnostic symbol IDs for precise
   cross-repo navigation. The long-term direction if Cortex ever needs exactness
   beyond tree-sitter heuristics.

## Recommended roadmap (priority order)

1. **Incremental reindex** — changed files only; makes the post-commit hook cheap
   (today every commit triggers a full rewipe+rescan).
2. **MCP query surface** — cortex_search / callers / callees / impact as fleet MCP
   tools (the CodeGraph pattern; replaces agents' dependence on CRG).
3. **detect_changes vs git diff** — change-aware, risk-scored review context
   (CRG's killer feature).
4. **Community summaries via fleet LLMs** — GraphRAG-style per-community
   summaries, re-summarize only changed communities.
5. **Provenance/confidence on edges** — EXTRACTED vs INFERRED tiers (graphify's
   one structural advantage), so downstream consumers can filter heuristic call
   edges (e.g. the dotty-resolver's kept-as-written externs).

## Related state

- Hook-fires-on-commit: verified 2026-06-12 (scratch repo, post-commit reindex +
  callers query both worked).
- Corpora cleanup: `ff brain corpus delete <slug> --yes` added (`0cf184885`);
  9 test/stale corpora removed.
- Python support: parallel `cortex_lang.rs` design (incl. Python extractor) is
  parked in `stash@{0}` + its untracked file in `stash@{0}^3` — superseded by the
  landed in-cortex.rs design; port the Python extractor onto the landed
  architecture rather than popping the stash.
