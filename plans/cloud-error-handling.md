# Cloud-CLI Error Handling & Headless Auto-Resume

**Status:** design (pending ff council validation)
**Owner:** capability-A follow-on
**Why:** A `529 Overloaded` hit a headless autonomous session; the only reason it
recovered is the operator manually typed "continue". ForgeFleet sub-agents,
`ff supervise`, and autonomous loops have **no human to type continue** — so a
transient API error stalls or kills the task. We must classify every vendor's
error responses and act on them headlessly: retry/auto-continue the transient
ones, fail over to another provider on the persistent ones.

## Scope: every backend we dispatch to

claude (Anthropic) · codex (OpenAI) · kimi (Moonshot) · gemini (Google) · grok (xAI)

Each vendor has its OWN codes; the SAME number means different things per vendor.
529 is Anthropic-only — OpenAI/Gemini signal overload with `503`. `429` splits
into rate-limit (retry) vs quota (don't retry). So detection MUST parse the
`error.type` / message text, not just the HTTP status.

## Layer 1 — Per-vendor code→class map (`ff-agent/src/cloud_error.rs`)

`enum CloudErrorClass { Overloaded, RateLimited, QuotaExhausted, Unauthenticated,
Forbidden, ContextTooLong, Timeout, Transient5xx, ModelNotFound, BadRequest,
ContentFiltered, Network, Unknown }`

`fn classify(provider: &str, exit_code: i32, stdout: &str, stderr: &str) -> CloudErrorClass`
— these are CLI subprocesses, so we classify from exit code + output text using
per-vendor regex patterns (e.g. claude prints `API Error: 529 Overloaded`).

| Class | claude | codex/OpenAI | kimi/Moonshot | gemini | grok |
|---|---|---|---|---|---|
| Overloaded | 529 overloaded_error | 503 | 429 overload subtype | 503 UNAVAILABLE | 5xx |
| RateLimited | 429 rate_limit_error | 429 rate_limit_exceeded | 429 rate_limit_reached_error | 429 RESOURCE_EXHAUSTED (RPM/TPM) | 429 |
| QuotaExhausted | 400 billing / 402 | 429 insufficient_quota | 429 exceeded_current_quota_error | 429 RESOURCE_EXHAUSTED (RPD) | 402 payment_required |
| Unauthenticated | 401 | 401 | 401 | 401/403 | 401 invalid_api_key |
| Forbidden/geo | 403 permission | 403 | 403 | 400 FAILED_PRECONDITION | 403 |
| ContextTooLong | 413 request_too_large | 400 context_length_exceeded | 400 | 400 token-cap | 400 |
| Timeout | stream stall | timeout | timeout | 504 DEADLINE_EXCEEDED | timeout |
| Transient5xx | 500 api_error | 500 | 500 | 500 INTERNAL | 5xx |
| ModelNotFound | 404 | 404 | 404 | 404 | 404 |
| BadRequest | 400 invalid_request | 400/422 | 400 | 400 INVALID_ARGUMENT | 400 |

## Layer 2 — Class→action policy

| Class | Action |
|---|---|
| Overloaded | exp backoff (1,2,4,8s, honor Retry-After) + **auto-continue same session**; count toward breaker |
| RateLimited | honor Retry-After; backoff + auto-continue; count toward breaker |
| Transient5xx / Timeout / Network | backoff + auto-continue (Timeout: 1 retry then switch); count toward breaker |
| QuotaExhausted | **do NOT retry** — long cooldown on backend, switch provider, alert operator |
| Unauthenticated | flip `computer_backends.authenticated=false`, switch provider, alert (re-auth) |
| Forbidden / ModelNotFound | terminal for this backend — switch provider/model, alert |
| ContextTooLong | not retryable as-is — compact/trim context then continue (same on every provider, no switch) |
| BadRequest / ContentFiltered | terminal — surface (our bug / policy), no blind retry |
| Unknown | conservative: 1 backoff retry, then switch; log raw output to grow the taxonomy |

## Layer 3 — Headless auto-continue (the core fix)

Two execution modes:
- **One-shot exec** (`claude -p`, `codex exec`): process exits non-zero → cli_executor
  re-runs the exec with backoff per Layer 2. Straightforward.
- **Interactive agent session** (the screenshot case): the agent prints the error and
  waits for input. Headless → hangs. Fix: the session driver (agent_loop / supervisor)
  watches for a classified transient error on the last turn and **auto-injects the
  continuation** (re-send "continue" / re-issue the pending request) after backoff,
  up to N attempts — no human needed. Terminal classes break out to Layer 4 instead.

## Layer 4 — Circuit-breaker → provider switch ("too many errors → switch providers")

Per-(computer,backend) rolling-window error counter. When transient errors exceed a
threshold (e.g. ≥4 in 5 min), **trip the breaker**: cooldown that backend and switch
dispatch to the next provider in `backend_rank` order (codex→claude→kimi→gemini→local
fleet). Half-open after cooldown to probe recovery. State in `computer_backends`
(new cols `breaker_open_until`, `recent_error_count`, `recent_error_window_start`) or a
`fleet_backend_health` table. `ff backends` surfaces breaker state; alert on trip.

Note: the local fleet (Qwen/etc.) is the last fallback — it can't 529, so a fully
cloud-overloaded moment still makes progress on local models.

## Reuse existing V107 infra (discovery-first — do NOT rebuild)

ff-agent already has the dispatch retry/breaker spine; cloud handling must
EXTEND it, not fork it:

- **`failure_taxonomy` (table, V107)** — `category → (transient, retryable)`.
  11 host/task categories today, **zero cloud ones**. Add seed rows whose
  `category` = `CloudErrorClass::as_str()` (overloaded, rate_limited,
  quota_exhausted, unauthenticated, forbidden, context_too_long, timeout,
  transient_5xx, model_not_found, bad_request, content_filtered, unknown).
  Flags: overloaded/rate_limited/timeout/transient_5xx = (transient=t,
  retryable=t); quota_exhausted/unauthenticated/forbidden/model_not_found/
  bad_request/content_filtered = (f,f); network → reuse `network_transient`.
- **`retry_policy::should_retry(pool, category, attempt)`** — already returns
  the 5/30/120s backoff for ≤3 attempts on transient+retryable categories.
  `cloud_error::classify()` → `.as_str()` category → feed THIS. No new
  backoff code.
- **`circuit_breaker` (per-HOST, `host_circuit_status`/`task_failures` V107)** —
  quarantines a worker after 3 same-category failures/10min. A cloud provider
  outage is NOT a host failure (claude 529 hits every host using claude), so
  add **provider-keyed** fns next to the host ones:
  `record_provider_failure(pool, computer_id, provider, category)` +
  `is_provider_open(pool, computer_id, provider)` backed by V149
  `fleet_backend_health`. Mirror the existing logic + council thresholds.
- **`dispatcher::classify_workload` + capability-A picker
  (`backend_detect` / `work_item_dispatch` lane-2 / `pg_dispatchable_backends`)**
  — the existing "who can do this?" leg. Layer 5 headroom-weighting plugs in
  HERE (extend the picker), not a new router.

So `cloud_error.rs` (#667) is the one genuinely-missing piece (classify CLI
output); everything downstream reuses the V107 spine. action()'s retry numbers
are a hint — the real retry uses retry_policy/failure_taxonomy; action()'s
unique value is the Switch/FlipAuth/Compact/Terminal decisions retry_policy
doesn't make.

## V149 schema (ready-to-build — ships with the usage-probe / breaker PRs)

```sql
-- fleet_provider_usage: latest usage snapshot per (computer, provider).
CREATE TABLE IF NOT EXISTS fleet_provider_usage (
    computer_id     UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    provider        TEXT NOT NULL,                 -- claude|codex|kimi|gemini|grok
    used_pct        DOUBLE PRECISION,              -- 0..100 (NULL if unknown)
    remaining_pct   DOUBLE PRECISION,              -- 100-used, or header-derived
    window_kind     TEXT,                          -- session|5h|weekly|monthly
    resets_at       TIMESTAMPTZ,
    source          TEXT,                          -- ratelimit_header|usage_api|portal
    raw             JSONB,                         -- raw probe payload for audit
    sampled_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (computer_id, provider, window_kind)
);

-- fleet_backend_health: durable circuit-breaker state per (computer, provider).
CREATE TABLE IF NOT EXISTS fleet_backend_health (
    computer_id          UUID NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    provider             TEXT NOT NULL,
    breaker_state        TEXT NOT NULL DEFAULT 'closed',   -- closed|open|half_open
    breaker_open_until   TIMESTAMPTZ,
    recent_error_count   INT NOT NULL DEFAULT 0,
    recent_req_count     INT NOT NULL DEFAULT 0,
    window_start         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    half_open_successes  INT NOT NULL DEFAULT 0,
    last_error_class     TEXT,
    last_error_at        TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (computer_id, provider)
);

-- fleet_session_dispatch: per-headless-session retry/continue state (durable so
-- sessions survive crashes/deploys/scheduler moves — council item 5).
CREATE TABLE IF NOT EXISTS fleet_session_dispatch (
    session_id          TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    attempt_count       INT NOT NULL DEFAULT 0,
    auto_continue_count INT NOT NULL DEFAULT 0,
    last_error_class    TEXT,
    last_error_at       TIMESTAMPTZ,
    last_retry_at       TIMESTAMPTZ,
    resume_token        TEXT,
    context_digest      TEXT,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

## Build phasing (one PR each, verify-upfront 1.88)

1. `cloud_error.rs` classifier + per-vendor patterns + unit tests (pure, fast).
2. Wire one-shot exec path (cli_executor): backoff + provider failover via picker.
3. Headless auto-continue in agent_loop/supervisor session driver.
4. Circuit-breaker DB state + cooldown + `ff backends` surfacing + alert.

## Layer 5 — Usage-aware routing (don't waste idle subscriptions)

Observed waste 2026-06-29: claude Max **16%/wk used**, codex Pro **~1%** (+ $166 idle
credits), kimi **1.09%**. We hammer claude while two paid subscriptions sit idle. The
router must spread load by **remaining headroom**, not just `backend_rank`.

**5a. Usage probe** (`fn probe_usage(provider) -> UsageSnapshot { used_pct, resets_at,
window }`), per-vendor:
- Universal cheap signal: capture rate-limit response headers on every call
  (`anthropic-ratelimit-*`, OpenAI `x-ratelimit-remaining-*`, Moonshot equivalents) and
  persist to a `fleet_provider_usage` table — ff already logs `ff_interactions`, extend it.
- Absolute subscription %: per-vendor usage endpoint where one exists (Anthropic usage
  report, OpenAI billing/usage, Moonshot/Kimi quota API). Scrape/portal only as last resort.
- Periodic tick (e.g. every 15 min) refreshes the snapshot per provider.

**5b. Headroom-weighted picker** — extend the capability-A backend picker: among
*dispatchable + breaker-closed + authenticated* backends, prefer the one with the most
remaining headroom (and not near reset starvation). Effect: default load shifts to
codex/kimi until claude's week resets; claude reserved for what it's uniquely best at.
This is for **ff AND other projects** — the picker lives behind `ff offload` / dispatch
so every terminal (claude/codex/kimi TUIs) and project benefits.

**5c. Same switch path as Layer 4** — error breaker and low-headroom both feed one
decision: "is backend X usable right now?" Usage just adds a soft signal (deprioritize
near-exhausted) on top of the hard error breaker.

## Layer 6 — Kimi desktop app integration (parity with claude/codex desktop)

Kimi Work / Kimi desktop (kimi.com/activities/kimi-work-promo) has Plugins, MCP servers,
Agent Swarm, Kimi Code, Kimi Claw, Scheduled Tasks — same surface as Claude Desktop /
Codex Desktop, both of which already have the forgefleet MCP wired in. Add ff to Kimi:
- `ff mcp install --for kimi-desktop` — write the forgefleet MCP server into Kimi
  desktop's MCP config (find its config path, mirror the claude/codex installer).
- Verify the 36 forgefleet MCP tools resolve inside Kimi desktop.
- Fold into `ff mcp install --for all`.

## ff council decisions (codex + kimi consensus, 2026-06-29)

Ran `ff council --members codex,kimi` on the open questions. Resolved:

1. **Transient retry** — short budget, fail over fast. Implemented as
   `action()` max_attempts: Overloaded/RateLimited/5xx = 3, Network = 3,
   Timeout = 2, Unknown = 1.
2. **Headless auto-continue** — **2** re-injections; backoff `5s` then `20s`,
   jitter ±25%; after the 2nd failed continue, switch provider and resume from
   a durable checkpoint/compressed context. (Codex's stricter cap chosen over
   Kimi's 3 to avoid quota burn on stuck sessions.)
3. **Circuit breaker** — trip when `5 transient failures in 5 min` OR
   `failure_rate ≥ 50% over 5 min (min 10 requests)`. Cooldown: `2 min` for
   Overloaded/5xx, `10 min` for RateLimited/quota. Half-open: allow 1 probe →
   on success allow 3 limited probes → close after 4 consecutive successes;
   any failure reopens for the full cooldown.
4. **Usage-aware score** —
   `score = 0.60*quota_headroom + 0.30*static_preference + 0.10*health + jitter(±0.02)`
   where `quota_headroom = clamp(remaining/expected_session_cost, 0, 1)`,
   `static_preference` = normalized backend_rank (best=1), `health` =
   {healthy 1.0, degraded 0.5, breaker-open 0.0}. Exclude `headroom < 0.15`
   unless no viable alternative. (Quota dominates — picking a backend that
   can't finish is worse than a slightly-less-preferred one.)
5. **Retry/continue + breaker state → Postgres** (durable, fleet-shared);
   in-process only as a short cache. Persist `session_id, provider,
   attempt_count, auto_continue_count, last_error_class, last_error_at,
   last_retry_at, checkpoint_id/resume_token/context_digest`. Rationale:
   headless sessions must survive crashes, deploys, scheduler moves, sub-agent
   migration — in-proc counters cause duplicate retries + non-deterministic
   failover. → drives the V149 `fleet_session_dispatch` + `fleet_backend_health`
   tables.
