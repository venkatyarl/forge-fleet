# E4: `endpoints-no-guard` triage

Date: 2026-07-20  
Source work item: `b19c5264-d3b0-4587-81a9-4f05108086eb`

## Result

The recovered item referred to roughly 784 findings in the old Taylor database. That snapshot is not present in the current database, so this triage reran `ff cortex audit --corpus fb --format json` against the current `fb` corpus and reviewed all 255 current `endpoints-no-guard` findings. The rule is intentionally a needs-verification heuristic: it reports handlers without a direct `guarded_by` edge and cannot currently propagate router-level middleware or prove that a router is reachable.

Three real gaps were promoted to high-priority `forge-fleet` work items:

| Work item | Promoted gap |
| --- | --- |
| `095959d6-632a-4296-821a-878c999450cd` | Gateway auth is applied globally, but the default listener is `0.0.0.0:8787` and an unset `FF_JWT_SECRET` permits anonymous reads and selected cost-bearing LLM POSTs. |
| `655d809d-b1f7-447e-a5eb-f6d31e7aa360` | `ff-agent` exposes task assignment, agent messaging, and task/status data without authentication on wildcard listeners. |
| `7ae65311-a03d-427b-9b0f-6d6e84db0d87` | `ff-api` defaults to a wildcard listener and exposes inference and mutable work-queue routes without authentication, with permissive CORS. |

## Disposition

| Findings | Disposition | Evidence |
| ---: | --- | --- |
| 227 | Dismiss the per-handler finding; retain one promoted router-policy gap. | The `ff-gateway`, `ff-mc`, and `ff-mcp` handlers are merged before `jwt_auth_middleware` is layered over the completed router in `crates/ff-gateway/src/server.rs`. Mission Control therefore inherits the same gate. The middleware enforces JWT/RBAC when configured and rejects ordinary mutations without a secret, but its secret-unset policy and wildcard default need the promoted hardening item above. |
| 7 | Promote as one `ff-agent` control-plane gap. | `crates/ff-agent/src/main.rs::run_http_server` binds `[0,0,0,0]` and serves unauthenticated `POST /assign` and `POST /agent/message`. A second router in `crates/ff-agent/src/lib.rs` binds `0.0.0.0:50002` and exposes messaging and task data. Health alone is an intentional public exception. |
| 10 | Promote as one `ff-api` gap. | `crates/ff-api/src/config.rs::ApiConfig::from_env` defaults `FF_API_HOST` to `0.0.0.0`. `crates/ff-api/src/server.rs::build_http_router` has no auth layer, includes work-queue mutations and inference, and uses `CorsLayer::permissive()`. Health/metrics may remain explicitly public. |
| 8 | Dismiss as currently unreachable library routes. | `ff_observability::dashboard::DashboardState::router` is read-only telemetry and Cortex reports no listener/caller that serves this router. Re-triage if it is mounted by a future binary. |
| 2 | Dismiss as currently unreachable library routes. | `ra2a::server::routes` has no indexed caller/listener. The task submission route must be authenticated if this router is mounted later. |
| 1 | Dismiss as intentional health endpoint. | The standalone `ff-cli` `/mcp/health` handler is liveness-only and exposes no control or private state. |

Total: 255 findings; every current finding is accounted for exactly once.

## False-positive cause and follow-up

The dominant false-positive class is structural rather than handler-specific: Axum middleware is attached after nested routers are merged, while the audit only looks for a direct handler-to-gate edge. Cortex should propagate a `guarded_by` edge from a router layer to endpoints mounted beneath that router, while retaining the middleware's explicit public-route exceptions. Reachability should also distinguish a router builder with no listener/caller from a deployed endpoint. Those audit-quality improvements are useful, but they do not block the three security remediations promoted above.

The promotion boundary was challenged with `ff council --members codex,kimi`; Codex independently selected the `ff-agent` and `ff-api` gaps and the same dismissal boundary for inactive library routers. Kimi was unavailable on this host. Direct source review then added the gateway secret-unset/wildcard-listener policy gap, which a simple middleware-presence check would otherwise conceal.
