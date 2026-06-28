# Cortex middleware-aware guard detection (endpoints-no-guard accuracy)

**Status:** part 1 shipped (severity reclassification); part 2 (router-scope
propagation) designed, deferred to operator review (security-sensitive).

## Problem

The `endpoints-no-guard` audit rule (`ff-brain/src/cortex/extractors/security.rs`
+ `cortex/audit.rs`) flags an `http:endpoint` when its handler `code:function`
has no `guarded_by → security:gate` edge. Gates are detected by scanning a
function body for tokens (`FF_JWT_SECRET`, `Authorization`, `verify_jwt`, …) and
linking the gate to the **enclosing function** (`enclosing_function` +
`add_guard`).

But ForgeFleet's gateway applies auth as a **global tower middleware**:
`app.layer(middleware::from_fn(jwt_auth_middleware))` (ff-gateway/src/server.rs).
The tokens live in `jwt_auth_middleware`, so *that* function gets the guard edge;
the actual handlers (`update_pr_info`, `mark_message_read`, …) never do. Result:
**258 false-positive `endpoints-no-guard` findings**, all `high` severity — every
ff-gateway route is in fact covered by the global JWT layer.

The middleware is not blanket: it exempts `is_public_route` (`/health`,
`/api/webhook`, `/metrics`, `/ws`) and carves out the LLM endpoints
(`/v1/chat/completions`, `/v1/embeddings`, … — intentionally open on loopback).

## ff-council consensus (codex + kimi, 2026-06-28): hybrid (c)

1. **Detect** `layer(middleware::from_fn(gate_fn))` on a router and propagate
   `guarded_by` to routes **proven** to be under that router scope, **minus the
   exemption allowlist**.
2. **Never suppress uncertainty.** If route scope, nesting, route merges,
   method-specific behavior, dynamic paths, or exemption matching can't be
   proven, downgrade to **`medium-needs-verification`** — not `high`, not guarded.
3. **Safeguard against false negatives (the #1 risk for a security tool):** the
   exemption policy must be **first-class and authoritative**, and propagation
   requires **negative proof** the route does not match it. Unknown/possibly
   exempt ⇒ unguarded/needs-verification, never guarded.
   - kimi's refinement: ONE authoritative allowlist consumed by BOTH the runtime
     middleware and the extractor, with a CI test failing on any drift.

## Part 1 — SHIPPED this PR (safe, zero false-negative risk)

Without router-scope analysis, *every* `endpoints-no-guard` finding is an
unprovable case, so `high` is overconfident. Reclassified the rule to `medium`
+ "needs-verification (may still be covered by app-level auth middleware)". The
emit gate (#649, high-only) then stops these from auto-flooding the work board,
while `ff cortex audit` still shows them. Nothing is marked guarded → no hole is
hidden.

## Part 2 — DEFERRED (router-scope propagation, the real accuracy win)

Needs, in order:
1. A `security:gate` of kind `app-middleware-auth` minted when the extractor sees
   `.layer(..from_fn(F))` where `F` is a known gate-bearing function.
2. Router-scope resolution: which `.route("/p", m(handler))` registrations (across
   `.merge()` / `.nest()`) fall under the router that got the `.layer()`.
3. The authoritative exemption allowlist (shared with runtime `is_public_route` +
   the LLM carve-out) + CI drift test.
4. Propagate `guarded_by` to in-scope, non-exempt handlers; re-promote provably
   unguarded + non-exempt routes back to `high`.

Security-sensitive (false negatives hide real holes) → wants operator review
before landing. See [[project_e3_fleet_builds_proven]] context and the E4 finding
in [[project_pivot_20260628]].
