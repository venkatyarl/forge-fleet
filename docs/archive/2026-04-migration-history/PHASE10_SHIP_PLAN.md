# Phase 10 Ship Plan (v0.1 internal)

Goal: merge Phase 10 work into `main`, verify release gates, and cut `v0.1.0-internal` safely.

## 1) Ordered merge + verification steps

1. **Prepare local main**
   - `git fetch --all --prune`
   - `git checkout main`
   - `git pull --ff-only origin main`
2. **Sanity-check release branch before merge** (example: `release/phase10-ship-prep`)
   - `git checkout release/phase10-ship-prep`
   - `cargo check --workspace`
   - `cargo test --workspace --lib`
   - `cargo run -p ff-cli -- --help`
3. **Merge to main**
   - `git checkout main`
   - `git merge --no-ff release/phase10-ship-prep`
4. **Post-merge verification on main**
   - Re-run the same three cargo commands on `main`.
   - Confirm working tree is clean (`git status --short` returns empty).
5. **Push merged main**
   - `git push origin main`
6. **Tag and push tag** (only after all checks pass)
   - `git tag -a v0.1.0-internal -m "ForgeFleet Rust rewrite internal release v0.1.0"`
   - `git push origin v0.1.0-internal`

## 2) Required commands before tag

Run from repo root on merged `main` and require all to pass:

```bash
cargo check --workspace
cargo test --workspace --lib
cargo run -p ff-cli -- --help
git status --short
git log --oneline -n 5
```

Tag only if:
- cargo commands exit successfully,
- `git status --short` is empty,
- latest commits match expected Phase 10 release content.

## 3) Rollback plan (post-merge regression)

If regression appears after merge or after tag:

1. **Stop forward changes**
   - Freeze additional merges into `main` until triage completes.
2. **Identify bad commit range**
   - `git log --oneline --decorate -n 20`
3. **If tag already pushed and must be withdrawn**
   - `git tag -d v0.1.0-internal`
   - `git push --delete origin v0.1.0-internal`
4. **Revert merge commit on main (preferred for auditability)**
   - `git revert -m 1 <merge_commit_sha>`
   - `git push origin main`
5. **Re-run full verification gates**
   - `cargo check --workspace`
   - `cargo test --workspace --lib`
   - `cargo run -p ff-cli -- --help`
6. **Patch forward on new branch**
   - Create a fix branch from repaired `main`, implement fix, repeat full ship plan.

Notes:
- Prefer `git revert` over force-push for shared branch safety.
- If regression is minor and fix is fast, patch-forward can replace full rollback; still re-run all gates before retagging.
