# ForgeFleet deploy behavior

ForgeFleet deploys sync the local source tree to `origin/main` before building.
When the working tree has local modifications, the deploy process now **preserves
them in a git stash** instead of discarding them with a hard reset.

## What changed

Previously, a deploy on a dirty tree would run `git reset --hard origin/main`,
which silently lost any in-progress changes. The deploy path now checks for
tracked modifications first and stashes them before resetting.

## Dirty-tree handling

1. Before `git reset --hard origin/main`, the deploy helper checks whether the
   working tree has tracked changes.
2. If it does, it pushes a labeled stash:

   ```bash
   git stash push -m "ff-deploy-dirty-guard-<timestamp>"
   ```

3. The deploy then fetches `origin` and resets to `origin/main` as usual.

The stash is left in place so an operator can inspect or restore it later with
`git stash list` and `git stash pop`.

## What counts as dirty

Only **tracked** modifications count. Untracked files (for example,
`research/`, `graphify-out/`, or other operator artifacts) are ignored so they
do not block a deploy that builds from tracked sources.

## Where this is implemented

- `crates/ff-deploy/src/lib.rs` — `git_fetch_and_reset_hard`,
  `git_tree_is_dirty`, and `git_stash_dirty_tree`.
- The leader-local guard in `ff-terminal/src/fleet_cmd.rs` uses the same rule.

## Recovering a deploy stash

To see stashes created by deploys:

```bash
git stash list
```

To restore the most recent one:

```bash
git stash pop stash@{0}
```

Review the stash before popping if you are unsure whether it conflicts with the
new `origin/main` state.
