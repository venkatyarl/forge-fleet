# Phase 12 — Final Command Bundle (Minimal)

Date: 2026-04-04  
Repo: `/Users/venkat/projects/forge-fleet`

> Purpose: one minimal, copy-paste command flow for Phase 12 release control.
> 
> Sections are ordered and map to the required stages:
> 1) validate state → 2) execute gates → 3) gather evidence → 4) finalize decision.

---

## 1) Validate state

### Copy/paste

```bash
cd /Users/venkat/projects/forge-fleet
mkdir -p .phase12-release
export RELEASE_BRANCH="${RELEASE_BRANCH:-main}"

git fetch origin --tags
git checkout "$RELEASE_BRANCH"
git pull --ff-only origin "$RELEASE_BRANCH"

git status --short | tee .phase12-release/git_status.txt
git rev-parse --abbrev-ref HEAD | tee .phase12-release/release_branch.txt
git rev-parse --short HEAD | tee .phase12-release/release_sha.txt

rustc --version | tee .phase12-release/rustc_version.txt
cargo --version | tee .phase12-release/cargo_version.txt
cargo metadata --no-deps >/dev/null
```

### Expected output hints

- `git_status.txt` should be empty (clean tree).
- `release_branch.txt` should match `RELEASE_BRANCH`.
- `release_sha.txt` should contain exactly one short SHA.
- `cargo metadata` exits `0` with no dependency graph errors.

---

## 2) Execute gates

### Copy/paste

```bash
cd /Users/venkat/projects/forge-fleet
set -euo pipefail

cargo fmt --all -- --check 2>&1 | tee .phase12-release/cargo_fmt_check.log
cargo clippy --workspace -- -D warnings 2>&1 | tee .phase12-release/cargo_clippy.log
cargo check --workspace 2>&1 | tee .phase12-release/cargo_check.log
cargo test --workspace --lib 2>&1 | tee .phase12-release/cargo_test_workspace_lib.log
cargo run -p ff-cli -- --help 2>&1 | tee .phase12-release/ff_cli_help.log
cargo build --workspace --release 2>&1 | tee .phase12-release/cargo_build_release.log
```

### Expected output hints

- `fmt` exits cleanly (no formatting diffs).
- `clippy` exits cleanly with `-D warnings`.
- `cargo check` log ends with `Finished`.
- test log contains `test result: ok` and zero failures.
- `ff-cli -- --help` prints usage/help without panic.
- release build finishes successfully.

---

## 3) Gather evidence

### Copy/paste

```bash
cd /Users/venkat/projects/forge-fleet
set -euo pipefail

find target/release -maxdepth 1 -type f -perm -111 -exec shasum -a 256 {} \; \
  | tee .phase12-release/artifacts.sha256

{
  echo "release_sha=$(cat .phase12-release/release_sha.txt)"
  echo "release_branch=$(cat .phase12-release/release_branch.txt)"
  echo "captured_at_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} | tee .phase12-release/release_context.txt

ls -1 .phase12-release | sort | tee .phase12-release/evidence_manifest.txt
```

### Expected output hints

- `artifacts.sha256` contains one or more executable checksums.
- `release_context.txt` includes SHA, branch, and UTC timestamp.
- `evidence_manifest.txt` lists all generated logs/artifacts for attachment.

---

## 4) Finalize decision

### Copy/paste

```bash
cd /Users/venkat/projects/forge-fleet
set -euo pipefail

missing=0
required=(
  .phase12-release/release_sha.txt
  .phase12-release/rustc_version.txt
  .phase12-release/cargo_version.txt
  .phase12-release/cargo_fmt_check.log
  .phase12-release/cargo_clippy.log
  .phase12-release/cargo_check.log
  .phase12-release/cargo_test_workspace_lib.log
  .phase12-release/ff_cli_help.log
  .phase12-release/cargo_build_release.log
  .phase12-release/artifacts.sha256
)

for f in "${required[@]}"; do
  if [[ -s "$f" ]]; then
    echo "OK   $f"
  else
    echo "MISS $f"
    missing=1
  fi
done

grep -q "test result: ok" .phase12-release/cargo_test_workspace_lib.log && echo "OK   test marker" || { echo "MISS test marker"; missing=1; }
grep -q "Finished" .phase12-release/cargo_check.log && echo "OK   check marker" || { echo "MISS check marker"; missing=1; }
grep -q "Finished" .phase12-release/cargo_build_release.log && echo "OK   build marker" || { echo "MISS build marker"; missing=1; }

if [[ -s .phase12-release/git_status.txt ]]; then
  echo "MISS clean-tree gate (git_status.txt is not empty)"
  missing=1
else
  echo "OK   clean-tree gate"
fi

if [[ "$missing" -eq 0 ]]; then
  echo "FINAL_DECISION=GO_CANDIDATE"
  echo "Next: collect Eng/Product/Ops approvals, then run tag dry-run."
else
  echo "FINAL_DECISION=NO_GO"
  echo "Next: resolve failed/missing evidence, then rerun bundle."
  exit 1
fi
```

### Expected output hints

- Every required evidence line prints `OK`.
- Decision line prints either:
  - `FINAL_DECISION=GO_CANDIDATE` (technical gates passed), or
  - `FINAL_DECISION=NO_GO` (at least one gate/evidence failure).

### If `GO_CANDIDATE`, run tag readiness dry-run

```bash
cd /Users/venkat/projects/forge-fleet
export RC_SHA="$(cat .phase12-release/release_sha.txt)"
export RC_TAG="<vX.Y.Z-rc.N>"

git show --no-patch --decorate "$RC_SHA"
git push --dry-run origin "$RC_TAG"
```

Expected hint: dry-run push succeeds with no conflicts/rejections.

---

This bundle is intentionally minimal and should be paired with formal sign-off artifacts (`Engineering`, `Product`, `Ops`) before any live tag push.
