# Consolidated Parity + Cutover Plan

Date: 2026-04-05

Inputs:
- `docs/PYTHON_FORGEFLEET_PARITY_AUDIT.md`
- `docs/MISSION_CONTROL_PARITY_AUDIT.md`
- `docs/DELETE_OR_ARCHIVE_RECOMMENDATION.md`

## Executive decision

**Delete old projects now: NO-GO.**

We start dismantling in a controlled way:
1. Freeze legacy repos
2. Port P0 parity gaps into Rust ForgeFleet
3. Re-verify parity
4. Archive legacy repos
5. Rename Rust repo to canonical `forge-fleet`

## Why NO-GO for immediate deletion

From audits:
- Mission Control route surface is materially larger than current Rust equivalent.
- Python ForgeFleet behavioral parity is partial in critical runtime flows.
- Main missing areas are integration behavior, not just module count.

## P0 gaps to close before deletion

1. `fleet_crew` must execute full crew flow (not planning-only)
2. Root daemon must run autonomous execution loop (claim → execute → report)
3. Ownership/lease/handoff tracking parity (persistent model + APIs)
4. Mission workflow parity (review/dependency/task-group operational paths)
5. MC → Rust migration coverage for core domains beyond current subset

## P1 after P0

- MCP federation client + topology validation parity
- Evolution/updater full runtime wiring in daemon path
- CLI control-plane commands with real side effects
- Node self-heal automation parity

## Dismantle sequence (safe)

### Phase A (now)
- Mark legacy repos frozen (read-only policy + migration note)
- Block net-new feature work on legacy repos
- Continue bugfix-only if required for production safety

### Phase B (after P0 complete)
- Run parity verification checklist
- Run live cutover drills (router, MC workflows, failover, replication)

### Phase C (archive)
- Archive snapshots:
  - `forge-fleet` → `forge-fleet-py-legacy`
  - `mission-control` → `mission-control-legacy`
- Keep archives read-only

### Phase D (canonical rename)
- Rename `forge-fleet-rs` → `forge-fleet`
- Update services/scripts/docs paths

## Current state

- Rust ForgeFleet is platform-strong and production-adjacent
- Legacy repos still hold behavior that is not fully ported
- Dismantling has started in governance/freeze mode; deletion deferred pending P0 completion

## Canonical execution checklist

- Final operational go/no-go ledger: `docs/FINAL_COMPLETION_CHECKLIST.md`
