#!/usr/bin/env bash
# pr-queue.sh — emit the queue of PR numbers to land on $BASE_BRANCH,
# oldest first, one per line. Used by scripts/build-train-branch.sh as the
# default source of PR numbers when none are passed on the command line.
#
# Requires the `gh` CLI to be authenticated against the target repo.
set -euo pipefail

BASE_BRANCH="${BASE_BRANCH:-main}"

if ! command -v gh >/dev/null 2>&1; then
    echo "pr-queue: 'gh' CLI not found; no PRs to queue" >&2
    exit 0
fi

gh pr list \
    --base "$BASE_BRANCH" \
    --state open \
    --json number,createdAt \
    --jq 'sort_by(.createdAt) | .[].number'
