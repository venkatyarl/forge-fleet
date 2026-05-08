# v0.1 Internal Release Checklist

Use this checklist when cutting `v0.1.0-internal`.

## A) Pre-merge readiness

- [ ] `git fetch --all --prune`
- [ ] Release branch is up to date with `main`
- [ ] `cargo check --workspace` passes on release branch
- [ ] `cargo test --workspace --lib` passes on release branch
- [ ] `cargo run -p ff-cli -- --help` passes on release branch
- [ ] Known scaffold crates/non-goals are documented

## B) Merge execution

- [ ] `git checkout main && git pull --ff-only origin main`
- [ ] Merge release branch with `git merge --no-ff <release-branch>`
- [ ] Resolve conflicts (if any) and ensure no accidental file drops
- [ ] Push merged `main`

## C) Pre-tag gates (required)

- [ ] `cargo check --workspace`
- [ ] `cargo test --workspace --lib`
- [ ] `cargo run -p ff-cli -- --help`
- [ ] `git status --short` is empty
- [ ] `git log --oneline -n 5` matches expected release commits

## D) Tag and publish

- [ ] `git tag -a v0.1.0-internal -m "ForgeFleet Rust rewrite internal release v0.1.0"`
- [ ] `git push origin v0.1.0-internal`
- [ ] Confirm tag is visible on remote

## E) Post-tag monitoring + rollback trigger

- [ ] Run immediate smoke checks on tagged commit
- [ ] Watch for regressions in first validation window
- [ ] If regression appears: delete tag (if needed), revert merge commit, re-run full gates
- [ ] Create fix branch and retag only after all gates pass again
