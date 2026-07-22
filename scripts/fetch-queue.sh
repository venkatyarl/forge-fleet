#!/usr/bin/env bash
set -euo pipefail

batch_size="${1:-${batch_size:-${BATCH_SIZE:-10}}}"

if [[ ! "$batch_size" =~ ^[0-9]+$ ]]; then
    echo "batch_size must be a non-negative integer" >&2
    exit 2
fi

# A large limit makes gh follow GitHub's paginated response instead of stopping
# after its default first page. The search qualifier keeps the resulting queue
# in creation order before jq filters out PRs whose checks did not pass.
gh pr list \
    --state open \
    --label queue/ready \
    --search 'sort:created-asc' \
    --limit 1000000 \
    --json number,checksConclusion \
    | jq -r --argjson batch_size "$batch_size" \
        '[.[] | select(.checksConclusion == "SUCCESS") | .number][:$batch_size][]'
