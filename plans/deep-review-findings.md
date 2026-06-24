I have all the verified findings and question answers. This is a synthesis task — I'll produce the decision-ready report directly. No further code exploration is needed since every claim is already grounded with verified locations.

# ForgeFleet Deep Review — Synthesis

## 1. Top conflicts (systems whose logic fights each other)

Ranked by severity. The dominant theme: **two daemons (legacy `ff daemon` / `daemon_cmd.rs` and production `forgefleetd` / `src/main.rs`) both spawn the same ticks.** If both run on a node, they race.

| # | Conflict | Real locations | One-line fix |
|---|---|---|---|
| 1 | **Arbiter is a no-op stub** — reserves hosts and advances cursors but never actually offloads/reloads models; the task-fence blocks claims on reserved hosts that are never drained. | `ff-agent/src/arbiter.rs:423-449` (`execute_step` shim) | Wire `execute_step` to real `ff offload` / `model_runtime::load_model`; until then keep arbiter_mode≠active. |
| 2 | **Dual-daemon racing on every shared tick** — defer-worker, scheduler, autoscaler-vs-CoverageGuard, AutoUpgradeTick, wave-reaper, mesh-refresh all exist in both daemons. | `daemon_cmd.rs:1309` (CoverageGuard actuating) vs `portfolio_maintenance.rs:214` (report-only); defer: `defer_worker.rs:72`+`src/main.rs:751` vs `daemon_cmd.rs:896`; auto-upgrade: `src/main.rs:3169` vs `daemon_cmd.rs:1318` | Retire the legacy `ff daemon` actuating ticks (make it read-only or delete); single-source every tick in `forgefleetd`. **UNBLOCKS most other conflicts.** |
| 3 | **Two shell executors with incompatible kill/timeout semantics** claim the same `deferred_tasks` rows — `process_group(0)`+group-kill / 2h cap vs `start_kill()` (orphans grandchildren) / 30m cap + `setsid`. Identical tasks behave differently by which wins the claim. | `defer_worker.rs:305-366` vs `daemon_cmd.rs:553,691` | Delete `daemon_cmd::execute_shell`; route all defer execution through `defer_worker`. |
| 4 | **Two leader-election engines run concurrently** — discovery/TOML election (`registry.set_leader`) vs DB `fleet_leader_state` (`LeaderTick`). They never reconcile; the HTTP `/api/fleet/leader` path only refreshes heartbeat, can't install a new leader. Stale winner leaks into gateway status as `leader_hint`. | `src/main.rs:1863` + `1859-1860`; `leader_tick.rs:98`; `pulse_api.rs:305`; `server.rs:2440` | Make DB `LeaderTick` authoritative; have it call `registry.set_leader` on transition, or delete the discovery election. |
| 5 | **Build-pause vs autoscaler placement** — a 64/96GB host running a wave build is fully eligible for autoscaler model loads (the `<=40GB` exclusion protects only small hosts; wave uses `excludes_computer_ids`, not `computers.reservation_state` which the autoscaler reads). | `autoscaler.rs:score_host:468` vs `task_runner.rs:compose_fleet_upgrade_wave_filtered` | Have the wave set `computers.reservation_state` on build targets so `score_host` gates them out. |
| 6 | **Reconciler vs build-pause crash window** — `pause_local_models_for_build` deletes `retired` deployments; if the SSH session is SIGKILL'd mid-build the resume never fires and reconciler pass B permanently deletes the row. | `model_runtime.rs:pause_local_models_for_build`/`unload_model:644`; `deployment_reconciler.rs:reconcile_local:270` | Snapshot paused models to a durable table; resume on next reconcile if the build task is dead. |
| 7 | **gemma-4 tool-capable disagreement** — `inference_router.rs:550` says gemma can't tool-call; `helpers.rs:133` marks gemma-4 tool-capable. Fallback chain reaches `detect_llm_from_db_or_local` → returns a gemma-4 URL → silent agent hang. | `inference_router.rs:model_supports_tools` vs `helpers.rs:detect_llm_from_db_or_local`; `main.rs:3605` | Single source of truth for `model_supports_tools`; make helpers import it. |
| 8 | **CLI bridge port swap** — `BACKENDS` puts gemini@51102/kimi@51103; schema seeds kimi@51102/gemini@51103. Requests cross-wire. | `cli_executor.rs:82-134`; `cli_bridge.rs:37-54`; `schema.rs:4774-4785` | Align the enumerate order in `cli_bridge.rs` with the DB seed (or seed from BACKENDS). |
| 9 | **disable-gate TTL ignored by every gate but auto-upgrade** — `ff secrets disable-gate` writes `expires_at`; only `auto_upgrade::is_enabled` reads it (`pg_read_safety_gate`). The other 7 gates call `pg_get_secret` (value only) → gate stays disabled forever after TTL, contradicting the CLI's printed promise. | `queries.rs:pg_get_secret:1801` vs `pg_read_safety_gate:1861`; `secrets_cmd.rs:159`; 7× `read_mode` | Route all gate reads through `pg_read_safety_gate`. |
| 10 | **Web update controls are ghosts** — dashboard pause/resume/abort mutate in-memory `UpdateRolloutState`; `upgrade_rollout.rs` reads Postgres `upgrade_rollouts` and ignores them. Operator "pause" does nothing. | `server.rs:6132-6174`; `upgrade_rollout.rs:212` | Back the gateway handlers with the `upgrade_rollouts` table. |
| 11 | **Hardcoded fleet endpoints** (no-hardcode violation): `create_chat` → marcus `192.168.5.102:51000` (`server.rs:7002`); `GatewayLlmExec::hardcoded_endpoint_for_tier` (`llm_exec.rs:71`); `brain_tools.rs:16` Redis IP; `health_cmd.rs:87` 10-IP fallback; `fleet_inference.rs:register_default_fleet` 9 IPs. | as cited | Replace with `FleetResolver` / DB lookups. |
| 12 | **`ff health` vs `ff fleet health` give opposite signals** — one live-probes port 50002, the other reads DB `computers`+pulse. A node can be DB-online but unreachable. Same class: `ff versions` (tooling JSONB) vs `ff fleet versions` (`ff_git` SHA); `ff task` (HTTP :50002) vs `ff tasks` (Postgres `fleet_tasks`). | `health_cmd.rs:5` vs `fleet_cmd.rs:2416`; `versions_cmd.rs:60` vs `fleet_cmd.rs:2614`; `task_cmd.rs:5` vs `tasks_cmd.rs:117` | Pick one backing source per concept; alias the other. |

## 2. Duplicates / overlapping services (should be unified)

| Area | Duplicates | Recommendation |
|---|---|---|
| **Endpoint routing** | Five scorers: `InferenceRouter`, `pg_pick_offload_endpoint`/`pg_route_deployments` (SQL), `TaskRouter` (orchestrator), `FleetInferenceManager` (dead, hardcoded IPs), `BackendRegistry→TierRouter→AdaptiveRouter` (fleet_run tier path). | Collapse to one DB-backed scorer (`pg_route_deployments`). Delete `FleetInferenceManager`. Make `ff run`/`supervise` use it instead of single-shot `pick_agent_capable_url`. **High-value unify.** |
| **Three-mode gate enums** | `AutoscalerMode/ArbiterMode/RolloutMode/DiskPolicyMode/ConformanceMode` (5 identical Off/DryRun/Active) + their per-module `read_mode`. (`IntegrityMode`, `HandoffMode` deviate — leave.) | Extract one `GateMode` + shared `read_gate(key)` that uses `pg_read_safety_gate` (fixes conflict #9 simultaneously). |
| **think-block stripping** | `strip_think`/`strip_think_block`/`strip_think_blocks`/`extract_completion_text` copy-pasted across ff-terminal/ff-mcp/ff-gateway/ff-brain/ff-agent with "keep in sync" comments; only `research.rs` handles `<thinking>`. | One `ff-core::llm_text` module; delete copies. |
| **Signal/fact extraction** | Three pipelines (`ff-brain/facts.rs`, `ff-agent/learning.rs`, `ff-memory/capture.rs`) with overlapping keyword lists, three backends, no dedup. Two are dormant. | Unify on one engine writing to one store; delete the dormant two. |
| **Context retrieval** | Canonical graph-aware `ff-brain::select_context` (dead, zero callers) vs `session_runner::gather_brain_context` (private ILIKE re-impl). | Wire `select_context` into session start; delete the private copy. |
| **Heartbeat publishers** | v1 `HeartbeatPublisher` + v2 `HeartbeatV2Publisher` both spawn, both `System::new_*` + port-scan 55000-55010 every 15s — double overhead. | Retire v1. |
| **Upstream checkers** | `software_upstream` vs `external_tools_upstream` — copied structs/fetchers (intentional). | Acceptable, but extract shared `fetch_*` to kill the brew-gap drift risk. |
| **SKILL.md parsing/walking** | Two `Frontmatter` structs + two parsers + three DFS walkers (`collect_skill_manifests`/`find_skill_files`/`find_agent_files`). | One frontmatter parser + one walker. |
| **NodeMetrics / FleetSnapshot** | 2× `NodeMetrics`, 3× `FleetSnapshot` across ff-pulse/ff-observability/ff-agent, no conversions. | Define canonical types in ff-pulse; others convert. |
| **Watchdog double-fire** | `handoff_stuck_tasks` runs both in `tick_once` (10s) and `spawn_leader_watchdog` (60s). | Keep one. |
| **Gateway route aliases** | `/api/audit/recent`+`/events`, `/api/proxy/stats`+`/v1/...`, `/api/fleet/status`+`/api/status`. | Harmless; dedupe opportunistically. |

## 3. Gaps (ranked)

1. **`ff run`/`ff supervise` agent loop logs nothing to `ff_interactions`** — per-turn tokens accumulate in-memory (`agent_loop.rs:1007`) and are lost. Plus telegram, web/brain, gateway proxy, `ff offload`, `fleet_offload`, `ff supervise` all write zero rows. (`steps` JSONB, `model_versions`, `ff_build_sha` always empty; no `worker_name` column.) **This is the #1 blocker for an ff-LLM training substrate.**
2. **Cortex is not in the agent loop** — ff-agent has no ff-brain/ff-mcp dependency; zero cortex tools registered; `discover_mcp_tools` is dead. Agents editing `.rs` files can't query callers/impact unless a human hand-writes cortex steps.
3. **No working-memory surface** — `plans/agent-working-memory.md` is DRAFT-only; no `agent_memory` table, no `memory_*` MCP tools; `inject_system_prompt` is a plain `Vec<String>` push.
4. **Web portal missing all write surfaces** — model lifecycle, secrets, SSH, power, fleet emergency ops, DB admin, cloud-LLM/OAuth/GitHub, training, swarm/supervise/research, conformance/self-heal/arbiter/storage/fabric/cortex. Gateway is read-only for models/alerts/software.
5. **CLI bridges are localhost-only** — `127.0.0.1:51100-51104`; no cross-node routing for `claude-cli-*`; no `fleet_cli` MCP tool; orchestrator doesn't know about CLI backends; `ff supervise --backend` has no `--node`; PR creation (`handle_agent_commit_back`) is manual-only.
6. **Mission-control parity holes** — features tier, subtasks/`parent_id`, ticket numbering, ideas, task comments absent; AI hierarchy-gen / manual timer / fleet pause-resume+metrics / node-messages / model-perf leaderboard / counsel / docker-stack are **stubs or type-only**.
7. **autoscaler idle signal is degenerate** — `request_count` never written, always 0, so the `request_count==0` idle test is always true; only the 180s health-age filter is real (`autoscaler.rs:659`).
8. **brew drift blind in production** — `AutoUpgradeTick` has no brew handler; `software_upstream` (the only brew checker) runs only in the legacy daemon.
9. **HF token key split** — `huggingface_api_token` (scout/upstream) vs `huggingface.token` (model_cmd). Operator setting one breaks the other.
10. **`fleet_scan(mode=full)` MCP tool ungated** — any MCP client can trigger the subnet scan that caused the 130% CPU outage (daemon gates it; the tool doesn't).
11. **`agent_procedures` write-only** — consolidation loop fills it; nothing ever SELECTs it.
12. **MCP per-call pools** — `open_operational_store`, `get_pg_pool` paths build fresh pools (pool-per-call anti-pattern); `cli_interaction_pool` too.
13. **Leader self-upgrade missing `free-for-build`** — dormant today (Taylor 96GB) but OOMs a memory-tight leader.

## 4. Answers to the 6 questions

**Q1 — Hybrid-LLM wiring & telemetry: PARTIAL.** One shared SQL scorer (`pg_route_deployments`) correctly serves offload/fleet_run-path3/crew. But `ff run`/`ff supervise` bypass it (single `pick_agent_capable_url` at session start, then hold the URL); `fleet_run` default `strategy=tier` uses a *parallel* `BackendRegistry→TierRouter→AdaptiveRouter` chain. **Key gap:** the two main interactive entry points neither route through the offload scorer nor record any per-turn telemetry; `InteractionRecord` has no `worker_name`/`endpoint`, so even recorded calls can't be traced to a computer, and `cost_usd` is always 0.

**Q2 — Cortex in the loop: ABSENT.** Cortex is reachable only via explicit MCP tool calls from an external client on :50001. ff-agent has no ff-brain/ff-mcp dependency, zero cortex tools in `core_tools()`/`all_tools()`, and `discover_mcp_tools` is never called. **Key gap:** a code-editing agent gets no structural context automatically — only flat-markdown `BrainLoader` injection.

**Q3 — Logging completeness & ff-LLM substrate: PARTIAL.** Four channels write `ff_interactions` (mcp/cli/session/gateway-jarvis), but telegram, web-brain, gateway proxy, `ff offload`, `fleet_offload`, and `ff supervise` write nothing. **Key gap:** `steps`/`model_versions`/`ff_build_sha` are always empty; `request_text` stores only the bare prompt (no system prompt / history); multi-turn sessions collapse to one row; errors are never logged. Not training-quality yet.

**Q4 — Web/TUI parity: PARTIAL, CLI far ahead.** ~24 CLI subsystems have no web write surface (model lifecycle, secrets, SSH, power, fleet emergency, DB admin, cloud-LLM, OAuth, training, swarm/research, cortex, conformance, self-heal, arbiter, storage, fabric, ports, logs, social). A few web-only views exist (JARVIS HUD, interaction console, BrainGraph, Workflow/Planning, topology). **Key gap:** the gateway is read-only for nearly all operational control.

**Q5 — Mission-control parity: PARTIAL.** ff-mc has the core primitives (work items, epics, sprints+burndown, review items, dependencies, task groups, portfolio — richer than legacy, plus legal). **Key gap:** features tier, subtasks, ticket numbering, ideas, comments are *absent*; AI hierarchy-gen, manual timer, fleet pause/resume+metrics, node-messages, model-perf leaderboard, docker-stack, counsel, chat-sessions are *stubs or type-only*.

**Q6 — CC/kimi/codex integration: PARTIAL.** Direction B (external CLI → ff via MCP, 36 tools) is solid. Direction A (ff driving vendor CLIs) has all three layers shipped (`cli_executor`, `ff cli`, HTTP bridges) plus OAuth distribution. **Key gap:** bridges bind `127.0.0.1` only — no cross-node routing, no `fleet_cli` MCP tool, orchestrator ignores CLI backends, `ff supervise --backend` is local-only, and the "cloud CLI → ff → fanout → PR" loop has no single automated entry point (`handle_agent_commit_back` is manual). The port swap (Q1/conflict #8) actively misroutes kimi/gemini.

## 5. Prioritized roadmap

Ordered. **★ = unblocks downstream items.**

**Phase 0 — Stop the bleeding (correctness)**
1. **★ Retire legacy `ff daemon` actuating ticks** (S–M) — single-source defer-worker, scheduler, auto-upgrade, wave-reaper, CoverageGuard, mesh-refresh in `forgefleetd`. *Unblocks conflicts #2,#3 and removes the whole dual-daemon racing class.*
2. **Fix the CLI-bridge port swap** (S) — align `cli_bridge.rs` enumerate with the DB seed. *Unblocks Q6 cross-vendor correctness.*
3. **Unify gate reads through `pg_read_safety_gate`** (S) — extract `GateMode`+`read_gate`; fixes the disable-gate TTL conflict *and* the 5-enum duplication in one change.
4. **Single `model_supports_tools` source** (S) — kill the gemma-4 silent-hang.
5. **Purge hardcoded fleet endpoints** (S–M) — `create_chat`, `llm_exec` fallback, `brain_tools` Redis, `health_cmd` fallback; delete dead `FleetInferenceManager`.

**Phase 1 — Telemetry + substrate (foundational for everything LLM)**
6. **★ Complete request/response logging** (M) — add `pg_record_interaction` to telegram, web-brain, gateway proxy, `ff offload`, `fleet_offload`, `ff supervise`, and the `ff run`/`supervise` agent loop; populate `steps`/`model_versions`/`ff_build_sha`; add `worker_name`/`endpoint` columns; log full `messages[]` + errors. *Unblocks the ff-LLM training substrate and any model-quality/cost analytics.*
7. **★ Unify endpoint routing on `pg_route_deployments`** (M) — make `ff run`/`supervise`/`fleet_run-tier` use the one scorer; fix the degenerate `request_count` idle signal (actually write request counts). *Unblocks smart model-swap scheduling and Q1.*

**Phase 2 — Build asks (capability)**
8. **Smart model-swap scheduler** (L) — depends on #6 (real load signals) + #7 (one router) + #1 (one daemon). Make the **arbiter `execute_step` real** (wire `ff offload`/`load_model`), set `reservation_state` on build targets, add free-for-build to leader self-upgrade, durable paused-model snapshot.
9. **★ Cortex-in-the-loop** (M) — add ff-brain dep to ff-agent (or an MCP self-call), register `cortex_*` as agent tools, pre-query corpus at session start. *Unblocks higher agent code-quality and reduces blind file scanning.*
10. **Working-memory** (M) — implement `agent_memory` table + `memory_add/replace/remove/get` MCP tools + capped block rendering in `inject_system_prompt`. Pairs with #9.
11. **CC/kimi/codex orchestration** (M–L) — bind bridges to LAN, add `fleet_cli` MCP tool with output capture, teach orchestrator about CLI backends, `ff supervise --backend --node`, auto-trigger `handle_agent_commit_back` post-task (the cloud-CLI→ff→fanout→PR loop). Depends on #2,#7.
12. **Web redesign — operational write surfaces** (L) — add gateway routes + dashboard for model lifecycle, fleet control, tasks, training, cloud-LLM/OAuth; back update pause/resume with the DB table. Closes the parity gap.
13. **Mission-control port — finish the stubs** (L) — features tier + subtasks + ticket numbering + comments + ideas; turn timer/AI-gen/fleet-pause/node-messages/model-perf/counsel/docker-stack from stubs into real implementations.
14. **TUI rebuild** (L) — align TUI surface with the unified router (#7), telemetry (#6), and new web write routes (#12) so the three stay in parity (per the TUI/Web-sync rule).

**Phase 3 — Cleanup (low-risk debt)**
15. Consolidate think-strip / signal-extraction / context-retrieval / SKILL parsing+walking / NodeMetrics+FleetSnapshot duplicates; delete dead code (`ff-memory`, `ff-mesh`, `agent_procedures` consumer-less loop, `self_cmd.rs`, `classify_workload`, `select_context` wire-in via #9); gate `fleet_scan(full)`; fix MCP per-call pools; unify HF token key; extract shared brew upstream fetcher. (S each.)

**Sequencing summary:** #1 unblocks the conflict cluster → #6+#7 are the foundation that #8 (model-swap) and #9–#11 (cortex/memory/CLI-orchestration) all depend on → #12–#14 (web/MC/TUI) are parallelizable once #6/#7 land → #15 is continuous low-risk cleanup.
---

## Research refinement — 2026-06-24 (loop iteration)

Re-scoped two findings against current code (the table's line cites predate the
SQLite-removal refactor and have drifted):

### Finding #4 (leader-election) — CONFIRMED live, precise current scope
Two leader engines genuinely run **concurrently in production `forgefleetd`**:
- **`LeaderTick`** (`ff_agent::leader_tick`) — claims the Postgres
  `fleet_leader_state` singleton every 15s. **Authoritative**: every actuating
  tick (defer-worker, scheduler, auto-upgrade, wave-reaper, revive_scan) gates on
  it.
- **`start_leader_election_subsystem`** (`src/main.rs:2232`) — a *second* loop
  using `ff_core::leader::{elect_leader, check_failover}` over
  `registry.node_health_for_election()` (discovery/TOML health). It HTTP-announces
  a winner (`announce_leader_to_fleet`) and calls `registry.set_leader`
  (`src/main.rs:2229`), which feeds the gateway's `leader_hint`.

They never reconcile and can disagree (TOML-health winner X vs Postgres+pulse
winner Y) → gateway reports X as `leader_hint` while Y is the real actuating
leader. **✅ DONE (PR #544, 2026-06-24):** chose (a) — deleted
`start_leader_election_subsystem` + `announce_leader_to_fleet` entirely; LeaderTick
is the single authority. Verified the discovery engine was pure redundancy: its
`announce` POSTs hit `post_leader`, which only bumps `heartbeat_at` on the EXISTING
`fleet_leader_state` row (can't install a leader; redundant with LeaderTick's own
heartbeat), and `registry.current_leader()` had exactly ONE consumer (the gateway
`leader_hint`). **Dogfood bonus:** that `leader_hint` was already `None` live — its
fallback (`registry → db_snapshot.role=='leader'`) couldn't resolve because
`fleet_worker_runtime` is empty in practice. Repointed `leader_hint` to read
`fleet_leader_state` directly (same source as the `/api/fleet/leader` GET) — it now
resolves to the real leader (`taylor`, proven runtime-resolvable: the identical
`operational_store.pg_pool()` path serves `/api/fleet/leader`→taylor). **Caveat —
the dashboard `is_leader` flag still doesn't light up** because the DOWNSTREAM
`build_fleet_worker_view` match (`node.config_name|hostname|display_name == leader`)
can't match taylor: its registry `FleetComputer` has `config_name=None`,
`hostname=None` (not a config-sourced node), and the DB-runtime role is empty. Same
root cause as the `fleet_worker_runtime`-empty follow-up below — tracked there, not
fixed here. So #4's HA core is closed; the dashboard leader rendering rides on the
runtime-table follow-up.

### NEW finding (surfaced by #4 dogfood) — `fleet_worker_runtime` is empty live
`fleet_worker_runtime` (the table `RuntimeRegistryStore::list_runtime_nodes` reads,
which becomes the gateway's `db_snapshot.runtime_nodes`) has **0 rows** on the live
fleet. So *every* consumer of `db_snapshot.runtime_nodes` in the fleet-status
payload silently degrades to empty — not just `leader_hint`. Either nothing
populates it anymore (superseded by pulse/heartbeat_v2?) or its writer isn't
running. **Next:** trace who is supposed to write `fleet_worker_runtime`; if it's
dead, repoint the remaining `db_snapshot.runtime_nodes` consumers at the live
source (pulse beats / `computers`+`fleet_workers`) or retire the table. Medium —
affects the accuracy of the whole fleet-status/dashboard payload. **Also gates the
dashboard leader rendering:** `build_fleet_worker_view` derives a node's `role`
from `db_node.role` (empty → "unknown") and matches `leader_hint` against
`config_name|hostname|display_name`. For nodes whose registry `FleetComputer` lacks
config_name/hostname (e.g. taylor, discovered not config-seeded), neither resolves,
so `is_leader` stays false even though `leader_hint`=taylor is correct. Fix here =
correlate the node to its DB/runtime identity (match `leader_hint` against the
resolved DB name too, and source `role` from the live table).

### Finding #15 (dead code) — `ff-mesh` CONFIRMED 100% orphaned
`cargo tree -i -p ff-mesh` → **zero reverse-dependencies**; no `src/` or other
crate's `Cargo.toml` references it (only the workspace-members list). The whole
crate (`election::ElectionManager`, `leader::LeaderDaemon`, `scheduler`,
`work_queue`, `worker`, `resource_pool`) is an abandoned parallel implementation —
compiled on every workspace build, pollutes leadership greps/Cortex, and is a
latent re-wire hazard (someone could spawn `LeaderDaemon` and create a real
split-brain). **✅ DONE (PR #543, 2026-06-24):** dropped `crates/ff-mesh` from
workspace members + deleted the crate (7 files); `cargo check --workspace`,
CI-exact clippy, and `cargo test --workspace --lib` all green; deployed
fleet-wide. Distinct from #4 — this was dead code, #4 is two *live* engines.
