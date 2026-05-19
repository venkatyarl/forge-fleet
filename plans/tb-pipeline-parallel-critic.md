# TB.2 — Pipeline-parallel critic across Taylor↔James over Thunderbolt

**Status:** ready to execute — Thunderbolt fabric measured at **18-20 Gbps**
end-to-end on 2026-05-19 (`ff fabric benchmark james taylor`).

## Goal

Run one large code-review/judge model split across Taylor (Mac Studio M3
Ultra 96GB) and James (Mac mini 64GB) so the model's parameters live on
two boxes but appear to clients as a single endpoint. Frees up Taylor's
RAM for other workloads while still serving a frontier-class critic.

## Candidate models

| Model | Size (Q5_K_M) | Why |
|-------|---------------|-----|
| **Qwen3-Next-80B-A3B** | ~50 GB | MoE; 3B active params; best fit for split |
| DeepSeek-R1-70B | ~48 GB | Dense; harder to split but strong on judgment |
| Llama-3.3-70B-Instruct | ~45 GB | Battle-tested baseline |

Qwen3-Next is the recommended start — sparse activations mean Thunderbolt
hop is mostly empty per token (only ~3B params active), so 18 Gbps is
plenty even at 64K context.

## Runtime choice

- **mlx**: Apple-native, only runs on Mac. `mlx_lm.server` doesn't
  support multi-host out of the box.
- **llama.cpp**: has experimental `--rpc` mode where one node is the
  master and others serve layer ranges. Less mature on macOS but works.
- **vLLM**: Linux/CUDA only — not an option for Taylor+James (both macOS).

Recommendation: **llama.cpp RPC mode**, master on Taylor, worker on James.

## Steps

1. **Build llama.cpp on both with `GGML_RPC=ON`**

```bash
# On Taylor + James:
cd ~/build
git clone https://github.com/ggerganov/llama.cpp llama-rpc
cd llama-rpc
make GGML_RPC=ON LLAMA_METAL=1 -j8
```

2. **Download Qwen3-Next-80B-A3B-Q5_K_M.gguf on Taylor**

```bash
ff model download qwen3-next-80b-a3b --quant Q5_K_M --node taylor
```

3. **Start RPC worker on James (port 50051)**

```bash
ssh james "cd ~/build/llama-rpc && ./bin/rpc-server -H 10.44.0.2 -p 50051 -t 16"
```

4. **Start RPC master on Taylor with worker pinned via TB IP**

```bash
~/build/llama-rpc/bin/llama-server \
  --model ~/models/qwen3-next-80b-a3b/qwen3-next-80b-a3b-Q5_K_M.gguf \
  --rpc 10.44.0.2:50051 \
  --host 0.0.0.0 --port 55005 \
  --ctx-size 32768 --parallel 2 \
  --n-gpu-layers 999 \
  --threads 24
```

5. **Register the deployment**

```bash
ff model load qwen3-next-80b-a3b --node taylor --port 55005 \
  --runtime llama.cpp \
  --note "pipeline-parallel across taylor+james via Thunderbolt rpc"
```

6. **Update fleet_resolver to route `consensus.judge.large` here**

```sql
INSERT INTO model_tiers (tier_id, model_id, endpoint, role, priority)
VALUES ('judge.large', 'qwen3-next-80b-a3b', 'http://taylor:55005/v1/chat/completions', 'judge', 100);
```

## Smoke tests

```bash
# Latency:
curl -s -X POST http://taylor:55005/v1/chat/completions \
  -d '{"model":"default","messages":[{"role":"user","content":"3+5?"}]}' \
  -w '\ntime=%{time_total}s\n'

# Compare to single-host baseline (taylor mlx 55001):
ff swarm run "compare a tricky code edit on both endpoints"
```

## Risks

- **Master crashes lose worker context** — set `--n-keep` and configure
  reconnect. Worth running under supervisor (`launchctl` already does
  this for the inference processes via fleet_model_deployments).
- **TB hop adds 1-2ms latency per token** — fine for 30-100 token/s
  steady-state generation but visible to interactive users.
- **James is also our Postgres hot-standby target** — coordinate with
  TB.3 to avoid contending for TB bandwidth during pgrestore windows.

## Rollback

```bash
ff model unload <deployment-id>
ssh james "pkill rpc-server"
```
