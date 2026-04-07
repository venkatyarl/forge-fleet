# Phase 12 — Release Command Cookbook (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`

> Purpose: copy/paste command sets for Phase 12 release execution.  
> Scope: preflight validation, build/test gates, RC tagging workflow, and rollback verification.

---

## 0) Operator baseline

Run all blocks from repo root unless noted otherwise.

```bash
cd /Users/venkat/taylorProjects/forge-fleet
mkdir -p .phase12-release
```

**Safety notes**
- Use a clean working tree before any release action.
- Do not tag from a local-only commit.
- Keep logs in `.phase12-release/` for audit evidence.

**Expected output**
- Directory `.phase12-release/` exists.
- No errors changing directory.

---

## 1) Preflight validation (GO/NO-GO gate A)

### 1.1 Repo + branch integrity

```bash
cd /Users/venkat/taylorProjects/forge-fleet
export RELEASE_BRANCH="<release-branch>"    # ex: main

git fetch origin --tags
git checkout "$RELEASE_BRANCH"
git pull --ff-only origin "$RELEASE_BRANCH"

git status --short
git rev-parse --abbrev-ref HEAD
git rev-parse --short HEAD | tee .phase12-release/release_sha.txt
```

**Safety notes**
- `git status --short` must be empty before proceeding.
- If `git pull --ff-only` fails, stop and reconcile branch divergence.
- Capture the SHA once and use it consistently in all downstream steps.

**Expected output**
- `git status --short` prints nothing.
- Branch name output matches `$RELEASE_BRANCH`.
- `.phase12-release/release_sha.txt` contains one short commit SHA.

### 1.2 Toolchain + CLI sanity

```bash
rustc --version | tee .phase12-release/rustc_version.txt
cargo --version | tee .phase12-release/cargo_version.txt
cargo metadata --no-deps >/dev/null
cargo run -p ff-cli -- --help > .phase12-release/ff_cli_help.txt
```

**Safety notes**
- If `cargo metadata` fails, stop (workspace/dependency graph is not healthy).
- Keep the CLI help snapshot as proof the binary entrypoint is functional.

**Expected output**
- Rust and Cargo version lines are written to files.
- `cargo metadata` exits 0 with no stderr failures.
- `ff_cli_help.txt` contains usage/help text.

---

## 2) Build/test suite (GO/NO-GO gates B + C)

### 2.1 Required quality gates

```bash
cd /Users/venkat/taylorProjects/forge-fleet

cargo fmt --all -- --check 2>&1 | tee .phase12-release/cargo_fmt_check.log
cargo clippy --workspace -- -D warnings 2>&1 | tee .phase12-release/cargo_clippy.log
cargo check --workspace 2>&1 | tee .phase12-release/cargo_check.log
cargo test --workspace --lib 2>&1 | tee .phase12-release/cargo_test_workspace_lib.log
```

**Safety notes**
- Treat any warning as a failure for release gating (`-D warnings`).
- Do not continue on partial pass; all four commands must be green.
- Preserve logs unchanged for post-release audit.

**Expected output**
- `cargo fmt` exits cleanly (no files needing formatting).
- `cargo clippy` completes without warning/error diagnostics.
- `cargo check` ends with `Finished` for workspace crates.
- `cargo test --workspace --lib` ends with `test result: ok` and zero failures.

### 2.2 Release build + artifact fingerprint

```bash
cd /Users/venkat/taylorProjects/forge-fleet

cargo build --workspace --release 2>&1 | tee .phase12-release/cargo_build_release.log

# macOS checksum (use shasum); adjust path if binary name differs
shasum -a 256 target/release/forgefleet | tee .phase12-release/forgefleet.sha256

cargo run -p ff-cli -- version 2>&1 | tee .phase12-release/ff_cli_version.log
```

**Safety notes**
- If `target/release/forgefleet` is not present, stop and verify binary target names.
- Capture checksum immediately after build to prevent artifact confusion.

**Expected output**
- `cargo build --release` finishes successfully.
- `forgefleet.sha256` has a single SHA256 line.
- CLI version command returns a version string / version payload.

---

## 3) Release candidate tagging workflow (template)

### 3.1 Set release variables (fill placeholders)

```bash
cd /Users/venkat/taylorProjects/forge-fleet

export RELEASE_BRANCH="<release-branch>"      # ex: main
export RC_TAG="<vX.Y.Z-rc.N>"                 # ex: v0.1.0-rc.1
export RC_SHA="<validated_commit_sha>"        # from Gate A/B/C evidence
export RELEASE_NOTES_FILE="<path/to/notes.md>"# ex: docs/releases/v0.1.0-rc.1.md
```

**Safety notes**
- Never use `HEAD` implicitly for tag creation; pin `RC_SHA` explicitly.
- Tag names must be unique and immutable after publish.

**Expected output**
- Environment variables are set for deterministic tag commands.

### 3.2 Verify RC commit + create annotated tag

```bash
git fetch origin --tags
git show --no-patch --decorate "$RC_SHA"
git tag -a "$RC_TAG" "$RC_SHA" -m "ForgeFleet Rust rewrite $RC_TAG"
git show --no-patch "$RC_TAG"
```

**Safety notes**
- Confirm `git show` points to the commit validated by gates.
- Use annotated tags (not lightweight tags) for release provenance.

**Expected output**
- Commit metadata for `RC_SHA` is displayed.
- Tag object shows annotation message and target commit.

### 3.3 Dry-run push, then publish tag

```bash
git push --dry-run origin "$RC_TAG"
git push origin "$RC_TAG"
git ls-remote --tags origin "$RC_TAG"
```

**Safety notes**
- If dry-run indicates rejection/conflict, stop and resolve before live push.
- Do not force-push tags for release candidates.

**Expected output**
- Dry-run shows a successful push plan.
- Live push succeeds without non-fast-forward errors.
- `ls-remote` prints the new remote tag ref.

### 3.4 (Optional) Publish GitHub release notes

```bash
# Requires GitHub CLI auth and an existing notes file
# gh release create "$RC_TAG" --title "ForgeFleet $RC_TAG" --notes-file "$RELEASE_NOTES_FILE"
```

**Safety notes**
- Run only after tag push is confirmed.
- Ensure release notes explicitly call out known deferred items.

**Expected output**
- GitHub release entry exists for `$RC_TAG`.

---

## 4) Rollback verification commands (post-rollback gate)

### 4.1 Verify rollback target identity

```bash
cd /Users/venkat/taylorProjects/forge-fleet

export ROLLBACK_TAG="<previous_known_good_tag>"   # ex: v0.1.0-rc.0

git fetch origin --tags
git show --no-patch --decorate "$ROLLBACK_TAG"
git rev-parse "$ROLLBACK_TAG^{commit}" | tee .phase12-release/rollback_target_sha.txt
```

**Safety notes**
- Roll back only to a previously validated tag.
- Record rollback target SHA for incident timeline and postmortem.

**Expected output**
- Tag metadata resolves cleanly.
- `rollback_target_sha.txt` contains one full commit SHA.

### 4.2 Compare failed RC vs rollback target (audit)

```bash
export FAILED_RC_TAG="<failed_rc_tag>"            # ex: v0.1.0-rc.1

git diff --name-status "$ROLLBACK_TAG".."$FAILED_RC_TAG" \
  | tee .phase12-release/rollback_diff_files.txt
```

**Safety notes**
- Use this diff for root-cause narrowing; do not skip evidence capture.
- If diff is unexpectedly empty, verify both tags resolve to distinct SHAs.

**Expected output**
- File-level diff list between rollback target and failed RC.

### 4.3 Post-rollback smoke verification

```bash
cargo check --workspace 2>&1 | tee .phase12-release/rollback_cargo_check.log
cargo test --workspace --lib 2>&1 | tee .phase12-release/rollback_cargo_test_lib.log
cargo run -p ff-cli -- health 2>&1 | tee .phase12-release/rollback_ff_cli_health.log
```

**Safety notes**
- Rollback is not complete until compile/test/health smoke re-pass.
- If any check fails, keep incident open and halt further rollout.

**Expected output**
- `cargo check` and `cargo test` pass again on rollback target.
- CLI health command returns healthy/expected status data.

---

## 5) Evidence checklist (attach to release report)

- `.phase12-release/release_sha.txt`
- `.phase12-release/cargo_fmt_check.log`
- `.phase12-release/cargo_clippy.log`
- `.phase12-release/cargo_check.log`
- `.phase12-release/cargo_test_workspace_lib.log`
- `.phase12-release/cargo_build_release.log`
- `.phase12-release/forgefleet.sha256`
- `.phase12-release/rollback_target_sha.txt` (if rollback executed)
- `.phase12-release/rollback_diff_files.txt` (if rollback executed)

This cookbook is command-ready, but execution decisions remain controlled by the Phase 12 go/no-go process and release owner approvals.
