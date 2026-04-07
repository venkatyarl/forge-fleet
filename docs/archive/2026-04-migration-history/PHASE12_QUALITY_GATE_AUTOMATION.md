# Phase 12 — Quality Gate Automation (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`

## 1) Goal

Establish a **minimal, enforceable CI quality gate** so regressions are blocked automatically on pull requests and protected branches.

This phase introduces a baseline gate focused on format, lint, compile, and core library tests.

---

## 2) Minimal CI workflow design

Run the following commands in CI, in this order:

1. `cargo fmt --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo check --workspace`
4. `cargo test --workspace --lib`

### Why this set

- **fmt**: prevents style drift and noisy formatting diffs.
- **clippy -D warnings**: turns lints into hard failures to catch issues early.
- **check**: fast compile verification across workspace.
- **test --lib**: validates core library behavior without broad integration scope expansion.

---

## 3) Recommended GitHub Actions YAML skeleton

> Suggested location: `.github/workflows/rust-quality-gates.yml`

```yaml
name: Rust Quality Gates

on:
  pull_request:
  push:
    branches:
      - main

jobs:
  rust-quality-gates:
    name: Rust Quality Gates
    runs-on: ubuntu-latest

    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Cache Rust artifacts
        uses: Swatinem/rust-cache@v2

      - name: cargo fmt --check
        run: cargo fmt --check

      - name: cargo clippy --workspace -- -D warnings
        run: cargo clippy --workspace -- -D warnings

      - name: cargo check --workspace
        run: cargo check --workspace

      - name: cargo test --workspace --lib
        run: cargo test --workspace --lib
```

---

## 4) Branch protection suggestions

For `main` (and any release branch):

1. **Require a pull request before merging**.
2. **Require status checks to pass before merging**.
   - Add the CI check from this workflow (match exact check name shown in GitHub UI, typically `Rust Quality Gates / Rust Quality Gates`).
3. **Require branches to be up to date before merging**.
4. **Require at least 1 approving review** (2 for critical paths if desired).
5. **Dismiss stale approvals when new commits are pushed**.
6. (Optional but recommended) **Include administrators** in branch protection enforcement.

---

## 5) Rollout notes

- This is intentionally the smallest meaningful gate for Phase 12.
- If CI time remains acceptable, future phases can extend with:
  - target matrix (OS/toolchain)
  - integration tests
  - docs checks
  - security scan (`cargo audit` / dependency policy)

---

## 6) Acceptance criteria

Phase 12 is complete when:

- CI workflow file is added using the command set above.
- Workflow runs on PR and push to `main`.
- Branch protection requires the quality gate check before merge.
- New PRs cannot merge while any of the four commands fail.
