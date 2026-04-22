#!/usr/bin/env bash
# Research-subsystem benchmark harness.
#
# Runs N research queries through `ff research`, captures each session's
# metrics (duration, sub-agents succeeded, total tokens), writes the
# markdown to a per-query file, and appends a summary row to a CSV.
#
# Usage:
#   scripts/research-benchmark.sh [OUTDIR]
# Default OUTDIR: ~/.forgefleet/research-benchmarks/<timestamp>
set -euo pipefail

OUTDIR="${1:-$HOME/.forgefleet/research-benchmarks/$(date +%Y%m%dT%H%M%S)}"
mkdir -p "$OUTDIR"
CSV="$OUTDIR/results.csv"
echo "id,duration_ms,subtasks_total,subtasks_succeeded,session_id,query" > "$CSV"

# Benchmark prompts — 10 queries covering different research difficulty profiles.
# Target mix:
#   - 3 parallel-web-research (Q1, Q2, Q3)
#   - 2 code-grounded (Q4, Q5)
#   - 2 multi-constraint synthesis (Q6, Q7)
#   - 2 factual knowledge questions — control for "does it get easy ones right" (Q8, Q9)
#   - 1 strategic/open-ended (Q10)
declare -a PROMPTS=(
  "What are the 5 most novel ideas in the Qwen3-Omni paper, how do they relate to the prior MiniCPM-o 2.6 work, and which of them would be feasible to implement on DGX Spark hardware?"
  "Survey every Rust crate that does 'multi-node LLM inference orchestration' — compare their architectures, licensing, GPU support matrix, and where ForgeFleet fits on that landscape."
  "What's the current state of the art for tensor-parallel LLM inference on ARM64 + Blackwell GB10? Summarize NVIDIA's official path, the community patches, and the known blockers for running Qwen3-235B."
  "Audit the ForgeFleet codebase at github.com/venkatyarl/forge-fleet for places where a distributed-systems expert would see a consistency bug. Start with leader_tick, pulse_materializer, and computer_software drift tracking."
  "Given our fleet has 4 DGX Sparks + 4 EVO-X2 + 2 Mac minis + 1 Mac Studio M3 Ultra, design the optimal model portfolio (which model on which box, why, fallback plan) for a 'Claude Sonnet tier on every query' goal."
  "Compare MiniMax-M1 (456B MoE, 1M context) vs DeepSeek-V3-0324 (671B MoE, 128K) vs Qwen3-235B-A22B for practical agent workloads. Weights: tool-use fidelity, coder benchmarks, license, inference cost. Which would you deploy on a 4-Spark cluster for a coding-agent fleet?"
  "What are the tradeoffs between Raft, Pulse-v2-style (Redis TTL + Postgres singleton + epoch fencing), and Sentinel for leader election in a 10-14 node hybrid fleet? Under what failure modes does each degrade gracefully vs catastrophically?"
  "What's the ConnectX-7 400Gbps NIC's maximum sustained throughput for NCCL all-reduce on 8B-parameter tensors between two DGX Spark boxes? Cite Mellanox/NVIDIA technical docs + any benchmarks published."
  "When was the Apache Iceberg v3 spec released, what are its three most significant changes from v2, and which data warehouses have shipped v3 read support as of April 2026?"
  "If the goal is 'a fleet of LLMs that can research anything better than Claude Code does', what are the 5 key capabilities that are missing today, and what's the fastest path to each one? Consider: tool diversity, planning depth, cross-verification, memory, and benchmarking."
)

IDS=(q1 q2 q3 q4 q5 q6 q7 q8 q9 q10)

echo "▶ Running ${#PROMPTS[@]} benchmark queries into $OUTDIR"
echo

for i in "${!PROMPTS[@]}"; do
  id="${IDS[$i]}"
  prompt="${PROMPTS[$i]}"
  log="$OUTDIR/$id.log"
  md="$OUTDIR/$id.md"

  echo "─── $id ────────────────────────────────────────"
  echo "Q: ${prompt:0:100}..."
  start_ms=$(python3 -c 'import time; print(int(time.time()*1000))')

  # Run ff research. Capture stderr to log, stdout to md.
  # Each query gets 5 parallel sub-agents, depth 6 turns.
  set +e
  ~/.local/bin/ff research "$prompt" \
    --parallel 5 \
    --depth 6 \
    --output "$md" \
    2> "$log" > /dev/null
  exit_code=$?
  set -e

  end_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
  duration=$((end_ms - start_ms))

  # Extract counters from the log (looks for the "research complete" line).
  summary_line=$(grep "research complete" "$log" | head -1 || true)
  succeeded=$(echo "$summary_line" | grep -oE '[0-9]+/[0-9]+' | head -1 | cut -d/ -f1 || echo "0")
  total=$(echo "$summary_line" | grep -oE '[0-9]+/[0-9]+' | head -1 | cut -d/ -f2 || echo "0")
  session_id=$(grep "session " "$log" | head -1 | awk '{print $NF}' || echo "unknown")

  # Escape the prompt for CSV (commas + quotes).
  csv_prompt=$(echo "$prompt" | sed 's/"/""/g')
  echo "$id,$duration,$total,$succeeded,$session_id,\"$csv_prompt\"" >> "$CSV"

  if [ "$exit_code" -eq 0 ]; then
    echo "  ✓ completed in ${duration}ms · $succeeded/$total sub-agents · session $session_id"
  else
    echo "  ✗ failed with exit $exit_code · see $log"
  fi
  echo
done

echo "▶ Benchmark complete. Results in $OUTDIR"
echo "  CSV summary: $CSV"
