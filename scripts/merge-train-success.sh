#!/usr/bin/env bash
# merge-train-success.sh — land a validated train batch. Run after CI succeeds
# on a train branch built by scripts/build-train-branch.sh: squash-merges every
# queued PR, annotates each PR description with the train merge commit, and
# posts a success comment on each queued PR.
#
# Usage:
#   scripts/merge-train-success.sh [PR_NUMBER...]
#
# With no arguments, PR numbers are read from the queue script
# ($PR_QUEUE_SCRIPT, default scripts/pr-queue.sh), one per line.
#
# Env overrides:
#   TRAIN_MERGE_COMMIT   commit the train batch landed as (default: HEAD)
#   TRAIN_BRANCH         train branch name for annotations (default: current branch)
#   PR_QUEUE_SCRIPT      path to the queue script (default: scripts/pr-queue.sh)
#
# Requires the `gh` CLI to be authenticated against the target repo. Keeps
# going past a PR that fails to merge/annotate and exits non-zero at the end
# if any PR failed; all progress logging goes to stderr.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

PR_QUEUE_SCRIPT="${PR_QUEUE_SCRIPT:-$SCRIPT_DIR/pr-queue.sh}"
TRAIN_MERGE_COMMIT="${TRAIN_MERGE_COMMIT:-$(git rev-parse HEAD)}"
TRAIN_BRANCH="${TRAIN_BRANCH:-$(git rev-parse --abbrev-ref HEAD)}"

log() { printf '[merge-train-success] %s\n' "$*" >&2; }

if ! command -v gh >/dev/null 2>&1; then
    log "'gh' CLI not found — cannot merge or annotate PRs"
    exit 1
fi

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
    log "PR queue is empty — nothing to merge"
    exit 1
fi

for pr in "${pr_numbers[@]}"; do
    [[ "$pr" =~ ^[0-9]+$ ]] || { log "invalid PR number in queue: '$pr'"; exit 1; }
done

log "Train ${TRAIN_BRANCH} passed CI at ${TRAIN_MERGE_COMMIT}; landing PRs: ${pr_numbers[*]}"

annotation="Train-merged: ${TRAIN_MERGE_COMMIT} (train branch \`${TRAIN_BRANCH}\`)"
success_comment="✅ Train batch \`${TRAIN_BRANCH}\` passed CI and this PR was merged as part of train merge commit ${TRAIN_MERGE_COMMIT}."

failed_prs=()
for pr in "${pr_numbers[@]}"; do
    log "Merging PR #${pr}"
    if ! gh pr merge "$pr" --squash --delete-branch; then
        log "failed to merge PR #${pr} — continuing with remaining PRs"
        failed_prs+=("$pr")
        continue
    fi

    log "Annotating PR #${pr} description with train merge commit"
    if ! body="$(gh pr view "$pr" --json body --jq '.body')"; then
        log "failed to read PR #${pr} description — continuing"
        failed_prs+=("$pr")
        continue
    fi
    if [[ "$body" != *"$annotation"* ]]; then
        if ! gh pr edit "$pr" --body "${body}"$'\n\n'"${annotation}"; then
            log "failed to update PR #${pr} description — continuing"
            failed_prs+=("$pr")
            continue
        fi
    fi

    log "Posting success comment on PR #${pr}"
    if ! gh pr comment "$pr" --body "$success_comment"; then
        log "failed to comment on PR #${pr} — continuing"
        failed_prs+=("$pr")
        continue
    fi
done

if [[ ${#failed_prs[@]} -gt 0 ]]; then
    log "train landed with failures on PR(s): ${failed_prs[*]}"
    exit 1
fi

log "train batch landed: ${#pr_numbers[@]} PR(s) merged and annotated"
