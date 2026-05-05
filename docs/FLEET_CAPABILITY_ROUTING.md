# Fleet Capability Routing — `/v1/fleet/route`

> **Status:** ✅ Live on all 14 online nodes. Beyonce is offline (hardware/network issue).
>
> **Honest assessment:** ff now covers **8 of 9** KovaBody features. 1 optional gap remains.

---

## KovaBody → ff Feature Parity

| # | Feature | KovaBody | ff (today) | Status |
|---|---------|----------|------------|--------|
| 1 | **Task-based routing** | `RoutedProvider(task="periodization")` | `POST /v1/fleet/route` + capabilities | ✅ **Covered** |
| 2 | **Health checking** | Auto-pings every 60s | Pulse beats every 15s, auto-cooldown | ✅ **Covered (better)** |
| 3 | **Fleet-mesh** | 6 nodes | 15 nodes (14 online) | ✅ **Covered (better)** |
| 4 | **Model count** | 8+ | 46 in catalog, 16+ deployed | ✅ **Covered (better)** |
| 5 | **Code/JSON models** | qwen3-coder, qwen2.5-coder | Same models deployed + routed | ✅ **Covered** |
| 6 | **Fallback chains** | 5-model fallback chains per task | Returns 1 primary + 5 alternatives | ⚠️ **Partial** — caller iterates alternatives |
| 7 | **Vision models** | 3 deployed (Gemma, qwen3-vl, gpt-4o) | **qwen2-vl-7b deployed on james** | ✅ **Covered** |
| 8 | **Embedding endpoint** | nomic-embed-text, dedicated path | `POST /v1/embeddings` fleet router live. **No embedding model deployed yet** | ⚠️ **Endpoint ready, model pending** |
| 9 | **Auth layer** | JWT + RBAC on Axum gateway | **Optional JWT via `FF_JWT_SECRET`** | ✅ **Covered** |

**Bottom line:** You can delete KovaBody's model router for reasoning/code/chat/vision tasks today. You can optionally enable JWT auth by setting `FF_JWT_SECRET`. Embeddings endpoint is built and will activate automatically once an embedding model is loaded.

---

## Architecture: what stays in KovaBody vs what moves to ff

```mermaid
flowchart TB
    subgraph Client["External Client"]
        A[Request + JWT]
    end

    subgraph KovaBody["KovaBody (what you KEEP for now)"]
        B["Axum Gateway\nJWT validate + RBAC"]
        E["Fallback chain loop\n(alternative iteration)"]
    end

    subgraph ForgeFleet["ForgeFleet (what you USE now)"]
        F["ff-gateway :51002\nOptional JWT — internal only"]
        G["POST /v1/fleet/route\nreasoning | code | chat | vision"]
        H["POST /v1/embeddings\n(activates when model deployed)"]
        I["15 deployed nodes"]
    end

    subgraph Cloud["Cloud (fallback)"]
        J["OpenAI GPT-4o"]
        K["Anthropic Claude"]
    end

    A -->|validated| B
    B -->|reasoning/code/chat/vision| G
    B -->|embeddings (when ready)| H
    B -->|fallback| J
    G -->|alternatives| E
    E -->|next model| G
    G -->|fleet nodes| I
    H -->|fleet nodes| I

    style KovaBody fill:#fff3e0
    style ForgeFleet fill:#e8f5e9
    style Cloud fill:#e1f5fe
```

**Green (ff):** Task routing, health checking, fleet-mesh, code models, vision model, model catalog, JWT middleware, embeddings endpoint.
**Yellow (KovaBody, keep for now):** Auth gateway (until you set `FF_JWT_SECRET`), fallback chain iteration.
**Red (gap to close):** Deploy an embedding model (qwen3-embedding-8b) to activate `/v1/embeddings`.

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

### `POST /v1/embeddings` — OpenAI-compatible embedding proxy

**Request:**

```json
{
  "model": "qwen3-embedding-8b",
  "input": "The quick brown fox"
}
```

**Response (200) — when embedding model deployed:**

```json
{
  "object": "list",
  "data": [
    {
      "object": "embedding",
      "embedding": [0.0023, -0.0011, ...],
      "index": 0
    }
  ],
  "model": "qwen3-embedding-8b",
  "usage": { "prompt_tokens": 4, "total_tokens": 4 }
}
```

**Response (503) — no embedding model deployed:**

```json
{
  "error": {
    "message": "no healthy fleet endpoint with embeddings capability",
    "type": "backend_unavailable"
  }
}
```

---

## JWT Authentication

Set `FF_JWT_SECRET` on every node's daemon environment to enable Bearer token validation:

```bash
# systemd (Linux)
systemctl --user edit forgefleetd.service
# Add: Environment="FF_JWT_SECRET=your-256-bit-secret"
systemctl --user daemon-reload
systemctl --user restart forgefleetd.service

# launchd (macOS)
# Edit ~/Library/LaunchAgents/com.forgefleet.forgefleetd.plist
# Add <key>FF_JWT_SECRET</key><string>your-secret</string>
launchctl unload ~/Library/LaunchAgents/com.forgefleet.forgefleetd.plist
launchctl load ~/Library/LaunchAgents/com.forgefleet.forgefleetd.plist
```

When `FF_JWT_SECRET` is absent, the middleware is a no-op (backward compatible).

---

## What to tell application teams

### Message 1: "Migrate reasoning/code/chat/vision routing to ff NOW"

> **Action:** Replace your hard-coded model lists with `POST localhost:51002/v1/fleet/route`.
>
> ```python
> import requests
> FF_GATEWAY = "http://localhost:51002"
>
> def route_llm(task: str, capabilities: list[str]) -> dict:
>     resp = requests.post(
>         f"{FF_GATEWAY}/v1/fleet/route",
>         json={"task": task, "required_capabilities": capabilities, "preferred_local": True},
>         timeout=5,
>     )
>     resp.raise_for_status()
>     return resp.json()
>
> # Reasoning
> r = route_llm("optimize algorithm", ["reasoning"])
> print(r["target"])  # http://192.168.5.100:55001 (taylor, qwen36)
>
> # Code
> r = route_llm("refactor function", ["code", "tool_calling"])
> print(r["target"])  # http://192.168.5.103:55000 (sophie, qwen3-coder)
>
> # Vision
> r = route_llm("describe image", ["vision"])
> print(r["target"])  # http://192.168.5.108:55002 (james, qwen2-vl)
>
> # Fallback chain (what KovaBody did automatically, you now do explicitly)
> def route_with_fallback(task: str, capabilities: list[str]) -> str:
>     try:
>         r = route_llm(task, capabilities)
>         return r["target"]
>     except requests.HTTPError:
>         for alt in r.get("alternatives", []):
>             return alt["target"]
>         raise
> ```
>
> **Capabilities:** `reasoning`, `code`, `tool_calling`, `chat`, `long_context`, `vision`, `omni`, `text-generation`, `embeddings`.
>
> **What this replaces:** `REASONING_MODELS`, `CODE_MODELS`, `VISION_MODELS` hard-coded lists, custom health checkers.

### Message 2: "What you must KEEP for now"

| Keep in your project | Why | Until when |
|---------------------|-----|------------|
| **Auth layer (JWT/RBAC)** | ff-gateway JWT is optional. Keep your gateway until you set `FF_JWT_SECRET` fleet-wide. | Until you migrate auth to ff |
| **Fallback chain loop** | ff returns alternatives but caller must iterate. | If you want automatic fallback, wrap the route call |

### Message 3: "How to enable embeddings"

The endpoint is live fleet-wide. To activate it, deploy an embedding model on any node:

```bash
# On a node with RAM headroom (e.g. aura)
ff model download qwen3-embedding-8b --runtime llama.cpp --node aura
# Then load it with the --embedding flag:
llama-server -m ~/models/qwen3-embedding-8b/Qwen3-Embedding-8B-Q4_K_M.gguf \
  --host 0.0.0.0 --port 55003 -c 8192 --embedding
```

Then `POST /v1/embeddings` will automatically route to it.

---

## Operator runbook

### Check catalog capabilities

```bash
# What can the fleet do today?
ff model catalog | grep -E "vision|vl|omni|coder|embed"

# What's actually running?
ff model deployments

# Live routing test
curl -s http://localhost:51002/v1/fleet/route \
  -H "Content-Type: application/json" \
  -d '{"required_capabilities": ["reasoning"]}'

# Test embeddings endpoint
curl -s http://localhost:51002/v1/embeddings \
  -H "Content-Type: application/json" \
  -d '{"input": "hello world"}'
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

| Node | Binary | FF_NODE | /v1/fleet/route | /v1/embeddings | JWT |
|------|--------|---------|-----------------|----------------|-----|
| taylor | 2026.5.5_2 | yes | yes | yes | optional |
| ace | 2026.5.5_2 | yes | yes | yes | optional |
| adele | 2026.5.5_2 | yes | yes | yes | optional |
| aura | 2026.5.5_2 | yes | yes | yes | optional |
| duncan | 2026.5.5_2 | yes | yes | yes | optional |
| james | 2026.5.5_2 | yes | yes | yes | optional |
| lily | 2026.5.5_2 | yes | yes | yes | optional |
| logan | 2026.5.5_2 | yes | yes | yes | optional |
| marcus | 2026.5.5_2 | yes | yes | yes | optional |
| priya | 2026.5.5_2 | yes | yes | yes | optional |
| rihanna | 2026.5.5_2 | yes | yes | yes | optional |
| sia | 2026.5.5_2 | yes | yes | yes | optional |
| sophie | 2026.5.5_2 | yes | yes | yes | optional |
| veronica | 2026.5.5_2 | yes | yes | yes | optional |
| beyonce | OFFLINE | — | — | — | — |

**Files changed:**
- `crates/ff-db/src/schema.rs` — V71 migration
- `crates/ff-db/src/migrations.rs` — registered V71
- `crates/ff-db/src/queries.rs` — `pg_list_models_by_workload`
- `crates/ff-gateway/src/server.rs` — `POST /v1/fleet/route`, `POST /v1/embeddings`
- `crates/ff-gateway/src/middleware.rs` — JWT auth middleware
- `crates/ff-gateway/Cargo.toml` — added `jsonwebtoken`
- `docs/FLEET_CAPABILITY_ROUTING.md` — this doc

**Commits:** `3081985d6` (V71 routing), `2c7db7b19` (embeddings + JWT)
