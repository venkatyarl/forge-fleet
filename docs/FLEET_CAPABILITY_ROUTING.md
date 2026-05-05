# Fleet Capability Routing — `/v1/fleet/route`

> **Status:** Live on all 15 nodes.
>
> **Honest assessment:** ff now covers **6 of 9** KovaBody features. 3 gaps remain before you can fully delete KovaBody's router.

---

## KovaBody → ff Feature Parity

| # | Feature | KovaBody | ff (today) | Verdict |
|---|---------|----------|------------|---------|
| 1 | **Task-based routing** | `RoutedProvider(task="periodization")` | `POST /v1/fleet/route` + capabilities | **Covered** |
| 2 | **Health checking** | Auto-pings every 60s | Pulse beats every 15s, auto-cooldown | **Covered (better)** |
| 3 | **Fleet-mesh** | 6 nodes | 15 nodes | **Covered (better)** |
| 4 | **Model count** | 8+ | 46 in catalog, 15+ deployed | **Covered (better)** |
| 5 | **Code/JSON models** | qwen3-coder, qwen2.5-coder | Same models deployed + routed | **Covered** |
| 6 | **Fallback chains** | 5-model fallback chains per task | Returns 1 primary + 5 alternatives | **Partial** — caller iterates alternatives |
| 7 | **Vision models** | 3 deployed (Gemma, qwen3-vl, gpt-4o) | 9 in catalog, **0 deployed** with vision tag | **Gap** |
| 8 | **Embedding models** | nomic-embed-text, dedicated path | qwen3-embed-8b in catalog, **no endpoint** | **Gap** |
| 9 | **Auth layer** | JWT + RBAC on Axum gateway | Enrollment token (onboarding only). **No JWT/RBAC on API** | **Gap** |

**Bottom line:** You can delete KovaBody's model router for reasoning/code/chat tasks today. You must keep KovaBody's auth layer, embedding endpoint, and vision fallback until those gaps are closed.

---

## Architecture: what stays in KovaBody vs what moves to ff

```mermaid
flowchart TB
    subgraph Client["External Client"]
        A[Request + JWT]
    end

    subgraph KovaBody["KovaBody (what you KEEP for now)"]
        B["Axum Gateway\nJWT validate + RBAC"]
        C["Embedding path\n/v1/embeddings → nomic-embed"]
        D["Vision fallback\n→ GPT-4o / Claude"]
        E["Fallback chain loop\n(alternative iteration)"]
    end

    subgraph ForgeFleet["ForgeFleet (what you USE now)"]
        F["ff-gateway :51002\nNO AUTH — internal only"]
        G["POST /v1/fleet/route\nreasoning | code | chat"]
        H["46 models in catalog"]
        I["15 deployed nodes"]
    end

    subgraph Cloud["Cloud (fallback)"]
        J["OpenAI GPT-4o"]
        K["Anthropic Claude"]
    end

    A -->|validated| B
    B -->|embedding| C
    B -->|reasoning/code/chat| G
    B -->|vision (for now)| D
    D -->|fallback| J
    G -->|alternatives| E
    E -->|next model| G
    G -->|fleet nodes| I

    style KovaBody fill:#fff3e0
    style ForgeFleet fill:#e8f5e9
    style Cloud fill:#e1f5fe
```

**Green (ff):** Task routing, health checking, fleet-mesh, code models, model catalog.
**Yellow (KovaBody, keep for now):** Auth gateway, embedding endpoint, vision fallback.
**Red (gap to close):** ff needs `/v1/embeddings`, vision model deployment, and optional JWT middleware.

---

## API Reference

### `POST /v1/fleet/route` — capability-based routing

**Request:**

```json
{
  "task": "refactor this Rust function",
  "required_capabilities": ["code", "tool_calling"],
  "preferred_local": true
}
```

**Response (200):**

```json
{
  "target": "http://192.168.5.103:55000",
  "node": "sophie",
  "model": "qwen3-coder-30b-a3b",
  "model_name": "Qwen3-Coder-30B-A3B-Instruct",
  "capabilities": ["code", "tool_calling", "reasoning"],
  "is_local": false,
  "reason": "fleet match, tier 2, queue_depth 0, tps 142.3",
  "queue_depth": 0,
  "tokens_per_sec": 142.3,
  "alternatives": [
    { "node": "marcus", "model": "qwen3-coder-30b-a3b", "target": "http://192.168.5.102:55000" }
  ]
}
```

**Response (503) — no match:**

```json
{
  "error": "no healthy fleet endpoint matches the required capabilities",
  "required_capabilities": ["vision"]
}
```

---

## What to tell application teams

### Message 1: "Migrate reasoning/code/chat routing to ff NOW"

**Action:** Replace your hard-coded model lists with `POST localhost:51002/v1/fleet/route`.

```python
import requests
FF_GATEWAY = "http://localhost:51002"

def route_llm(task: str, capabilities: list[str]) -> dict:
    resp = requests.post(
        f"{FF_GATEWAY}/v1/fleet/route",
        json={"task": task, "required_capabilities": capabilities, "preferred_local": True},
        timeout=5,
    )
    resp.raise_for_status()
    return resp.json()

# Reasoning
r = route_llm("optimize algorithm", ["reasoning"])
print(r["target"])  # http://192.168.5.100:55001 (taylor, qwen36)

# Code
r = route_llm("refactor function", ["code", "tool_calling"])
print(r["target"])  # http://192.168.5.103:55000 (sophie, qwen3-coder)

# Fallback chain (what KovaBody did automatically, you now do explicitly)
def route_with_fallback(task: str, capabilities: list[str]) -> str:
    try:
        r = route_llm(task, capabilities)
        return r["target"]
    except requests.HTTPError:
        for alt in r.get("alternatives", []):
            return alt["target"]
        raise
```

**Capabilities:** `reasoning`, `code`, `tool_calling`, `chat`, `long_context`, `vision`, `omni`, `text-generation`.

**What this replaces:** `REASONING_MODELS`, `CODE_MODELS`, `VISION_MODELS` hard-coded lists, custom health checkers.

### Message 2: "What you must KEEP for now"

| Keep in your project | Why | Until when |
|---------------------|-----|------------|
| **Auth layer (JWT/RBAC)** | ff-gateway has no API auth. It trusts localhost. | Until JWT middleware is added to ff-gateway |
| **Embedding endpoint** | ff has no `/v1/embeddings` fleet router. | Until embedding endpoint is built |
| **Vision cloud fallback** | No vision model is deployed in the fleet. `vision` capability returns 503. | Until qwen3-vl or gemma-vision is deployed |

### Message 3: "How to close the vision gap yourself"

If you want vision routing through ff today, deploy a vision model:

```bash
# On a node with GPU headroom (e.g. james or duncan)
ff model download qwen3-omni-7b --node james
ff model load qwen3-omni-7b --node james --port 55002
```

Then its capability tag `["omni", "vision", "chat"]` will automatically appear in `/v1/fleet/route` responses.

---

## Operator runbook

### Check catalog capabilities

```bash
# What can the fleet do today?
ff model catalog | grep -E "vision|vl|omni|coder"

# What's actually running?
ff model deployments

# Live routing test
curl -s http://localhost:51002/v1/fleet/route \
  -H "Content-Type: application/json" \
  -d '{"required_capabilities": ["reasoning"]}'
```

### Add a new model to the catalog

```sql
INSERT INTO fleet_model_catalog
    (id, name, family, parameters, tier, description, gated,
     preferred_workloads, variants, updated_at)
VALUES
    ('my-model', 'My Model', 'custom', '7B', 1, 'Desc', false,
     '["chat", "vision"]'::jsonb,
     '[{"runtime": "llama.cpp", "quant": "Q4_K_M", "hf_repo": "...", "size_gb": 4}]'::jsonb,
     NOW())
ON CONFLICT (id) DO UPDATE SET
    preferred_workloads = EXCLUDED.preferred_workloads,
    updated_at = NOW();
```

---

## Fleet deployment status

| Node | Binary | FF_NODE | /v1/fleet/route |
|------|--------|---------|-----------------|
| taylor | 2026.5.5_1 | yes | yes |
| ace | 2026.5.5_1 | yes | yes |
| adele | 2026.5.5_1 | yes | yes |
| aura | 2026.5.5_1 | yes | yes |
| beyonce | 2026.5.5_1 | yes | yes |
| duncan | 2026.5.5_1 | yes | yes |
| james | 2026.5.5_1 | yes | yes |
| lily | 2026.5.5_1 | yes | yes |
| logan | 2026.5.5_1 | yes | yes |
| marcus | 2026.5.5_1 | yes | yes |
| priya | 2026.5.5_1 | yes | yes |
| rihanna | 2026.5.5_1 | yes | yes |
| sia | 2026.5.5_1 | yes | yes |
| sophie | 2026.5.5_1 | yes | yes |
| veronica | 2026.5.5_1 | yes | yes |

**Files changed:**
- `crates/ff-db/src/schema.rs` — V71 migration
- `crates/ff-db/src/migrations.rs` — registered V71
- `crates/ff-db/src/queries.rs` — `pg_list_models_by_workload`
- `crates/ff-gateway/src/server.rs` — `POST /v1/fleet/route`
- `docs/FLEET_CAPABILITY_ROUTING.md` — this doc

**Commit:** `3081985d6`
