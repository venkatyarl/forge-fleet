# Phase 12 — CI Bootstrap (Rust Quality Gates)

Date: 2026-04-04  
Repo: `/Users/venkat/projects/forge-fleet`

## What was added

- Workflow: `.github/workflows/rust-quality-gates.yml`
- Triggered on:
  - Pull requests
  - Pushes to `main`
- Deterministic baseline:
  - `ubuntu-24.04`
  - Rust toolchain `1.85.0`

## Quality gates enforced

The workflow runs four required CI jobs:

1. `cargo fmt --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo check --workspace`
4. `cargo test --workspace --lib`

## Enable branch protection (GitHub UI)

1. Push this workflow to the default branch (`main`).
2. Open GitHub → **Settings** → **Branches**.
3. Under **Branch protection rules**, click **Add rule**.
4. Branch name pattern: `main`.
5. Enable:
   - **Require a pull request before merging**
   - **Require status checks to pass before merging**
   - **Require branches to be up to date before merging**
   - (Recommended) **Dismiss stale pull request approvals when new commits are pushed**
   - (Recommended) **Include administrators**
6. In required checks, select all four jobs from this workflow:
   - `Rust Quality Gates / cargo fmt --check`
   - `Rust Quality Gates / cargo clippy --workspace -- -D warnings`
   - `Rust Quality Gates / cargo check --workspace`
   - `Rust Quality Gates / cargo test --workspace --lib`
7. Save the rule.

## Notes

- The check names only appear in the branch protection picker after the workflow has run at least once.
- This bootstrap intentionally avoids matrices/caches to keep behavior simple and reproducible.
