# Multi-project git policy — design

ForgeFleet drives builds for many repos with different git conventions (HireFlow
base branch = `dev` with feature-branch→PR; some repos commit straight to `main`;
others feature-branch→`main`). Across multiple computers and multiple agents
building different projects concurrently, the dispatch→build→commit-back flow must
resolve and honor each project's git policy instead of hardcoding `origin/main`.

## What already exists (build on it, don't reinvent)
- `projects` table (ff-db `schema.rs`): `repo_url`, `default_branch`,
  `target_computers`, `metadata`.
- `work_items`: `project_id`, `base_branch`, `integration_branch`, `base_sha`
  (per-task overrides — already present, just not populated).
- The scheduler flow already has per-run isolation: `work_item_worktrees`
  (`repo_path`/`worktree_path`/`base_branch`/`task_branch`).

## The gap
The ad-hoc dispatch path (`ff agent fanout` / `dispatch-each` / `commit-back`)
hardcodes `origin/main` (the `clean_sync_prefix` D2 reset) and `gh pr create
--base main`. It ignores `projects`. Even `hireflow360.default_branch` is wrong
(`main`, should be `dev`).

## Design — per-project policy resolved once, honored everywhere

### 1. Schema (this PR — cheap, non-breaking)
Add to `projects`:
- `integration_strategy TEXT NOT NULL DEFAULT 'feature_pr'` — one of
  `direct` (commit straight to base_branch), `feature_pr` (branch → PR → merge
  into base_branch), `feature_push` (branch → push, no PR / manual merge).
- `branch_prefix TEXT NOT NULL DEFAULT 'feat'` — feature-branch name prefix.
- `git_remote TEXT NOT NULL DEFAULT 'origin'`.
Fix data: `hireflow360.default_branch = 'dev'`. (`default_branch` is the
integration target; per-task `work_items.base_branch` overrides it.)

### 2. Resolver (later PR)
Every dispatched task carries `project_id` → resolve `GitTarget { repo_url,
base_branch (= work_item.base_branch ?? project.default_branch), strategy,
remote, branch_prefix }`. Three consumers:
- **clean-sync / worktree base**: `git worktree add --detach <runs>/<proj>/<task>
  <remote>/<base_branch>` — per-run isolation + correct base in one (this is the
  GAP-D-iso fix from hybrid-build-orchestration; reuses the scheduler's
  `work_item_worktrees` pattern).
- **commit-back honors strategy**: `direct` → commit+push to base_branch (gated);
  `feature_pr` → branch `<prefix>/…` → push → `gh pr create --base <base_branch>`;
  `feature_push` → branch → push, no PR.

### 3. The matrix falls out
| Project | base_branch | strategy | flow |
|---|---|---|---|
| HireFlow360 | `dev` | `feature_pr` | worktree off `origin/dev` → `feat/…` → PR `--base dev` |
| direct repo | `main` | `direct` | worktree off `origin/main` → commit+push main (CI-gated) |
| feature→main | `main` | `feature_pr` | worktree off `origin/main` → `feat/…` → PR `--base main` |

### 4. Multi-computer / multi-agent / multi-project concurrency
- one canonical clone per project per computer (`~/.forgefleet/repos/<project>/`,
  clone-on-first-use from `repo_url`); per-run worktrees off it
  (`~/.forgefleet/runs/<project>/<task-id>/`) → N agents × M projects on one
  computer never collide, each on the right base branch. `projects.target_computers`
  gates which computers build which project.

### 5. Safety (for `direct`)
- new projects default to `feature_pr` (safe); `direct` is opt-in.
- `direct` must be gated: CI green + trusted project; never force-push a raw LLM
  diff to a protected branch. If base_branch is protected → fall back to `feature_pr`.

## Sequencing
1. **(this PR)** schema + data: add `integration_strategy`/`branch_prefix`/
   `git_remote` columns; set `hireflow360 → dev`. No behavior change yet.
2. resolver + per-run worktree flow (the GAP-D-iso build), dogfood-validated on
   forge-fleet (`main`/`feature_pr`) then HireFlow (`dev`/`feature_pr`).
3. commit-back strategy honoring + retire the hardcoded `--base main` + the
   `clean_sync_prefix` shim.
