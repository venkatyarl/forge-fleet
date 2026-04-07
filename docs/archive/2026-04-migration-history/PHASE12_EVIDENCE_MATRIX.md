# Phase 12 — Evidence Matrix (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`

> Purpose: map major release claims to concrete, auditable evidence artifacts and verification commands.

---

## 1) Claim → Evidence Matrix

| Major claim | Evidence artifacts (source of truth) | Verification command(s) | PASS evidence pattern |
|---|---|---|---|
| **Build green** | - `.phase9-smoke/cargo_check.log` (baseline compile proof)  
- `docs/PHASE12_RELEASE_COMMANDS.md` (Sections 2.1, 2.2)  
- `.phase12-release/cargo_check.log` *(release-day)*  
- `.phase12-release/cargo_build_release.log` *(release-day)* | ```bash
cargo check --workspace
cargo build --workspace --release 2>&1 | tee .phase12-release/cargo_build_release.log
``` | `cargo check` exits 0 and release build log ends with successful `Finished` output for workspace targets. |
| **Tests pass** | - `.phase9-smoke/cargo_test_workspace_lib.log` (baseline test proof)  
- `docs/PHASE11_GO_GATES.md` (G2, G7)  
- `.phase12-release/cargo_test_workspace_lib.log` *(release-day)* | ```bash
cargo test --workspace --lib 2>&1 | tee .phase12-release/cargo_test_workspace_lib.log
``` | Test log contains `test result: ok` with zero failures. |
| **Docs complete** | - `docs/INDEX.md`  
- `docs/PHASE12_FINAL_CONSOLIDATION.md`  
- `docs/PHASE12_CLOSEOUT_SUMMARY.md`  
- `docs/PHASE12_SIGNOFF_PACKAGE.md` | ```bash
find docs -maxdepth 2 -type f | sort
``` | Required Phase 9–12 operational/governance docs exist, are linkable, and include release controls/checklists. |
| **CI gate ready** | - `.github/workflows/rust-quality-gates.yml`  
- `docs/PHASE12_CI_BOOTSTRAP.md`  
- `docs/PHASE12_QUALITY_GATE_AUTOMATION.md` | ```bash
grep -n "cargo fmt --check" .github/workflows/rust-quality-gates.yml
grep -n "cargo clippy --workspace -- -D warnings" .github/workflows/rust-quality-gates.yml
grep -n "cargo check --workspace" .github/workflows/rust-quality-gates.yml
grep -n "cargo test --workspace --lib" .github/workflows/rust-quality-gates.yml
``` | Workflow file contains all four required gates (`fmt`, `clippy`, `check`, `test --lib`) and is active on PR + `main` push. |
| **Release readiness** | - `docs/PHASE11_GO_GATES.md` (G1–G10)  
- `docs/PHASE12_RELEASE_DAY_CHECKLIST.md`  
- `docs/PHASE12_RELEASE_COMMANDS.md`  
- `docs/PHASE12_SIGNOFF_PACKAGE.md`  
- `docs/PHASE12_STATUS_DASHBOARD.md` (current decision state) | ```bash
# Execute gate checklist + capture artifacts
# (see docs/PHASE11_GO_GATES.md and docs/PHASE12_RELEASE_COMMANDS.md)
``` | All gates G1–G10 marked PASS, required artifacts attached, and Engineering/Product/Ops/QA sign-offs recorded before tag cut. |

---

## 2) Evidence Missing (Fill on Release Day)

> The following are expected **release-day artifacts**. Fill all placeholders before final GO.

| Missing evidence item | Placeholder to fill | Expected location / format | Owner |
|---|---|---|---|
| Candidate release SHA captured from clean tree | `<RELEASE_SHA>` | `.phase12-release/release_sha.txt` | RE |
| Fresh fmt gate log | `<ATTACH_LOG_PATH_OR_LINK>` | `.phase12-release/cargo_fmt_check.log` | QE |
| Fresh clippy gate log | `<ATTACH_LOG_PATH_OR_LINK>` | `.phase12-release/cargo_clippy.log` | QE |
| Fresh check gate log | `<ATTACH_LOG_PATH_OR_LINK>` | `.phase12-release/cargo_check.log` | QE |
| Fresh test gate log | `<ATTACH_LOG_PATH_OR_LINK>` | `.phase12-release/cargo_test_workspace_lib.log` | QE |
| Release build log | `<ATTACH_LOG_PATH_OR_LINK>` | `.phase12-release/cargo_build_release.log` | RE |
| Release artifact checksum | `<SHA256_VALUE>` | `.phase12-release/forgefleet.sha256` | RE |
| CI run URL for candidate SHA (all jobs green) | `<GITHUB_ACTIONS_RUN_URL>` | GitHub Actions run page | RE/OE |
| Branch protection proof (`main`) | `<SETTINGS_URL_OR_SCREENSHOT_LINK>` | GitHub branch protection evidence | OE |
| GO/NO-GO gate sheet fully marked | `<DOC_LINK_WITH_COMPLETED_GATES>` | `docs/PHASE11_GO_GATES.md` | RC |
| Final sign-offs (Engineering/Product/Ops/QA) | `<NAMES + TIMESTAMPS + ACKS>` | `docs/PHASE12_SIGNOFF_PACKAGE.md` | RC |
| RC tag push proof | `<TAG_NAME + LS_REMOTE_OUTPUT_LINK>` | `git push` / `git ls-remote --tags origin <tag>` evidence | RE |
| Rollback artifacts *(if triggered)* | `<ROLLBACK_EVIDENCE_LINKS>` | `.phase12-release/rollback_*.log|txt` | IC/RE |

---

## 3) Quick Usage Notes

1. Use this matrix as the **index** for release evidence collection.
2. Link every claim to either a file path in-repo or an immutable external URL (CI run, branch settings capture).
3. Any unfilled placeholder in Section 2 keeps release decision at **HOLD / NO-GO**.
