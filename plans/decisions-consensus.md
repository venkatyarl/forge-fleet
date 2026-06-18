# 3-Way Consensus — the 3 unblocking decisions

Sources: Claude + codex (`/tmp/ff_consensus_codex.md`) + kimi (`/tmp/ff_consensus_kimi.md`), 2026-06-18, all grounded in `plans/deep-review-findings.md` + the real code.

## Decision 1 — Gate-TTL 3-state restore  →  **UNANIMOUS**
All three independently picked the same design: **snapshot the prior value, restore *that* on expiry.**
- Add `previous_value` column to `fleet_secrets` (migration).
- `pg_disable_safety_gate`: stash current `value` → `previous_value` in-txn, then write the disabled value (`false` for booleans, `off` for mode gates).
- New TTL-aware reader that restores `previous_value` when falsy + expired (works for bool *and* mode).
- **Extract one shared `GateMode` enum + `read_gate()`**; route all 5 mode gates (autoscaler/arbiter/rollout/disk/conformance) through it — *also kills the 5-identical-enum duplication (conflict #9 + the dup) in the same change.*
- Safety: clear `previous_value` on a normal non-TTL `ff secrets set` (kimi); do **not** restore prior mode for pre-existing `value=false` rows (codex). Test: disable `active`, advance past TTL, assert restores to `active`.
- **Size: S–M · Risk: low** (additive). **→ Execute as-is.**

## Decision 2 — Retire legacy `ff daemon` ticks  →  **UNANIMOUS (phased)**
Both: **make legacy non-actuating now (behind a flag), delete after a rollout window — but move the legacy-only ticks into `forgefleetd` FIRST.**
- **Phase A (prereq):** port the ticks that live *only* in the legacy daemon into `forgefleetd`: defer kinds `internal`/`mesh_retry` → `defer_worker` (codex); model local-scan, model-revision auto-upgrade, SSH mesh-repair, placement rebalancer → leader-gated ticks (kimi). *Without this, retiring legacy strands those tasks.*
- **Phase B:** gate legacy actuation **off by default** (env flag e.g. `FF_DAEMON_ACTUATING=1` to opt in); default = deprecation warning + no mutating ticks + `--once` dry-run. (The existing legacy-daemon reaper at `src/main.rs:1113` already SIGTERMs stale `ff daemon` procs.)
- **Phase C:** update deploy units (systemd/launchd) to launch `forgefleetd`, not `ff daemon`; add a `legacy_daemon_running` alert; confirm all nodes on Pulse v2/Postgres (the scheduler lives in `start_pulse_v2_subsystems`).
- **Phase D:** after a clean rollout window, delete the actuating code.
- **Size: M · Risk: medium** (delicate) — mitigated by the phased rollout + the existing reaper. **→ Execute phased; Phase A first.**

## Decision 3 — `ff-mc` SQLite→Postgres  →  **FORK, resolved**
- **codex:** build *real relational* Postgres MC tables + `McStore` trait + importer (proper schema, enforced joins).
- **kimi:** route `ff-mc` through the *existing* `ff_db::OperationalStore` (config_kv, already mounted in production via `mc_router_operational`) in all modes, close the parity-gap stubs, drop SQLite — **do not** do a relational rewrite now.
- **My resolution (consensus):** **kimi's path now, codex's deferred.** Production *already* uses `OperationalStore` for MC in Postgres mode — SQLite only survives in embedded/dev. So the low-risk fix that actually removes the discrepancy is: close the `operational_api.rs` stubs (velocity/overdue/legal/counsel/timer/…), add a one-shot `ff mc migrate --from mission-control.db` importer, then delete the SQLite backend (`db.rs`, `rusqlite`, always mount `mc_router_operational`). **Defer** codex's full relational schema until config_kv query/constraint needs justify it (kimi's own step 5).
- **Size: M · Risk: low–medium.** **→ Execute kimi's path; relational schema = documented future option.**

## Execution plan (improved mode: distributed + batched)
1. **Gate-TTL** (unanimous, cleanest) — one batched PR (migration + shared `GateMode` + 5 readers). *I take this — intricate, single-threaded.*
2. **ff-mc → OperationalStore** — parallelizable: dispatch stub-closing to codex/kimi, importer + SQLite-drop integrated by me.
3. **Retire legacy daemon** — phased; Phase A (move ticks) first, carefully. *I take this — delicate.*

Operator already mandated "you, codex and kimi reach consensus and do that" → proceeding to execute. Surfacing this doc so the call is visible and reversible.
