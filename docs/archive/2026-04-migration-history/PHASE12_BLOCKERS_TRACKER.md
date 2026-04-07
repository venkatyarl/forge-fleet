# Phase 12 — Release Blockers Burn-down Tracker

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`  
Release target: `v0.1.0-internal`  
Current decision state: **HOLD / NO-GO**

This tracker consolidates open release blockers from:
- `docs/PHASE12_UNRESOLVED_GAPS.md`
- `docs/PHASE12_DECISION_MEMO.md`
- `docs/PHASE12_STATUS_DASHBOARD.md`
- `docs/PHASE11_GO_GATES.md`
- `docs/PHASE11_RISK_BURNDOWN.md`
- `docs/PHASE11_REMEDIATION_PLAN.md`

> Note on target dates: where explicit calendar dates are not provided, dates below are pre-filled from the Phase 11 ETA buckets (24h / 48h / 1w) anchored to 2026-04-04.

| blocker | owner area | status | evidence required | target date |
|---|---|---|---|---|
| **UG-01: Release-content integrity drift** (modified/untracked release surface) | Release Engineering + Workspace maintainers | **OPEN (P0 blocker)** — Decision memo still flags release-content drift; gate **G4** not recorded as PASS. | Updated include/defer scope matrix + clean in-scope tree evidence (`git status --short`), and **G4 = PASS** recorded in `docs/PHASE11_GO_GATES.md`. | **2026-04-05** (24h bucket) |
| **UG-02: Formal GO gates not executed to completion** (G1–G10 + sign-offs incomplete) | Release Coordination + Engineering/Product/Ops/QA leadership | **OPEN (P0 blocker)** — Gate table remains unchecked; sign-off fields incomplete. | All gates **G1–G10** marked PASS with links/notes + completed sign-off blocks (Eng/Product/Ops/QA) in `docs/PHASE11_GO_GATES.md` and `docs/PHASE12_SIGNOFF_PACKAGE.md`. | **2026-04-06** (48h bucket / final GO checkpoint) |
| **UG-03: CI governance enforcement evidence incomplete** (workflow exists, merge enforcement proof pending) | Infra / Ops Engineering | **IN PROGRESS (P1)** — `.github/workflows/rust-quality-gates.yml` exists, but branch-protection enforcement proof is still pending; gate **G5** not yet PASS. | Green CI runs for fmt/clippy/check/test on candidate SHA + branch protection screenshots/settings proof showing required checks on `main` + **G5 = PASS**. | **2026-04-06** (48h bucket) |
| **UG-04: Integration maturity ambiguity** (`src/main.rs` bootstrap path + `ff-pipeline` placeholder/defer decision) | Core Platform (CLI/Control/Pipeline) | **OPEN (P1)** — Integration gap remains listed in final audit/decision memo; gate **G6** not yet PASS. | Either (a) placeholder paths replaced with accepted MVP behavior, or (b) explicit defer record with owner/date/risk acceptance; **G6 = PASS** recorded. | **2026-04-06** (48h for defer/closure decision) / **2026-04-11** (1w if completing full pipeline MVP) |
| **UG-05: Fresh candidate evidence packet missing** (`.phase12-release`/smoke artifacts not fully refreshed for candidate SHA) | Quality Engineering + Release Engineering | **OPEN (P1)** — Evidence matrix/signoff docs still call for fresh release-day artifacts. | Fresh logs/artifacts linked for candidate SHA: fmt/clippy/check/test/build/help + smoke logs; gates **G1/G2/G3/G7 = PASS** with artifact paths. | **2026-04-06** (48h bucket) |
| **UG-06: Ops startup/health + rollback readiness not evidenced** | Operations / On-call | **OPEN (P1)** — Ops gates are defined but not yet captured as PASS evidence for candidate release. | API/agent startup + health command outputs and rollback readiness (or dry-run) captured and linked; gates **G8/G9 = PASS**. | **2026-04-06** (48h bucket / before final sign-off) |
| **UG-07: FF10 backlog truth-state not fully reconciled** | Program + Core Engineering | **OPEN (P1)** — Backlog reconciliation remains required before GO call. | FF10-001..FF10-013 marked Done / GO-critical / Deferred with owner + evidence link; no undefined blocker status remains. | **2026-04-05** (24h for status rebaseline) / **2026-04-11** (1w if GO-critical implementation remains) |

## Burn-down usage

- Update **status** at least at each release checkpoint (T-24h, T-4h, T-1h, T+0).
- Add evidence links inline as blockers move to `IN PROGRESS` / `DONE`.
- Do not flip overall decision to GO until all blocker rows are closed and reflected in `docs/PHASE11_GO_GATES.md`.
