# LLM Router improvement plan (2026-07-29)

Grounded in external research (LiteLLM Proxy reliability docs, RouteLLM/lmsys,
TensorZero gateway, Envoy AI Gateway) mapped onto ff's actual router:
`ff_db::pg_route_deployments` (SQL scorer), `ff_agent::fleet_oneshot` (dispatch
+ failover), `ff_agent::InferenceRouter` (agent local-first), `ff-gateway`
(proxy with route cache), `fleet_backend_health` (circuit breaker).

## What the field does that we don't (gap list)

| Feature | Where seen | ff today |
|---|---|---|
| Quality/cost-aware routing learned from outcomes | RouteLLM (matrix factorization on preference data; 95% of GPT-4 quality at 14-26% of the calls) | Static tier ordering only; outcomes logged to `ff_interactions` but never fed back |
| Named fallback chains + retries with per-error-class policy | LiteLLM (`fallbacks`, `num_retries`, `allowed_fails`+`cooldown_time`) | Linear candidate iteration, fail-next on any error; no retry-same-candidate for transient errors |
| Pre-call admission (prompt fits ctx) | LiteLLM `enable_pre_call_checks` + `max_input_tokens` | Just added for codegen only (#1502), not generalized |
| Response cache (exact/semantic) | LiteLLM cache, TensorZero | None (council/health probes recompute every time) |
| Shared cooldown registry across all callers | LiteLLM cooldowns | Split: InferenceRouter in-memory 60s, DB health freshness, gateway route cache (`"cached": true` — served devstral for the thinking alias hours after detagging) |
| Router explainability | TensorZero/OpenRouter dashboards | `ff fleet route` shows the winner, not WHY |

## Plan (priority order)

### R1. Outcome-aware quality scoring (RouteLLM-lite, using OUR data)
We don't need preference learning — we have `ff_interactions` + `work_item_provenance`
(builder_model → merged/failed). Add a rolling per-(catalog_id, workload)
`build_success_rate` view and inject it into `pg_route_deployments` between tier
and load in the ORDER BY. Effect: GLM vs qwen3-coder ordering stops being
tier-dogma and reflects measured build success. This is the honest version of
"is GLM a going choice" — let the scoreboard rank it.

### R2. Error-class-aware failover + jittered retry
Classify candidate errors (timeout / conn-reset / 429 / 5xx / bad-output).
Transient (timeout, reset, 5xx): one jittered retry on the SAME candidate
before failing over. Bad-output (prose, no edit blocks): fail over immediately
AND mark a short per-deployment cooldown (see R3). Today every error is
treated identically.

### R3. Unified cooldown registry
Persist per-deployment cooldowns in `fleet_backend_health` (table exists) and
have ALL four callers read it: fleet_oneshot, InferenceRouter, gateway route
cache, circuit_breaker. Kill the gateway's independent route cache or TTL it
to ≤60s — it served devstral for the `thinking` alias hours after the catalog
detag (observed 2026-07-29).

### R4. Generalized pre-call admission
Promote the codegen min_ctx floor into a shared util: estimate prompt tokens
(chars/4), require usable_agent_ctx ≥ est + max_tokens + reserve, for every
fleet_oneshot caller (council, review, research synth) not just codegen.

### R5. Exact-prompt response cache (idempotent one-shots only)
Hash(prompt+model+system) → response, TTL 10 min, at the fleet_oneshot layer.
Opt-in per caller (council, health probes). Never for codegen/review.

### R6. DB-driven workload synonym clusters
Move `WORKLOAD_SYNONYM_CLUSTERS` from a const array (queries.rs) to a table so
new tags don't need a redeploy + fleet-wide rollout (today: edit → PR → merge
→ deploy --all → leader dance).

### R7. `ff fleet route --explain`
Per-candidate scorer breakdown (tier, success rate, load, freshness, caps,
cooldown) so "why did qwen beat GLM" is one command, not a forensics session.

## Sequencing
R1+R7 first (measurement before policy), then R3 (shared cooldown kills a
whole bug class), R2, R4, then R5/R6 as conveniences. Each is independently
shippable; all fit existing tables (fleet_backend_health, ff_interactions)
with at most one new migration (R1's success-rate view, R6's cluster table).

## Addenda from the fleet's own research run (Lucy lane, verified)
- Semantic routing (embed the prompt, route by task class) is the common
  "next step" across Kong/TensorZero/Redis writeups — the fleet already runs
  bge-m3 embedding deployments, so R6 could grow a semantic layer later.
- Observability via OpenTelemetry spans per routed call (OpenRouter/Portkey
  parity) — R7's --explain is the cheap first step; OTel export is the full one.
- NeurIPS 2025 cost-aware contrastive routing corroborates R1: outcome/
  cost-aware selection beats static tiers, and our ff_interactions corpus is
  exactly the training signal these systems wish they had.
