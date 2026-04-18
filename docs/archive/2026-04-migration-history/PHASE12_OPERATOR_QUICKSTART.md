# Phase 12 — Operator Quickstart Card (One Page)

Date: 2026-04-04  
Repo: `/Users/venkat/projects/forge-fleet`

> Fast operator reference for release checks, health checks, and rollback first steps.

---

## 0) Baseline (run first)

```bash
cd /Users/venkat/projects/forge-fleet
mkdir -p .phase12-release
export RELEASE_BRANCH="main"
```

---

## 1) Preflight + Release Gates (must all pass)

```bash
git fetch origin --tags
git checkout "$RELEASE_BRANCH"
git pull --ff-only origin "$RELEASE_BRANCH"
git status --short
git rev-parse --short HEAD | tee .phase12-release/release_sha.txt

rustc --version | tee .phase12-release/rustc_version.txt
cargo --version | tee .phase12-release/cargo_version.txt
cargo metadata --no-deps >/dev/null
cargo run -p ff-cli -- --help > .phase12-release/ff_cli_help.txt

cargo fmt --all -- --check 2>&1 | tee .phase12-release/cargo_fmt_check.log
cargo clippy --workspace -- -D warnings 2>&1 | tee .phase12-release/cargo_clippy.log
cargo check --workspace 2>&1 | tee .phase12-release/cargo_check.log
cargo test --workspace --lib 2>&1 | tee .phase12-release/cargo_test_workspace_lib.log
```

---

## 2) Health: where to look (first 24h)

### A) Operator CLI snapshot (primary)
```bash
cargo run -p ff-cli -- status
cargo run -p ff-cli -- health
cargo run -p ff-cli -- nodes
```

### B) Service endpoints (API/agent/gateway)
```bash
curl -fsS http://127.0.0.1:4000/health
curl -fsS http://127.0.0.1:8787/health
curl -fsS http://<agent-host>:<agent-port>/health
curl -fsS http://<agent-host>:<agent-port>/status
```

### C) Additional sources
- CI required workflows: `check`, `test`, release smoke
- Runtime logs: panic/crash spikes, repeated non-zero exits
- Release evidence folder: `.phase12-release/*.log`

---

## 3) Rollback — first steps (do in order)

1. Declare incident severity + capture failing signal (`status`/`health`/`nodes`, CI, or endpoint).
2. Identify last known-good tag.
3. Verify rollback target + capture evidence:

```bash
export ROLLBACK_TAG="<previous_known_good_tag>"
export FAILED_RC_TAG="<failed_rc_tag>"

git fetch origin --tags
git show --no-patch --decorate "$ROLLBACK_TAG"
git rev-parse "$ROLLBACK_TAG^{commit}" | tee .phase12-release/rollback_target_sha.txt
git diff --name-status "$ROLLBACK_TAG".."$FAILED_RC_TAG" \
  | tee .phase12-release/rollback_diff_files.txt
```

4. Execute approved rollback path (release owner / incident commander approval required).
   - If rollback is a merge revert on `main`:

```bash
git checkout main
git pull --ff-only origin main
git revert -m 1 <merge_commit_sha>
git push origin main
```

5. Re-run post-rollback smoke checks:

```bash
cargo check --workspace 2>&1 | tee .phase12-release/rollback_cargo_check.log
cargo test --workspace --lib 2>&1 | tee .phase12-release/rollback_cargo_test_lib.log
cargo run -p ff-cli -- health 2>&1 | tee .phase12-release/rollback_ff_cli_health.log
```

---

## 4) Canonical references

- `docs/PHASE12_RELEASE_COMMANDS.md`
- `docs/PHASE12_POST_RELEASE_MONITORING.md`
- `docs/PHASE12_FINAL_OPS_CHECKLIST.md`
- `docs/PHASE10_OPERATOR_RUNBOOK.md`
