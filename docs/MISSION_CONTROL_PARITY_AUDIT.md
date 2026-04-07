# Mission Control → ForgeFleet Rust Parity Audit

Date: 2026-04-04  
Author: OpenClaw subagent parity audit  
Repos audited:
- Mission Control: `/Users/venkat/taylorProjects/mission-control-legacy`
- ForgeFleet Rust: `/Users/venkat/taylorProjects/forge-fleet`

---

## Audit method (what was checked)

I reviewed both codebases across:
- Backend/API route surface
- Data models + DB schema/migrations
- Frontend/admin UI pages and endpoint usage
- Worker/ops scripts and deployment flows
- LLM routing + MCP integration

Key quantified surface checks:
- Mission Control backend routes (from `backend/src/main.rs`): **96** route paths
- ForgeFleet runtime routes (from `crates/ff-gateway/src/server.rs` + `crates/ff-mc/src/api.rs`): **29** route paths
- Mission Control frontend API references (`frontend/src/**/*.ts*`): **~60** endpoint references
- ForgeFleet dashboard expected API refs (`dashboard/src/**/*.ts*`): **19** endpoint refs; **13 are currently not implemented** by gateway/mc routes

Status legend used below:
- **COMPLETE** = feature-level parity is effectively present
- **PARTIAL** = some capability exists but behavior/data/workflow is materially reduced
- **MISSING** = no meaningful equivalent found
- **INTENTIONALLY_REPLACED** = not 1:1 ported; replaced by different architecture or product direction

---

## 1) Product / Backend capability ledger

| Capability | Mission Control file(s) | ForgeFleet equivalent(s) | Status | Notes / rationale |
|---|---|---|---|---|
| Work item CRUD (basic) | `backend/src/routes/work_items.rs`, `backend/src/models.rs` | `crates/ff-mc/src/work_item.rs`, `crates/ff-mc/src/api.rs` (`/api/mc/work-items*`) | PARTIAL | CRUD exists, but FF schema is much thinner (no MC workflow/review/dependency metadata breadth). |
| Work item claim/complete/fail/escalate/counsel workflows | `backend/src/routes/work_items.rs` (`claim`, `complete`, `fail`, `escalate`, `counsel_response`) | `ff-mc` workflow routes (`/api/mc/work-items/{id}/{claim,complete,fail,escalate}`) + status updates | PARTIAL | Core claim/complete/fail/escalate flows now exist; `counsel_response` and advanced MC orchestration semantics are still not 1:1. |
| Review lifecycle state machine (submit/start/complete/meta review) | `backend/src/routes/workflow.rs`, `backend/src/routes/review_items.rs` | `ff-mc` review transition routes (`/api/mc/work-items/{id}/review/{submit,start,complete}`) | PARTIAL | Core review transitions now exist; meta-review/advanced policy behavior remains reduced vs MC. |
| Review checklist/review item CRUD reset/batch | `backend/src/routes/review_items.rs`, `work_items.rs` (`review-checklist` routes) | `ff-mc` `review_items` domain + routes (`GET/POST /work-items/{id}/review-items`, `PATCH/DELETE /review-items/{id}`, reset endpoint) | PARTIAL | Checklist CRUD/reset now exists; broader MC-specific batch/meta flows still reduced. |
| Dependency graph persistence and checks | `backend/src/routes/workflow.rs`, `task_groups.rs`, `work_items.rs`, table `work_item_dependencies` | `ff-mc` persisted `work_item_dependencies` + check endpoint + auto-link suggestions | PARTIAL | FF now persists work-item dependencies and exposes check APIs; epic/feature dependency parity remains incomplete. |
| Task groups / sequence ordering / PR linkage | `backend/src/routes/task_groups.rs`, `work_items` fields (`task_group`, `sequence_order`, `pr_*`) | `ff-mc` `task_groups` table + assignment/list routes + sequence ordering fields on work_items | PARTIAL | Task-group sequencing now exists; PR-linkage metadata/workflows are still not ported. |
| Epic CRUD + progress | `backend/src/routes/epics.rs`, migrations for epics/features | `crates/ff-mc/src/epic.rs`, `/api/mc/epics*` | PARTIAL | Epic CRUD/progress exists; MC additionally has feature-level dependencies and richer refresh/dependency ops. |
| Feature CRUD + feature dependencies | `backend/src/routes/features.rs`, migration `20240302-epics-features-deps.sql` | None | MISSING | FF MC has no `features` table/domain. |
| Sprint stats/burndown | (MC had partial model support, limited route exposure) | `crates/ff-mc/src/sprint.rs`, `/api/mc/sprints*` | INTENTIONALLY_REPLACED | FF adds explicit sprint endpoints; this is more a redesign than direct route parity. |
| Project CRUD + GitHub metadata | `backend/src/routes/projects.rs` | None in `ff-mc` | MISSING | MC project object (`github_url`, owner/private/options) is not represented in FF MC. |
| Tasks CRUD (`/api/tasks`) | `backend/src/routes/tasks.rs` | `ff-db` tasks (`crates/ff-db/src/schema.rs`, `queries.rs`) | INTENTIONALLY_REPLACED | FF tasks are infrastructure/agent tasks, not MC’s product/task CRUD model. |
| Nodes CRUD + heartbeat | `backend/src/routes/nodes.rs` | Discovery status in `ff-gateway` (`/api/fleet/status`), agent crate (`ff-agent`) | PARTIAL | FF has discovery/health, but no MC-style nodes CRUD endpoint set. |
| Fleet pause/resume + fleet metrics | `backend/src/routes/system.rs` (`fleet_pause`, `fleet_resume`, `fleet_metrics`) | `ff-gateway` has `/api/fleet/status`; no pause/resume parity | PARTIAL | Monitoring exists; control operations do not match MC API behavior. |
| Docker container lifecycle APIs | `backend/src/routes/system.rs` (`/api/system/containers*`, `/api/fleet/containers`) | None | MISSING | No equivalent container control endpoints in FF gateway/API. |
| GitHub scan/collaborator/visibility APIs | `backend/src/routes/github.rs`, `system.rs` | None (except updater checking GitHub commits) | MISSING | Updater GitHub usage (`ff-updater`) is not equivalent to MC repo management endpoints. |
| MCP endpoint for internal MC tools | `backend/src/routes/mcp.rs` (`mc_tickets`, `mc_claim`, etc.) | `crates/ff-mcp/src/*` mounted at `/mcp` in gateway | INTENTIONALLY_REPLACED | FF has broader MCP tool registry (fleet tools), but not the same MC ticket toolset surface. |
| LLM routing/fleet route decision | `backend/src/model_router.rs`, `system.rs` (`fleet_route`) | `crates/ff-api/src/router.rs`, `registry.rs`, gateway `/v1/chat/completions` | PARTIAL | FF routing is more robust (tier/backoff), but MC role-model matrix style behavior is not 1:1 exposed. |
| AI hierarchy generation (prompt → epics/features/tasks/dependencies) | `backend/src/routes/ai.rs`, `backend/src/llm.rs`, `routes/generate.rs` | No direct endpoint; related concepts in `ff-orchestrator/src/task_decomposer.rs` | PARTIAL | FF has decomposition primitives, but not wired as MC-compatible hierarchy generation API/DB write flow. |
| Work-item event log history | `backend/src/routes/work_item_events.rs`, migration `20240301-create_work_item_events.sql` | FF audit/traces (`ff-db` audit_log + `/api/traces/recent`) | PARTIAL | FF has generic audit/tracing but no work-item event API parity. |
| Node message queue/event publish-subscribe | `backend/src/routes/events.rs`, `node_messages.rs` | `ff-gateway` websocket + `/api/messages`, `/api/messages/raw`, `/api/send` | INTENTIONALLY_REPLACED | Messaging exists but protocol/domain differs from MC node-queue APIs. |
| Chat/ideas/activity/settings/skills CRUD domains | `backend/src/routes/{chat,chat_sessions,ideas,activity,settings,skills}.rs` | Sessions/memory/config infra in FF (`ff-db`, `ff-sessions`, `ff-memory`, `/api/config`) | INTENTIONALLY_REPLACED | FF is platform-centric; MC’s product CRUD modules are mostly not ported as-is. |

---

## 2) Operational scripts & tooling parity ledger

| Capability | Mission Control file(s) | ForgeFleet equivalent(s) | Status | Notes / rationale |
|---|---|---|---|---|
| Build + deploy orchestration (docker build/push/service update + smoke) | `scripts/build-and-deploy.sh` | `deploy/install.sh`, `deploy/linux/forgefleet.service`, `deploy/macos/com.forgefleet.daemon.plist` | INTENTIONALLY_REPLACED | FF moved to single daemon install/service supervision, not MC docker image pipeline. |
| Smoke API regression checks | `scripts/smoke-test.sh` | None equivalent script | MISSING | FF CI checks Rust quality/tests, but no MC-style endpoint smoke script included. |
| Integration test script for live endpoints | `scripts/integration-test.sh` | None equivalent script | MISSING | No parity for end-to-end API integration script. |
| Merge gate enforcement (author != reviewer) | `scripts/merge-gate.sh` | No repository script equivalent | MISSING | Could be replaced by branch protection policy, but no code-level replacement in repo. |
| PR auto-routing reviewer bot logic | `scripts/pr-review.py` | No equivalent script/service wiring | MISSING | FF has orchestration crates, but not this operational PR triage workflow. |
| Stale task watchdog reset | `scripts/watchdog.sh`, `worker/watchdog.py` | Some stale-node logic in `src/main.rs` leader loop; no stale-work-item reset parity | PARTIAL | Node staleness exists; task reset behavior does not. |
| Feature scaffolding generator | `scripts/feature-scaffold.py` | None | MISSING | No equivalent full-stack scaffolder. |
| Type generation Rust→TS API/types sync | `scripts/generate-types.py` | None equivalent found | MISSING | FF dashboard types are hand-maintained currently. |
| Self-update orchestration | `scripts/openclaw-update.sh`, `scripts/self-update.sh` | `crates/ff-updater/src/*` | PARTIAL | Update crate exists with rich design, but no fully surfaced gateway/dashboard/CLI flow parity. |
| Worker execution runtime (legacy Python agent) | `worker/agent.py` | `crates/ff-agent/src/main.rs` + `src/main.rs` using `ff_agent::run` | PARTIAL | Critical wiring gap: root daemon uses lightweight heartbeat loop (`ff-agent/src/lib.rs`), not full task poller/executor binary path. |
| CI quality gates | (MC largely script-driven local ops) | `.github/workflows/rust-quality-gates.yml` | INTENTIONALLY_REPLACED | FF adds formal Rust CI gates (fmt/clippy/check/tests). |

---

## 3) UI / admin surface parity ledger

| Capability / page | Mission Control file(s) | ForgeFleet equivalent(s) | Status | Notes / rationale |
|---|---|---|---|---|
| Fleet status/health overview | `frontend/src/components/Agents.tsx`, `Dashboard.tsx` | `dashboard/src/pages/FleetOverview.tsx`, `Metrics.tsx` | PARTIAL | FF has strong fleet-centric pages; MC had deeper operational actions. |
| Kanban orchestration + drag/drop + timers + workflow actions | `frontend/src/components/Orchestration.tsx` | `dashboard/src/pages/MissionControl.tsx` | PARTIAL | FF MissionControl is read-oriented board refresh; MC orchestration interactions are much richer. |
| Mission board data contract alignment | MC board/work-item contract in `Orchestration.tsx` + backend | FF `MissionControl.tsx` + `lib/normalizers.ts` + `ff-mc/src/board.rs` | COMPLETE | Dashboard normalizer now maps FF board (`columns[].items/status/label`) into UI card contract. |
| Project management + GitHub collaborator/visibility controls | `frontend/src/components/Projects.tsx` | `dashboard/src/pages/Projects.tsx` | PARTIAL | FF now has a Projects portfolio screen (companies/projects/repos/environments + status updates), but MC-specific GitHub collaborator/visibility controls are still not 1:1. |
| Settings/role model routing editor | `frontend/src/components/Settings.tsx` | `dashboard/src/pages/ConfigEditor.tsx` | COMPLETE | Gateway now returns `/api/config` with `content`, accepts JSON `{content}` TOML writes, and exposes `/api/config/reload-status`. |
| Node detail view | MC agents + node endpoints | `dashboard/src/pages/NodeDetail.tsx` | COMPLETE | `GET /api/fleet/nodes/{id}` implemented in gateway for direct node detail contract. |
| LLM proxy analytics page | MC had model performance + fleet metrics pages/components | `dashboard/src/pages/LLMProxy.tsx` | PARTIAL | Gateway now implements `/api/proxy/*` and `/v1/proxy/*` compatibility endpoints backed by tier-router metrics snapshots. |
| Audit log page | MC had event/history/review logs (`events`, `work_item_events`) | `dashboard/src/pages/AuditLog.tsx` | PARTIAL | Gateway now implements `/api/audit/recent` and `/api/audit/events` using embedded audit log; work-item-specific event parity remains broader in MC. |
| Updates page | MC had script-based update flows | `dashboard/src/pages/Updates.tsx` | PARTIAL | Gateway now provides `/api/update/*` contract-compatible endpoints with lightweight rollout state; full updater orchestration parity is still pending. |
| Chat / ideas / activity / skills pages | `frontend/src/components/{Chat,Ideas,Activity,Skills}.tsx` | No direct dashboard equivalents | INTENTIONALLY_REPLACED | FF shifted to platform/session/memory model rather than MC product CRUD pages. |
| My Tasks page | `frontend/src/components/MyTasks.tsx` | `dashboard/src/pages/MyTasks.tsx` | PARTIAL | FF now has a dedicated assignment/work queue UI with workflow action buttons and create/filter controls; legacy timer-focused behavior is still reduced. |

### Dashboard/API integrity check (important)

Phase 23 closure materially reduced this gap:
- Dashboard contract endpoints now implemented in gateway:
  - `/api/audit/recent`, `/api/audit/events`
  - `/api/update/*`
  - `/api/proxy/*` and `/v1/proxy/*`
  - `/api/fleet/nodes/{id}`
  - `/api/status`
  - `/api/config/reload-status`
- Remaining parity concern is depth/behavior fidelity (especially updater orchestration and richer analytics), not missing route surface.

---

## 4) DB / migration surface parity ledger

| Capability | Mission Control file(s) | ForgeFleet equivalent(s) | Status | Notes / rationale |
|---|---|---|---|---|
| Primary datastore architecture | `docker-compose.yml` (Postgres + Redis), `backend/src/db.rs`, migrations | Embedded SQLite (`crates/ff-db`), optional MC SQLite (`crates/ff-mc`) | INTENTIONALLY_REPLACED | Architectural shift from PG/Redis app stack to embedded Rust-native data stores. |
| Rich work item schema (review/pr/dependency/docker/ownership fields) | `backend/src/db.rs` (32 `ALTER TABLE ... ADD COLUMN` on `work_items`), `models.rs`, `20260303-v2-workflow-engine.sql` | `ff-mc` `work_items` table has 11 columns (`crates/ff-mc/src/db.rs`) | PARTIAL | Core ticket fields exist; advanced execution/review/dependency metadata absent. |
| Review tables | `review_items` migration + route usage | `review_items` in `ff-mc` schema + API routes | PARTIAL | Core table/domain is now present in FF MC; MC-specific advanced review semantics remain richer. |
| Dependency tables | `work_item_dependencies`, `epic_dependencies`, `feature_dependencies` migrations | `work_item_dependencies` in `ff-mc` schema + dependency check API | PARTIAL | Work-item dependency persistence/checking is now present; epic/feature dependency tables remain unported. |
| Docker stack state persistence | `docker_stacks` migration + routes | None in FF MC DB | MISSING | No parity for MC docker stack lifecycle tracking. |
| Work item event timeline table | `work_item_events` migration + routes | Generic `audit_log` table in `ff-db` | PARTIAL | Generic logs exist but no direct work-item event model/endpoint parity. |
| Epic + feature + hierarchy storage | MC epics + features + dependencies migrations | FF epics + work-item linkage only | PARTIAL | Epics ported; features/dependency graph not ported. |
| Session/chat/settings/skills persistence | MC tables via route modules (`chat_sessions`, `chat_messages`, `settings`, `skills`) | FF `sessions`, `memories`, `config_kv` in `ff-db` | INTENTIONALLY_REPLACED | Data model changed from MC app-domain CRUD to platform primitives. |
| Migration system maturity | Multiple SQL migration files in MC (`backend/migrations/*.sql`) | `ff-db` has one schema migration version (`initial_schema`), `ff-mc` uses create-if-not-exists setup | PARTIAL | Functional but significantly less evolved migration history for MC-equivalent domains. |
| Data migration from legacy MC Postgres | MC schema + data live in PG | `tools/migrate_from_postgres.rs` (+ `--mc-out`) | PARTIAL | Tool now migrates MC core domains (epics/work_items/review_items/work_item_dependencies/task_groups), but not full MC app-domain surface. |

---

## Safely absorbed into ForgeFleet (today)

These are good candidates to treat as already absorbed/replaced:

1. **Fleet-level runtime platform** (discovery, gateway, OpenAI-compatible proxy, replication/tracing foundations)
   - `crates/ff-gateway`, `crates/ff-api`, `crates/ff-discovery`, `src/main.rs`
2. **Service supervision/deployment baseline** (install + launchd/systemd)
   - `deploy/install.sh`, `deploy/linux/forgefleet.service`, `deploy/macos/com.forgefleet.daemon.plist`
3. **MCP-first operational control model** (fleet tools)
   - `crates/ff-mcp/src/tools.rs`, `handlers.rs`
4. **Basic mission board CRUD domain (reduced)**
   - `crates/ff-mc/src/{work_item,epic,sprint,board,dashboard}.rs`

---

## Still exists only in Mission Control (high-confidence)

1. **Advanced work orchestration workflows**
   - claim/fail/escalate/counsel, review lifecycle, review checklist, dependency checks, task groups
2. **Project/GitHub operations domain**
   - project CRUD integrated with scan/collaborator/visibility flows
3. **Product-style CRUD modules**
   - ideas, activity, chat, skills, node_messages (as MC app entities)
4. **Scripted operator workflows**
   - merge gate, PR routing, feature scaffolding, smoke/integration scripts
5. **Rich Postgres workflow schema**
   - `review_items`, `work_item_dependencies`, `docker_stacks`, expanded `work_items` columns

---

## What should still be brought over (if parity is required)

1. **Workflow-critical backend parity in ff-mc**
   - review state machine + review_items + dependency tables + task group sequencing
2. **Dashboard/API contract closure**
   - either implement missing `/api/audit/*`, `/api/update/*`, `/api/proxy/*`, `/api/fleet/nodes/{id}`, `/api/status` or remove pages expecting them
3. **Agent control-plane wiring**
   - root daemon currently invokes lightweight `ff_agent::run` heartbeat; full agent task protocol path is not the default wiring
4. **MC data migration tooling**
   - add migration path for MC domains (projects/work_items/review/dependency/event tables)
5. **Project/GitHub domain decision**
   - explicitly port or explicitly retire this domain

---

## What should be retired instead of ported

1. **MC’s ad-hoc shell-heavy ops scripts** where FF already has service/CI architecture
   - replace with tested Rust control-plane commands + CI workflows
2. **Docker Swarm-specific deployment assumptions** in MC scripts
   - FF deployment model is host daemon + service supervision
3. **Legacy duplicate app CRUD surfaces** (ideas/activity/chat/skills as standalone MC tables)
   - if product direction is platform-first, keep sessions/memory/config model instead

---

## Prioritized recommendations (P0 / P1 / P2)

### P0 (blockers before archive/delete decisions)

1. **Close dashboard/backend contract breaks immediately**
   - Fix mismatches between `dashboard/src/pages/*` and actual gateway routes.
   - Especially: `AuditLog`, `Updates`, `LLMProxy`, `NodeDetail`, `ConfigEditor`, and `MissionControl` board shape mismatch.
2. **Wire real agent execution path in the unified daemon**
   - `src/main.rs` currently starts `ff_agent::run` (heartbeat loop in `crates/ff-agent/src/lib.rs`), not the fuller `crates/ff-agent/src/main.rs` task poller/executor flow.
3. **Implement minimal workflow parity for safe operations**
   - Port review/dependency critical path (`review_items`, dependency persistence/checks, review transition endpoints).
4. **Add MC-domain migration utility**
   - Existing `tools/migrate_from_postgres.rs` does not cover MC core entities.

### P1 (important next parity wave)

1. **Project/GitHub parity decision + implementation**
   - Port project CRUD and key GitHub operations, or explicitly retire with migration plan.
2. **Port work-item event/history surfaces**
   - Add work-item scoped event API and UI timeline parity.
3. **Replace script-only quality gates with first-class Rust commands/tests**
   - Bring smoke/integration checks into reproducible CI and/or control-plane commands.

### P2 (cleanup and strategic simplification)

1. **Retire or rebuild low-signal MC pages (ideas/activity/etc.) based on product scope**
2. **Consolidate stale docs**
   - Several phase docs in `forge-fleet-rs/docs` are stale relative to current code wiring.
3. **Unify operational control surfaces**
   - Expose `ff-control`/`ff-updater` capabilities through supported CLI/API pathways instead of fragmented scripts.

---

## Phase 23 closure update (2026-04-05)

This section records parity movements completed in Phase 23 implementation work.

### Status changes (material)

1. **Workflow parity (review/dependency/task-group)**
   - `Work item claim/complete/fail/escalate` moved **MISSING → PARTIAL**
     - Implemented workflow routes:
       - `POST /api/mc/work-items/{id}/claim`
       - `POST /api/mc/work-items/{id}/complete`
       - `POST /api/mc/work-items/{id}/fail`
       - `POST /api/mc/work-items/{id}/escalate`
   - `Review lifecycle state machine` moved **MISSING → PARTIAL**
     - Implemented:
       - `POST /api/mc/work-items/{id}/review/submit`
       - `POST /api/mc/work-items/{id}/review/start`
       - `POST /api/mc/work-items/{id}/review/complete`
   - `Review checklist/review item CRUD reset/batch` moved **MISSING → PARTIAL**
     - Implemented:
       - `GET/POST /api/mc/work-items/{id}/review-items`
       - `PATCH/DELETE /api/mc/review-items/{id}`
       - `POST /api/mc/work-items/{id}/review-items/reset`
   - `Dependency graph persistence and checks` remains **PARTIAL** (improved)
     - Added persisted dependency routes:
       - `GET/POST /api/mc/work-items/{id}/dependencies`
       - `DELETE /api/mc/work-items/{id}/dependencies/{depends_on_id}`
       - `GET /api/mc/work-items/{id}/dependencies/check`
   - `Task groups / sequence ordering` moved **MISSING → PARTIAL**
     - Added task-group domain and assignment routes:
       - `GET/POST /api/mc/task-groups`
       - `GET/PATCH/DELETE /api/mc/task-groups/{id}`
       - `GET /api/mc/task-groups/{id}/items`
       - `POST/DELETE /api/mc/task-groups/{id}/items/{work_item_id}`

2. **Dashboard/API contract closure**
   - Added/closed missing route surface:
     - `GET /api/status` (alias of fleet status)
     - `GET /api/fleet/nodes/{id}`
     - `GET /api/audit/recent`, `GET /api/audit/events`
     - `GET /api/proxy/stats`, `GET /api/proxy/requests`
     - `GET /v1/proxy/stats`, `GET /v1/proxy/requests`
     - `GET /api/update/status`, `GET /api/update/check`
     - `POST /api/update/pause`, `POST /api/update/resume`, `POST /api/update/abort`
     - `GET /api/config/reload-status`
   - `ConfigEditor` contract gap reduced by supporting JSON `{content}` TOML writes and returning `content` in `/api/config` response.
   - `MissionControl` board contract mismatch reduced by normalizing FF board (`columns[].items`) in `dashboard/src/lib/normalizers.ts`.

3. **Migration coverage for MC core domains**
   - `tools/migrate_from_postgres.rs` now supports optional MC-domain migration target via `--mc-out` and migrates:
     - `epics`
     - `work_items`
     - `review_items`
     - `work_item_dependencies`
     - `task_groups`
   - `Data migration from legacy MC Postgres` moved **MISSING (for MC domains) → PARTIAL**.

### Remaining major gaps (still not full parity)

- MC-specific `counsel_response` and richer orchestration semantics are still not 1:1.
- Epic/feature dependency parity is still incomplete (feature domain not ported).
- Update/proxy endpoints now satisfy dashboard contracts, but updater orchestration is still lightweight compatibility behavior rather than full MC rollout parity.

## Phase 37B update (2026-04-05)

This phase focused on Mission Control screen parity and route/nav wiring.

### Implemented in ForgeFleet dashboard

1. **My Tasks parity screen**
   - Added route: `GET /my-tasks` (frontend route)
   - Added page: `dashboard/src/pages/MyTasks.tsx`
   - Wired endpoints/actions:
     - `GET /api/mc/work-items` (with assignee filter support)
     - `POST /api/mc/work-items` (create task)
     - `POST /api/mc/work-items/{id}/claim`
     - `PATCH /api/mc/work-items/{id}` (start/in_progress transition)
     - `POST /api/mc/work-items/{id}/review/submit`
     - `POST /api/mc/work-items/{id}/complete`
     - `POST /api/mc/work-items/{id}/fail`
     - `POST /api/mc/work-items/{id}/escalate`
   - Includes summary cards, status columns, action buttons, and create/filter controls.

2. **Projects parity screen**
   - Added route: `GET /projects` (frontend route)
   - Added page: `dashboard/src/pages/Projects.tsx`
   - Wired portfolio/project endpoints:
     - `GET /api/mc/portfolio/summary`
     - `GET/POST /api/mc/companies`
     - `GET/POST /api/mc/projects`
     - `PATCH /api/mc/projects/{id}` (status updates)
     - `GET/POST /api/mc/projects/{id}/repos`
     - `GET/POST /api/mc/projects/{id}/environments`
   - Includes company/project creation forms, project inventory selector, repo/env lists, and add forms.

3. **Navigation / route parity wiring**
   - Added sidebar nav links for:
     - `My Tasks`
     - `Projects`
   - Ensured links map to real routes (no dead nav items).

4. **Shared API utility update**
   - Added `patchJson` helper in `dashboard/src/lib/api.ts` for PATCH-based Mission Control actions.

### Post-implementation status

- `My Tasks page`: **MISSING → PARTIAL**
- `Project management page`: **MISSING → PARTIAL**
- Build validation: `dashboard` production build passes (`npm run build`).

## Phase 37C update (2026-04-05)

This continuation wave closed the highest-value remaining Mission Control workflow/planning UI gaps using existing `ff-mc` APIs.

### Implemented in ForgeFleet dashboard

1. **Workflow Workbench screen** (`/workflow`)
   - Added page: `dashboard/src/pages/WorkflowWorkbench.tsx`
   - Added sidebar route/nav wiring for `Workflow Workbench`
   - Wired real workflow parity actions/endpoints:
     - `POST /api/mc/work-items/{id}/claim`
     - `PATCH /api/mc/work-items/{id}` (start/in-progress transitions)
     - `POST /api/mc/work-items/{id}/review/start`
     - `POST /api/mc/work-items/{id}/review/submit`
     - `POST /api/mc/work-items/{id}/review/complete`
     - `POST /api/mc/work-items/{id}/complete`
     - `POST /api/mc/work-items/{id}/fail`
     - `POST /api/mc/work-items/{id}/escalate`
     - `GET/POST /api/mc/work-items/{id}/dependencies`
     - `DELETE /api/mc/work-items/{id}/dependencies/{depends_on_id}`
     - `GET /api/mc/work-items/{id}/dependencies/check`
     - `GET/POST /api/mc/work-items/{id}/review-items`
     - `PATCH/DELETE /api/mc/review-items/{id}`
     - `POST /api/mc/work-items/{id}/review-items/reset`
     - `GET/POST /api/mc/task-groups`
     - `POST /api/mc/task-groups/{id}/items/{work_item_id}`
     - `DELETE /api/mc/task-groups/{id}/items/{work_item_id}`

2. **Planning Hub screen** (`/planning`)
   - Added page: `dashboard/src/pages/PlanningHub.tsx`
   - Added sidebar route/nav wiring for `Planning Hub`
   - Wired epics/sprints planning and assignment endpoints:
     - `GET/POST /api/mc/epics`
     - `PATCH /api/mc/epics/{id}`
     - `GET /api/mc/epics/{id}/progress`
     - `GET/POST /api/mc/sprints`
     - `GET /api/mc/sprints/{id}/stats`
     - `GET /api/mc/sprints/{id}/burndown`
     - `PATCH /api/mc/work-items/{id}` for epic/sprint assignment

3. **Shared API utility support**
   - Added `deleteJson` helper to `dashboard/src/lib/api.ts` to support REST delete flows in workflow parity UI.

### Post-implementation status delta

- `Workflow parity screen (review/dependency/task-group)`: **MISSING → PARTIAL**
- `Epics/sprints planning screen`: **MISSING → PARTIAL**
- Build validation re-confirmed: `dashboard` production build passes (`npm run build`).

## Bottom line for archive/delete safety

- **Not safe to claim full Mission Control parity today.**
- ForgeFleet has absorbed the fleet/platform foundation and a reduced mission-board core, but **major workflow/product parity remains incomplete**.
- If archive/delete decisions depend on preserving MC’s orchestration/review/dependency/project/GitHub workflows, those should be treated as **not yet migrated**.
- If the strategic decision is to narrow scope to fleet platform + simplified mission board, then many MC modules can be intentionally retired — but this should be explicit and documented as a product decision, not inferred as completed parity.
