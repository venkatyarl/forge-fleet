# Phase 11 Risk Burndown Plan (Top 10)

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`  
Source: `docs/PHASE11_FINAL_AUDIT.md` (Section 3: Unresolved risks)

## Source unresolved risks (audit trace)

- **R1 (HIGH):** Release-content drift
- **R2 (MEDIUM):** CI gating gap
- **R3 (MEDIUM):** Integration maturity gap
- **R4 (MEDIUM):** Execution backlog not fully burned down

---

## Prioritized burndown backlog (top 10)

| Priority | Derived risk (trace) | Mitigation action | Owner area | ETA bucket | Measurable completion signal |
|---|---|---|---|---|---|
| 1 | Release tag scope is ambiguous because intended v0.1 assets are not explicitly enumerated (R1) | Create a release manifest that classifies all currently untracked/staged workspace assets into **include/defer/exclude** for `v0.1.0-internal`. | doc | 24h | `docs/PHASE11_RELEASE_MANIFEST.md` committed with path-level decisions and commit references for included assets. |
| 2 | Critical crate/doc assets may still be outside versioned release content (R1) | Stage and commit all manifest-marked v0.1 assets; ignore/archive non-release leftovers. | crate | 24h | `git status --porcelain` is clean for intended release branch head (no critical untracked phase crates/docs). |
| 3 | No automated CI guardrail for regression prevention on push/PR (R2) | Add a checked-in CI workflow running `cargo check --workspace` and `cargo test --workspace --lib`. | infra | 24h | `.github/workflows/ci.yml` exists and latest workflow run is green on both checks. |
| 4 | CI may exist but still not enforced as a merge gate (R2) | Configure branch protection to require CI statuses before merge. | infra | 48h | Required status checks include workspace check/test; merge is blocked when checks fail. |
| 5 | `ff-pipeline` remains placeholder, blocking integrated stage flow confidence (R3) | Complete FF10-003: implement stage interfaces, typed payloads/errors/events, and stage-chain integration tests. | crate | 1w | `crates/ff-pipeline/src/lib.rs` is non-placeholder; `cargo test -p ff-pipeline` includes passing success/failure chain tests. |
| 6 | Root executable path is still scaffolded (`Hello, world!`) and not control-plane aligned (R3) | Complete FF10-001: wire root `src/main.rs` to real `ff-cli`/`ff-control` command path. | crate | 48h | `src/main.rs` no longer prints `Hello, world!`; `cargo run -- --help` and `cargo run -- status` execute through real handlers. |
| 7 | FF10 backlog progress is not evidenced in a single authoritative tracker (R4) | Create a status board mapping FF10-001..FF10-013 to owner, status, dependency, and evidence links. | doc | 24h | `docs/PHASE11_BACKLOG_STATUS.md` committed with all 13 tickets and evidence pointers (tests/PRs/commits). |
| 8 | Core dependency chain (FF10-002..FF10-006) may still be partially scaffolded despite green unit smoke (R4) | Burn down FF10-002→006 in strict order with acceptance-test evidence for each ticket. | crate | 1w | FF10-002..006 marked done in tracker with passing test references and linked commits. |
| 9 | Runtime/orchestrator/persistence/API chain (FF10-007..FF10-012) may not yet deliver operator parity (R4) | Burn down FF10-007→012 with endpoint-level and routing fallback verification artifacts. | crate | 1w | FF10-007..012 marked done with reproducible validation evidence (`run-task/status/health/recent-events`, fallback logs, persistence checks). |
| 10 | Final GO evidence pack for internal tag cut is missing after remediation (R1/R2/R3/R4) | Re-run full smoke/readiness checks and publish final release addendum with command outputs and decision log. | doc | 48h | Addendum doc committed with fresh outputs for required checks + explicit GO decision replacing current NO-GO. |

---

## Burndown exit criteria

Phase 11 risk burndown is complete when:
1. All 10 completion signals above are satisfied.
2. `docs/PHASE11_FINAL_AUDIT.md` NO-GO conditions are either closed or explicitly deferred with approved rationale.
3. Internal `v0.1.0-internal` tag is cut only after evidence package is committed.
