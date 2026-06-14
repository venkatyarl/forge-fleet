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
| Multi-language code | ✅ tree-sitter, many langs | ✅ | ✅ rust/ts/tsx/js/java/python |
| Change-aware review (detect_changes, risk-scored diff) | ✅ | – | ✅ `ff cortex review` (hunk-level: narrows to symbols whose bodies the diff touched) |
| Test-coverage mapping (tests_for) | ✅ | – | ❌ |
| Execution flows / affected-flows | ✅ | – | ❌ |
| Provenance + confidence tiers (EXTRACTED/INFERRED) | – | ✅ | ❌ (all edges equal) |
| Visualization (HTML graph) | – | ✅ | ❌ |
| Budgeted GraphRAG answers (query --budget) | partial | ✅ | partial (semantic search, no answer synthesis) |
| Cross-repo merge | ✅ | ✅ | partial (multi-corpus, no merged view) |
| Wiki generation | ✅ | ✅ | ❌ |
| Incremental (changed-files-only) reindex | ✅ | ✅ --update | ✅ `ff cortex index --incremental` (V123 ledger) |

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

1. ✅ **Incremental reindex** (PR #182, V123 ledger) — changed files only; the
   post-commit hook + watcher now use `ff cortex index --incremental`.
2. ✅ **MCP query surface** (PR #181) — cortex_corpora/callers/callees/impact as
   fleet MCP tools (the CodeGraph pattern; replaces agents' dependence on CRG).
3. ✅ **detect_changes vs git diff** (PR #183) — `ff cortex review [--base <ref>]
   [--format json]`: risk-scored (fan-in / external fan-in / transitive blast)
   review map, ranked most-actionable-first. Follow-ups: (i) ✅ **hunk-level**
   (PR #184, V124) — persists 1-based symbol line spans on `code:*` nodes; review
   narrows to only the symbols whose bodies overlap the git-diff line ranges
   (fail-open on NULL spans / files absent from the diff map). (ii) ✅
   **`cortex_review` MCP tool** (PR #186) — takes `corpus` + `repo_dir` (the
   indexed checkout) + optional `base`/`depth`; the daemon shells `git` in
   `repo_dir` to derive changed files + hunk line ranges, then scores them
   against the graph. The pure diff parsing (`ext_lang`,
   `parse_diff_line_ranges`, `parse_hunk_new_span`) moved from the terminal layer
   into `ff_brain::cortex` so the CLI and MCP frontends share it.
4. **Community summaries via fleet LLMs** — GraphRAG-style per-community
   summaries, re-summarize only changed communities (the idle fleet is the lever;
   pairs with the incremental ledger).
   - ✅ **Generation** (`ff cortex summarize`, `community_summary.rs`) — DB-routed
     fleet LLM per community, `--all`/`--max`/`--min-members`, stable member-hash
     so unchanged communities keep their summary across re-detection.
   - ✅ **Consumer surface** (PR #295) — `ff cortex explain <symbol>` + the
     `cortex_explain` MCP tool: resolve a symbol → its community → that
     community's summary + top non-extern members. The summaries were write-only
     until this; now an agent gets "what is this subsystem responsible for?" in
     one call.
   - ✅ **Meaningful clusters — BLOCKER RESOLVED** (PR #297, `3c9ee3379`, V127).
     Was: `detect_communities` is union-find **connected components over ALL
     `brain_vault_edges`**; the `contains` tree (corpus→file→symbol) + `imports`
     bridged the whole code graph into **one mega-component** (largest community =
     **44,993** nodes; god node a non-code software-version vault node, summary
     garbage). Excluding externs barely helped (→32,897) — the dominant bridge is
     the containment tree. **Fix shipped:** a cortex-specific clustering —
     **single-level Louvain (modularity local-moving)** over the **`calls`
     subgraph among non-extern `code:*` nodes** (`cluster_calls_graph` in
     `communities.rs`, pure + unit-tested — splits two cliques joined by one
     bridge, which connected-components merges; deterministic across visit orders
     so `member_hash` is stable). Output lands in a **parallel** `code_community_id`
     column + `brain_code_communities` registry, leaving the brain KG's
     `community_id`/`brain_communities` untouched for `ff brain communities/stats`.
     `ff cortex explain`/`summarize` + the summary-refresh tick repoint to the code
     variant; `ff cortex embed` runs both clusterers. **Measured live after deploy:**
     largest code community **44,993 → 249 symbols**, 36,418 communities (4,054
     multi-member persisted); `ff cortex explain summarize_communities` → a coherent
     4-member cluster; summaries are now specific ("execution layer for a suite of
     specialized agent tools…", "agency portal API endpoints…") instead of
     "V8 JavaScript engine file system". **Possible follow-ups:** multi-level
     Louvain (hierarchical communities) if 249-symbol clusters prove too coarse;
     weight `calls` edges by call multiplicity in the modularity gain (currently
     adjacency multiplicity already approximates this).
5. **Provenance/confidence on edges** — EXTRACTED vs INFERRED tiers (graphify's
   one structural advantage), so downstream consumers can filter heuristic call
   edges (e.g. the dotty-resolver's kept-as-written externs).

## Recall (internal call-resolution) — gaps surfaced by `ff cortex doctor`

`ff cortex doctor` (read-only resolution/health probe) reports the internal-
resolution rate plus *internally-rooted suspicious externs* (a `code:extern`
whose head is one of the corpus's own module roots AND whose leaf collides with a
real internal symbol — the genuine mis-resolution signal, stdlib noise filtered).
Live forge-fleet baseline: 30.6% of `calls` edges resolve internally
(5127/16730). The internally-rooted suspicious list named three concrete gaps:

1. **Std-prelude types written bare** (`Vec::new`, `Arc::new`) — was fabricating
   `<caller_module>::Vec::new`. ✅ FIXED (`is_std_prelude_type` guard) — kept as a
   shared std extern. Lowest-risk, removes the bulk of internally-rooted noise.
2. **Inherent-impl methods** (top hit: `AgentToolResult::ok`/`err`) — Rust
   `impl Foo { fn bar() }` methods are registered as `module::bar`, NOT
   `module::Foo::bar` (a deliberate tradeoff at the `impl_item` arm so bare calls
   *inside* a method body resolve to sibling free fns). So `Foo::bar()` call sites
   became externs. ✅ FIXED (PR #278, `resolve_impl_method_call`) — rather than
   re-index methods (which would break in-body bare-call resolution), a
   **redirect-only** pass collapses an extern `<P>::<Type>::<leaf>` onto the real
   `<P>::<leaf>` method whenever `<P>::<Type>` is a known internal type AND
   `<P>::<leaf>` a known internal fn (gated by a new `internal_types` set). Never
   fabricates. Verified live: internal call recall **30.7% → 36.2%**, the
   `AgentToolResult::ok`/`err` phantoms (fan-in 58 + 53) absorbed into the real
   methods.
2b. **Glob-imported type segment in test modules** ✅ FIXED (PR #280,
   `resolve_glob_impl_method_call`) — the residual `X::new` misses were all in
   `mod tests { use super::*; }` blocks: the type segment got the test module glued
   on (`…::tests::NodeRegistry::new`), so #278's direct redirect couldn't fire.
   Re-anchor the type segment through the file's glob imports (same mechanism as
   `resolve_glob_call`) before the method redirect, gated to the
   `<caller_module>::Type::leaf` shape the resolver itself fabricated. Verified live
   (full reindex): internal call recall **36.2% → 40.6%**.
2c. **Std-prelude TRAIT heads** ✅ FIXED (PR #281, `is_std_prelude_trait`) — sibling
   to lever #1 but for traits used in associated-call position
   (`Default::default`, `From::from`, `FromStr::from_str`). The uppercase trait head
   was fabricating `<caller_module>::Default::default`, whose leaf collided with
   every internal `default`/`from`. Denylist them at the same fallback site as the
   std types. Verified live: `…::tests::Default::default` phantoms gone, extern
   placeholders **2798 → 2718**.
3. **Re-export / facade paths** (`ff_db::run_migrations` →
   `ff_db::migrations::run_migrations`). `pub use` re-exports aren't modeled, so
   calls via the facade path become externs. Hardest (cross-crate pub-use
   resolution); medium payoff.
4. **3rd-party crate heads written bare** (`toml::from_str` →
   `<caller_module>::toml::from_str`, fan-in 13+9). Same caller-module fabrication
   as levers #1/#2c, but the head is an arbitrary lowercase 3rd-party crate (`toml`,
   `serde_yaml`, …), not enumerable like std types/traits. `looks_external` only
   lists a fixed set. Better fix than growing that list by hand: treat a bare head
   that is NEITHER a known internal module root NOR `crate`/`self`/`super` as
   already-qualified external (the inverse of the current "assume same-module"
   default) — needs the corpus's own module-root set threaded into `resolve_call`.
   Low payoff (pure noise, these are genuinely external), but removes the last big
   `tests::` cluster. Surfaced by the iter-70 doctor run.
5. **Bare `new`/method in test-local structs** (`ff_pipeline::executor::tests::new`,
   fan-in 6). A `Foo::new()` where `Foo` is defined *inside* the test module itself
   (not imported via `use super::*`) — the type segment is dropped/mis-attributed.
   Lowest priority (small, test-only).

## Related state

- Hook-fires-on-commit: verified 2026-06-12 (scratch repo, post-commit reindex +
  callers query both worked).
- Corpora cleanup: `ff brain corpus delete <slug> --yes` added (`0cf184885`);
  9 test/stale corpora removed.
- Python support: ✅ SHIPPED (PR #190, `aab18fbfa`). `parse_python_file` (tree-
  sitter-python) ported onto the landed in-cortex.rs architecture — `code:class`/
  `code:function` symbols, package-path modules (`__init__.py` packages), import-
  alias call resolution, `self`→enclosing-class. Verified live: forge-fleet index
  picks up `.py` (5 files/21 symbols/19 calls resolved); `ff cortex callers` /
  `callees` query Python symbols correctly. The old parallel `cortex_lang.rs`
  design parked in `stash@{0}` is now fully obsolete and can be dropped.
