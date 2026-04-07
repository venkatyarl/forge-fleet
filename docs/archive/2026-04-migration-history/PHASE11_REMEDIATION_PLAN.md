# Phase 11 Remediation Plan (Fastest Path to GO)

Date: 2026-04-04  
Input audit: `docs/PHASE11_FINAL_AUDIT.md`  
Target: flip recommendation from **NO-GO** to **GO** for `v0.1.0-internal`

---

## GO Criteria (derived from final audit)

A GO decision is allowed when all of the following are true:
1. Release scope is clean (no critical untracked drift for in-scope v0.1 content).
2. CI gating exists and enforces `cargo check --workspace` + `cargo test --workspace --lib`.
3. Top integration placeholders are closed or explicitly deferred with owner/date (`ff-pipeline`, root bootstrap path).
4. Smoke evidence is fresh and attached after remediation.

---

## 1) Blocker: Release-content drift (HIGH)

### Concrete tasks

| Task ID | Concrete task | Owner suggestion (crate/module) | Success criteria | Effort |
|---|---|---|---|---|
| P11-RD-01 | Create a v0.1 scope matrix for all workspace members (`include` vs `defer`). | Workspace root: `Cargo.toml` + `docs/` release governance docs | Every workspace crate has an explicit scope decision and rationale; no ambiguous entries. | S |
| P11-RD-02 | Resolve untracked drift by either (a) committing in-scope crates/docs or (b) removing deferred crates from workspace members for this tag. | Workspace root + crate roots (`crates/*`) | `git status --porcelain` contains no `??` items for v0.1 in-scope artifacts; workspace members match scope matrix. | M |
| P11-RD-03 | Add release provenance doc (commit SHA, crate list, deferred list). | `docs/` (release notes/checklist module) | Single doc records exactly what `v0.1.0-internal` contains and excludes. | S |

---

## 2) Blocker: CI gating gap (MEDIUM)

### Concrete tasks

| Task ID | Concrete task | Owner suggestion (crate/module) | Success criteria | Effort |
|---|---|---|---|---|
| P11-CI-01 | Add GitHub Actions workflow for required smoke gates. | `.github/workflows/ci-rust.yml` | Workflow triggers on `push` + `pull_request` and runs: `cargo check --workspace`, `cargo test --workspace --lib`. | S |
| P11-CI-02 | Add CI badge/status + “required checks” release rule in docs. | `README.md` + `docs/` release checklist | Docs name required checks and block tag cut when checks are red/missing. | S |
| P11-CI-03 | Add deterministic local parity script used by humans and CI (same commands, same order). | `scripts/ci_smoke.sh` (or equivalent) | Local script reproduces CI smoke path exactly; referenced by release checklist. | S |

---

## 3) Blocker: Integration maturity gap (MEDIUM)

### Concrete tasks

| Task ID | Concrete task | Owner suggestion (crate/module) | Success criteria | Effort |
|---|---|---|---|---|
| P11-IM-01 | Remove root executable placeholder path (`Hello, world!`) by wiring to real CLI/control bootstrap or deleting unused stub path. | Root `src/main.rs` + `crates/ff-cli` (+ `ff-control` wiring if needed) | No placeholder executable path remains; `cargo run -p ff-cli -- --help` stays green. | S |
| P11-IM-02 | Replace `ff-pipeline` placeholder with MVP pipeline primitives (request/response types, stage trait(s), typed errors, events). | `crates/ff-pipeline/src/*` | `ff-pipeline` is no longer placeholder-only; has compile-tested API surface and unit/integration tests. | L |
| P11-IM-03 | Document explicit deferrals for any still-scaffolded integration areas with owner + target phase/date. | `docs/PHASE11_REMEDIATION_PLAN.md` follow-up section or dedicated defer doc | Each remaining scaffold item has defer rationale, owner crate, and revisit date; no silent placeholders. | S |

---

## 4) Blocker: Phase-10 execution backlog not fully burned down (MEDIUM)

### Concrete tasks

| Task ID | Concrete task | Owner suggestion (crate/module) | Success criteria | Effort |
|---|---|---|---|---|
| P11-BL-01 | Rebaseline FF10-001..FF10-013 into statuses: `Done`, `GO-critical remaining`, `Deferred post-v0.1`. | `docs/PHASE10_EXECUTION_BACKLOG.md` (or new tracking doc) | All 13 tickets have status, owner crate/module, and evidence link (PR/commit/log/doc). | S |
| P11-BL-02 | Execute GO-critical remainder in strict dependency order (minimum: entrypoint + pipeline + control-plane smoke coverage). | Primary: `ff-cli`, `ff-control`, `ff-pipeline`, `ff-sessions`, `ff-skills` | GO-critical tickets closed with passing tests and updated docs. | L |
| P11-BL-03 | Convert non-critical tickets to explicit defer records with risk acceptance + next milestone. | `docs/` backlog + release notes | Deferred tickets have owner, risk, and due phase; no undefined backlog items block GO. | S |

---

## Ordered execution plan (fastest path to GO)

1. **P11-RD-01** — freeze release scope decisions first (prevents churn).  
2. **P11-RD-02** — eliminate untracked drift immediately (highest risk blocker).  
3. **P11-CI-01** — add CI smoke gates early so every next change is guarded.  
4. **P11-IM-01** — remove root placeholder executable ambiguity.  
5. **P11-IM-02** — implement `ff-pipeline` MVP (largest integration blocker).  
6. **P11-BL-01** — mark FF10 ticket truth state against current code.  
7. **P11-BL-02** — close GO-critical remaining FF10 items in dependency order.  
8. **P11-IM-03 + P11-BL-03** — explicitly defer non-critical scope with owners/dates.  
9. **P11-CI-02 + P11-CI-03** — finalize release governance docs + local parity script.  
10. **Final validation gate** — rerun smoke, attach artifacts, make GO/NO-GO call.

---

## 48-hour execution plan (short)

### 0–6 hours
- Complete **P11-RD-01/02** (scope matrix + drift cleanup).
- Land **P11-CI-01** baseline workflow.
- Deliverable: clean scoped tree + CI running check/test.

### 6–18 hours
- Complete **P11-IM-01** (root bootstrap cleanup).
- Start **P11-IM-02** with minimal pipeline API and tests.
- Deliverable: no placeholder executable ambiguity; pipeline crate no longer one-line stub.

### 18–30 hours
- Finish **P11-IM-02** and validate workspace compile/tests.
- Complete **P11-BL-01** ticket status reconciliation.
- Deliverable: FF10 status matrix with evidence links.

### 30–42 hours
- Execute **P11-BL-02** for GO-critical leftovers.
- Draft **P11-IM-03/P11-BL-03** defer records for non-critical scope.
- Deliverable: only explicit, owned deferrals remain.

### 42–48 hours
- Finalize **P11-CI-02/03** docs + local parity command path.
- Rerun smoke (`cargo check --workspace`, `cargo test --workspace --lib`, CLI smoke), attach artifacts, issue final GO decision.
- Deliverable: release packet ready for `v0.1.0-internal` tag review.

---

## Exit condition for this remediation plan

This plan is complete when all tasks above are either:
- **Done with evidence**, or
- **Explicitly deferred with owner/date/risk acceptance**,

and final smoke artifacts support a documented **GO** recommendation.
