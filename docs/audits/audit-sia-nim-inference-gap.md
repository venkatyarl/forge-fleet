# Audit: sia NIM inference gap

Date: 2026-07-20

## Executive summary

`sia` is not offline and no longer has a missing local inference process. It is an
ARM64 DGX Spark / GB10 host with a responding NVIDIA NIM model-free container:

- Node: `sia`, `192.168.5.116`
- CPU/OS: `aarch64`, Ubuntu 24.04.4 LTS, DGX/Linux profile
- GPU: NVIDIA GB10, driver `580.159.03`
- Fleet-reported unified GPU memory: `127.600748 GB`
- Existing fleet model: llama.cpp `Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf`
  on `55000`
- Existing NIM container: `sia-nim-model-free`,
  `nvcr.io/nim/nvidia/model-free-nim:2.0.8`, published on `55020`
- NIM-served model: `gpt-oss-20b`, OpenAI-compatible `/v1/*`, max model length
  `131072`

Live validation passed:

```text
curl http://127.0.0.1:55020/v1/models
=> model id: gpt-oss-20b

curl http://127.0.0.1:55020/v1/chat/completions ...
=> assistant content: READY
```

The remaining inference gap is integration, not basic model serving: `ff` reports
`sia` as healthy, but `http://127.0.0.1:51000/health` on `sia` still returns
`total_backends: 0`, `healthy_backends: 0`, `busy_backends: 0`. The NIM endpoint
is alive on `55020`, but the daemon/router layer does not appear to register it
as a healthy backend yet.

## NIM / NeMo feasibility

NVIDIA has an official Spark NIM playbook. It describes NIM as containerized
LLM serving for DGX Spark, with Docker, NVIDIA Container Toolkit, NGC auth,
model caching, and HTTP validation as the intended workflow. It also notes that
Spark-compatible NIMs include more than the example Llama 3.1 8B NIM.

Model-free NIM is the right primitive to evaluate on `sia`. NVIDIA documents
that model-free NIM can run supported models from Hugging Face, NGC, S3,
ModelScope, Google Cloud Storage, or a local directory. It is not "arbitrary
model" support in the unlimited sense: unsupported architectures in the selected
backend remain unsupported. For `sia`, that means model choice must be constrained
by the ARM64 container, vLLM/SGLang/TRT-LLM backend support, GB10 memory behavior,
and published/generated NIM profiles.

Fine-tuned/custom models are possible when they are in a supported Hugging Face
or TensorRT-LLM checkpoint shape. NVIDIA's fine-tuned model documentation says
NIM can build a local optimized TensorRT-LLM engine from HF-format weights, but
also warns that fine-tuned TRT-LLM serving requires the legacy TRT-LLM backend in
that documented path.

TensorRT-LLM itself supports many current model architectures, including
Nemotron, DeepSeek-V3, Gemma 3, Llama 3.x/4, Qwen, VILA/LLaVA-class VLMs, and
others. This makes TensorRT-LLM strategically important for Spark, but it is not
the only practical path: the currently running `model-free-nim:2.0.8` selected
vLLM for `gpt-oss-20b`.

Nemotron is competitive enough to keep in the evaluation lane. NVIDIA positions
Nemotron as open, multimodal, agent-focused models for reasoning, coding, vision,
and enterprise use, with DGX Spark support called out. Treat that as a vendor
claim until the fleet has its own coding/reasoning/latency evals; do not replace
Qwen/DeepSeek/Kimi defaults solely on marketing benchmarks.

Sources:

- NVIDIA Spark NIM playbook: https://build.nvidia.com/spark/nim-llm
- NVIDIA model-free NIM docs: https://docs.nvidia.com/nim/large-language-models/latest/deployment/model-free-nim.html
- NVIDIA NIM profile selection docs: https://docs.nvidia.com/nim/large-language-models/latest/deployment/model-profiles-and-selection.html
- NVIDIA fine-tuned model support: https://docs.nvidia.com/nim/large-language-models/1.15.0/ft-support.html
- TensorRT-LLM support matrix: https://nvidia.github.io/TensorRT-LLM/reference/support-matrix.html
- NVIDIA DGX Spark specs: https://www.nvidia.com/en-us/products/workstations/dgx-spark/
- NVIDIA Nemotron overview: https://www.nvidia.com/en-us/ai-data-science/foundation-models/nemotron/

## Live operational findings

1. NIM is already deployed and serving on `sia`.
   - Container: `sia-nim-model-free`
   - Image: `nvcr.io/nim/nvidia/model-free-nim:2.0.8`
   - Host bind: `0.0.0.0:55020 -> 8000/tcp`
   - Model path: `hf://openai/gpt-oss-20b`
   - Served model: `gpt-oss-20b`
   - Backend: vLLM `0.23.0`

2. The endpoint is alive and OpenAI-compatible.
   - `/v1/models` lists `gpt-oss-20b`.
   - `/health` returns HTTP 200.
   - `/v1/chat/completions` returned `content: "READY"` with
     `finish_reason: "stop"`.

3. The daemon/router is not counting the NIM backend.
   - `curl http://127.0.0.1:51000/health` on `sia` returned:
     `total_backends: 0`, `healthy_backends: 0`, `busy_backends: 0`.
   - Fleet status still advertises the llama.cpp `qwen3-coder-30b` server on
     `55000`, not the NIM `gpt-oss-20b` server on `55020`.

4. There is a cache permission problem in the NIM container logs.
   - Repeated log line: `mkdir: cannot create directory '/opt/nim/.cache/tmp':
     Permission denied`.
   - The bind mount is `/var/lib/forgefleet/nim-cache:/opt/nim/.cache`.
   - Fix ownership/permissions for the container user before considering this a
     durable service. This matters for restarts, profile manifests, generated
     kernels, and air-gap cache reuse.

5. GB10 unified memory behavior is visible.
   - `nvidia-smi --query-gpu=memory.total,memory.used` returned `[N/A]` for
     memory fields on GB10.
   - ForgeFleet correctly reports `gpu_total_vram_gb` from the unified memory
     fallback path at roughly `127.6 GB`.
   - NIM logs detected UMA and clamped `gpu_memory_utilization` from `0.92` to
     `0.50`. Capacity planning must reserve substantial host RAM for the OS,
     daemon, KV cache, and concurrent containers.

6. A secret is present in the container environment.
   - `docker inspect` showed a Hugging Face token in environment variables.
   - Do not store long-lived tokens directly in Docker env if the inspect surface
     is available to operators or logs. Move model credentials into ForgeFleet
     secrets or a root-readable env file/systemd credential and rotate the
     exposed token.

## Recommended fix for the actual fleet gap

1. Keep the current NIM container running as the experimental Spark NIM backend,
   but make it durable:
   - Create a systemd unit or ForgeFleet-managed service for
     `sia-nim-model-free`.
   - Mount `/var/lib/forgefleet/nim-cache` with write permissions for the NIM
     runtime user.
   - Move `HF_TOKEN` out of the Docker inspectable command/env path and rotate
     the current token.
   - Pin `nvcr.io/nim/nvidia/model-free-nim:2.0.8` initially; only upgrade after
     profile and startup validation.

2. Register `http://127.0.0.1:55020/v1` as a local inference backend for `sia`.
   - Model id: `gpt-oss-20b`
   - Runtime: `nim-vllm`
   - Capabilities: `chat`, `reasoning`, `tool_use` only after tool-call evals
   - Context: `131072`
   - Health probe: `GET /health` and `GET /v1/models`
   - Completion probe: deterministic one-word response with enough `max_tokens`
     for gpt-oss reasoning tokens, e.g. `max_tokens >= 64`

3. Keep `55000` llama.cpp Qwen3-Coder on `sia` until NIM passes fleet routing
   tests. It is currently the known code lane on the node.

4. Add a short fleet health distinction if not already present:
   - daemon reachable
   - advertised local model servers
   - registered router backends
   - direct unaffiliated local listeners such as NIM on `55020`

That distinction would have made this task clearer: `sia` had a responding NIM
server, but not a registered backend.

## Best open-model lineup by lane and memory envelope

Assumption: a single DGX Spark has 128 GB unified memory, but effective safe
serving budget should be treated as about 60-90 GB for one interactive model
unless the service has measured headroom. Two Spark nodes can stretch larger via
networked/distributed setups, but the first target here is one stable local
backend on `sia`.

### 0-10 GB effective model size

- Fast lane / SLM: `qwen35-9b` or `qwen3-7b` via llama.cpp.
- Small tool/chat: `phi-4-mini` or `llama31-8b` when licensing allows.
- Embedding: `bge-m3`; rerank: `bge-reranker-v2-m3`.
- Vision fallback: `qwen3-vl-8b` or `llava-onevision-qwen2-7b-si`.

Use this envelope for low-latency routing, always-on small nodes, embeddings,
rerankers, and cheap speculative/draft work.

### 10-25 GB effective model size

- Coding: `qwen3-coder-30b` GGUF Q4 or vLLM/NIM if a compatible profile is
  available.
- General/tool calling: `qwen36-35b-a3b` or `qwen35-35b-a3b`.
- Reasoning: `deepseek-r1-distill-qwen-32b`.
- Vision/doc: `qwen3-vl-30b-a3b`, already represented in the catalog.
- NIM evaluation: `gpt-oss-20b` on model-free NIM is currently proven live on
  `sia`.

This is the best default Spark lane: strong quality, fast enough, and compatible
with unified-memory safety margins.

### 25-60 GB effective model size

- Coding/general: `qwen3-70b`, `qwen3-72b`, or `llama-3.3-70b-instruct` when
  license/auth are acceptable.
- Vision: `llama-3.2-vision-90b` quantized if local runtime support is stable.
- Dense long-context: prefer Qwen 70B/72B variants with explicit context caps to
  avoid KV-cache surprises.

Use this for high-quality interactive work on idle Spark nodes. Avoid running it
beside other large services unless routing enforces exclusivity.

### 60-100 GB effective model size

- Multilingual/tool/general: `mistral-large-2411` quantized.
- Larger code/reasoning experiments: DeepSeek-Coder-V2 only if quantized size
  and KV cache are proven under the actual workload.

This is a batch/offload lane, not an always-on default for `sia`.

### >100 GB / distributed lane

- Frontier code: `qwen3-coder-480b` quantized/distributed.
- Frontier reasoning: `kimi-k2-thinking` or large DeepSeek/Qwen MoE variants.
- Very large Nemotron 3 variants: evaluate through NIM Day 0 or multi-node
  profiles, not as first-line `sia` service.

This belongs on a ring of Spark/DGX nodes or a leader-class host with explicit
admission control. Do not use it to solve the immediate `sia` backend gap.

## Nemotron recommendation

Evaluate Nemotron, but do not make it the first production default on `sia`.

Recommended order:

1. Keep `gpt-oss-20b` model-free NIM as the NIM canary because it is already
   live and validated.
2. Add one Nemotron model that fits a single Spark with headroom, preferably the
   smallest current Nemotron/Nemotron-Nano profile available for ARM64 Spark, and
   benchmark it against `qwen36-35b-a3b`, `qwen3-coder-30b`, and
   `deepseek-r1-distill-qwen-32b`.
3. Use objective fleet tasks for the comparison:
   - code patch generation and review
   - tool-call JSON validity
   - long-context repo Q&A
   - small reasoning/math
   - latency, memory pressure, restart time
4. Promote Nemotron only for lanes where it wins on local evals or provides a
   specific NIM/TRT-LLM operational advantage.

## Verification commands run

```text
ff/fleet status refresh
fleet_worker_detail sia
fleet_pulse sia
ssh sia: hostname; uname -m; nvidia-smi; docker ps
ssh sia: curl http://127.0.0.1:55000/v1/models
ssh sia: curl http://127.0.0.1:55020/v1/models
ssh sia: curl http://127.0.0.1:55020/health
ssh sia: curl http://127.0.0.1:55020/v1/chat/completions
ssh sia: docker logs --tail=80 sia-nim-model-free
ssh sia: docker inspect sia-nim-model-free
```

No Rust/TypeScript code was changed for this task; the required diff is this
audit report.
