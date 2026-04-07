# Phase 10 Execution Backlog (v0.1 Integration Path)

Date: 2026-04-04  
Input: `docs/PHASE9_SCOPE_RECONCILIATION.md`

Execution rule: **run tickets in the exact ID order below**.

## 1) Strict execution order (fastest path)

| Order | ID | Crate | Summary | Complexity | Dependencies |
|---|---|---|---|---|---|
| 1 | FF10-001 | `ff-cli` | Replace top-level `src/main.rs` hello-world with real CLI entrypoint wiring. | S | None |
| 2 | FF10-002 | `ff-control` | Implement production command handlers (discover/run/status/health) backed by real subsystem handles. | M | FF10-001 |
| 3 | FF10-003 | `ff-pipeline` | Implement pipeline stages/types/errors/events (session input → tool/orchestrator/runtime output). | L | FF10-002 |
| 4 | FF10-004 | `ff-sessions` | Connect session lifecycle to `ff-pipeline` request/response flow (including subagent context handoff). | M | FF10-003 |
| 5 | FF10-005 | `ff-skills` | Add first-class built-in tool runtime set (read/write/edit/exec/web-search/web-fetch) with registry bindings. | L | FF10-004 |
| 6 | FF10-006 | `ff-security` | Enforce approval/rate-limit policy on built-in tool execution paths. | M | FF10-005 |
| 7 | FF10-007 | `ff-runtime` | Add cloud provider adapter baseline (OpenAI-compatible first, Bedrock second). | L | FF10-006 |
| 8 | FF10-008 | `ff-orchestrator` | Add routing policy for local-vs-cloud model/provider selection and fallback. | M | FF10-007 |
| 9 | FF10-009 | `ff-skills` | Implement MCP server lifecycle manager (start/stop/health/capability cache) and registry integration. | M | FF10-006 |
| 10 | FF10-010 | `ff-memory` | Add SQLx migration assets and startup migration execution for session/tool/event persistence. | M | FF10-004 |
| 11 | FF10-011 | `ff-observability` | Emit pipeline/tool/runtime events + health metrics with correlation IDs. | M | FF10-008, FF10-009, FF10-010 |
| 12 | FF10-012 | `ff-api` | Expose integrated control-plane endpoints (run task, status, health, recent events). | M | FF10-011 |
| 13 | FF10-013 | `ff-control` | Add end-to-end integration suite and v0.1 smoke flow (`discover -> select -> run -> observe -> persist`). | M | FF10-012 |

---

## 2) Tickets grouped by crate

### `ff-cli`

#### FF10-001
- **Summary:** Replace top-level binary stub with real CLI bootstrap into `ff-cli`/`ff-control`.
- **Acceptance criteria:**
  - Root `src/main.rs` delegates to CLI command runner.
  - `cargo run -- --help` and `cargo run -- status` execute through real command path.
  - No `Hello, world!` placeholder remains in executable path.
- **Complexity:** S
- **Dependencies:** None

---

### `ff-control`

#### FF10-002
- **Summary:** Implement production command handlers for discover/run/status/health using real subsystem handles.
- **Acceptance criteria:**
  - `ControlCommand` variants execute against concrete subsystem implementations (not placeholders).
  - Health aggregation returns structured status for discovery/runtime/scheduler.
  - Failure paths return typed `ControlError` with actionable messages.
- **Complexity:** M
- **Dependencies:** FF10-001

#### FF10-013
- **Summary:** Add workspace-level integration tests for full control-plane flow to lock v0.1 behavior.
- **Acceptance criteria:**
  - Integration tests cover: discover nodes/models, route task, execute task, store memory, emit observability events.
  - Smoke test command path documented and reproducible in CI/local.
  - v0.1 checklist references test outputs/log artifacts.
- **Complexity:** M
- **Dependencies:** FF10-012

---

### `ff-pipeline`

#### FF10-003
- **Summary:** Build the pipeline crate from placeholder to orchestration glue.
- **Acceptance criteria:**
  - Define stage interfaces and typed payloads for input/context/tool/runtime/output.
  - Add typed pipeline errors and event envelopes.
  - Add integration tests proving stage chaining with success and failure cases.
- **Complexity:** L
- **Dependencies:** FF10-002

---

### `ff-sessions`

#### FF10-004
- **Summary:** Wire session lifecycle into pipeline execution path.
- **Acceptance criteria:**
  - Session start/continue maps to pipeline request objects.
  - Session metadata (thread/subagent/approval context) is propagated through execution.
  - Session result state persists final status and references event correlation IDs.
- **Complexity:** M
- **Dependencies:** FF10-003

---

### `ff-skills`

#### FF10-005
- **Summary:** Implement built-in tool runtime parity for core tool suite.
- **Acceptance criteria:**
  - Built-ins available via skill registry: read, write, edit, exec, web-search, web-fetch.
  - Tool invocation schema validation and execution contracts are documented in-crate.
  - Pipeline can execute at least one end-to-end tool call from session input.
- **Complexity:** L
- **Dependencies:** FF10-004

#### FF10-009
- **Summary:** Add MCP lifecycle manager and bind MCP tools into active registry.
- **Acceptance criteria:**
  - MCP servers can be started/stopped/health-checked programmatically.
  - Capabilities are cached and refreshed with TTL/invalidations.
  - MCP tools appear in tool selection/execution path with clear error handling.
- **Complexity:** M
- **Dependencies:** FF10-006

---

### `ff-security`

#### FF10-006
- **Summary:** Integrate approvals and rate limits into built-in tool execution.
- **Acceptance criteria:**
  - Policy hooks run before tool execution and can allow/deny/require approval.
  - Rate limits enforce per-session and per-tool ceilings.
  - Approval-required actions emit audit events and return pending states correctly.
- **Complexity:** M
- **Dependencies:** FF10-005

---

### `ff-runtime`

#### FF10-007
- **Summary:** Expand runtime provider breadth with cloud baseline adapters.
- **Acceptance criteria:**
  - OpenAI-compatible adapter added and callable through runtime abstraction.
  - Bedrock adapter added behind feature/config gate.
  - Standardized capability metadata exposed for orchestrator routing.
- **Complexity:** L
- **Dependencies:** FF10-006

---

### `ff-orchestrator`

#### FF10-008
- **Summary:** Implement provider/model routing policy with fallback behavior.
- **Acceptance criteria:**
  - Router selects local vs cloud provider using policy inputs (cost/latency/capability/availability).
  - Fallback route executes on primary failure with bounded retries.
  - Routing decisions are emitted as structured events.
- **Complexity:** M
- **Dependencies:** FF10-007

---

### `ff-memory`

#### FF10-010
- **Summary:** Add migration assets and migration execution path for persisted state.
- **Acceptance criteria:**
  - `migrations/` assets exist for session/tool/event core tables.
  - Startup path runs migrations safely (idempotent, versioned).
  - Session + execution outputs persist and can be queried by ID.
- **Complexity:** M
- **Dependencies:** FF10-004

---

### `ff-observability`

#### FF10-011
- **Summary:** Instrument integrated flow with correlated events and health metrics.
- **Acceptance criteria:**
  - Pipeline/tool/runtime/session events include shared correlation ID.
  - Health metrics available for runtime, tools, queue depth, and error rate.
  - Event stream is consumable by API/status commands.
- **Complexity:** M
- **Dependencies:** FF10-008, FF10-009, FF10-010

---

### `ff-api`

#### FF10-012
- **Summary:** Publish integrated control-plane endpoints for operators/automation.
- **Acceptance criteria:**
  - Endpoints for run-task, status, health, and recent events are available and documented.
  - API handlers call the same `ff-control` command path as CLI.
  - Error and approval-pending responses are structured and stable.
- **Complexity:** M
- **Dependencies:** FF10-011

---

## 3) Definition of done for “fully integrated v0.1”

- FF10-001 through FF10-013 complete in order.
- One documented smoke run succeeds end-to-end: **discover → select provider/model → execute tool-enabled task → observe events → verify persisted memory**.
- `cargo check --workspace`, `cargo test --workspace --lib`, and CLI/API smoke commands all pass on a clean checkout.
