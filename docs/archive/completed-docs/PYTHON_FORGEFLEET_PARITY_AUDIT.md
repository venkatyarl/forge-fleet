# Python ForgeFleet → Rust ForgeFleet Parity Audit

Date: 2026-04-05  
Legacy repo: `/Users/venkat/projects/forge-fleet` (`27e84bc1`)  
Rust repo: `/Users/venkat/projects/forge-fleet` (`f4a2e08`)

## Method and grading rules

This audit is based on direct source inspection of both repos (module surfaces + implementation wiring), with conservative scoring:

- **COMPLETE** = capability exists and is implemented in an active runtime path (not just a type definition).
- **PARTIAL** = capability exists but is reduced, scaffolded, or not fully wired end-to-end.
- **MISSING** = no clear Rust equivalent found.
- **INTENTIONALLY_REPLACED** = legacy shape is not 1:1 ported, but Rust has a different architecture covering most intent.

---

## 1) Core orchestration lifecycle

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| Autonomous ticket execution loop (claim → decompose → crew → commit/push → MC update) | `forgefleet/engine/autonomous.py`, `forgefleet/engine/mc_client.py`, `forgefleet/engine/git_ops.py` | `crates/ff-agent/src/main.rs`, `crates/ff-orchestrator/src/*`, `crates/ff-mc/src/*` | PARTIAL | Rust has pieces (agent runtime, orchestration, MC DB/API) but root daemon uses lightweight `ff_agent::run` heartbeat (`crates/ff-agent/src/lib.rs`) rather than the fuller autonomous flow. |
| Lifecycle phase manager (`WORK/LEARN/ANALYZE/UPDATE/VERIFY/RESEARCH/IDLE`) | `forgefleet/engine/lifecycle.py`, `forgefleet/engine/lifecycle_policy.py` | `src/main.rs`, `crates/ff-control/src/bootstrap.rs`, `crates/ff-control/src/control_plane.rs` | PARTIAL | Rust has subsystem bootstrap/lifecycle startup, but no direct phase-machine equivalent to legacy lifecycle loop. |
| Execution state machine with explicit transitions/errors/escalation flags | `forgefleet/engine/state_machine.py` | `crates/ff-pipeline/src/step.rs`, `crates/ff-pipeline/src/error.rs`, `crates/ff-control/src/errors.rs` | PARTIAL | Rust has state abstractions, but no direct centralized execution state machine mirroring legacy semantics. |
| Task decomposition/planning | `forgefleet/engine/task_decomposer.py`, `forgefleet/engine/prompt_templates.py` | `crates/ff-orchestrator/src/task_decomposer.rs`, `crates/ff-orchestrator/src/decomposer.rs`, `crates/ff-orchestrator/src/planner.rs` | COMPLETE | Rust decomposition/planning stack is substantial and tested. |
| Multi-agent crew runtime (Context→Code→Review execution) | `forgefleet/engine/crew.py`, `forgefleet/engine/autonomous.py`, `forgefleet/mcp_server.py` | `crates/ff-mcp/src/handlers.rs` (`fleet_crew`), `crates/ff-orchestrator/src/agent_team.rs` | PARTIAL | Rust `fleet_crew` currently returns planning/decomposition metadata (`status: planned`), not full agent execution loop parity from legacy. |
| Pipeline execution orchestration | `forgefleet/engine/pipeline.py` | `crates/ff-pipeline/src/executor.rs`, `crates/ff-pipeline/src/graph.rs`, `crates/ff-pipeline/src/step.rs` | PARTIAL | Rust pipeline crate is rich, but not wired into root daemon startup path yet. |
| Ownership / lease / handoff / escalation controls | `forgefleet/engine/ownership.py` | (no direct match; closest: `crates/ff-mesh/src/work_queue.rs`, `crates/ff-db/src/schema.rs`) | MISSING | Legacy has explicit ownership leasing/handoff/escalation persisted flows; Rust lacks direct parity data model/API. |
| Execution/event tracking at ticket granularity | `forgefleet/engine/execution_tracking.py` | `crates/ff-db/src/schema.rs` (`audit_log`, `tasks`, `task_results`) | PARTIAL | Rust has generic audit/task tables, but not equivalent ticket-centric execution timeline model. |
| Git branch/commit/PR automation inside worker loop | `forgefleet/engine/git_ops.py`, `forgefleet/engine/autonomous.py` | (no direct equivalent in runtime path) | MISSING | Rust updater/checker touches git/version checks, but not legacy-style repo mutation workflow. |
| Idle-aware work gating (user activity scheduler) | `forgefleet/engine/scheduler.py`, `scheduling_policy.py` | `crates/ff-cron/src/policy.rs`, `crates/ff-mesh/src/scheduler.rs` | PARTIAL | Rust has scheduling policy and task scoring primitives; end-to-end idle-gated autonomous work loop is not wired like legacy. |

---

## 2) Node management / discovery / health

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| Known-node + subnet network discovery | `forgefleet/engine/discovery.py` | `crates/ff-discovery/src/scanner.rs`, `src/main.rs` discovery subsystem, `crates/ff-mcp/src/handlers.rs` (`fleet_scan`) | COMPLETE | Rust scanning is robust and actively integrated in daemon loop. |
| Node registry with config + discovered nodes | `forgefleet/engine/discovery.py`, `forgefleet/engine/peer_mesh.py` | `crates/ff-discovery/src/registry.rs` | COMPLETE | Rust registry is richer and concurrent (`DashMap`), with stale handling and health application. |
| Health probes and status classification | `forgefleet/engine/discovery.py`, `node_manager.py`, `daemon.py` | `crates/ff-discovery/src/health.rs`, `scanner.rs` | COMPLETE | Rust has explicit `Online/Degraded/Offline` and periodic checks wired in main daemon. |
| Leader election and announcements | `forgefleet/engine/peer_mesh.py`, `daemon.py` | `src/main.rs` leader election subsystem + `ff_core::leader` | COMPLETE | Implemented with config-aware failover/election behavior; architecture differs from legacy peer mesh internals. |
| Peer mesh task-claim coordination model | `forgefleet/engine/peer_mesh.py` | `crates/ff-mesh/src/*` | PARTIAL | Rust mesh crate exists, but not integrated into root daemon runtime path. |
| Self-healing local services (restart LLM/docker) | `forgefleet/engine/node_manager.py` | `src/main.rs` self-heal loop + `crates/ff-runtime/src/process_manager.rs`; MCP/manual ops in `crates/ff-mcp/src/handlers.rs` | PARTIAL | Rust now runs an automatic local self-heal loop (scan/adopt + health checks + restart for managed llama-server processes). Docker-specific restart parity is intentionally not mirrored in the default Rust daemon path. |
| Runtime process/model management | `forgefleet/engine/node_manager.py`, `discovery.py` | `crates/ff-runtime/src/process_manager.rs`, `model_manager.rs`, `src/main.rs` self-heal wiring | COMPLETE | Runtime process manager is now actively wired into daemon automation (`start_self_heal_subsystem`) instead of being operator-only. |
| Node status endpoint for peer querying | `forgefleet_subagent.py` (`/api/status`, `/health`, `/api/fleet`) | `crates/ff-gateway/src/server.rs` (`/health`, `/api/fleet/status`, node/fleet APIs) | INTENTIONALLY_REPLACED | Rust centralizes status in gateway API rather than standalone per-node Python HTTP server. |
| Remote model install + readiness wait | `forgefleet/engine/discovery.py`, `forgefleet/mcp_server.py` | `crates/ff-mcp/src/handlers.rs` (`fleet_install_model`, `fleet_wait`) | COMPLETE | Rust parity is strong here, including verify/wait behavior. |

---

## 3) LLM / router / governance

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| Tiered model routing with fallback | `forgefleet/engine/fleet_router.py`, `forgefleet/server.py`, `forgefleet/mcp_server.py` | `crates/ff-api/src/router.rs`, `crates/ff-gateway/src/server.rs`, `crates/ff-mcp/src/handlers.rs` (`fleet_run`) | COMPLETE | Rust routing is explicit and robust in both proxy and MCP paths. |
| Adaptive task classification for routing | (implicit/limited in Python) | `crates/ff-api/src/classifier.rs`, `adaptive_router.rs` | INTENTIONALLY_REPLACED | Rust introduces stronger adaptive routing architecture beyond legacy shape. |
| Backend registry/health/busy-aware selection | `forgefleet/engine/fleet_router.py` | `crates/ff-api/src/registry.rs`, `router.rs`, gateway proxy | COMPLETE | Clear parity and stronger implementation. |
| Governance performance persistence and scoring | `forgefleet/engine/model_governance.py` | `crates/ff-api/src/quality_tracker.rs`, `crates/ff-mcp/src/handlers.rs` (`QUALITY_SNAPSHOT_KEY`) | PARTIAL | Rust scoring/persistence exists but primarily updated through MCP `fleet_run`; governance feedback loop is not uniformly wired across all request surfaces. |
| Model recommendation API | `forgefleet/engine/model_governance.py` (and CLI hooks) | `crates/ff-mcp/src/handlers.rs` (`model_recommend`) | COMPLETE | Implemented and wired in Rust MCP handlers. |
| Model stats API | `forgefleet/engine/model_governance.py` (and CLI hooks) | `crates/ff-mcp/src/handlers.rs` (`model_stats`) | COMPLETE | Implemented and wired in Rust MCP handlers. |
| OpenAI-compatible chat/models API | `forgefleet/server.py` (`/v1/chat/completions`, `/v1/models`) | `crates/ff-gateway/src/server.rs` (`/v1/chat/completions`, `/v1/models`) | COMPLETE | Strong parity. |
| Cost tracking/accounting | `forgefleet/engine/cost_tracker.py` | (no clear direct module) | MISSING | No direct Rust parity module found for legacy cost-tracker semantics. |
| Benchmark-based model leaderboard feedback | `forgefleet/engine/benchmarker.py` | `crates/ff-benchmark/src/*` | PARTIAL | Benchmark crate exists, but not visibly integrated into governance decisions in daemon path. |

---

## 4) MCP / OpenClaw bridge

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| MCP server (stdio JSON-RPC) | `forgefleet/mcp_server.py` | `crates/ff-mcp/src/server.rs`, `transport.rs` | COMPLETE | Core server parity exists for listed fleet tools. |
| MCP HTTP transport | `forgefleet/server.py` (`/mcp`) | `crates/ff-mcp/src/transport.rs` + gateway mount (`/mcp`) | COMPLETE | Rust has dedicated transport layer and gateway integration. |
| Fleet tool surface (`fleet_status/scan/run/...`) | `forgefleet/mcp_server.py` | `crates/ff-mcp/src/tools.rs`, `handlers.rs` | COMPLETE | Tool set is broadly matched and implemented in Rust. |
| `fleet_crew` runtime behavior | `forgefleet/mcp_server.py` (`fleet_crew` executes crew) | `crates/ff-mcp/src/handlers.rs` (`fleet_crew`) | PARTIAL | Rust currently plans/decomposes only; no actual multi-agent execution parity. |
| MCP client federation (connect to external MCP servers and re-expose tools) | `forgefleet/engine/mcp_client.py` | `crates/ff-mcp/src/federation.rs`, `crates/ff-mcp/src/server.rs` (federated `tools/list` merge + unknown-tool proxy) | PARTIAL | Rust now includes HTTP MCP client federation discovery and proxy routing into remote MCP tools; stdio transport parity from legacy is still not mirrored. |
| MCP topology validation (`required/optional` link graph) | `forgefleet/engine/mcp_topology.py` | `crates/ff-mcp/src/federation.rs` + daemon `start_mcp_federation_subsystem` in `src/main.rs` | COMPLETE | Rust now validates required/optional service dependencies and required/optional tools, with continuous topology checks in daemon runtime. |
| OpenClaw outbound notification bridge | `forgefleet/engine/openclaw_bridge.py` | `crates/ff-gateway/src/{telegram.rs,discord.rs,message.rs,router.rs}` | INTENTIONALLY_REPLACED | Rust moved to unified gateway/channel architecture rather than a dedicated OpenClaw POST bridge helper class. |
| Mission Control bridge from gateway | Legacy via `mc_client.py` and autonomous flow | `crates/ff-gateway/src/server.rs` mounting `ff-mc` routes | PARTIAL | Rust exposes MC APIs, but autonomous worker loop integration against MC work-items is not equivalent. |

---

## 5) Memory / context

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| Persistent memory store | `forgefleet/memory/store.py` | `crates/ff-memory/src/store.rs` | COMPLETE | Rust has stronger schema and APIs. |
| Memory retrieval/search | `forgefleet/memory/store.py`, `forgefleet/engine/context_store.py` | `crates/ff-memory/src/retrieval.rs`, `workspace.rs` | COMPLETE | Retrieval capability is present and richer. |
| Session memory lifecycle | `forgefleet/engine/conversation.py`, `transcript.py` | `crates/ff-memory/src/session.rs`, `crates/ff-sessions/src/session.rs`, `history.rs` | COMPLETE | Rust session/history infrastructure is robust. |
| Context compaction/summarization | `forgefleet/context/context_mode.py`, `forgefleet/engine/context_store.py` | `crates/ff-sessions/src/context.rs`, `history.rs` compaction | PARTIAL | Rust has context windowing/compaction, but not a direct parity wrapper around legacy ContextMode integration behavior. |
| Repo/doc chunking + indexed context retrieval | `forgefleet/engine/context_store.py` | `crates/ff-memory/src/rag.rs` | PARTIAL | Rust has RAG ingestion/retrieval; direct BM25/local-sqlite context store parity is replaced rather than mirrored. |
| Transcript auto-capture to long-term memory | (ad-hoc in legacy) | `crates/ff-memory/src/capture.rs` | INTENTIONALLY_REPLACED | Rust introduces explicit auto-capture engine beyond legacy shape. |
| Workspace-scoped memory | limited in legacy | `crates/ff-memory/src/workspace.rs` | INTENTIONALLY_REPLACED | Rust has explicit workspace abstractions not directly mirrored in Python. |
| Cross-node memory sync | `forgefleet/engine/memory_sync.py` | (no direct equivalent found) | MISSING | Legacy has push/pull node sync methods; direct Rust parity not found. |
| Context-mode bridge helper | `forgefleet/context/context_mode.py` | (no direct crate-level equivalent; external OpenClaw context-mode used) | INTENTIONALLY_REPLACED | Capability likely shifted to external runtime tooling rather than in-repo crate. |

---

## 6) Evolution / improvement loops

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| Learning log (error pattern + model outcome history) | `forgefleet/engine/self_improve.py` | `crates/ff-evolution/src/learning.rs`, `crates/ff-api/src/quality_tracker.rs` | PARTIAL | Rust has richer modules, but cross-crate learning loop integration is incomplete. |
| Continuous improvement cycle generation | `forgefleet/engine/continuous_improvement.py` | `crates/ff-evolution/src/loop.rs`, `analyzer.rs`, `backlog.rs`, daemon `start_evolution_subsystem` in `src/main.rs` | COMPLETE | Evolution loop is now scheduled in the default daemon path using live discovery/health observations. |
| Evolution proposals and prioritization | `forgefleet/engine/evolution.py`, `improvement_proposal.py` | `crates/ff-evolution/src/backlog.rs`, `verification.rs`, `repair.rs` + daemon wiring in `src/main.rs` | COMPLETE | Proposal/prioritization path is now exercised by runtime loop activation instead of remaining library-only. |
| Automated self-update cycle from findings | `forgefleet/engine/self_update.py` | `crates/ff-updater/src/orchestrator.rs`, `checker.rs`, `verifier.rs`, `swapper.rs`, daemon `start_updater_subsystem` in `src/main.rs` | PARTIAL | Daemon now runs periodic updater checks and optional apply pipeline (`[loops.updater].auto_apply`), but full always-on autonomous rollout remains guarded. |
| Auto-fix + verification loop | `forgefleet/engine/auto_fix.py`, `self_update.py` | `crates/ff-evolution/src/repair.rs`, `verification.rs`, daemon `start_evolution_subsystem` | PARTIAL | Runtime now exercises repair+verification cycles, but current observation source is health-focused and not yet fed by all legacy build/test failure channels. |
| Benchmark feedback into improvement | `forgefleet/engine/benchmarker.py` | `crates/ff-benchmark/src/*` | PARTIAL | Benchmarking exists; feedback coupling to evolution/governance not fully demonstrated in runtime path. |
| Scheduled/continuous activation in daemon | `lifecycle.py`, `continuous_improvement.py` | `src/main.rs` loop subsystems (`start_evolution_subsystem`, `start_updater_subsystem`) + `ff-evolution` | COMPLETE | Root daemon now actively schedules evolution and updater loops by default via `[loops.*]` runtime config. |

---

## 7) Deployment / update / ops

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| Service installation (systemd/launchd) | (legacy mostly scriptless/manual runtime) | `deploy/install.sh`, `deploy/linux/forgefleet.service`, `deploy/macos/com.forgefleet.daemon.plist` | INTENTIONALLY_REPLACED | Rust operational maturity is stronger and standardized. |
| Rolling/canary rollout orchestration | partial legacy via scripts/self-update | `crates/ff-updater/src/{rollout.rs,canary.rs,orchestrator.rs}` | PARTIAL | Rich implementation exists but not surfaced through default daemon command flow yet. |
| Binary swap + rollback + smoke verify | `forgefleet/engine/self_update.py` (restart/check loops) | `crates/ff-updater/src/{swapper.rs,rollback.rs,verifier.rs}` | PARTIAL | Strong Rust modules, but integration gap remains. |
| Deployment strategy/gating framework | ad-hoc in Python | `crates/ff-deploy/src/*` | PARTIAL | Framework is present; runtime command path mostly acceptance/planning today (`ff-control`). |
| Dependency installer tooling | `forgefleet/engine/dependency_installer.py` | (no direct equivalent found) | MISSING | No direct parity crate or command found. |
| Docker monitor/control loop | `forgefleet/engine/docker_monitor.py`, `node_manager.py` | (no direct equivalent found) | MISSING | Rust focuses model/runtime/gateway stack; docker-monitor parity absent. |
| Postgres-to-embedded migration utility | N/A in legacy | `tools/migrate_from_postgres.rs` | INTENTIONALLY_REPLACED | Rust adds migration capability not present in legacy architecture. |

---

## 8) CLI and daemon flows

| Capability/module | Legacy file(s) | Rust equivalent(s) | Status | Notes |
|---|---|---|---|---|
| Primary daemon entrypoint | `forgefleet_subagent.py` | `src/main.rs` (`forgefleetd`) | INTENTIONALLY_REPLACED | Rust daemon is subsystem-based and service-oriented rather than single Python script bootstrap. |
| Daemon subsystem startup composition | `forgefleet_subagent.py` (NodeManager + PeerMesh + status server + AutonomousWorker) | `src/main.rs` (discovery + leader election + API proxy + gateway + cron + agent heartbeat) | PARTIAL | Rust composition is broader platform-wise, but lacks legacy autonomous worker execution parity. |
| CLI fleet status/recommend/model-stats | `forgefleet/cli.py` | `crates/ff-cli/src/main.rs`, MCP handlers | PARTIAL | Rust MCP has recommend/stats; installed daemon/CLI path does not directly mirror legacy command UX. |
| CLI operational commands with real side effects | lightweight in legacy but functional for its scope | `crates/ff-cli/src/main.rs` | PARTIAL | Several ff-cli commands are currently informational/placeholders rather than executing control-plane actions. |
| Start/stop/restart operator flow | legacy via process mgmt/manual | Rust via service manager + `forgefleetd start` and install artifacts | PARTIAL | `forgefleetd` command surface is minimal (`Start/Status/Version`); lifecycle controls largely delegated to systemd/launchd tooling. |
| Health/status API exposure | `forgefleet_subagent.py` (`/health`, `/api/status`) | `crates/ff-gateway/src/server.rs` (`/health`, fleet endpoints) | COMPLETE | Runtime status endpoints are present and wired. |
| Session/sub-agent runtime control surface | limited/implicit in legacy | `crates/ff-sessions/src/*` + gateway routes | INTENTIONALLY_REPLACED | Rust adds structured session/subagent management beyond legacy patterns. |

---

## High-confidence parity summary

- **Strong parity/upgrade areas**: discovery/health, tiered routing, OpenAI proxy endpoints, MCP server/transport, config handling, service installation/daemonization.
- **Largest parity gaps**: autonomous execution loop wiring, ownership/lease tracking, cross-node memory sync, cost/dependency operational gaps, and full MCP federation transport parity (stdio side).
- **Important caveat**: Rust repo contains many advanced crates that are currently **partially integrated** into the default `forgefleetd` runtime path.

---

## P0 / P1 / P2 backlog (explicit missing parity work)

### P0 (blockers for claiming functional parity)

1. **Implement true `fleet_crew` execution parity**
   - Current Rust `fleet_crew` returns planning output only.
   - Bring behavior to actual Context→Code→Review execution flow (or explicitly rename contract to avoid mismatch).

2. **Wire real autonomous execution loop into default daemon path**
   - Root daemon currently uses heartbeat-only `ff_agent::run`.
   - Integrate task claim/execute/report loop comparable to legacy `AutonomousWorker` lifecycle.

3. **Add ownership/lease/handoff parity model**
   - Port core capabilities from `ownership.py`/`execution_tracking.py` (claim, renew, handoff, escalation, timeline).

### P1 (important parity wave)

1. **MCP federation parity** — **PARTIAL (Phase 24)**
   - Added HTTP client federation discovery/routing (`crates/ff-mcp/src/federation.rs`) and server-side proxy fallback for unknown tools.
   - Remaining: legacy-like stdio transport federation parity and deeper contract coverage.

2. **MCP topology validation parity** — **CLOSED (Phase 24)**
   - Added required/optional service-link + required/optional tool validation and continuous daemon checks.

3. **Activate evolution/updater pipelines in runtime** — **MOSTLY_CLOSED (Phase 24)**
   - Added daemon loop wiring for evolution and updater (`start_evolution_subsystem`, `start_updater_subsystem` in `src/main.rs`).
   - Remaining: broader signal sources for evolution and fully autonomous updater rollout policy (currently guarded by `auto_apply`).

4. **Upgrade ff-cli from placeholder outputs to control-plane actions**
   - Bind CLI actions to real subsystem commands and status/runtime effects.

5. **Node self-heal automation parity** — **PARTIAL (Phase 24)**
   - Added automatic model-process self-heal loop (`start_self_heal_subsystem` + `ff-runtime` process manager).
   - Remaining: Docker-specific restart semantics from legacy are intentionally not mirrored in the default Rust daemon path.

### P2 (nice-to-have / cleanup parity)

1. **Cross-node memory sync parity** (`memory_sync.py` behavior).
2. **Cost tracker parity** (`cost_tracker.py`) or explicit retirement decision.
3. **Dependency installer parity** (`dependency_installer.py`) or document operational replacement.
4. **Documented deprecations for intentionally replaced modules** to prevent future parity confusion.

---

## Phase 24 closure summary (this implementation pass)

### Closed in code

- MCP topology validation parity (required/optional dependency + tool validation).
- Evolution loop runtime activation in default daemon path.
- Scheduled/continuous daemon activation for evolution/updater loops.
- Runtime process/model daemon wiring (self-heal loop uses `ff-runtime` process manager in live path).

### Improved but not fully closed

- MCP federation parity (HTTP federation discovery + proxy added; stdio federation still missing).
- Updater runtime parity (check loop active, apply path optional/guarded).
- Node self-heal parity (LLM process self-heal is automated; Docker restart semantics intentionally not mirrored).
- Auto-fix + verification parity (runtime active, but signal source coverage is still narrower than legacy).

### Still open (highest-impact)

- Full autonomous ticket execution loop parity (`fleet_crew` / autonomous worker behavior).
- Ownership / lease / handoff / escalation parity model.
- Cross-node memory sync parity.
- Cost tracker and dependency-installer parity decisions.

## Final verdict

Rust ForgeFleet has surpassed Python in several core platform dimensions (typed architecture, gateway/API composition, discovery/registry robustness, deployment scaffolding).  
However, **full behavioral parity is not complete** yet due to key orchestration-loop and integration gaps listed above. The current state should be treated as **platform-forward with partial legacy workflow parity**.