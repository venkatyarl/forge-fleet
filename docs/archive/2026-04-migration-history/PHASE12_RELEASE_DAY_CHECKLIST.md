# Phase 12 — Release Day Checklist (ForgeFleet Rust Rewrite)

Date: `<YYYY-MM-DD>`  
Release tag: `<vX.Y.Z-rc.N>`  
Repo: `/Users/venkat/projects/forge-fleet`

> Purpose: minute-marked execution checklist for release day.  
> Scope: pre-release prep (T-60 to T-10), release window (T-10 to T+15), immediate post-release validation (T+15 to T+120).  
> Rule: if any rollback trigger fires, stop forward actions and execute rollback workflow from `docs/PHASE12_RELEASE_COMMANDS.md`.

---

## Roles

- **RC** — Release Coordinator
- **RE** — Release Engineer (Git/tag/build executor)
- **QE** — QA Engineer
- **OE** — Observability/Ops Engineer
- **IC** — Incident Commander (can be RC in small team)

---

## 1) Pre-Release Block (T-60 → T-10)

| Time | Owner role | Command / Evidence | Rollback trigger |
|---|---|---|---|
| T-60 | RC | Confirm freeze + comms channel open. Evidence: release thread message with owners on-call and go/no-go time. | Missing critical owner coverage or no incident channel. |
| T-55 | RE | `git fetch origin --tags && git checkout "$RELEASE_BRANCH" && git pull --ff-only origin "$RELEASE_BRANCH"`  Evidence: clean pull log. | `--ff-only` fails or branch divergence detected. |
| T-50 | RE | `git status --short` + `git rev-parse --short HEAD | tee .phase12-release/release_sha.txt`  Evidence: empty status + SHA file. | Dirty tree or SHA cannot be captured/reproduced. |
| T-45 | RE | `rustc --version && cargo --version && cargo metadata --no-deps >/dev/null`  Evidence: version output + metadata exit 0. | Toolchain mismatch or metadata failure. |
| T-40 | QE | `cargo fmt --all -- --check`  Evidence: fmt check log with no diffs. | Formatting gate fails (indicates unready commit). |
| T-35 | QE | `cargo clippy --workspace -- -D warnings`  Evidence: clippy log with zero warnings/errors. | Any clippy warning/error (release gate red). |
| T-30 | QE | `cargo check --workspace`  Evidence: workspace check log shows successful finish. | Compile/regression failure in any crate. |
| T-25 | QE | `cargo test --workspace --lib`  Evidence: test report includes `test result: ok`. | Any test failure, panic, or timeout. |
| T-20 | RE | `cargo build --workspace --release && shasum -a 256 target/release/forgefleet`  Evidence: `.phase12-release/forgefleet.sha256`. | Release binary missing or checksum generation fails. |
| T-15 | OE | Baseline telemetry snapshot (error/latency/throughput) from dashboard. Evidence: screenshot/link posted in release thread. | Existing elevated error rate/SLO risk before cut. |
| T-10 | RC | Formal go/no-go checkpoint. Evidence: explicit **GO** in release thread tied to captured SHA. | Any unresolved gate failure or unknown risk; declare NO-GO. |

---

## 2) Release Window Block (T-10 → T+15)

| Time | Owner role | Command / Evidence | Rollback trigger |
|---|---|---|---|
| T-10 | RE | Set immutable vars: `RC_SHA`, `RC_TAG`, `RELEASE_BRANCH`. Evidence: values echoed in release log. | SHA/tag mismatch with approved go/no-go SHA. |
| T-8 | RE | `git show --no-patch --decorate "$RC_SHA"`  Evidence: commit metadata pasted to thread for cross-check. | Commit author/tree differs from approved candidate. |
| T-6 | RE | `git tag -a "$RC_TAG" "$RC_SHA" -m "ForgeFleet Rust rewrite $RC_TAG" && git show --no-patch "$RC_TAG"`  Evidence: annotated tag object shown. | Tag creation fails or resolves to wrong commit. |
| T-4 | RE | `git push --dry-run origin "$RC_TAG"`  Evidence: successful dry-run output captured. | Dry-run rejection/conflict/permission issue. |
| T-2 | RE | `git push origin "$RC_TAG" && git ls-remote --tags origin "$RC_TAG"`  Evidence: remote tag ref confirmed. | Push fails or remote ref not visible. |
| T+0 | RC | Announce release cut with tag + SHA + artifact checksum. Evidence: release announcement message. | Inability to prove released ref (tag/SHA/checksum mismatch). |
| T+2 | QE | `cargo run -p ff-cli -- version` against released build context. Evidence: version output matches tag intent. | Reported version incorrect/unexpected. |
| T+5 | OE | Verify health and startup telemetry (dashboard + critical logs). Evidence: “green” snapshot in thread. | Error spike, crash loop, or failed health checks >5 min. |
| T+10 | QE | Execute critical smoke path(s): CLI help/primary commands. Evidence: command transcript attached. | Core user path failure or data correctness issue. |
| T+15 | RC + IC | First stabilization decision: continue rollout vs rollback. Evidence: explicit decision post. | Any P1/P0 defect, sustained SLO breach, or repeated smoke failure. |

---

## 3) Immediate Post-Release Block (T+15 → T+120)

| Time | Owner role | Command / Evidence | Rollback trigger |
|---|---|---|---|
| T+15 | OE | Start high-frequency monitoring window (5-min cadence). Evidence: monitoring cadence noted in thread. | Unknown monitoring state or telemetry blind spot. |
| T+20 | QE | Re-run focused regression suite (highest-risk modules). Evidence: pass/fail summary posted. | Regression in release-critical function. |
| T+30 | OE | Compare live metrics vs pre-release baseline (T-15 snapshot). Evidence: delta summary (error, p95 latency). | Sustained degradation beyond agreed threshold. |
| T+45 | RE | Re-verify release provenance: tag, SHA, checksum, binary identity. Evidence: command outputs linked. | Provenance mismatch suggests wrong artifact deployed. |
| T+60 | RC | 60-min stability gate review with IC/OE/QE. Evidence: “Stable @60” or “Rollback initiated” note. | Any unresolved sev issue or unstable trend line. |
| T+75 | OE | Log/alert sweep (warning bursts, retry storms, queue backlogs). Evidence: alert summary attached. | Alert storm indicating latent failure pattern. |
| T+90 | QE | Confirm no delayed failures in key workflows; sample user-facing operations. Evidence: checklist with outcomes. | Late-onset functional failures or data integrity risk. |
| T+105 | RE | Verify no emergency patches or untracked hotfixes landed outside plan. Evidence: `git log --oneline <prev_tag>..$RC_TAG` + release notes parity. | Unplanned code delta requiring containment. |
| T+120 | RC + IC | Close immediate window; handoff to standard monitoring + T+24h closeout. Evidence: signed “Release Stable (T+120)” message. | Any open critical incident; keep incident active and rollback if required. |

---

## Rollback Activation (If Any Trigger Fires)

1. **IC declares rollback** in release channel (time-stamped).  
2. **RE executes rollback target verification + rollback commands** from `docs/PHASE12_RELEASE_COMMANDS.md` (Section 4).  
3. **QE/OE validate rollback health** (`cargo check`, `cargo test --workspace --lib`, health/smoke checks).  
4. **RC posts rollback complete** with target tag/SHA and impact summary.

---

## Required Evidence Artifacts

- `.phase12-release/release_sha.txt`
- `.phase12-release/cargo_fmt_check.log`
- `.phase12-release/cargo_clippy.log`
- `.phase12-release/cargo_check.log`
- `.phase12-release/cargo_test_workspace_lib.log`
- `.phase12-release/cargo_build_release.log`
- `.phase12-release/forgefleet.sha256`
- rollback artifacts (if triggered):
  - `.phase12-release/rollback_target_sha.txt`
  - `.phase12-release/rollback_diff_files.txt`
  - `.phase12-release/rollback_cargo_check.log`
  - `.phase12-release/rollback_cargo_test_lib.log`

This checklist is execution-ready and intentionally operator-focused; it does not replace go/no-go ownership decisions.
