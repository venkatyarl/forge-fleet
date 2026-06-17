# Mission-Control → ForgeFleet Project-Management Parity

Source: codex deep-dive of `/Users/venkat/taylorProjects/mission-control-legacy` vs ff (2026-06-17). Full transcript: `/tmp/ff_missioncontrol_parity.md`.

**Headline:** ForgeFleet already has a substantial PM surface (`crates/ff-mc` API + dashboard pages + `ff pm`/`ff project` CLI + Postgres `projects`/`fleet_work_items`). The gaps are mostly **UI/UX depth + a few high-leverage features**, plus one real **architecture discrepancy**.

## 1. Mission-Control notable features
- Project surface: CRUD + status, GitHub URL/owner, commit count, setup-option checklist, per-project KPIs, Docker/container status per fleet node.
- Work hierarchy: **epics → features → tickets**, parent-child items, epic/feature/work-item dependencies, task groups, sequence ordering.
- **Planning automation**: `/api/work-items/generate` — one LLM prompt → epics/features/tickets.
- My Tasks workflow: deadlines, **manual timers** (start/pause/resume/stop/reset), urgency recalc from deadlines, "today's focus", next-task modes.
- Hierarchy UI: expandable epic→feature→ticket tree, progress bars, dependency chips, badges, search/sort/filter.
- Review workflow: checklists, review rounds, reviewer evidence, rejection history, bounce counts, approvals, PR-linked review state.
- Agent execution fields: required/used model, min RAM, assigned node, timing, tokens/cost, counsel mode/responses, confidence/dissent, escalation.
- PR/branch fields + Docker-stack-per-work-item tracking.
- Pages: Dashboard, My Tasks, Projects, Kanban, Agents, Chat, Ideas, Activity, Skills, Settings, role-model-matrix-editor.

## 2. Already in ForgeFleet
- `crates/ff-mc`: `/api/mc/work-items` CRUD + claim/complete/fail/escalate, review submit/start/complete, dependencies, task groups, companies/projects/repos/environments, portfolio summary, epics+progress, sprints+burndown, board/dashboard endpoints.
- Dashboard pages: MyTasks, Projects, PlanningHub (epics/sprints/burndown), MissionControl home, TaskKanban.
- CLI: `ff pm list/create/show/import-claude-tasks`, `ff project list/status/sync`.
- Postgres (`ff-db`): `projects`, `milestones`, `fleet_work_items`, `work_outputs`, `project_branches`, `project_environments`, `project_ci_runs`, `fleet_tasks`.

## 3. MISSING / worth porting (prioritized)
- **S** — deadline + manual-timer UX in MyTasks; **prompt-to-plan generation** (legacy `/api/work-items/generate` → epics/features/tickets); richer expandable hierarchy view; surface work-item **dependencies** in dashboard (already modeled, not exposed).
- **M** — first-class **feature layer** (epic→feature→ticket) in `ff-mc`; **⚠ unify PM storage** (`ff-mc` uses **SQLite** while `ff-db` is **Postgres** + `fleet_tasks` — ambiguous canonical surface, and SQLite violates the "Postgres-backed not flat files" rule); review checklist depth (evidence/rejection history/bounce/approvals); project operational cards; branch/PR/CI/output linkage on work-item detail; deadline-based priority recalc.
- **L** — Docker-stack-per-work-item view; selective agent-execution/counsel telemetry fields; consolidate Ideas/Activity/Skills/PR-tracker side surfaces.

## Key flags for the roadmap
1. **`ff-mc` SQLite vs `ff-db` Postgres split** — a real duplicate-storage discrepancy to resolve (canonical = Postgres).
2. **Prompt-to-plan generation** is the highest-leverage single feature to port (one prompt → structured PM tree) — and composes with the auto skill-gen + working-memory work.
