# Phase 10 — API Governance Policy (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Scope: `crates/*` for workspace v0.1.x
Reference: `docs/PHASE10_API_SURFACE.md`

This document defines how API compatibility is governed after the Phase 10 surface inventory.

---

## 1) Compatibility Baseline for v0.1.x

Although Rust semver allows broader change before `1.0.0`, ForgeFleet adopts a **stricter policy** for `0.1.x`:

- **Patch releases (`0.1.x`)** should be non-breaking for declared stable surfaces.
- **Minor release (`0.2.0`)** is the planned point for removals/true breaks after deprecation.
- Additive changes are preferred over mutating/removing existing contracts.

---

## 2) Per-crate API Stability Policy

### Policy legend
- **Frozen in v0.1.x** = must not break in patch releases.
- **Can change in v0.1.x** = allowed if documented and tested.

| Crate | Tier | Frozen in v0.1.x | Can change in v0.1.x |
|---|---|---|---|
| ff-core | stable | Public module names; exported core types/traits; error/result aliases | Additive helper fns/types, new optional config fields, perf/internal refactors |
| ff-runtime | stable | `InferenceEngine` contract, `EngineConfig/Status`, runtime selection API shape | New engines/backends, additive config fields, non-breaking defaults |
| ff-orchestrator | stable | Planner/router/decomposer type contracts and enum variant meaning | Additive planning metadata, new optional strategy knobs |
| ff-mesh | stable | Leader/scheduler/queue public type signatures | Internal algorithms, additive telemetry/resource fields |
| ff-sessions | stable | Session lifecycle, approval/context manager contract shapes | Additive status fields, extra helper methods |
| ff-skills | stable | Skill registry/selector/executor core trait and data contracts | New adapters and additive metadata |
| ff-observability | stable | Event/metrics/alert type shape and semantics | Additional metrics/events, additive dashboard fields |
| ff-cron | stable | Schedule/job/dispatch core type semantics | New policies, additive retry/scheduling options |
| ff-api | beta | Top-level server start path (`run(ApiConfig)`), existing request/response required fields | Router behavior, optional DTO fields, backend/model routing internals |
| ff-control | beta | Primary control command envelope names | Bootstrap and subsystem wiring contracts, additive command options |
| ff-discovery | beta | Core scanner entry points and node identity model | Probe heuristics, registry details, additive health/model metadata |
| ff-evolution | beta | High-level loop/analyzer/repair service entry points | Scoring/repair strategy behavior, additive analysis fields |
| ff-gateway | beta | Normalized message primitives (`IncomingMessage`/`OutgoingMessage`) required semantics | Channel adapter payload mapping, optional message metadata |
| ff-memory | beta | Core retrieval query/result envelopes | Ranking/retrieval behavior, additive memory metadata |
| ff-security | beta | Approval/policy/rate-limit decision envelope semantics | Policy engine internals, additive audit fields |
| ff-ssh | beta | Connection/config command entry points | Connectivity strategy, tunnel internals, additive diagnostics |
| ff-voice | beta | Pipeline event/turn envelope shapes | Provider-specific behavior, additive audio/STT/TTS options |
| ff-benchmark | beta | Report/regression/capacity output top-level structures | Scenario internals, additive metrics |
| ff-agent | beta (binary) | Daemon startup contract and core runtime behavior flags documented in CLI/help | Internal modules/types (no library API commitment) |
| ff-cli | beta (binary) | Existing top-level command names; core flag meanings; machine-readable outputs when documented | Additive flags/subcommands; human-oriented text formatting |
| ff-deploy | experimental | Nothing frozen yet | Any API/module/type may change or be removed |
| ff-pipeline | experimental | Nothing frozen yet | Any API/module/type may change or be removed |

### Additional notes by tier
- **stable crates:** no removals/renames/signature breaks in `0.1.x`.
- **beta crates:** limited shape stability at top-level APIs, but internals and non-core semantics may evolve.
- **experimental crates:** explicitly unstable; breakage is expected until promoted.

---

## 3) Deprecation & Versioning Rules

### 3.1 Deprecation lifecycle
1. **Introduce replacement first** (or document no replacement).
2. Mark deprecated item with:
   - `#[deprecated(since = "0.1.N", note = "Use <new_api>; removal in 0.2.0")]`
3. Keep deprecated item for **at least 2 patch releases** (recommended) unless critical security issue.
4. Remove deprecated item only in **`0.2.0`** (or later), with migration notes.

### 3.2 What is considered breaking (must not happen in 0.1.x stable surfaces)
- Removing or renaming public modules/types/functions/trait methods.
- Changing function signatures or required fields.
- Reinterpreting enum variants or command meaning incompatibly.
- Changing wire schemas by removing/renaming required JSON fields.

### 3.3 Allowed in patch releases (`0.1.x`)
- Additive optional fields (Rust and wire DTOs).
- New functions/types/modules that do not conflict with existing behavior.
- Internal refactors and bug fixes that preserve contract semantics.
- Performance/observability improvements without API breakage.

### 3.4 CLI/API wire compatibility
- **CLI (`ff-cli`)**: existing command names and documented flags remain valid through `0.1.x`.
- **HTTP/Gateway DTOs (`ff-api`, `ff-gateway`)**:
  - Additive optional fields: allowed.
  - Removing/renaming existing required fields: defer to `0.2.0` with deprecation/migration.

---

## 4) Module Visibility Recommendations (`pub(crate)` candidates)

Goal: narrow external surface area and stabilize through facade exports (`lib.rs` re-exports) rather than exposing every module.

> Recommendation: apply these gradually, with deprecating re-exports first where needed; do not do silent breaks in `0.1.x` stable crates.

| Crate | Candidate `pub(crate)` modules | Rationale |
|---|---|---|
| ff-runtime | `detector`, backend-specific modules (`llamacpp`, `mlx`, `ollama`, `vllm`) | Consumers should prefer `InferenceEngine` + factory APIs, not backend internals |
| ff-api | `registry`, `router`, `server`, possibly `error` | Keep external API centered on config + DTOs + `run` entrypoint |
| ff-control | `bootstrap`, `health`, internal handle wiring modules | Expose control-plane facade and command contracts, hide wiring details |
| ff-discovery | `ports`, `profile`, low-level probe helpers | Keep scanner/registry contracts public; hide implementation helpers |
| ff-gateway | channel adapters (`discord`, `telegram`, `webhook`), `embed` | Expose normalized message model + router/server facade |
| ff-memory | `capture`, `workspace` internals (if not intended for direct external use) | Encourage use through retrieval/store/session APIs |
| ff-security | `audit` and policy internals if only used via facade | Keep approval/policy decisions as primary public contract |
| ff-ssh | `key_manager`, low-level connectivity internals | Expose high-level connection/remote-exec/tunnel contracts |
| ff-voice | provider-specific integrations (`twilio` and concrete clients) behind traits | Prefer stable pipeline/STT/TTS traits over provider internals |
| ff-benchmark | `collector`, `runner` internals if only orchestration plumbing | Stabilize report/regression/capacity outputs as contract |

Implementation pattern:
- Keep internal modules `pub(crate)`.
- Re-export curated stable items from crate root.
- Document “supported imports” in crate-level docs.

---

## 5) Minimal API Change Review Checklist

Use this checklist for any PR that touches public APIs or command/wire contracts.

### 5.1 Classify and scope
- [ ] Which crate tier? (`stable` / `beta` / `experimental` / binary)
- [ ] Is this Rust API, CLI contract, wire DTO/schema, or behavior-only?

### 5.2 Compatibility gate
- [ ] Any removed/renamed/reshaped public item?
- [ ] If yes, is there a deprecation path and planned removal version (`0.2.0+`)?
- [ ] For wire contracts: only additive optional fields in `0.1.x`?

### 5.3 Documentation gate
- [ ] Update crate docs and `PHASE10_API_SURFACE.md` when surface changes.
- [ ] Add migration note for deprecated or behaviorally significant changes.
- [ ] Update changelog/release notes with compatibility impact.

### 5.4 Testing gate
- [ ] Contract tests added/updated for changed public behavior.
- [ ] Serialization compatibility tests for DTO changes.
- [ ] CLI snapshot/behavior tests for command/flag changes.

### 5.5 Visibility gate
- [ ] New modules default to private; justify `pub`.
- [ ] If `pub` is required, consider curated root re-export instead of direct module exposure.

---

## 6) Governance Ownership

- API governance owner: ForgeFleet maintainers (Phase 10).
- Any exception to this policy should be explicitly called out in PR description and release notes.
- Reassess tiers at each minor release boundary (`0.2`, `0.3`, …).
