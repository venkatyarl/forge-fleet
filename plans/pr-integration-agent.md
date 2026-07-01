# Fleet PR Integration Agent (ff council, 2026-07-01)

## Verdict: build the INTEGRATION AGENT (option b) first — highest leverage
The failure mode is overlapping CORRECT work that needs SYNTHESIS, not blind
rebasing. When one feature is decomposed into N leaf tasks built in isolated
worktrees, the N PRs overlap; merging needs conflict-resolution + dedup + a
holistic test pass.

## Ranked
1. **(b) Integration agent — M, highest leverage.** For a goal-scoped batch:
   fetch all `wi/<id>` branches (children of the goal via parent_id), merge/
   cherry-pick into `integrate/<goal-id>`, resolve conflicts, dedup, normalize
   APIs, run full tests, open ONE coherent PR with an integration summary
   mapping the included work_items. BUILD FIRST.
2. **(c) Smarter decomposition — L, high but not sufficient.** Add ownership
   hints to `ff pm decompose`: expected files/modules, dependency order, shared
   interfaces, do-not-touch boundaries. Reduces conflicts; not a full solution
   (features cross shared files: exports, routing, schemas, tests, config).
3. **(a) Serialize-and-rebase — S/M, fallback.** For INDEPENDENT PRs that
   lightly conflict; bad default for one-feature-split-into-overlapping.
4. **(d) Sequential build — S, lowest.** Only for hard-ordering/tiny features;
   don't make it the default (gives up the fleet's core advantage).

## Build first (minimal integration executor)
create `integrate/<goal-id>` → merge child wi/ branches in dependency order →
stop on conflict → assign integration agent to resolve → run tests → open one
PR. Then add decomposition metadata (option c).
