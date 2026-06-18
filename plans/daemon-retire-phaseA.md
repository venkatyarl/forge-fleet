# Decision 2 — Legacy `ff daemon` Retirement, Phase A scope

Synthesized from codex (`/tmp/ff_daemon_delta_codex.md`) + kimi (`/tmp/ff_daemon_delta_kimi.md`), 2026-06-18. Both agree, and both flagged: **Phase A is bigger than the consensus first scoped (14 legacy-only ticks, not 4), and each needs a port / drop / subsumed decision — not a blind port.**

## Already DUPLICATE in forgefleetd (the racing set — just stop legacy actuating; no port)
scheduler · defer-worker (shell/http/upgrade) · disk sampler · deployment reconciler · stale-job sweeper · version-check · mesh-refresh · software auto-upgrade · model-upstream · model-scout · external-tools-upstream · sub-agent reaper · wave reaper. → These are the actual conflict #2 race; Phase B (gate legacy actuation off) fixes them wholesale.

## SUBSUMED — no port needed (legacy-only but already covered)
- **software_upstream** → folded into production `AutoUpgradeTick::run_once` (auto_upgrade.rs:735).
- **local_healer** (restarts forgefleetd) → once legacy `ff daemon` is gone, **systemd/launchd** restarts forgefleetd; the in-process healer is redundant (verify the unit has `Restart=`).
- **coverage-guard auto-load** → production deliberately runs `report_once` (read-only); the review's stance is the **autoscaler** is the actuator, not coverage-guard. Keep OFF (do not port the actuating path).

## PORT — genuinely useful, legacy-only, not elsewhere (recommend KEEP)
| tick | why keep | target in forgefleetd |
|---|---|---|
| defer kinds **`internal`** + **`mesh_retry`** (+ auto_upgrade/external_tool finalizers) | else these deferred tasks **strand** — clear must-do | `ff_agent::defer_worker` |
| **model library scan** | keeps `fleet_model_library` accurate per node | per-node tick |
| **SSH mesh auto-repair** | self-heals dead mesh pairs (attempts≥3) | leader-gated tick |
| **fleet task liveness / circuit-breaker / notify** | the watchdog that kills stuck tasks (Priya-hang class) | per-node tick |
| **node_online wake** | immediate defer drain on node-online (vs poll latency) | defer_worker subscriber |
| **model auto-upgrade download** (revision_available) | keeps model files current | leader-gated tick |

## JUDGMENT CALLS — port or drop? (operator nod before I delete a feature)
| tick | question |
|---|---|
| **brain vault re-index** (`index_vault` of Yarli_KnowledgeBase) | still want the legacy vault index, or is cortex/vault-sync enough? |
| **project GitHub sync** | still used? (no production equivalent) |
| **OAuth token probe** | still want periodic OAuth endpoint probing? |
| **placement rebalance** (cold-library move on >80% disk) | keep (port) or rely on the gated disk-policy reconciler? |
| **LAN link probe** (rsync throughput benchmark) | likely **vestigial** → drop? |
| **fabric benchmark sweep** (daily) | likely **vestigial** → drop? |

## Recommended execution
1. **Phase A1 (clear KEEP, low-risk, start now):** port defer kinds `internal`/`mesh_retry` + finalizers into `defer_worker`. Bounded, definitely needed.
2. **Phase A2:** port the other KEEP ticks (model-scan, mesh-repair, task-liveness, node_online, model-auto-upgrade) — one batched PR, leader-gate per the table.
3. **Operator triage** the 6 judgment-call ticks (port vs drop).
4. **Phase B (separate PR):** gate legacy actuation off by default (`FF_DAEMON_ACTUATING` env), default = deprecation warning + `--once` dry-run. The existing legacy-daemon reaper (src/main.rs:1113) already SIGTERMs stale procs.
5. **Phase C:** after a clean rollout window, delete the actuating code.
