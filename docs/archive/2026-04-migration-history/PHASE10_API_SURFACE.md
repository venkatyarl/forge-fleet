# Phase 10 — API Surface Inventory (ForgeFleet Rust Rewrite)

Date: 2026-04-04
Scope: workspace crates under `crates/*` (v0.1.0 workspace)

This inventory captures:
1. Public module surface (`pub mod ...`) per crate.
2. Key public types/functions intended as the main API touchpoints.
3. A stability recommendation per crate (`stable` / `beta` / `experimental`) for post-v0.1 consumer guidance.

---

## Stability Snapshot

| Crate | Recommendation | Notes |
|---|---|---|
| ff-core | **stable** | Core primitives, broad API, strong test coverage. |
| ff-runtime | **stable** | Unified runtime abstraction + tested engine/model manager surface. |
| ff-orchestrator | **stable** | Rich decomposition/router/planner API with mature type surface. |
| ff-mesh | **stable** | Leader/worker + scheduler/resource APIs are well-shaped and tested. |
| ff-sessions | **stable** | Comprehensive session/approval/context APIs with substantial test surface. |
| ff-skills | **stable** | Unified skill abstraction with adapters and selector/executor APIs. |
| ff-observability | **stable** | Telemetry/events/metrics/alerting/dashboard APIs are cohesive. |
| ff-cron | **stable** | Scheduler/dispatch/persistence APIs are broad and exercised by tests. |
| ff-api | **beta** | Public server/types surface present; needs more compatibility guarantees. |
| ff-control | **beta** | Good façade surface; still evolving wiring-level contracts. |
| ff-discovery | **beta** | Solid discovery APIs; test/compatibility hardening still needed. |
| ff-evolution | **beta** | Feature-rich loop/analyzer/repair APIs; behavior likely to evolve. |
| ff-gateway | **beta** | Multi-channel API is broad but adapter payload semantics may change. |
| ff-memory | **beta** | Good memory/RAG/session APIs; likely schema/behavior iteration ahead. |
| ff-security | **beta** | Core policy/approval/rate-limit surface exists; tightening expected. |
| ff-ssh | **beta** | Useful connectivity/tunnel/fanout API; operational semantics still maturing. |
| ff-voice | **beta** | Broad STT/TTS/pipeline API; provider integration behavior likely to shift. |
| ff-benchmark | **beta** | Complete report/regression/capacity APIs; may iterate quickly with use. |
| ff-agent | **beta** | Binary daemon crate (internal modules), not a stable library API yet. |
| ff-cli | **beta** | Binary command surface present; no library API contract. |
| ff-deploy | **experimental** | Marked scaffold in manifest/shape; rollout contract likely to change. |
| ff-pipeline | **experimental** | Placeholder crate; no public API yet. |

---

## Crate-by-crate API Surface

## `ff-core` (stable)
- **Public modules:** `activity`, `config`, `db`, `error`, `hardware`, `leader`, `node`, `task`, `types`
- **Key public API:**
  - Errors/config: `ForgeFleetError`, `Result`, `FleetConfig`, `ConfigHandle`, `load_config*`
  - Node/task primitives: `NodeRole`, `ActivityLevel`, `AgentTask`, `AgentTaskKind`, `TaskResult`
  - Activity/hardware: `ActivitySignals`, `ActivityState`, `YieldMode`, `detect*` hardware helpers

## `ff-runtime` (stable)
- **Public modules:** `detector`, `engine`, `error`, `llamacpp`, `mlx`, `model_manager`, `ollama`, `vllm`
- **Key public API:**
  - Core traits/types: `InferenceEngine`, `EngineConfig`, `EngineStatus`, `RuntimeError`
  - Runtime selection: `RuntimeRecommendation`, `recommend`, `create_engine*`
  - Model mgmt: `ModelManager`, `ManagedModel`

## `ff-orchestrator` (stable)
- **Public modules:** `crew`, `decomposer`, `parallel`, `planner`, `router`
- **Key public API:**
  - Crew/decomposition: `AgentRole`, `CrewAssignment`, `CrewDefinition`, `SubTask`, `TaskDecomposition`
  - Planning/execution: `ExecutionPlan`, `PlanNode`, `PlanStage`, `ParallelExecutor`, `ExecutionResult`
  - Routing: `TaskRouter`, `RouteDecision`, `ModelScore`

## `ff-mesh` (stable)
- **Public modules:** `election`, `leader`, `resource_pool`, `scheduler`, `work_queue`, `worker`
- **Key public API:**
  - Top-level services: `ElectionManager`, `LeaderDaemon`, `ResourcePool`, `TaskScheduler`, `WorkQueue`, `WorkerAgent`
  - Queue/cluster types: `QueueEntry`, `TaskPriority`, `TaskState`, `NodeResources`, `FleetResourceSummary`

## `ff-sessions` (stable)
- **Public modules:** `approval`, `context`, `history`, `session`, `subagent`, `workspace`
- **Key public API:**
  - Session lifecycle: `Session`, `SessionState`, `SessionStore`
  - Approvals/context: `ApprovalManager`, `Approval`, `SecurityMode`, `AskMode`, `ContextManager`, `ContextBudget`
  - Sub-agent/history/workspace: `SubAgentManager`, `SubAgentStatus`, `HistoryStore`, `WorkspaceManager`

## `ff-skills` (stable)
- **Public modules:** `adapters`, `error`, `executor`, `loader`, `registry`, `selector`, `types`
- **Key public API:**
  - Skill model: `SkillMetadata`, `ToolDefinition`, `ToolInvocation`, `ToolExecutionResult`
  - Registry/selection/execution: `SkillRegistry`, `SkillSelector`, `ScoredSkill`, `SkillExecutor`, `ExecutorConfig`
  - Adapters/import: `SkillAdapter`, `McpAdapter`, `ClaudeAdapter`, `OpenClawAdapter`, `CustomAdapter`, `auto_import`

## `ff-observability` (stable)
- **Public modules:** `alerting`, `dashboard`, `events`, `log_ingest`, `metrics`, `telemetry`
- **Key public API:**
  - Alerts/events: `AlertEngine`, `AlertRule`, `AlertSeverity`, `FleetEvent`, `EventSink`
  - Metrics/dashboard: `MetricsCollector`, `FleetMetrics`, `NodeMetrics`, `DashboardState`, `FleetSnapshot`
  - Telemetry/logs: `TelemetryConfig`, `init_telemetry`, `LogIngestor`, `LogEntry`, `LogLevel`

## `ff-cron` (stable)
- **Public modules:** `dispatcher`, `engine`, `heartbeat`, `job`, `persistence`, `policy`, `schedule`
- **Key public API:**
  - Scheduling core: `CronSchedule`, `CronEngine`, `EngineError`
  - Job model: `JobDefinition`, `JobTask`, `JobMetadata`, `RetryPolicy`, `RunStatus`
  - Dispatch/persistence/policy: `Dispatcher`, `DispatchRequest`, `CronPersistence`, `SchedulingPolicy`, `QuietHoursPolicy`

## `ff-api` (beta)
- **Public modules:** `config`, `error`, `registry`, `router`, `server`, `types`
- **Key public API:**
  - Runtime entrypoint: `run(config: ApiConfig)`
  - HTTP/server surface: `ApiConfig`, `build_http_router`, `BackendRegistry`, `ModelRouter`
  - Schema types: `ChatCompletionRequest`, `ChatCompletionResponse`, `CompletionRequest`, `ModelListResponse`

## `ff-control` (beta)
- **Public modules:** `bootstrap`, `commands`, `control_plane`, `errors`, `health`
- **Key public API:**
  - Bootstrap/health: `BootstrapOptions`, `BootstrapPlan`, `validate_startup_order`, `aggregate_health_snapshot`
  - Command contracts: `ControlCommand`, `DiscoverRequest`, `RunTaskRequest`, `ScheduleRequest`, `DeployRequest`
  - Facade types: `ControlPlane`, `ControlPlaneHandles`, subsystem handles (`Discovery*`, `Runtime*`, etc.)

## `ff-discovery` (beta)
- **Public modules:** `activity`, `hardware`, `health`, `models`, `ports`, `profile`, `registry`, `scanner`
- **Key public API:**
  - Discovery/scanning: `ScannerConfig`, `DiscoveredNode`, `scan_subnet`, `DiscoveryError`
  - Registry/hardware: `NodeRegistry`, `FleetNode`, `HardwareProfile`, `detect_hardware_profile`
  - Health/model probing: `collect_health_snapshot`, `HealthMonitor`, `query_models_endpoints`, known port helpers

## `ff-evolution` (beta)
- **Public modules:** `analyzer`, `backlog`, `learning`, `r#loop`, `repair`, `verification`
- **Key public API:**
  - Analysis/backlog: `FailureAnalyzer`, `AnalysisReport`, `BacklogService`, `BacklogItem`
  - Loop/repair: `EvolutionEngine`, `EvolutionRun`, `RepairPlanner`, `RepairAction`, `RepairStrategy`
  - Verification/learning: `VerificationModel`, `VerificationReport`, `LearningStore`, `LearningRecord`

## `ff-gateway` (beta)
- **Public modules:** `discord`, `embed`, `message`, `router`, `server`, `telegram`, `webhook`
- **Key public API:**
  - Transport clients: `TelegramClient`, `DiscordClient`, associated `*Error` types
  - Shared message model: `IncomingMessage`, `OutgoingMessage`, `Reaction`, `MessageMedia*`
  - Server/router: `GatewayServer`, `GatewayConfig`, `MessageRouter`, `RouteTarget`, `build_router`, `run`

## `ff-memory` (beta)
- **Public modules:** `capture`, `rag`, `retrieval`, `session`, `store`, `workspace`
- **Key public API:**
  - Capture/session: `AutoCaptureEngine`, `CaptureCandidate`, `SessionMemoryManager`, `SessionMemoryItem`
  - Retrieval/RAG: `MemoryRetrievalEngine`, `RetrievalQuery`, `RagEngine`, `RagQuery`, `RagResult`
  - Storage/workspace: `MemoryStore`, `Memory`, `SearchMemoriesParams`, `WorkspaceMemoryManager`

## `ff-security` (beta)
- **Public modules:** `approvals`, `audit`, `policy`, `rate_limit`, `sandbox`, `secrets`
- **Key public API:**
  - Approvals/audit: `ApprovalManager`, `ApprovalRequest`, `ApprovalStatus`, `AuditLog`, `AuditEvent`
  - Policy/rate limit: `PolicyEngine`, `PolicyRule`, `PolicyDecision`, `RateLimiter`, `RateLimitDecision`
  - Sandbox/secrets: `SandboxProfile`, `SandboxMode`, `SecretResolver`, `SecretRef`

## `ff-ssh` (beta)
- **Public modules:** `config`, `connection`, `connectivity`, `key_manager`, `remote_exec`, `tunnel`
- **Key public API:**
  - Config/connection: `FleetSshConfig`, `SshNodeConfig`, `SshConnection`, `SshConnectionOptions`, `SshAuth`
  - Fleet operations: `ConnectivityChecker`, `ConnectivityMatrix`, `RemoteExecutor`, `FanoutCommandResult`
  - Key+tunnel mgmt: `SshKeyManager`, `KeyPair`, `TunnelManager`, `TunnelSpec`, `TunnelHandle`

## `ff-voice` (beta)
- **Public modules:** `audio`, `pipeline`, `stt`, `tts`, `twilio`, `wake_word`
- **Key public API:**
  - Core error/result: `VoiceError`, `Result<T>`
  - Voice pipeline: `VoicePipeline`, `VoicePipelineConfig`, `PipelineEvent`, `ConversationTurn`
  - Engines/integrations: `SttEngine`, `WhisperApiClient`, `TtsEngine`, `ElevenLabsClient`, `TwilioClient`, `WakeWordDetector`

## `ff-benchmark` (beta)
- **Public modules:** `capacity`, `collector`, `regression`, `report`, `runner`, `scenarios`
- **Key public API:**
  - Scenarios/runner: `BenchmarkScenario`, `ScenarioKind`, `BenchmarkRunner`, `RunnerConfig`
  - Metrics/reports: `RequestSample`, `MetricSummary`, `BenchmarkReport`, `ScenarioReport`
  - Regression/capacity: `detect_regressions`, `RegressionReport`, `plan_capacity`, `CapacityPlan`

## `ff-agent` (beta)
- **Public modules:** _none (binary crate; internal `mod` declarations only)_
- **Key public API (internal-to-binary):** `AgentConfig`, `LeaderClient`, `AgentState`, `build_router`, activity helpers
- **Note:** Current surface is daemon executable behavior rather than reusable library API.

## `ff-cli` (beta)
- **Public modules:** _none (binary crate)_
- **Key public API:** CLI command surface (`start`, `agent`, `status`, `nodes`, `models`, `proxy`, `discover`, `health`, `config`, `version`)
- **Note:** API stability here is command contract compatibility, not Rust library exports.

## `ff-deploy` (experimental)
- **Public modules:** `deployer`, `health_gate`, `release`, `rollback`, `rollout`, `strategy`
- **Key public API:** `DeploymentOrchestrator`, `DeploymentAdapter`, `RolloutPlanner`, `RollbackPlanner`, `HealthGate`
- **Reason for status:** Explicit scaffold positioning + likely near-term contract churn.

## `ff-pipeline` (experimental)
- **Public modules:** _none_
- **Key public API:** _none (placeholder crate)_
- **Reason for status:** No exposed API yet.

---

## Immediate Post-v0.1 Hardening Priorities

1. **Publish explicit API contracts for stable crates**
   - Add `#[doc]` API guarantees and semver notes for `ff-core`, `ff-runtime`, `ff-orchestrator`, `ff-mesh`, `ff-sessions`, `ff-skills`, `ff-observability`, `ff-cron`.

2. **Lock external compatibility surfaces**
   - Pin and document wire contracts for `ff-api` request/response DTOs and `ff-gateway` normalized message schemas.

3. **Promote beta crates with focused test hardening**
   - Add contract/integration tests for crates currently light on tests (`ff-discovery`, `ff-memory`, `ff-ssh`, `ff-gateway`, `ff-api`).

4. **Formalize binary crate compatibility policy**
   - For `ff-cli`/`ff-agent`, define which command flags/output fields are stable vs. best-effort.

5. **Resolve experimental placeholders quickly**
   - Either define initial API for `ff-pipeline` and non-scaffold guarantees for `ff-deploy`, or keep clearly marked internal/unstable until v0.2.
