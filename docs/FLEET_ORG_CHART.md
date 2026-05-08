# ForgeFleet Org Chart

> The complete hierarchy of ForgeFleet subsystems, data stores, and compute nodes — in ForgeFleet terminology.

---

## 1. Current Org Chart

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│  🌐 CLIENT LAYER                                            │
│  ├── ff CLI / TUI                                           │
│  ├── Dashboard (Web)                                        │
│  ├── Telegram / Discord Bot                                 │
│  └── MCP Client                                             │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  🛡️ ff-gateway (:51002)                              │    │
│  │  ├── POST /v1/tasks/{type}  ← TaskRouter             │    │
│  │  ├── POST /v1/chat/completions  ← Chat routing       │    │
│  │  ├── POST /v1/embeddings  ← Embedding routing        │    │
│  │  └── JWT Middleware                                  │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  🧠 ORCHESTRATION LAYER (Leader-gated)               │    │
│  │                                                      │    │
│  │  ┌─ SessionRunner ───────────────────────────────┐   │    │
│  │  │  • agent_sessions + agent_steps DAG walker    │   │    │
│  │  │  • Dispatches steps as fleet_tasks shell rows │   │    │
│  │  │  • Tick interval: 5s                          │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ AutoUpgrade ─────────────────────────────────┐   │    │
│  │  │  • Hourly version drift check                  │   │    │
│  │  │  • Enqueues upgrade waves as fleet_tasks      │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Backup Orchestrator ─────────────────────────┐   │    │
│  │  │  • Postgres + Redis backup every 4h            │   │    │
│  │  │  • Distributes to replica nodes                │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Alert Evaluator ─────────────────────────────┐   │    │
│  │  │  • Evaluates alert rules every 60s             │   │    │
│  │  │  • Triggers fleet_tasks for remediation       │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Metrics Downsampler ─────────────────────────┐   │    │
│  │  │  • Compresses Pulse metrics every 60s          │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ SubAgent Reaper ─────────────────────────────┐   │    │
│  │  │  • Resets stuck sub_agents slots every 10min   │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  🗄️  DATA LAYER (Postgres :55432 — Taylor)          │    │
│  │  ⚠️  SINGLE POINT OF FAILURE                         │    │
│  │                                                      │    │
│  │  ┌─ fleet_tasks ─────────────────────────────────┐   │    │
│  │  │  • Distributed queue                           │   │    │
│  │  │  • status: pending → running → completed       │   │    │
│  │  │  • Heartbeat + handoff (120s timeout)         │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ agent_sessions / agent_steps ─────────────────┐   │    │
│  │  │  • Project DAG orchestration                   │   │    │
│  │  │  • Dependencies + role assignments             │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ sub_agents ──────────────────────────────────┐   │    │
│  │  │  • Slot registry (idle / busy / error)         │   │    │
│  │  │  • Per-computer workspace directories          │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ work_items / work_outputs ───────────────────┐   │    │
│  │  │  • Deliverables + provenance                   │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ computers / fleet_nodes / fleet_models ───────┐   │    │
│  │  │  • Node catalog + model catalog                  │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ fleet_leader_state ───────────────────────────┐   │    │
│  │  │  • Elected leader (Taylor)                     │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  💻 WORKER LAYER (Every Node)                        │    │
│  │                                                      │    │
│  │  ┌─ TaskRunner ───────────────────────────────────┐   │    │
│  │  │  • Polls fleet_tasks every 10s                 │   │    │
│  │  │  • Claims via FOR UPDATE SKIP LOCKED           │   │    │
│  │  │  • Executes shell with $FF_* env               │   │    │
│  │  │  • Heartbeat every 30s                         │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ SubAgent Slots ──────────────────────────────┐   │    │
│  │  │  • In-memory pool (Mutex<Vec<bool>>)           │   │    │
│  │  │  • Workspace: ~/.forgefleet/sub-agent-{N}/     │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ InferenceRouter ─────────────────────────────┐   │    │
│  │  │  • Local-first endpoint selection              │   │    │
│  │  │  • Fleet fallback (Pulse health)              │   │    │
│  │  │  • Cloud fallback (last resort)               │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Pulse Beat Emitter ──────────────────────────┐   │    │
│  │  │  • Emits health every 10s to Redis            │   │    │
│  │  │  • CPU, memory, GPU, active models            │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ☁️  CLOUD FALLBACK                                         │
│  ├── OpenAI (GPT-5)                                         │
│  ├── Anthropic (Claude Opus)                                │
│  └── Google (Gemini Pro)                                    │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

---

## 2. What's Missing (Unwired Code)

These components exist in the codebase but are **not spawned in src/main.rs**:

```
┌─────────────────────────────────────────────────────────────┐
│  📦 UNWIRED COMPONENTS (Library-only)                        │
│                                                             │
│  ┌─ AgentCoordinator ───────────────────────────────────┐   │
│  │  crates/ff-agent/src/agent_coordinator.rs            │   │
│  │  • work_items → sub_agents slot → HTTP LLM call     │   │
│  │  • Uses PulseReader for health + endpoint selection │   │
│  │  • Persists to work_outputs                         │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                             │
│  ┌─ OrchestratorAgent ──────────────────────────────────┐   │
│  │  crates/ff-agent/src/orchestrator_agent.rs           │   │
│  │  • analyze_task() — keyword matching (NOT LLM)      │   │
│  │  • select_nodes() — capability-based routing        │   │
│  │  • orchestrate() — single-node or parallel dispatch │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                             │
│  ┌─ MultiAgentOrchestrator ─────────────────────────────┐   │
│  │  crates/ff-agent/src/multi_agent.rs                  │   │
│  │  • run_parallel() — fan out N agents                │   │
│  │  • VerificationPipeline — code→test→verify          │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                             │
│  ┌─ ff-mesh (entire crate) ─────────────────────────────┐   │
│  │  crates/ff-mesh/src/                                 │   │
│  │  • WorkerAgent — registration + heartbeat            │   │
│  │  • LeaderDaemon — in-memory worker tracking          │   │
│  │  • TaskScheduler — load-aware scoring                │   │
│  │  • WorkQueue — in-memory priority queue              │   │
│  │  • ElectionManager — failover logic                  │   │
│  │  ⚠️  NO HTTP transport wired                        │   │
│  │  ⚠️  NO Postgres integration                        │   │
│  │  ⚠️  NO crate dependency from forgefleetd           │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                             │
│  ┌─ ff-orchestrator (partially used) ───────────────────┐   │
│  │  crates/ff-orchestrator/src/                         │   │
│  │  • ParallelExecutor — stage-ordered dispatch         │   │
│  │  • TaskDecomposer — template-based splitting         │   │
│  │  • Crew — role definitions                           │   │
│  │  • Planner — DAG execution planning                  │   │
│  │  (Imported by ff-agent lib but not daemon-spawned)   │   │
│  └───────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. Target Org Chart (After Migration)

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│  🌐 CLIENT LAYER                                            │
│  ├── ff CLI / TUI                                           │
│  ├── Dashboard (Web)                                        │
│  ├── Telegram / Discord Bot                                 │
│  └── MCP Client                                             │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  🛡️ ff-gateway                                       │    │
│  │  ├── :51002 — Synchronous (chat / tasks / embed)     │    │
│  │  │   ├── TaskRouter (8 types + capability routing)   │    │
│  │  │   ├── ChatCompletionsRouter                       │    │
│  │  │   └── EmbeddingRouter                             │    │
│  │  ├── :51004 — 🆕 Async Job Server                    │    │
│  │  │   └── POST /v1/jobs → ticket + poll/webhook      │    │
│  │  └── JWT Middleware                                  │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  🧠 ORCHESTRATION LAYER (Leader-gated)               │    │
│  │                                                      │    │
│  │  ┌─ 🆕 Orchestration LLM ──────────────────────────┐   │    │
│  │  │  • Model: qwen3.5-9b or gemma-4 (local)         │   │    │
│  │  │  • Input: user prompt                           │   │    │
│  │  │  • Output: JSON plan                            │   │    │
│  │  │    { task_type, complexity, parallelism,        │   │    │
│  │  │      review_required, preferred_capabilities,    │   │    │
│  │  │      subtasks: [{name, capability, deps}] }     │   │    │
│  │  │  • Replaces keyword matcher in                  │   │    │
│  │  │    orchestrator_agent::analyze_task()           │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ SessionRunner (enhanced) ─────────────────────┐   │    │
│  │  │  • agent_sessions + agent_steps DAG walker      │   │    │
│  │  │  • 🆕 Review gate enforcement                   │   │    │
│  │  │  • 🆕 Native LLM dispatch (no shell subprocess) │   │    │
│  │  │  • Budget watchdog per session                  │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ AgentCoordinator (wired) ─────────────────────┐   │    │
│  │  │  • work_items → sub_agents slot claim          │   │    │
│  │  │  • PulseReader for LLM endpoint selection      │   │    │
│  │  │  • HTTP POST to node LLM server                │   │    │
│  │  │  • Persist result to work_outputs              │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ MultiAgentOrchestrator (wired) ───────────────┐   │    │
│  │  │  • Parallel fan-out across fleet nodes         │   │    │
│  │  │  • 🆕 VerificationPipeline (code→test→verify)  │   │    │
│  │  │  • Result aggregation + synthesis              │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ AutoUpgrade ──────────────────────────────────┐   │    │
│  │  │  • Hourly version drift check                  │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Backup Orchestrator ──────────────────────────┐   │    │
│  │  │  • Postgres + Redis backup every 4h            │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Alert Evaluator ──────────────────────────────┐   │    │
│  │  │  • Evaluates alert rules every 60s             │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Metrics Downsampler ──────────────────────────┐   │    │
│  │  │  • Compresses Pulse metrics every 60s          │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ SubAgent Reaper ──────────────────────────────┐   │    │
│  │  │  • Resets stuck sub_agents slots every 10min   │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  🗄️  DATA LAYER (Postgres HA)  🟢                    │    │
│  │                                                      │    │
│  │  ┌─ Primary: Taylor (:55432) ─────────────────────┐   │    │
│  │  │  • Read + write                                │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │  ┌─ Hot Standby: Marcus ──────────────────────────┐   │    │
│  │  │  • Streaming replication from Taylor            │   │    │
│  │  │  • 🆕 Auto-promote on Taylor failure (Patroni) │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │  ┌─ Patroni + etcd (3-node consensus) ────────────┐   │    │
│  │  │  • Taylor, Marcus, Sophie                      │   │    │
│  │  │  • Health checks every 5s                      │   │    │
│  │  │  • Automatic failover ~15s                     │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ fleet_tasks ──────────────────────────────────┐   │    │
│  │  │  • 🆕 complexity column (1-10)                 │   │    │
│  │  │  • status: pending → running → completed       │   │    │
│  │  │  • Heartbeat + handoff (120s timeout)         │   │    │
│  │  │  • 🆕 Load-aware claim scoring                 │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ agent_sessions / agent_steps ─────────────────┐   │    │
│  │  │  • Project DAG orchestration                   │   │    │
│  │  │  • 🆕 review_required flag                     │   │    │
│  │  │  • 🆕 review_status tracking                   │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ sub_agents ───────────────────────────────────┐   │    │
│  │  │  • Slot registry (idle / busy / error)         │   │    │
│  │  │  • Per-computer workspace directories          │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ work_items / work_outputs ───────────────────┐   │    │
│  │  │  • Deliverables + provenance                   │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ 🆕 async_jobs ────────────────────────────────┐   │    │
│  │  │  • Async chat + task queue                     │   │    │
│  │  │  • Priority = 100 (highest)                    │   │    │
│  │  │  • Poll or webhook on completion               │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ computers / fleet_nodes / fleet_models ───────┐   │    │
│  │  │  • Node catalog + model catalog                  │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ fleet_leader_state ───────────────────────────┐   │    │
│  │  │  • Elected leader                              │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  💻 WORKER LAYER (Every Node)                        │    │
│  │                                                      │    │
│  │  ┌─ TaskRunner (enhanced) ────────────────────────┐   │    │
│  │  │  • Polls fleet_tasks + async_jobs every 10s    │   │    │
│  │  │  • 🆕 Load-aware claim scoring                 │   │    │
│  │  │  • Claims via FOR UPDATE SKIP LOCKED           │   │    │
│  │  │  • Executes shell with $FF_* env               │   │    │
│  │  │  • Heartbeat every 30s                         │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ SubAgent Slots ──────────────────────────────┐   │    │
│  │  │  • In-memory pool                              │   │    │
│  │  │  • Workspace: ~/.forgefleet/sub-agent-{N}/     │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ InferenceRouter ─────────────────────────────┐   │    │
│  │  │  • Local-first endpoint selection              │   │    │
│  │  │  • Fleet fallback (Pulse health)              │   │    │
│  │  │  • Cloud fallback (last resort)               │   │    │
│  │  │  • 60s failure cooldown + auto-heal           │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ 🆕 Delegate Server (:51003) ──────────────────┐   │    │
│  │  │  • POST /v1/internal/delegate                  │   │    │
│  │  │  • Accepts tasks from other nodes              │   │    │
│  │  │  • Streams SSE progress back                   │   │    │
│  │  │  • LAN-only binding (192.168.5.x)             │   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  │                                                      │    │
│  │  ┌─ Pulse Beat Emitter ──────────────────────────┐   │    │
│  │  │  • Emits health every 10s to Redis            │   │    │
│  │  │  • CPU, memory, GPU, active models            │   │    │
│  │  │  • 🆕 Yield mode (Protected/Interactive/Assist)│   │    │
│  │  └───────────────────────────────────────────────┘   │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  ☁️  CLOUD FALLBACK (Last Resort)                           │
│  ├── OpenAI (GPT-5)                                         │
│  ├── Anthropic (Claude Opus)                                │
│  └── Google (Gemini Pro)                                    │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

---

## 4. Per-Node Breakdown

### Taylor (Leader + Primary Postgres)

```
💻 Taylor
├── 🟢 Postgres Primary (:55432)
├── 🟢 Patroni + etcd
├── 🟢 Gateway (:51002)
├── 🟢 SessionRunner
├── 🟢 AutoUpgrade
├── 🟢 Backup Orchestrator
├── 🟢 Alert Evaluator
├── 🟢 Metrics Downsampler
├── 🟢 SubAgent Reaper
├── 🟢 TaskRunner
├── 🆕 Orchestration LLM (qwen3.5-9b)
├── 🆕 AgentCoordinator
├── 🆕 Async Job Server (:51004)
├── 🆕 Delegate Server (:51003)
├── 🟢 InferenceRouter
├── 🟢 Pulse Beat Emitter
├── 🟢 Local LLMs (:55000-55003)
│   ├── qwen36-35b-a3b
│   └── gemma-4-31b-it
└── 🟢 SubAgent Slots (8)
```

### Marcus (Standby Postgres + Worker)

```
💻 Marcus
├── 🟢 Postgres Standby (streaming from Taylor)
├── 🟢 Patroni + etcd
├── 🟢 TaskRunner
├── 🆕 Delegate Server (:51003)
├── 🟢 InferenceRouter
├── 🟢 Pulse Beat Emitter
├── 🟢 Local LLM (:55000)
│   └── qwen3-coder-30b-a3b
└── 🟢 SubAgent Slots (4)
```

### Sophie (Worker)

```
💻 Sophie
├── 🟢 TaskRunner
├── 🆕 Delegate Server (:51003)
├── 🟢 InferenceRouter
├── 🟢 Pulse Beat Emitter
├── 🟢 Local LLM (:55000)
│   └── qwen3-coder-30b-a3b
└── 🟢 SubAgent Slots (4)
```

### Any Follower Node (e.g. Adele, Beyoncé)

```
💻 Any Follower
├── 🟢 TaskRunner
├── 🆕 Delegate Server (:51003)
├── 🟢 InferenceRouter
├── 🟢 Pulse Beat Emitter
├── 🟢 Local LLM (if loaded)
└── 🟢 SubAgent Slots (cores/4, max 4)
```

---

## 5. Data Flows

### Flow 1: Simple Chat (Synchronous)

```
Client → Gateway (:51002)
  → TaskRouter::handle_task()
    → Capability check (fleet_model_catalog)
    → PulseReader::list_servers() — find healthy nodes
    → Score candidates (capability match + queue_depth)
    → HTTP POST to best node LLM (:55000)
    ← Response
  ← Response
← Client

Fallback path:
  If no fleet match → cloud_llm::try_route_to_cloud()
  If cloud fails → 503 (before Phase 6)
  If cloud fails → enqueue async_jobs (after Phase 6)
```

### Flow 2: Complex Project (Session + LLM Planner)

```
Client → Gateway (:51002) → POST /v1/orchestrate
  → OrchestratorAgent::orchestrate(prompt)
    → Orchestration LLM (qwen3.5-9b) analyzes prompt
    ← Returns JSON plan: { task_type, complexity, subtasks[] }

  → SessionRunner::create_session(goal, plan)
    → INSERT agent_sessions (goal, team, budget_usd_cap)
    → INSERT agent_steps (plan subtasks as steps)
    → Step 1: dispatch as fleet_task
      → TaskRunner claims → executes
      ← result back to agent_steps
    → Step 2+3: dispatched in parallel (deps satisfied)
      → TaskRunner claims → executes
      ← results back
    → Step 4: review gate (review_required=true)
      → Auto-insert reviewer step
      → Dispatch to node with 'review' capability
      ← approved / rejected
    → Step 5: deploy (after review approval)
    → All steps terminal → session status='succeeded'
    → mirror_session_to_vault()
  ← OrchestratedResult JSON
← Client
```

### Flow 3: Direct Node Delegation (Inter-Node RPC)

```
Node A (Taylor) needs Node B (Marcus) to do work

Before Phase 3:
  Taylor → INSERT fleet_tasks (preferred_computer_id = marcus)
  Marcus → polls every 10s → claims → executes
  Latency: 0-10s

After Phase 3:
  Taylor → HTTP POST to Marcus:51003/v1/internal/delegate
    Body: { task_id, task_type, payload, timeout_secs }
  Marcus → accepts → executes → streams SSE progress
  Taylor ← receives result via SSE
  Latency: ~50ms + execution
```

### Flow 4: Async Chat (After Phase 6)

```
Client → Gateway (:51002)
  → POST /v1/chat/completions?async=true
    → Creates async_jobs row
    ← Returns: { job_id: "ff-4821", status: "queued" }

  (Background)
  TaskRunner → claims async_jobs row
  TaskRunner → HTTP POST to node LLM
  TaskRunner → UPDATE async_jobs (status='completed', result=...)

  (Client retrieves)
  Client → GET /v1/jobs/ff-4821
    ← Returns: { status: 'completed', result: {...} }

  OR (Webhook)
  Postgres → POST webhook_url with result
```

---

## 6. Before vs After (One Table)

| Component | Before | After |
|-----------|--------|-------|
| **Database** | Single Postgres on Taylor | Primary (Taylor) + Hot Standby (Marcus) + Patroni |
| **Failover** | Manual WAN promotion | Auto-failover ~15s |
| **Orchestration** | Keyword matching (`contains("write")`) | LLM-powered JSON plan |
| **Node Delegation** | Postgres polling (10s) | Direct HTTP RPC (~50ms) |
| **Task Claiming** | FIFO (`priority, created_at`) | Load-aware scoring |
| **Review** | None | Auto-enforced reviewer step |
| **Chat Under Load** | 503 if all LLMs down | Async queue + ticket |
| **Session Steps** | Shell subprocess (`ff agent --model ...`) | Native LLM dispatch |
| **Dead Code** | `ff-mesh` maintained | Harvested + archived |

---

## 7. Migration Priority

```
        Phase 1          Phase 2          Phase 3
     ┌─────────┐     ┌─────────┐     ┌─────────┐
     │ Postgres│     │    LLM  │     │   RPC   │
     │    HA   │     │ Planner │     │         │
     └─────────┘     └─────────┘     └─────────┘
     Weeks 1-2       Weeks 3-4       Weeks 5-6

     (ops + docker)   (code)          (code)

        Phase 4          Phase 5          Phase 6
     ┌─────────┐     ┌─────────┐     ┌─────────┐
     │  Load   │     │ Review  │     │  Async  │
     │ Scoring │     │ Pipeline│     │  Jobs   │
     └─────────┘     └─────────┘     └─────────┘
     Weeks 7-8       Weeks 9-10      Weeks 11-12

     (SQL)            (code)          (code)
```

**Phase 1 and Phase 2 are independent** — run in parallel.
