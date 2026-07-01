# ForgeFleet: Single-Leader → Postgres Peer-to-Peer (ff council + research, 2026-07-01)

## Verdict (codex + kimi unanimous; matches 2026 industry patterns)
Drop the single-leader/worker model. Make **every forgefleetd a peer** that
claims + schedules + reconciles work via `FOR UPDATE SKIP LOCKED` (already used
for atomic claims). Keep only **narrow singleton DB leases** for genuinely
serialized global jobs. **Do NOT build Raft** — Postgres is already the
coordination substrate. **Do NOT split into subprocesses.**

## Why (risks of today's single-leader)
- Leader wedge stalls the ENTIRE control plane (scheduling, autoscale, merge,
  reindex, upgrades).
- Leader is a throughput/latency bottleneck under high task churn.
- Leader-only responsibilities accumulate unrelated failure modes.
- **Self-upgrade exclusion = permanent manual care + drift** — the exact
  `leader_self_upgrade` gate pain. In a peer model taylor is a normal node, so
  the gate + its "self-suicide" problem DISAPPEAR.
- Failover only treats symptoms; the shape stays fragile.

## Key architectural separation (from research)
Task-ORCHESTRATION (planner → workers, hierarchical decomposition — the
`ff pm decompose` flow) is SEPARABLE from infra-LEADERSHIP (who runs the
scheduler). Keep hierarchical task decomposition; make the INFRASTRUCTURE
leaderless. Industry: orchestrator-worker ~70% of prod for task routing; swarm
(leaderless peer claims from shared store) is exactly right for high-churn
atomic dispatch — which is ff's.

## Target model
- Nodes are peers; no elected leader for the common path.
- Work claimed atomically from Postgres (SKIP LOCKED, in a txn).
- Leases carry `lease_owner`, `lease_expires_at`, `heartbeat_at`,
  `attempt_count`, `generation` (fencing token).
- Abandoned work auto-reclaimed by any peer.
- Work-stealing + shard-affinity as a PERFORMANCE layer (not the correctness
  model).
- Actor/supervision LOCAL per daemon (slot lifecycle, restarts, heartbeats).
- Singleton Postgres leases ONLY where exactly-one-planner is genuinely required
  (merge-drain coordination, full cortex reindex planning, schema migration).

## Migration path (incremental — taylor stays fallback throughout)
1. Classify leader-ticks: {per-work schedulers · per-node reconcilers ·
   singleton-global · maintenance}. (Same classification as the tick-registry
   work — they MERGE.)
2. Standardize lease fields across claim tables.
3. Move work_item scheduling to SKIP-LOCKED claims first.
4. Run all-node schedulers in SHADOW/limited mode while taylor remains fallback.
5. Convert global ticks to singleton leases with TTLs.
6. Add local actor supervision per daemon.
7. Remove leader special-casing → taylor becomes a normal peer.

## Relationship to the tick registry (plans/daemon-tick-registry.md)
Same effort. The registry's tick classification IS migration step 1; the
registry's "one cached leader gate" becomes "singleton lease where serialization
is genuinely required, peer-claim everywhere else." Build the registry as the
VEHICLE for the peer-to-peer migration.

## Open question for a follow-up council
The exact singleton-lease boundary: enumerate which specific jobs truly need
exactly-one-planner vs which are safe to run on every peer.

Sources: LangChain multi-agent architecture; Azure AI agent orchestration
patterns; 2026 multi-agent orchestration surveys (supervisor/orchestrator-worker/
swarm topologies).
