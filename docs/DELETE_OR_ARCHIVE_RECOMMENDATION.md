# DELETE OR ARCHIVE Recommendation (Legacy ForgeFleet + Mission Control)

Date: 2026-04-04  
Prepared for: `/Users/venkat/projects` cleanup and cutover

## Executive Summary

**Do not delete any legacy repo yet.**

Current repo/runtime reality is mixed:
- Python ForgeFleet (`forge-fleet`) is still operationally active (live Python MCP/lifecycle processes, legacy integration logic).
- Mission Control (`mission-control-legacy`) is live on `:60002` with active work-item data and broad API surface.
- Rust ForgeFleet (`forge-fleet-rs`) has substantial architecture, but parity with legacy Mission Control APIs/workflows is **partial**.

Recommended trajectory:
1. Keep legacy repos active short-term (stabilization + parity closure).
2. Freeze legacy repos (no new features, break/fix only) once hard preconditions pass.
3. Archive read-only after successful cutover soak.
4. Delete only after retention window + restore drill + explicit sign-off.

---

## 1) Decision Framework per Legacy Repo

Use the same lifecycle for each legacy repo:

## State Definitions

- **KEEP ACTIVE**
  - Repo can receive operational fixes and required updates.
  - Used in production/runtime paths.
- **FREEZE LEGACY**
  - No new features.
  - Break/fix and migration support only.
  - Rust is the primary implementation target.
- **ARCHIVE READ-ONLY**
  - Immutable snapshot (git tag + compressed archive + documented restore path).
  - No deployments from this repo.
- **DELETE**
  - Working copy removed after retention and recovery validation.

## Transition Gate Rules (apply to both legacy repos)

### KEEP ACTIVE -> FREEZE LEGACY (must all pass)
1. Rust path has documented owner and runbook for equivalent operation.
2. Parity checklist exists for GO-critical functions and is signed off.
3. No unresolved data-migration blocker.
4. Rollback path to legacy is rehearsed.

### FREEZE LEGACY -> ARCHIVE READ-ONLY (must all pass)
1. Rust path has run in production/canary without critical incidents for agreed soak window.
2. Legacy runtime is no longer in active process list/critical ports.
3. Final backup captured (repo + DB/data + config snapshots).
4. Archive location and restore instructions are tested once.

### ARCHIVE READ-ONLY -> DELETE (must all pass)
1. Retention period elapsed (recommended: 60–90 days minimum).
2. One successful restore drill from archive.
3. No remaining references in scripts/systemd/launchd/CI/docs.
4. Explicit human approval (Ops + Product owner).

---

## 2) Repo-by-Repo Current State + Preconditions

## A) `/Users/venkat/projects/forge-fleet` (Legacy Python ForgeFleet)

**Current recommended state: KEEP ACTIVE**

### Why (repo reality)
- Contains mature operational logic (autonomous worker, MCP server, fleet routing, mission-control client integration).
- Local runtime evidence shows active Python processes from this repo.
- Working tree is dirty/unfinalized (modified + untracked files), so it is not yet archive-grade.

### Preconditions to move to FREEZE LEGACY
1. Rust `forgefleetd` is primary runtime for fleet control + MCP tool serving in real use.
2. All GO-critical Python capabilities are either ported or intentionally dropped with written decision.
3. Legacy Python process count for this repo stays at 0 during normal operation for soak window.
4. Legacy repo is checkpointed (final tag/commit/snapshot) despite dirty state today.

### Preconditions to move to ARCHIVE READ-ONLY
1. Python repo not used for day-to-day operations.
2. Rust equivalent has passed operational soak.
3. Archive artifact exists (`tar.zst` or similar) + restore tested.

### Preconditions to DELETE
1. 60–90 day retention complete.
2. No hardcoded path references to `.../forge-fleet` in active scripts/services.
3. Restore drill from archive confirmed.

---

## B) `/Users/venkat/projects/mission-control` (Legacy Mission Control)

**Current recommended state: KEEP ACTIVE**

### Why (repo/runtime reality)
- Service is live on `http://127.0.0.1:60002` and returns active data (`/api/work-items/stats`, `/api/nodes/online`, etc.).
- Docker stack (`mc_backend`, `mc_frontend`, `mc_postgres`, `mc_redis`) is running.
- API breadth is large (legacy backend route surface is far wider than current Rust `ff-mc` endpoints).

### Preconditions to move to FREEZE LEGACY
1. Rust side has closed GO-critical MC parity (especially work-item lifecycle APIs used by agents).
2. Data migration path from MC Postgres to Rust storage is validated for required tables/entities.
3. Consumer cutover complete (agents/tools no longer depend on `/api/work-items` legacy semantics unless bridged).
4. Mission Control deployment no longer needed for normal operation.

### Preconditions to move to ARCHIVE READ-ONLY
1. MC containers can be stopped with no production impact.
2. DB backup (`pg_dump` + volume snapshot) captured and restore-tested.
3. Rust dashboard/control-plane covers required operator workflows.

### Preconditions to DELETE
1. Retention complete (60–90 days after archive).
2. No active endpoint/client references to legacy MC host/ports.
3. Signed decommission approval.

---

## 3) Proposed `~/projects` Folder Cleanup Sequence

Goal: eventually keep **one primary ForgeFleet folder**.

## Phase 0 — Safety Baseline (no renames/deletes yet)
1. Capture immutable checkpoints:
   - Current git SHA/tag for all three repos.
   - MC database backup + restore test notes.
   - `~/.forgefleet/fleet.toml` backup.
2. Generate a dependency map of scripts/services referencing current paths.

## Phase 1 — Freeze intent without breaking runtime
1. Mark `forge-fleet` and `mission-control` as legacy in docs/process (break/fix only).
2. Keep physical paths unchanged while cutover work continues.

## Phase 2 — Name normalization (after freeze preconditions pass)
Recommended rename plan:
- `forge-fleet` -> `forge-fleet-py-legacy` (**YES, but only after Rust cutover readiness**)  
- `mission-control` -> `mission-control-legacy` (**YES, after MC freeze gate passes**)  
- `forge-fleet-rs` -> `forge-fleet` (**YES, only when Rust is active primary**)  

Practical safety tip: use temporary compatibility symlinks for 2–4 weeks after rename to catch hidden path dependencies.

## Phase 3 — Archive
1. Create read-only archives of `forge-fleet-py-legacy` and `mission-control-legacy`.
2. Move archives to a dedicated archive path (example: `~/projects/_archive/`).

## Phase 4 — Delete (final)
1. After retention + restore validation, delete legacy working trees.
2. End-state in `~/projects` should keep:
   - `forge-fleet/` (Rust primary)
   - archived artifacts only (outside primary dev path)

---

## 4) If Missing Capabilities Are Found: Port / Place / Drop Rules

Use this triage model for any discovered gap.

## Triage Policy

- **GO-CRITICAL** (must port before freeze)
  - Needed for runtime safety, task execution, or operator control.
- **OPS-CRITICAL** (port or provide equivalent before archive)
  - Needed for observability, backup/restore, incident handling.
- **NICE-TO-HAVE** (can defer)
  - UX/reporting/quality-of-life functions.
- **INTENTIONAL DROP**
  - Legacy-only behavior with low value or high maintenance cost.

## Capability Mapping Recommendations

| Legacy capability found | Port? | Rust destination | Recommendation |
|---|---|---|---|
| Work-item lifecycle APIs used by agents (`claim`, `update`, `stats`, dependency/workflow paths) | **Yes (GO-critical)** | `ff-mc` + `ff-gateway` route mount + `ff-db` schema alignment | Port first; keep API compatibility shim if endpoint names differ. |
| MC event/pubsub behavior (Redis/SSE style flows) | **Yes/Equivalent (OPS-critical)** | `ff-observability`, `ff-gateway`, optionally adapter layer | Port equivalent behavior, not necessarily identical implementation. |
| Fleet config as single source of truth | **Yes (GO-critical)** | `ff-core::config` + `ff-mcp` handlers | Preserve semantics and hot-reload behavior. |
| Python operational scripts (`watchdog`, smoke tests, update scripts) | **Selective** | `ff-updater`, `ff-deploy`, `ff-cron` | Port only scripts still used operationally; drop duplicates. |
| Legacy MC frontend breadth (all pages/components) | **Partial** | `dashboard/` + `ff-gateway` APIs | Port only operator-critical screens first; defer cosmetic/low-use pages. |
| Python-specific packaging/venv/egg artifacts | **No** | N/A | Intentional drop. |

### Mandatory handling when a gap is discovered mid-cutover
1. Classify (GO-critical / OPS-critical / NICE / DROP).
2. Record explicit decision owner + due date.
3. If GO-critical: block transition to FREEZE until resolved.
4. If DROP: document rationale and replacement/none.

---

## 5) Explicit Path/Name Recommendations

These are the recommended names, with timing constraints:

1. `forge-fleet` -> `forge-fleet-py-legacy`  
   - **Recommended: YES**, but **not immediately**. Perform after Rust cutover readiness and path audit.

2. `forge-fleet-rs` -> `forge-fleet`  
   - **Recommended: YES** as the final canonical primary repo name.

3. `mission-control` -> `mission-control-legacy`  
   - **Recommended: YES** once Rust has replaced required MC operational surface.

4. Archive location recommendation:
   - `~/projects/_archive/forge-fleet-py-legacy-YYYYMMDD.tar.zst`
   - `~/projects/_archive/mission-control-legacy-YYYYMMDD.tar.zst`

---

## 6) Operational Risks of Deleting Too Early

1. **Live service interruption**: Mission Control API currently serves active operational data.
2. **Data loss risk**: MC Postgres/Redis state may be needed for audits/history/migration.
3. **Hidden dependency breakage**: hardcoded repo paths in scripts/services can fail silently.
4. **Control-plane regression**: Rust MC/API parity is not complete for legacy route surface.
5. **Rollback impossible**: deleting before archive+restore drill removes safety net.
6. **Uncommitted legacy state loss**: Python repo currently has modified/untracked files.
7. **Agent workflow breakage**: legacy claim/update flows may still be consumed by automation.

---

## 7) Final Recommendation: What Now vs Wait vs Port First

## What can be dismantled now (safe now)
1. **Dismantle feature expansion in legacy repos**: move both legacy repos to policy-level freeze intent (break/fix only).
2. **Do not delete repos yet.**
3. Optional (if disk pressure): remove/rebuild transient artifacts only after backup verification (non-source outputs), but treat this as operational cleanup, not decommission.

## What must wait
1. Physical delete of `forge-fleet` and `mission-control`.
2. Final rename to canonical Rust `forge-fleet` until parity/cutover gates pass.
3. MC archive/delete until data migration and endpoint consumer cutover are validated.

## What should be ported first (priority order)
1. **GO-critical Mission Control API compatibility layer** for agent workflows.
2. **Data migration + validation** from legacy MC/Postgres and legacy ForgeFleet stores into Rust-side persistence where needed.
3. **Operational equivalence**: health/status, watchdog/recovery flows, and update/deploy tooling.
4. **Operator dashboard parity** for must-have workflows only.

---

## Bottom-Line Call

- **Today:** KEEP ACTIVE for both legacy repos.
- **Next:** Move to FREEZE LEGACY only after GO-critical parity + migration + rollback gates pass.
- **Later:** ARCHIVE READ-ONLY after soak period.
- **Finally:** DELETE only after retention + restore proof + explicit sign-off.

This is the conservative path that minimizes outage/data-loss risk while still enabling eventual cleanup to a single primary ForgeFleet repo.
