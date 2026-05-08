# ForgeFleet Comprehensive Audit Report

**Date:** 2026-05-04
**Scope:** ~33 Rust crates + Dashboard
**Audits Completed:** 3 of 4 (API Gateway, Performance, Security — Code Quality still in progress)

---

## Executive Summary

| Severity | API Gateway | Performance | Security | Total |
|----------|-------------|-------------|----------|-------|
| P0 / CRITICAL | 2 | 0 | 1 | **3** |
| P1 / HIGH | 3 | 14 | 14 | **31** |
| P2 / MEDIUM | 5 | 24 | 13 | **42** |
| P3 / LOW | 0 | 12 | 9 | **21** |
| **Total** | **10** | **50+** | **37** | **97+** |

---

## P0 / CRITICAL Issues (Fix Immediately)

### P0-1: API 404s Return HTML (Gateway)
- **File:** `crates/ff-gateway/src/server.rs:641`
- **Issue:** `app.fallback(crate::static_files::serve_dashboard)` serves `index.html` for **ALL** unknown paths, including `/api/*` routes. API clients expecting JSON get HTML.
- **Fix:** Check path prefix in fallback. If path starts with `/api/`, return JSON 404.

### P0-2: Leader Election POSTs to GET-Only Route (API Mismatch)
- **File:** `crates/ff-gateway/src/server.rs` (route definition) + `crates/ff-agent/src/leader_tick.rs` (caller)
- **Issue:** `/api/fleet/leader` is registered as `GET` only, but leader election sends `POST` announcements. Peers return 405 Method Not Allowed.
- **Fix:** Change route to accept both `GET` and `POST`.

### P0-3: SQL Injection in Tool Registry Search (CRITICAL)
- **File:** `crates/ff-gateway/src/tool_registry_api.rs:91-94`
- **Issue:** `query.name` is directly interpolated into SQL:
  ```rust
  sql.push_str(&format!(" AND tool_name ILIKE '%{}%'", query.name.as_ref().unwrap()));
  ```
- **Fix:** Use parameterized query: `ILIKE '%' || $1 || '%'` with `.bind(name)`.

---

## P1 / HIGH Issues (Fix This Week)

### P1-1: JWT Validation Disabled (Security)
- **File:** `crates/ff-gateway/src/middleware.rs:156-159`
- **Issue:** `validation.validate_exp = false` and `validation.required_spec_claims.clear()`. Tokens never expire.
- **Fix:** Remove both lines. Add explicit `aud`/`iss` validation.

### P1-2: Authentication Globally Optional (Security)
- **File:** `crates/ff-gateway/src/middleware.rs:139-142`
- **Issue:** When `FF_JWT_SECRET` is unset, middleware is a complete no-op. Zero auth on all endpoints by default.
- **Fix:** Make auth mandatory for mutating routes. Apply selectively via `Router::merge`.

### P1-3: GitHub Webhooks Without Signature Verification (Security)
- **File:** `crates/ff-gateway/src/server.rs:780-918`
- **Issue:** `github_webhook_handler` parses JSON and inserts `project_ci_runs` rows without verifying `X-Hub-Signature-256`.
- **Fix:** Verify HMAC-SHA256 signature against configured webhook secret before processing.

### P1-4: Command Injection in Agent Tools (Security)
- **Files:** `crates/ff-agent/src/tools/bash.rs`, `computer.rs`, `multimodal.rs`, `code_quality.rs`, `uiux.rs`, `utility_ext.rs`, `docker_manage.rs`
- **Issue:** 11 tool implementations pass user input through `bash -c` without adequate sanitization. Blacklists are trivially bypassed.
- **Fix:** Replace `bash -c` with direct `std::process::Command` invocations using discrete args. Use `regex` crate for regex ops, `walkdir` for file traversal.

### P1-5: Missing reqwest Timeouts (Performance)
- **Files:** `server.rs`, `embeddings.rs`, `agent_loop.rs`, `handlers.rs`, `research.rs`, `notifications.rs`, and all runtime backends
- **Issue:** 12+ locations use `reqwest::Client::new()` with no timeout (default = infinite). Hanging connections consume resources indefinitely.
- **Fix:** Create a shared client with `timeout(Duration::from_secs(30))`.

### P1-6: Unbounded DB Queries (Performance)
- **Files:** `crates/ff-db/src/queries.rs`
- **Issue:** 15 queries lack `LIMIT` clauses: `pg_list_nodes`, `pg_list_models`, `pg_list_catalog`, `pg_list_brain_vault_nodes_current`, `list_tasks_by_status`, `find_active_sessions`, `ownership_list_stale`, `config_list`, `pg_list_brain_threads`, `pg_list_brain_vault_edges`, `pg_list_brain_knowledge_candidates`, `pg_list_brain_communities`, plus `tool_audit_log`, `fleet_tasks`, `agent_procedures`.
- **Fix:** Add `LIMIT 100` (configurable) to all list queries.

### P1-7: `std::sync::Mutex` Held Across Await Points (Performance)
- **Files:** `crates/ff-agent/src/sub_agents.rs`, `crates/ff-gateway/src/inference_router.rs`, `crates/ff-agent/src/leader_tick.rs`
- **Issue:** Blocking `std::sync::Mutex` guards are held across `await` points, causing thread pool starvation and potential deadlocks.
- **Fix:** Use `tokio::sync::Mutex` for async contexts.

### P1-8: Permissive CORS Applied Globally (Security + Performance)
- **File:** `crates/ff-gateway/src/server.rs:646`
- **Issue:** `CorsLayer::permissive()` on all routes including `/metrics`, admin, MCP. Enables cross-origin attacks.
- **Fix:** Restrict CORS to known dashboard origins. Use `CorsLayer::new().allow_origin(...)`.

### P1-9: Tool Inventory Page is Static (Dashboard)
- **File:** `dashboard/src/pages/ToolInventory.tsx`
- **Issue:** 70+ hardcoded tools, not wired to live `/api/tools` endpoint.
- **Fix:** Replace hardcoded array with `fetch('/api/tools')` + rendering.

### P1-10: WebSocket Heartbeat Never Spawned (Performance)
- **File:** `crates/ff-gateway/src/ws_hub.rs`
- **Issue:** `WsHub::spawn_heartbeat_task()` exists but is never called. Dead WS clients accumulate indefinitely.
- **Fix:** Call `spawn_heartbeat_task()` during `WsHub` initialization.

---

## P2 / MEDIUM Issues (Fix This Sprint)

### P2-1: Dashboard Orphan Pages
- **Files:** `dashboard/src/pages/ChatStudio.tsx` (never imported), `Chats.tsx` (only used in redirects)
- **Issue:** Unused pages increase bundle size and confuse navigation.
- **Fix:** Remove or wire up properly.

### P2-2: Sidebar Missing 12+ Links
- **File:** `dashboard/src/components/Sidebar.tsx`
- **Issue:** Missing: Fleet, topology, model-hub, tools, metrics, config, llm-proxy, audit, updates, versions, mesh, nodes/:nodeId.
- **Fix:** Add navigation links for all existing API endpoints.

### P2-3: WS Reconnect Has No Backoff
- **File:** Dashboard WebSocket client
- **Issue:** Fixed 3-second retry, indefinite. Creates thundering herd on gateway restart.
- **Fix:** Exponential backoff with jitter (1s, 2s, 4s, 8s, cap 30s).

### P2-4: Image/Audio Routes Permanently 501
- **File:** `crates/ff-gateway/src/server.rs`
- **Issue:** `/v1/images/generations` and `/v1/audio/transcriptions` registered but handlers always return NOT_IMPLEMENTED.
- **Fix:** Either implement or remove and return proper 501 with JSON error.

### P2-5: Blocking `std::fs` in Async Contexts (Performance)
- **Files:** 10 files identified in audit
- **Issue:** Blocking filesystem operations in async contexts stall the runtime.
- **Fix:** Replace with `tokio::fs` equivalents.

### P2-6: Unbounded Channels and Caches (Performance)
- **Files:** `crates/ff-gateway/src/server.rs` (web_clients), `ws_hub.rs` (inbound/outbound DashMaps)
- **Issue:** `web_clients` uses unbounded sender, `agent_sessions` never evicted, `LlmRoutingCache` has no eviction.
- **Fix:** Use bounded channels, add TTL eviction to caches.

### P2-7: Dynamic SQL with Limit Interpolation (Security)
- **File:** `crates/ff-gateway/src/pulse_api.rs:787-808`
- **Issue:** `limit` formatted directly into SQL despite being clamped.
- **Fix:** Use `.bind(limit)` for the LIMIT value.

### P2-8: Column Name Interpolation in Alert Evaluator (Security)
- **File:** `crates/ff-agent/src/alert_evaluator.rs:544-549`
- **Issue:** Metric column name interpolated via `format!("SELECT {col}::FLOAT8 AS v ...")`.
- **Fix:** Use static query map or strict allowlist validation.

### P2-9: Secrets Stored as Plaintext (Security)
- **Files:** `crates/ff-db/src/queries.rs:1744-1750`, `crates/ff-terminal/src/main.rs:5345-5352`
- **Issue:** `fleet_secrets` values stored and returned in plaintext. No encryption at rest.
- **Fix:** Encrypt with AES-GCM or KMS.

### P2-10: Agent Session Arbitrary Working Directory (Security)
- **File:** `crates/ff-gateway/src/server.rs:5835-5841`
- **Issue:** `req.working_dir` used directly without validation.
- **Fix:** Canonicalize and check against allowed base directory.

### P2-11: Generic Webhook No Auth (Security)
- **File:** `crates/ff-gateway/src/webhook.rs:81-96`
- **Issue:** `/api/webhook` accepts arbitrary payloads with no verification.
- **Fix:** Add shared-secret header check or IP allowlist.

### P2-12: MCP Dual Exposure (Security)
- **Files:** `crates/ff-gateway/src/server.rs`, `crates/ff-mcp/src/server.rs`
- **Issue:** MCP mounted under `/mcp` AND standalone server on port 50001.
- **Fix:** Consolidate to one endpoint with proper auth.

### P2-13: Task Runner User-Provided Shell (Security)
- **File:** `crates/ff-agent/src/task_runner.rs:831-844`
- **Issue:** `shell` field from task payload can be overridden (e.g., `/tmp/evil`).
- **Fix:** Validate against allowlist: `["/bin/bash", "/bin/sh", "/bin/zsh"]`.

### P2-14: Embedding Model is Stub (Code Quality)
- **File:** `crates/ff-brain/src/embeddings.rs:12`
- **Issue:** TODO to replace deterministic hash with real model.
- **Fix:** Integrate with ONNX runtime or call embedding API.

### P2-15: No Role-Based Access Control (Security)
- **File:** `crates/ff-gateway/src/server.rs:3910-3950`
- **Issue:** `POST /api/config` accepts full config replacement with only JWT validation.
- **Fix:** Add `Scope::Admin` check.

### P2-16: Bootstrap Script Shell Injection (Security)
- **File:** `crates/ff-gateway/src/onboard.rs:152-163`
- **Issue:** Query params substituted into shell script without escaping.
- **Fix:** Shell-escape all values or generate script programmatically.

---

## P3 / LOW Issues (Fix When Convenient)

### P3-1: Hardcoded Default DB Password in Test Config
- **File:** `crates/ff-core/src/config.rs:1830`
- **Issue:** `password = "forgefleet"` in default test string.
- **Fix:** Document as "must be changed".

### P3-2: Hardcoded Secrets in Test Code
- **Files:** `crates/ff-gateway/src/server.rs:3675,3720`, `crates/ff-security/src/node_auth.rs:118`, `crates/ff-security/src/auth.rs:345`
- **Issue:** Test-only secrets. Low risk but should be marked `#[cfg(test)]`.

### P3-3: JWT Errors Leaked to Client
- **File:** `crates/ff-gateway/src/middleware.rs:167-171`
- **Issue:** `format!("invalid token: {}", e)` returns detailed errors to client.
- **Fix:** Log full error server-side, return generic `"invalid token"`.

### P3-4: Unsafe Code in main.rs Without SAFETY Comments
- **File:** `src/main.rs:121-122,136-137,469`
- **Issue:** Multiple `unsafe` blocks without `// SAFETY:` documentation.
- **Fix:** Add SAFETY comments.

### P3-5: Unsafe env::set_var in Non-Test Path
- **File:** `crates/ff-mcp/src/handlers.rs:3092-3106`
- **Issue:** `unsafe { std::env::set_var(...) }` used in test helpers. Unsound in Rust 2024+.
- **Fix:** Use `serial_test` crate or scoped mutex.

### P3-6: Clippy Warnings
- **Scope:** 100+ warnings across workspace
- **Issues:** Unused imports, collapsible ifs, complex types, etc.
- **Fix:** Run `cargo clippy --fix`.

### P3-7: Dynamic SQL Building in CLI (Fragile but Safe)
- **Files:** `crates/ff-terminal/src/tools_cmd.rs`, `tasks_cmd.rs`, `main.rs`
- **Issue:** `format!` used to determine `$N` parameter numbers.
- **Fix:** Migrate to `sqlx::QueryBuilder`.

---

## Performance Audit Key Findings (Summary)

1. **No request timeouts** — 12+ `reqwest` clients without timeout (infinite hang risk)
2. **Unbounded queries** — 15 queries without LIMIT (OOM risk on large datasets)
3. **No connection pooling** — `reqwest::Client::new()` per-call in many places (TCP exhaustion)
4. **Blocking fs in async** — 10 files use `std::fs` in async contexts
5. **Unbounded channels/caches** — Web clients, WS messages, agent sessions, LLM cache
6. **`std::sync::Mutex` in async** — 3 locations with deadlock risk
7. **No backpressure on WS** — Unbounded inbound/outbound queues
8. **No query result streaming** — All queries load full result into memory
9. **String allocations in hot paths** — Repeated `format!` in logging and serialization
10. **Inefficient health check loops** — Fixed intervals without adaptive backoff

---

## Security Audit Key Findings (Summary)

| Category | Count | Top Issues |
|----------|-------|------------|
| Auth/Authz | 5 | JWT exp disabled, auth optional globally, no RBAC |
| SQL Injection | 4 | Tool registry ILIKE, pulse limit, alert evaluator column |
| Command Injection | 11 | bash -c in tools, grep injection, clipboard, ffmpeg |
| Input Validation | 3 | Arbitrary working_dir, unvalidated file paths |
| Secrets | 3 | Plaintext storage, hardcoded defaults |
| Unsafe Code | 3 | env::set_var, FFI, missing SAFETY comments |
| CORS | 1 | Permissive on all routes |
| Webhooks | 2 | No signature verification |

---

## Recommended Fix Order

### Week 1 (P0 — Critical)
1. Fix API 404 fallback to return JSON for `/api/*` paths
2. Fix leader election route to accept POST
3. Fix SQL injection in `tool_registry_api.rs`

### Week 1-2 (P1 — High)
4. Re-enable JWT expiration validation
5. Make auth mandatory for mutating routes
6. Add GitHub webhook HMAC verification
7. Add reqwest timeouts everywhere
8. Add LIMIT to all DB queries
9. Replace `std::sync::Mutex` with `tokio::sync::Mutex` in async
10. Restrict CORS to known origins
11. Wire ToolInventory to live API
12. Spawn WS heartbeat task

### Sprint 2-3 (P2 — Medium)
13. Replace `bash -c` tool implementations with direct Command
14. Add cache/channel eviction
15. Fix dashboard orphans and sidebar links
16. Encrypt secrets at rest
17. Validate working_dir and shell paths
18. Fix WS reconnect backoff
19. Remove or implement 501 routes
20. Add MCP auth consolidation

### Sprint 3+ (P3 — Low)
21. Clippy cleanup (`cargo clippy --fix`)
22. Add SAFETY comments to unsafe blocks
23. Migrate CLI to `sqlx::QueryBuilder`
24. Replace stub embedding model
25. Clean up test-only hardcoded secrets

---

*Report compiled from 3 completed audit passes. Code quality/dead code audit still in progress and will be appended when complete.*
