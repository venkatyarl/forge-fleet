#!/usr/bin/env bash
# build-train-branch.sh — build a train branch by squashing a queue of PRs
# onto $BASE_BRANCH, one commit per PR, then push it.
#
# Usage:
#   scripts/build-train-branch.sh [PR_NUMBER...]
#
# With no arguments, PR numbers are read from the queue script
# ($PR_QUEUE_SCRIPT, default scripts/pr-queue.sh), one per line.
#
# Env overrides:
#   BASE_BRANCH       base branch to build from (default: main)
#   REMOTE            git remote to fetch/push (default: origin)
#   PR_QUEUE_SCRIPT   path to the queue script (default: scripts/pr-queue.sh)
#   TIMESTAMP         override the batch timestamp (default: UTC now)
#
# Fails immediately (non-zero exit, aborted merge) on the first PR that
# does not squash-merge cleanly. On success, prints the train branch ref
# to stdout; all progress logging goes to stderr.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

BASE_BRANCH="${BASE_BRANCH:-main}"
REMOTE="${REMOTE:-origin}"
PR_QUEUE_SCRIPT="${PR_QUEUE_SCRIPT:-$SCRIPT_DIR/pr-queue.sh}"
TIMESTAMP="${TIMESTAMP:-$(date -u +%Y%m%d%H%M%S)}"
TRAIN_BRANCH="train/batch-${TIMESTAMP}"

log() { printf '[build-train-branch] %s\n' "$*" >&2; }

pr_numbers=()
if [[ $# -gt 0 ]]; then
    pr_numbers=("$@")
else
    if [[ ! -x "$PR_QUEUE_SCRIPT" ]]; then
        log "no PR numbers given and queue script is missing/not executable: $PR_QUEUE_SCRIPT"
        exit 1
    fi
    while IFS= read -r line; do
        [[ -n "$line" ]] && pr_numbers+=("$line")
    done < <("$PR_QUEUE_SCRIPT")
fi

if [[ ${#pr_numbers[@]} -eq 0 ]]; then
    log "PR queue is empty — nothing to build"
    exit 1
fi

for pr in "${pr_numbers[@]}"; do
    [[ "$pr" =~ ^[0-9]+$ ]] || { log "invalid PR number in queue: '$pr'"; exit 1; }
done

log "Queued PRs: ${pr_numbers[*]}"

git fetch "$REMOTE" "$BASE_BRANCH"
git checkout "$BASE_BRANCH"
git reset --hard "$REMOTE/$BASE_BRANCH"
git checkout -b "$TRAIN_BRANCH"

for pr in "${pr_numbers[@]}"; do
    pr_head="pr-${pr}-head"

    log "Fetching PR #${pr}"
    git fetch "$REMOTE" "pull/${pr}/head:${pr_head}"

    log "Squash-merging PR #${pr}"
    if ! git merge --squash "$pr_head"; then
        log "merge conflict on PR #${pr} — aborting train build"
        # `--squash` never sets MERGE_HEAD, so `merge --abort` doesn't apply —
        # reset the index/worktree back to the last good commit instead.
        git reset --hard HEAD || true
        git branch -D "$pr_head" >/dev/null 2>&1 || true
        exit 1
    fi

    git commit --allow-empty -m "squash PR #${pr}"
    git branch -D "$pr_head" >/dev/null 2>&1 || true
done

git push "$REMOTE" "$TRAIN_BRANCH"

log "train branch ready: ${TRAIN_BRANCH}"
echo "refs/heads/${TRAIN_BRANCH}"
