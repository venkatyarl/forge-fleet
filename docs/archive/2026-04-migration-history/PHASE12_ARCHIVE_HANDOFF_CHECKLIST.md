# Phase 12 — Archive Handoff Checklist (Release-Prep Campaign)

Date: `<YYYY-MM-DD>`  
Archive Owner: `<name>`  
Repo: `/Users/venkat/taylorProjects/forge-fleet`

> Purpose: final checklist to archive the Phase 12 release-prep campaign with complete docs, evidence, sign-offs, and clear handoff pointers.

---

## 0) Archive control metadata

- [ ] Final campaign status recorded (`GO` / `NO-GO` / `HOLD`): `docs/PHASE12_GO_NO_GO_LEDGER.md`
- [ ] Final closeout summary updated and dated: `docs/PHASE12_CLOSEOUT_SUMMARY.md`
- [ ] Archive timestamp + owner recorded in this file.
- [ ] Final release/candidate commit SHA captured: `.phase12-release/release_sha.txt`

---

## 1) Documentation completeness (required)

### A) Canonical index and consolidation
- [ ] Master index includes all final Phase 9–12 deliverables: `docs/PHASE12_MASTER_INDEX.md`
- [ ] Final consolidation narrative is complete: `docs/PHASE12_FINAL_CONSOLIDATION.md`
- [ ] Release packet index is complete and link-valid: `docs/PHASE12_RELEASE_PACKET_INDEX.md`
- [ ] Executive summary reflects final decision and risks: `docs/PHASE12_EXECUTIVE_BRIEF.md`

### B) Governance and readiness docs
- [ ] Final audit retained: `docs/PHASE11_FINAL_AUDIT.md`
- [ ] GO gates finalized with outcomes: `docs/PHASE11_GO_GATES.md`
- [ ] Remediation plan finalized (closed/deferred with owner+date): `docs/PHASE11_REMEDIATION_PLAN.md`
- [ ] Decision memo finalized: `docs/PHASE12_DECISION_MEMO.md`
- [ ] Readiness scorecard finalized: `docs/PHASE12_READINESS_SCORECARD.md`
- [ ] Unresolved gaps list finalized: `docs/PHASE12_UNRESOLVED_GAPS.md`

### C) Ops execution docs
- [ ] Release commands source of truth finalized: `docs/PHASE12_RELEASE_COMMANDS.md`
- [ ] Release-day checklist finalized: `docs/PHASE12_RELEASE_DAY_CHECKLIST.md`
- [ ] Final ops checklist completed: `docs/PHASE12_FINAL_OPS_CHECKLIST.md`
- [ ] Release execution template filled (actuals, not placeholders): `docs/PHASE12_RELEASE_EXECUTION_TEMPLATE.md`
- [ ] Operator quickstart finalized: `docs/PHASE12_OPERATOR_QUICKSTART.md`
- [ ] GO activation playbook finalized: `docs/PHASE12_GO_ACTIVATION_PLAYBOOK.md`
- [ ] Post-release monitoring plan finalized: `docs/PHASE12_POST_RELEASE_MONITORING.md`

---

## 2) Evidence capture checklist (required)

### A) Evidence index docs
- [ ] Claim→proof mapping complete: `docs/PHASE12_EVIDENCE_MATRIX.md`
- [ ] Release proof bundle complete and current: `docs/PHASE12_RELEASE_PROOF_BUNDLE.md`
- [ ] Status dashboard reflects final evidence state: `docs/PHASE12_STATUS_DASHBOARD.md`

### B) Command/log artifacts (local paths)
- [ ] Format check log: `.phase12-release/cargo_fmt_check.log`
- [ ] Clippy log: `.phase12-release/cargo_clippy.log`
- [ ] Cargo check log: `.phase12-release/cargo_check.log`
- [ ] Cargo test log: `.phase12-release/cargo_test_workspace_lib.log`
- [ ] Release build log: `.phase12-release/cargo_build_release.log`
- [ ] CLI help smoke log: `.phase12-release/ff_cli_help.log`
- [ ] Release checksum file: `.phase12-release/forgefleet.sha256`
- [ ] Baseline smoke check log: `.phase9-smoke/cargo_check.log`
- [ ] Baseline smoke test log: `.phase9-smoke/cargo_test_workspace_lib.log`
- [ ] Baseline CLI smoke log: `.phase9-smoke/ff_cli_help.log`

### C) External evidence pointers (record links)
- [ ] CI run URL(s) for candidate SHA captured in release docs.
- [ ] Branch protection evidence for `main` captured in release docs.
- [ ] Tag push / release publication evidence captured (if GO path executed).

---

## 3) Sign-off records (required)

- [ ] Engineering sign-off complete: `docs/PHASE12_SIGNOFF_PACKAGE.md`
- [ ] Product sign-off complete: `docs/PHASE12_SIGNOFF_PACKAGE.md`
- [ ] Ops sign-off complete: `docs/PHASE12_SIGNOFF_PACKAGE.md`
- [ ] QA sign-off complete: `docs/PHASE12_SIGNOFF_PACKAGE.md`
- [ ] Final GO/NO-GO decision entry finalized: `docs/PHASE12_GO_NO_GO_LEDGER.md`
- [ ] Final decision rationale aligned across docs:
  - `docs/PHASE12_DECISION_MEMO.md`
  - `docs/PHASE12_CLOSEOUT_SUMMARY.md`
  - `docs/PHASE12_STATUS_DASHBOARD.md`

---

## 4) Handoff pointers (required)

- [ ] Handoff packet for operators/reviewers confirmed: `docs/PHASE11_HANDOFF_PACK.md`
- [ ] Communication templates ready for downstream consumers: `docs/PHASE12_COMMUNICATION_PACK.md`
- [ ] Command bundle ready for repeatable execution: `docs/PHASE12_COMMAND_BUNDLE.md`
- [ ] Active blockers and unresolved items explicitly pointed out:
  - `docs/PHASE12_BLOCKERS_TRACKER.md`
  - `docs/PHASE12_UNRESOLVED_GAPS.md`
- [ ] Final “where to start” path documented for next owner:
  1. `docs/PHASE12_MASTER_INDEX.md`
  2. `docs/PHASE12_RELEASE_PACKET_INDEX.md`
  3. `docs/PHASE12_SIGNOFF_PACKAGE.md`
  4. `docs/PHASE12_CLOSEOUT_SUMMARY.md`

---

## 5) Archive completion gate

Mark archive handoff complete only when all required sections above are checked.

- [ ] **ARCHIVE HANDOFF COMPLETE**
- [ ] Archive accepted by next owner/reviewer: `<name>`
- [ ] Acceptance timestamp: `<YYYY-MM-DD HH:MM TZ>`
