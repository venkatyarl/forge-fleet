# Phase 12 — Unresolved Gaps Register (NO-GO -> GO)

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`  
Release target: `v0.1.0-internal`

Current release posture in Phase 12 docs is **HOLD / NO-GO**.  
This register lists the **remaining open gaps required to reach GO**.

---

## Open gaps that must close for GO

| Gap ID | Open gap | Owner area | Severity | Current evidence | Closure condition (GO-ready) |
|---|---|---|---|---|---|
| UG-01 | **Release-content integrity drift** (broad modified/untracked surface in candidate tree). | Release Engineering + Workspace maintainers | **HIGH (P0)** | `docs/PHASE11_FINAL_AUDIT.md` (R1), `docs/PHASE12_DECISION_MEMO.md` Section 2/3, `git status --short` snapshot | Candidate release branch has reconciled include/defer scope and no critical untracked in-scope assets; gate **G4 = PASS** with evidence recorded. |
| UG-02 | **Formal GO gates not executed to completion** (G1–G10 unchecked, sign-off records incomplete). | Release Coordination + Engineering/Product/Ops/QA leadership | **HIGH (P0)** | `docs/PHASE11_GO_GATES.md`, `docs/PHASE12_SIGNOFF_PACKAGE.md`, `docs/PHASE12_READINESS_SCORECARD.md` (governance=2.0) | All gates G1–G10 explicitly marked PASS with evidence links, and full sign-off block completed (Eng/Product/Ops/QA). |
| UG-03 | **CI governance not fully enforced in evidence** (workflow exists, enforcement proof pending). | Infra / Ops Engineering | **MEDIUM (P1)** | `.github/workflows/rust-quality-gates.yml`, `docs/PHASE12_CI_BOOTSTRAP.md`, `docs/PHASE12_DECISION_MEMO.md` | Candidate SHA has green CI runs for fmt/clippy/check/test and branch protection on `main` requires those checks; gate **G5 = PASS** with settings/run proof. |
| UG-04 | **Integration maturity ambiguity** (`src/main.rs` hello-world bootstrap; `ff-pipeline` placeholder). | Core Platform (CLI/Control/Pipeline) | **MEDIUM (P1)** | `docs/PHASE11_FINAL_AUDIT.md` (R3), `docs/PHASE12_DECISION_MEMO.md`, `src/main.rs`, `crates/ff-pipeline/src/lib.rs` | Either (a) placeholders are replaced with accepted MVP integration behavior, or (b) explicit defer record exists with owner/date/risk acceptance; gate **G6 = PASS**. |
| UG-05 | **Fresh release evidence packet missing for candidate SHA** (release-day artifacts still placeholders). | Quality Engineering + Release Engineering | **MEDIUM (P1)** | `docs/PHASE12_EVIDENCE_MATRIX.md` Section 2, `docs/PHASE12_SIGNOFF_PACKAGE.md` required artifacts list | `.phase12-release/` logs + checksum + SHA are generated and linked (fmt/clippy/check/test/build/help), plus fresh smoke artifacts; gates **G1/G2/G3/G7** pass with attached evidence. |
| UG-06 | **Ops startup/health and rollback readiness not evidenced for release candidate.** | Operations / On-call | **MEDIUM (P1)** | `docs/PHASE11_GO_GATES.md` (G8/G9), `docs/PHASE10_OPERATOR_RUNBOOK.md`, `docs/PHASE10_SHIP_PLAN.md` | API/agent startup + health checks and rollback readiness (or dry-run) are executed and captured in release evidence; gates **G8/G9 = PASS**. |
| UG-07 | **FF10 backlog truth-state not fully reconciled to release decisioning.** | Program + Core Engineering | **MEDIUM (P1)** | `docs/PHASE11_FINAL_AUDIT.md` (R4), `docs/PHASE11_REMEDIATION_PLAN.md` (P11-BL-01..03), `docs/PHASE11_RISK_BURNDOWN.md` | FF10-001..FF10-013 are explicitly marked Done / GO-critical / Deferred with owner + evidence link; no undefined blocker remains before GO call. |

---

## Tiny deferable-after-v0.1 section

The following can be deferred **post-v0.1** if explicitly documented with owner/date/risk acceptance:

- Full production pipeline depth and end-to-end stage hardening beyond MVP (`ff-pipeline` advanced behavior).
- Broader provider/cloud runtime matrix and routing hardening.
- CI expansion beyond baseline gates (integration/e2e/perf/compat snapshots).
- Promotion of experimental crates (`ff-pipeline`, `ff-deploy`) from experimental to beta/stable contracts.

(Aligned with deferred scope notes in `docs/PHASE11_RELEASE_CANDIDATE_NOTES.md` and “Do later” guidance in `docs/PHASE12_DECISION_MEMO.md`.)
