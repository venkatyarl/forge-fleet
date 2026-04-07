# Phase 12 — Final Ops Checklist (ForgeFleet Rust Rewrite)

Date: `<YYYY-MM-DD>`  
Release tag: `<vX.Y.Z-rc.N>`  
Release branch: `<main|release-branch>`  
Repo: `/Users/venkat/taylorProjects/forge-fleet`

> Purpose: single operator checklist to run final readiness before release execution.  
> Format: checkbox-first for quick execution. Check every item before declaring **GO**.

---

## 0) Execution metadata (fill first)

- [ ] Release Coordinator (RC): `<name>`
- [ ] Release Engineer (RE): `<name>`
- [ ] QA Engineer (QE): `<name>`
- [ ] Ops/Observability Engineer (OE): `<name>`
- [ ] Incident Commander (IC): `<name>`
- [ ] Evidence directory created: `mkdir -p .phase12-release`

---

## 1) Environment / Toolchain checks

- [ ] Repository root confirmed:
  ```bash
  cd /Users/venkat/taylorProjects/forge-fleet
  pwd
  ```
- [ ] Working tree clean and branch synced:
  ```bash
  git fetch origin --tags
  git checkout "$RELEASE_BRANCH"
  git pull --ff-only origin "$RELEASE_BRANCH"
  git status --short
  ```
- [ ] Rust + Cargo installed and version output captured:
  ```bash
  rustc --version | tee .phase12-release/rustc_version.txt
  cargo --version | tee .phase12-release/cargo_version.txt
  ```
- [ ] Toolchain matches Phase 12 baseline (Rust `1.85.0` expected in CI).
- [ ] Required components available (`rustfmt`, `clippy`):
  ```bash
  rustup component list --installed | rg "rustfmt|clippy"
  ```
- [ ] Workspace metadata resolves:
  ```bash
  cargo metadata --no-deps >/dev/null
  ```
- [ ] Local quality gates pass (same gate set as CI):
  ```bash
  cargo fmt --all -- --check 2>&1 | tee .phase12-release/cargo_fmt_check.log
  cargo clippy --workspace -- -D warnings 2>&1 | tee .phase12-release/cargo_clippy.log
  cargo check --workspace 2>&1 | tee .phase12-release/cargo_check.log
  cargo test --workspace --lib 2>&1 | tee .phase12-release/cargo_test_workspace_lib.log
  ```

---

## 2) Workflow / Branch protection checks

> GitHub repo path: **Settings → Branches → Branch protection rules → `main`**

- [ ] PR required before merge (`main` is not directly pushable by default contributors).
- [ ] At least one approval required before merge.
- [ ] “Dismiss stale approvals when new commits are pushed” enabled.
- [ ] “Require review from Code Owners” enabled (if CODEOWNERS is used).
- [ ] “Require conversation resolution before merging” enabled.
- [ ] “Require status checks to pass before merging” enabled.
- [ ] Required checks include all Rust gate jobs from `.github/workflows/rust-quality-gates.yml`:
  - [ ] `cargo fmt --check`
  - [ ] `cargo clippy --workspace -- -D warnings`
  - [ ] `cargo check --workspace`
  - [ ] `cargo test --workspace --lib`
- [ ] “Require branches to be up to date before merging” enabled.
- [ ] Force pushes blocked on protected branch.
- [ ] Branch deletion blocked on protected branch.
- [ ] Admin bypass policy reviewed and explicitly accepted for this release.

---

## 3) Smoke verification checks

- [ ] CLI help smoke passes:
  ```bash
  cargo run -p ff-cli -- --help 2>&1 | tee .phase12-release/ff_cli_help.log
  ```
- [ ] CLI version smoke passes:
  ```bash
  cargo run -p ff-cli -- version 2>&1 | tee .phase12-release/ff_cli_version.log
  ```
- [ ] Release build succeeds:
  ```bash
  cargo build --workspace --release 2>&1 | tee .phase12-release/cargo_build_release.log
  ```
- [ ] Artifact checksum captured:
  ```bash
  shasum -a 256 target/release/forgefleet | tee .phase12-release/forgefleet.sha256
  ```
- [ ] Smoke evidence reviewed for success markers:
  - [ ] `cargo check`: contains `Finished`
  - [ ] `cargo test --workspace --lib`: contains `test result: ok`
  - [ ] `ff-cli -- --help`: prints usage/help without panic
  - [ ] Release build: finishes without errors

---

## 4) Release-day readiness checks

- [ ] Release control variables prepared:
  ```bash
  export RELEASE_BRANCH="<release-branch>"
  export RC_SHA="<approved_commit_sha>"
  export RC_TAG="<vX.Y.Z-rc.N>"
  ```
- [ ] RC commit re-verified before tag creation:
  ```bash
  git show --no-patch --decorate "$RC_SHA"
  ```
- [ ] Annotated tag command validated (do not run until GO):
  ```bash
  git tag -a "$RC_TAG" "$RC_SHA" -m "ForgeFleet Rust rewrite $RC_TAG"
  ```
- [ ] Tag push dry-run validated:
  ```bash
  git push --dry-run origin "$RC_TAG"
  ```
- [ ] Rollback target identified (`<previous_known_good_tag>`) and confirmed reachable.
- [ ] Rollback triggers agreed (P0/P1 defect, sustained SLO breach, repeated smoke failure).
- [ ] Incident/comms channel active with owners present.
- [ ] Evidence bundle complete in `.phase12-release/`.
- [ ] Go/No-Go checkpoint recorded with explicit decision:
  - [ ] **GO** (all checks green)
  - [ ] **NO-GO** (any gate red; release blocked)

---

## 5) Final sign-off

- [ ] RC sign-off: `<name> / <timestamp>`
- [ ] QE sign-off: `<name> / <timestamp>`
- [ ] OE sign-off: `<name> / <timestamp>`
- [ ] IC acknowledgment: `<name> / <timestamp>`

If any item remains unchecked, release is not ready.
