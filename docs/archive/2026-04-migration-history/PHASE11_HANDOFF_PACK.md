# Phase 11 Handoff Pack (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Repo: `/Users/venkat/projects/forge-fleet`

This is the fast onboarding pack for the next engineer picking up the Rust rewrite.

---

## 1) Where to start (new engineer path)

### Step 0 — Read order (30–45 min)
1. `README.md` (current rewrite status snapshot)
2. `docs/PHASE9_SCOPE_RECONCILIATION.md` (what was included/excluded + known gaps)
3. `docs/PHASE9_SMOKE_CHECKLIST.md` (known-good smoke commands/log expectations)
4. `docs/PHASE10_RELEASE_READINESS.md` (what is green vs still blocked)
5. `docs/PHASE10_EXECUTION_BACKLOG.md` (ordered implementation queue)
6. `docs/PHASE10_API_SURFACE.md` + `docs/PHASE10_API_GOVERNANCE.md` (public API contract baseline)

### Step 1 — Validate your environment
From repo root:

```bash
cargo --version
cargo check --workspace
cargo test --workspace --lib
cargo run -p ff-cli -- --help
```

If all pass, you are starting from a healthy baseline.

### Step 2 — Understand current state before coding
- Workspace has 22 crates in `Cargo.toml`.
- `ff-api`, `ff-control`, `ff-runtime`, `ff-sessions`, `ff-skills`, `ff-memory`, `ff-observability`, `ff-gateway`, `ff-orchestrator` have meaningful library surfaces.
- `ff-pipeline` is still placeholder.
- Root binary `src/main.rs` is currently stub (`Hello, world!`); operational CLI entrypoint is `crates/ff-cli/src/main.rs`.

### Step 3 — Start implementation from backlog order
Use `docs/PHASE10_EXECUTION_BACKLOG.md` and execute FF10-001 → FF10-013 in strict order.

---

## 2) Architecture entry points (crates + docs)

## A. Code entry points

| Layer | Start here | Why it matters |
|---|---|---|
| Top-level binary | `src/main.rs` | Current root executable state (still stub; tracks FF10-001 gap). |
| CLI | `crates/ff-cli/src/main.rs` | Real command surface (`start`, `agent`, `status`, `nodes`, `models`, `proxy`, `discover`, `health`, `config`, `version`). |
| Control plane facade | `crates/ff-control/src/lib.rs` | Central wiring + command/result contracts + health aggregation exports. |
| HTTP API | `crates/ff-api/src/lib.rs` (`run(config)`) | Network entrypoint and router/server startup path. |
| Sessions | `crates/ff-sessions/src/lib.rs` | Session lifecycle, approvals, history, subagent/workspace interfaces. |
| Skills/tools | `crates/ff-skills/src/lib.rs` | Universal skill model, registry/selector/executor, adapter system. |
| Orchestration | `crates/ff-orchestrator/src/lib.rs` | Task decomposition, routing, DAG planning, parallel execution. |
| Runtime engines | `crates/ff-runtime/src/lib.rs` | Inference runtime abstraction (llama.cpp/vLLM/MLX/Ollama). |
| Gateway | `crates/ff-gateway/src/lib.rs` | Multi-channel message transport + router/server. |
| Memory | `crates/ff-memory/src/lib.rs` | Capture, retrieval, RAG, session/workspace memory APIs. |
| Observability | `crates/ff-observability/src/lib.rs` | Telemetry, events, metrics, alerting, dashboard types. |
| Pipeline (gap) | `crates/ff-pipeline/src/lib.rs` | Placeholder today; planned orchestration glue crate. |

## B. Documentation entry points

| Doc | Purpose |
|---|---|
| `docs/PHASE9_SCOPE_RECONCILIATION.md` | Scope truth source: included references, exclusions, and core gap map. |
| `docs/PHASE9_SMOKE_CHECKLIST.md` | Exact smoke procedure and historical pass/fail evidence. |
| `docs/PHASE10_RELEASE_READINESS.md` | Crate inventory + readiness gates + release blockers. |
| `docs/PHASE10_EXECUTION_BACKLOG.md` | Ordered ticket queue (implementation sequence). |
| `docs/PHASE10_API_SURFACE.md` | Public API inventory and stability tier recommendations. |
| `docs/PHASE10_API_GOVERNANCE.md` | API compatibility/deprecation policy for v0.1.x. |
| `docs/PHASE10_SHIP_PLAN.md` | Merge/tag/rollback playbook for internal release. |

---

## 3) Daily workflow commands

Run from repo root.

### A. Core dev loop

```bash
# 1) Build health
cargo check --workspace

# 2) Library tests
cargo test --workspace --lib

# 3) CLI contract smoke
cargo run -p ff-cli -- --help
```

### B. Quick CLI behavior checks

```bash
cargo run -p ff-cli -- status
cargo run -p ff-cli -- nodes
cargo run -p ff-cli -- models
cargo run -p ff-cli -- health
```

### C. Capture reproducible smoke artifacts

```bash
mkdir -p .phase9-smoke
cargo check --workspace > .phase9-smoke/cargo_check.log 2>&1
cargo test --workspace --lib > .phase9-smoke/cargo_test_workspace_lib.log 2>&1
cargo run -p ff-cli -- --help > .phase9-smoke/ff_cli_help.log 2>&1
```

### D. Pre-merge release-gate mini checklist

```bash
cargo check --workspace
cargo test --workspace --lib
cargo run -p ff-cli -- --help
git status --short
```

If touching APIs, also review and update:
- `docs/PHASE10_API_SURFACE.md`
- `docs/PHASE10_API_GOVERNANCE.md`

---

## 4) Troubleshooting quick map

| Symptom | Likely cause | Where to look | Fastest recovery |
|---|---|---|---|
| `cargo check --workspace` fails with missing module (`E0583`) | `mod` declarations without corresponding files | Crate `src/lib.rs` and missing module files (historically hit in `ff-control`) | Add missing module files or remove stale `mod` declarations; rerun check. |
| CLI looks incomplete / root binary prints `Hello, world!` | Root `src/main.rs` not wired yet (FF10-001) | `src/main.rs`, `crates/ff-cli/src/main.rs` | Use `cargo run -p ff-cli -- ...` for actual CLI until root wiring is implemented. |
| API run path unclear | API entrypoint is in crate, not root binary | `crates/ff-api/src/lib.rs` | Start from `run(config)` and trace through `server::build_http_router`. |
| Orchestration flow feels fragmented | Pipeline integration crate still placeholder | `crates/ff-pipeline/src/lib.rs`, `docs/PHASE10_EXECUTION_BACKLOG.md` (FF10-003) | Implement FF10-003 stage contracts before deep feature work. |
| Session/tool behavior inconsistent across crates | Integration path not fully end-to-end yet | `ff-sessions`, `ff-skills`, `ff-control`, FF10 backlog dependencies | Work backlog in order; avoid skipping dependency chain. |
| “Release ready?” uncertainty | Some readiness gates still open (tracking + CI + scaffolds) | `docs/PHASE10_RELEASE_READINESS.md` | Close gate checklist there before tagging `v0.1.0-internal`. |

---

## 5) First-week execution priority (recommended)

1. Land FF10-001 (root binary wiring) and FF10-002 (real control handlers).
2. Build FF10-003 pipeline skeleton with typed stage contracts/tests.
3. Continue strict ticket order from Phase 10 backlog.
4. Keep smoke logs updated after each meaningful integration step.

This keeps risk low and avoids rework from out-of-order integration.
