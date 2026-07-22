#!/bin/sh

# Fetch CI-green PR queue
_BATCH_SIZE=100
_OUTPUT_FILE="scripts/fetch-queue.out"

# List open PRs with label `queue/ready` and passing CI checks
echo "Fetching CI-green PR queue..."
gh pr list --state open --json number,checksConclusion --filter "label:queue/ready" | while read -r line; do
    IFS=',' read -r pr_number checks_conclusion <<< "$line"
    if [ "$checks_conclusion" == "success" ]; then
        echo "$pr_number" >> "$OUTPUT_FILE"
        echo "$pr_number" $(date +%s) >> "$OUTPUT_FILE"
    fi
done

# Sort by creation time (earliest first)
echo "Sorting by creation time..."
sort -t' ' -k1,1 -k2,2 -k3,3 -k4,4 "$OUTPUT_FILE" | awk '{print $1}' | head -n "$_BATCH_SIZE" >> "$OUTPUT_FILE"
echo "Batched PRs:"
cat "$OUTPUT_FILE"
