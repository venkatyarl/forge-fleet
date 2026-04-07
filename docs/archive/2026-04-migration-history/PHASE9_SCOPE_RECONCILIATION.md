# Phase 9 Scope Reconciliation (CC_sources → forge-fleet-rs)

Date: 2026-04-04  
Workspace: `/Users/venkat/taylorProjects/forge-fleet`  
Source corpus: `/Users/venkat/Downloads/CC_sources`

## 1) CC_sources top-level structure (what exists)

CC_sources contains 25+ repos that fall into 4 buckets:

1. **Primary architecture/code references**
   - `anthropic-leaked-source-code-main`
   - `claurst-main` (clean-room spec + Rust implementation)
   - `skills-main`
2. **Provider/routing/extensions references**
   - `claude-code-router-main`
   - `claude-multimodel-main`
   - `claude-notifications-go-main`
   - `claude_agent_teams_ui-main`
   - `everything-claude-code-main`
3. **Duplicate/near-duplicate source snapshots and forks**
   - `claude-code-source-code-full-main`, `collection-claude-code-source-code-main`, `claude-source-leaked-main`, `claude-code-rev-main`, `Claude-Code-Compiled-main`, `start-claude-code-main`, `claude-code-working-main`, `anthropic-leaked-source-code-main` mirrors, etc.
4. **Prompt/docs/templates-only collateral**
   - `leaked-system-prompts-main`, `GPTs-main`, `claude-code-system-prompts-main`, `claude-cookbooks-main`, `claude-code-templates-main`, `claude-code-action-main`, `awesome-claude-code-subagents-main`, `Zip files`

---

## 2) Included items (explicit mapping to current Rust crates)

| Included source item | Why included | forge-fleet-rs crate mapping | Current status |
|---|---|---|---|
| `anthropic-leaked-source-code-main` (agent/server/tools/session architecture) | Canonical architecture reference | `ff-sessions`, `ff-security`, `ff-skills`, `ff-gateway`, `ff-agent`, `ff-api`, `ff-memory`, `ff-runtime`, `ff-orchestrator` | **Partially implemented** (good subsystem coverage, but tool-runtime parity not complete) |
| `claurst-main/spec` (clean-room behavior specs) | Safer spec-first reference path | `ff-orchestrator`, `ff-sessions`, `ff-memory`, `ff-security`, `ff-cli` | **Partially implemented** |
| `skills-main` (skill format + lifecycle) | Directly relevant to universal skill system | `ff-skills` | **Implemented core adapters/executor/registry** |
| `claude-code-router-main` (model routing + multi-provider config patterns) | Routing/provider abstraction patterns | `ff-orchestrator`, `ff-api`, `ff-runtime` | **Partially implemented** (routing present; broad provider matrix missing) |
| `claude-multimodel-main` (provider switching/runtime strategy) | Provider selection + runtime ideas | `ff-runtime`, `ff-orchestrator` | **Partially implemented** (local runtimes present; cloud providers mostly absent) |
| `claude-notifications-go-main` (notification/webhook patterns) | Notification/event-delivery reference | `ff-gateway`, `ff-observability` | **Partially implemented** (chat/webhooks exist; desktop notifier plugin layer missing) |
| `claude_agent_teams_ui-main` (multi-agent tasking/approval UX concepts) | Team/session orchestration concepts | `ff-sessions`, `ff-mesh`, `ff-orchestrator`, `ff-control` | **Backend mostly present**, UI/control-plane still incomplete |
| `everything-claude-code-main` (hooks/memory/skills/perf ops patterns) | Operational patterns, skill evolution, harness ops | `ff-evolution`, `ff-memory`, `ff-skills`, `ff-observability`, `ff-benchmark` | **Partially implemented** |

---

## 3) Excluded items (explicit) + rationale

| Excluded item(s) | Rationale |
|---|---|
| `claude-code-source-code-full-main`, `collection-claude-code-source-code-main`, `claude-source-leaked-main`, `claude-code-rev-main`, `Claude-Code-Compiled-main`, `start-claude-code-main`, `claude-code-working-main`, `claude-leaked-files-main` | Largely duplicate/derivative snapshots; adds noise and legal/maintenance risk without new architectural signal. |
| `leaked-system-prompts-main`, `GPTs-main`, `claude-code-system-prompts-main` | Prompt corpora are not required for ForgeFleet infrastructure rewrite scope. |
| `claude-code-templates-main`, `claude-code-action-main`, `awesome-claude-code-subagents-main`, `claude-cookbooks-main` | Useful ecosystem collateral/examples, but not core to ForgeFleet runtime/control-plane implementation scope for Phase 9. |
| `Zip files` | Archive container, not a source of unique implementation requirements. |

---

## 4) Gaps still missing in current Rust workspace

### A. Structural/compilation gaps
1. **`ff-pipeline` is still a placeholder** (`src/lib.rs` one-line placeholder).
2. **`ff-control` is incomplete**: `lib.rs` declares `commands` and `health` modules that do not exist in `src/`.
3. **Top-level binary is not wired** (`src/main.rs` is still `Hello, world!`).

### B. Capability gaps vs included source scope
4. **Tool execution parity gap**: architecture references include concrete tools (bash/read/write/edit/web/search/fetch/etc.); current workspace mainly has permission/executor abstractions (`ff-skills`) but no full built-in tool suite crate.
5. **Provider breadth gap**: current runtime focuses on local engines (llama.cpp/vLLM/MLX/Ollama); major cloud/provider routes seen in router/multimodel references (OpenRouter/DeepSeek/Bedrock/Vertex/Foundry) are not yet represented as first-class runtime/provider modules.
6. **MCP server lifecycle gap**: `ff-skills` can parse/import MCP tool definitions, but dedicated MCP server process management/integration layer is not clearly present.
7. **Control-plane/API integration gap**: many crates are feature-rich libraries, but end-to-end operator surface remains thin (CLI still largely bootstrap/status scaffolding).
8. **DB migration assets gap**: no migrations directory/schema migration flow found despite sqlx-backed subsystems.

### C. Likely intentionally out-of-scope (do not block Phase 9)
9. Rich desktop/Kanban UI parity from `claude_agent_teams_ui-main`.
10. Non-essential novelty systems (e.g., buddy/pet mechanics, prompt-only bundles).

---

## 5) Suggested next implementation order (actionable)

1. **Stabilize compile surface first**
   - Finish `ff-control` module set (`commands`, `health`) or remove declarations temporarily.
   - Replace top-level `src/main.rs` with actual control-plane bootstrap entry.

2. **Implement `ff-pipeline` as orchestration glue**
   - Define pipeline stages connecting `ff-sessions` → `ff-skills`/tool execution → `ff-orchestrator` → `ff-runtime`.
   - Add typed pipeline errors/events and integration tests.

3. **Add a dedicated tool-runtime crate or module cluster**
   - Implement first-class built-ins (read/write/edit/grep/glob/exec/web-fetch/web-search).
   - Wire to `ff-security` approvals/rate limits and `ff-observability` events.

4. **Expand provider/runtime adapters**
   - Add provider modules for at least one cloud path first (e.g., OpenAI-compatible + Bedrock), then Vertex/Foundry.
   - Keep router decisions in `ff-orchestrator`, execution in `ff-runtime`.

5. **Complete MCP lifecycle support**
   - Add MCP server process management (start/stop/health/capability cache) and connect to `ff-skills` registry.

6. **Wire production CLI flows**
   - Route CLI commands through `ff-control` and real subsystem handles (not status placeholders).
   - Validate end-to-end flows: discover → select model → run task → observe → store memory.

7. **Add migrations and system integration tests**
   - Introduce migrations per crate domain and a workspace-level smoke test suite.

---

## Bottom line

The Rust workspace already covers most **architectural domains** (sessions, skills, runtime, orchestration, memory, security, gateway, observability).  
Phase 9 gap is less about adding many new crates and more about **closing integration/completeness gaps**: control-plane wiring, pipeline implementation, built-in tool runtime parity, and provider/MCP expansion.