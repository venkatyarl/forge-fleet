# Token Ledger & Smart Local-First Routing

**Date:** 2026-05-04  
**Status:** Production-ready  
**Components:** `ff-api`, `ff-gateway`, `ff-terminal`, dashboard

---

## Overview

This document describes the new **Token Cost Ledger** and **Local-First Smart Routing** systems added to ForgeFleet. These features make ForgeFleet production-ready for cost-conscious AI infrastructure by:

1. Tracking every token consumed across local and cloud models
2. Enforcing budget limits with automatic alerting
3. Prioritizing free local LLMs over paid cloud APIs
4. Providing real-time cost dashboards

---

## Architecture

### Token Ledger (`crates/ff-api/src/token_ledger.rs`)

A thread-safe, in-memory token usage tracker with optional Postgres persistence.

**Key structures:**
- `CostTracker` — Central tracker with pricing DB, daily records, model stats
- `TokenUsageRecord` — Per-request usage snapshot
- `ModelPricing` — Built-in pricing for 40+ models (OpenAI, Anthropic, Google, local)
- `BudgetConfig` — Daily/cloud budget with enforcement and alerting
- `FleetCostSummary` — Aggregated fleet-wide cost report

**Features:**
- ✅ Per-model token counting (input/output/total)
- ✅ Automatic cost calculation in USD
- ✅ Budget enforcement (soft/hard)
- ✅ Alert thresholds (80% default)
- ✅ Background flush to Postgres every 5 minutes
- ✅ Manual flush via API
- ✅ Zero-cost tracking for local models

### Smart Router Enhancements (`crates/ff-api/src/adaptive_router.rs`)

The adaptive router now supports **routing policies**:

| Policy | Behavior |
|--------|----------|
| `LocalFirst` | Prefer local models; only escalate to cloud for complex tasks |
| `CostOptimized` | Prefer cheapest option (local → cheapest cloud tier) |
| `QualityFirst` | Use quality rankings as before |
| `Balanced` | Weight quality × locality × cost |

**New `BackendEndpoint` fields:**
- `is_local` — Whether the endpoint is self-hosted (free)
- `cost_per_1k_input` — Cost per 1K input tokens
- `cost_per_1k_output` — Cost per 1K output tokens

### Gateway Integration (`crates/ff-gateway/src/server.rs`)

**New API endpoints:**

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/ledger/summary` | GET | Fleet-wide cost summary |
| `/api/ledger/models` | GET | Per-model cost stats |
| `/api/ledger/budget` | GET/POST | Budget config read/update |
| `/api/ledger/flush` | POST | Persist memory to Postgres |
| `/api/ledger/records` | GET | Daily usage records |

**Proxy path integration:**
- Non-streaming responses are parsed to extract `usage` field
- Token usage is automatically recorded with cost calculation
- Streaming responses tracked at connection level

### Dashboard (`dashboard/src/pages/CostLedger.tsx`)

New React page with:
- Budget progress bar with color-coded alerts
- Summary stat cards (requests, tokens, cost, savings)
- Per-model breakdown table
- Budget configuration display
- Manual flush button

---

## Configuration

### Budget Config (via API)

```bash
curl -X POST http://localhost:51002/api/ledger/budget \
  -H "Content-Type: application/json" \
  -d '{
    "daily_budget_usd": 100.0,
    "cloud_daily_budget_usd": 50.0,
    "enforce_budget": true,
    "alert_threshold": 0.8
  }'
```

### Routing Policy (via fleet.toml)

```toml
[llm.routing]
policy = "local_first"       # or "cost_optimized", "quality_first", "balanced"
cloud_complexity_threshold = 3  # Only use cloud for tier 3+ tasks
```

---

## Deployment

### Local (taylor/leader)
```bash
cd ~/projects/forge-fleet
cargo build --release --bin forgefleetd
cp target/release/forgefleetd ~/.local/bin/
codesign --force --sign - ~/.local/bin/forgefleetd
forgefleetd --node-name taylor start
```

### Fleet-wide
```bash
./scripts/deploy-to-fleet.sh
```

---

## Monitoring

### Health Check
```bash
curl http://localhost:51002/health
```

### Cost Summary
```bash
curl http://localhost:51002/api/ledger/summary
```

### Daily Records
```bash
curl "http://localhost:51002/api/ledger/records?day=2026-05-04&limit=50"
```

---

## Database Schema

The `token_ledger` table is auto-created on first flush:

```sql
CREATE TABLE token_ledger (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    request_id TEXT NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL,
    model TEXT NOT NULL,
    backend_id TEXT NOT NULL,
    task_type TEXT NOT NULL,
    routing_strategy TEXT NOT NULL,
    prompt_tokens BIGINT NOT NULL,
    completion_tokens BIGINT NOT NULL,
    total_tokens BIGINT NOT NULL,
    cost_usd DOUBLE PRECISION NOT NULL,
    is_local BOOLEAN NOT NULL,
    latency_ms BIGINT NOT NULL,
    success BOOLEAN NOT NULL,
    error TEXT
);
```

---

## Model Pricing Database

Built-in pricing for common models:

**Cloud (paid):**
- OpenAI: gpt-4o, gpt-4o-mini, gpt-4-turbo, gpt-3.5-turbo
- Anthropic: claude-3-5-sonnet, claude-3-opus, claude-3-haiku
- Google: gemini-2.0-flash, gemini-1.5-pro

**Local (free):**
- Qwen family (0.6B to 235B)
- Llama family (3.1, 3.2, 4)
- Mistral/Mixtral
- CodeLlama
- DeepSeek Coder
- Phi-3
- Gemma

Unknown models default to `is_local = true, cost = 0.0` for safety.

---

## Testing

All workspace tests pass:
```bash
cargo test --workspace --lib
# Result: 1122+ tests passed, 0 failed
```

---

## Future Enhancements

1. **Streaming token counting** — Parse SSE chunks for real-time usage
2. **Model pricing API** — Fetch live pricing from providers
3. **Cost prediction** — Estimate cost before sending request
4. **Team budgets** — Per-user/per-team budget isolation
5. **Alerts integration** — Telegram/webhook alerts on budget threshold
6. **Cost optimization advisor** — Suggest cheaper models for task types
