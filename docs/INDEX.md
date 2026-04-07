# ForgeFleet Docs Index

This repo now uses a split documentation model:

- **Active docs** = current operational truth
- **Archive docs** = migration history, phase records, and superseded rollout materials

---

## Active canonical docs

### Start here
- [../README.md](../README.md) — project overview, install, configure, run
- [FINAL_COMPLETION_CHECKLIST.md](./FINAL_COMPLETION_CHECKLIST.md) — official completion/deletion/archive gates
- [CONSOLIDATED_PARITY_AND_CUTOVER.md](./CONSOLIDATED_PARITY_AND_CUTOVER.md) — cutover decision framework

### Parity + legacy decisions
- [PYTHON_FORGEFLEET_PARITY_AUDIT.md](./PYTHON_FORGEFLEET_PARITY_AUDIT.md)
- [MISSION_CONTROL_PARITY_AUDIT.md](./MISSION_CONTROL_PARITY_AUDIT.md)
- [DELETE_OR_ARCHIVE_RECOMMENDATION.md](./DELETE_OR_ARCHIVE_RECOMMENDATION.md)

### Operational docs
- [FINAL_STATUS.md](./FINAL_STATUS.md)
- [FLEET_BRINGUP_PLAYBOOK.md](./FLEET_BRINGUP_PLAYBOOK.md)
- [POSTGRES_RUNTIME_MODE.md](./POSTGRES_RUNTIME_MODE.md)
- [PHASE_38A_SQLITE_POSTGRES_INVENTORY.md](./PHASE_38A_SQLITE_POSTGRES_INVENTORY.md)
- [checklists/POSTGRES_FULL_CUTOVER_CHECKLIST.md](./checklists/POSTGRES_FULL_CUTOVER_CHECKLIST.md)
- [PORTFOLIO_LAYER.md](./PORTFOLIO_LAYER.md)

---

## Archive docs

Historical migration and phase-by-phase materials are now archived at:

- [archive/2026-04-migration-history/](./archive/2026-04-migration-history/)

These docs are retained for:
- audit trail
- migration history
- release governance evidence
- rollback/reconstruction context

They are **not** the primary operational source of truth anymore.

---

## Retention policy

- Archive docs remain in-repo until legacy deletion and final cutover are complete.
- After full cutover, archive docs may be reduced or exported, but should remain available at least through the first stable post-cutover period.
- Canonical owner: ForgeFleet maintainers / ops.

---

## Current decision snapshot

As of now:
- delete `forge-fleet-py-legacy` → **NO-GO**
- delete `mission-control-legacy` → **NO-GO**
- archive migration docs → **DONE (moved to archive path)**
- declare ForgeFleet complete → **NO-GO**

Use [FINAL_COMPLETION_CHECKLIST.md](./FINAL_COMPLETION_CHECKLIST.md) for the authoritative gate status.
