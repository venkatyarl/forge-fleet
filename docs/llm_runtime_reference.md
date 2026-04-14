# LLM Runtime Reference

Concrete launch, health, and shutdown commands for the three runtimes
ForgeFleet uses to serve OpenAI-compatible inference: **llama.cpp**,
**MLX** (Apple Silicon), and **vLLM** (NVIDIA).

All three expose the OpenAI `/v1/chat/completions` and `/v1/completions`
routes so the `ff` CLI can talk to them uniformly.

---

## 1. llama.cpp (CPU / Metal / CUDA)

### Install / Binary path

Built from source on Taylor:

```
~/taylorProjects/llama.cpp/build/bin/llama-server
```

Build (one-time):

```bash
cd ~/taylorProjects/llama.cpp
cmake -B build -DGGML_METAL=ON
cmake --build build --config Release -j
```

On NVIDIA boxes substitute `-DGGML_CUDA=ON`.

### Launch command

```bash
~/taylorProjects/llama.cpp/build/bin/llama-server \
  --model /models/qwen2.5-coder-32b-instruct-q4_k_m.gguf \
  --host 0.0.0.0 \
  --port 8080 \
  --ctx-size 32768 \
  --parallel 4 \
  --n-gpu-layers 99 \
  --threads 8 \
  --cont-batching \
  --mlock \
  --alias qwen2.5-coder-32b
```

Flag mapping:

| Concept           | Flag               |
|-------------------|--------------------|
| Model path        | `--model` / `-m`   |
| Bind host         | `--host`           |
| Port              | `--port`           |
| Context size      | `--ctx-size` / `-c`|
| Parallel requests | `--parallel` / `-np` |
| GPU layers        | `--n-gpu-layers` / `-ngl` |
| CPU threads       | `--threads` / `-t` |
| Model name alias  | `--alias`          |

Set `--n-gpu-layers 99` to push all layers onto Metal/CUDA. Use `0` for
pure CPU.

### Health endpoint

```
GET http://<host>:8080/health
```

Returns `{"status":"ok"}` once the model is loaded. Also exposes
`GET /v1/models` for OpenAI-compatible discovery.

```bash
curl -s http://192.168.5.102:8080/health
curl -s http://192.168.5.102:8080/v1/models
```

### Shutdown

`llama-server` handles `SIGTERM` cleanly: it stops accepting new requests,
finishes in-flight generations, then exits. No drain flag needed.

```bash
kill -TERM <pid>          # clean shutdown
kill -KILL <pid>          # only if wedged
```

### Warm-up

30B Q4_K_M on Mac Studio M3 Ultra (Metal): ~8 seconds from launch to
`/health` returning ok. On a 32GB Ubuntu box with CUDA offload of ~30
layers: ~15 seconds. On pure CPU: ~40 seconds.

---

## 2. MLX (Apple Silicon only)

### Install

```bash
pip install mlx-lm
```

Pulls `mlx`, `mlx-lm`, `transformers`, `huggingface_hub`.

### Launch command

```bash
mlx_lm.server \
  --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit \
  --host 0.0.0.0 \
  --port 8081 \
  --log-level INFO
```

If you have a local path instead of an HF repo id:

```bash
mlx_lm.server \
  --model /Users/venkat/models/qwen2.5-coder-32b-mlx-4bit \
  --host 0.0.0.0 \
  --port 8081
```

Flag mapping:

| Concept           | Flag / Mechanism                              |
|-------------------|-----------------------------------------------|
| Model path        | `--model` (HF id or local dir)                |
| Bind host         | `--host`                                      |
| Port              | `--port`                                      |
| Context size      | Controlled per-request via `max_tokens`; server reads model config for max |
| Parallel requests | MLX serves requests sequentially; run multiple `mlx_lm.server` processes on different ports for concurrency |
| GPU layers        | N/A — MLX always uses the unified-memory GPU  |

### Health endpoint

MLX does not ship a `/health` route. Use the OpenAI discovery endpoint:

```
GET http://<host>:8081/v1/models
```

```bash
curl -s http://192.168.5.100:8081/v1/models
```

Returns `{"object":"list","data":[{"id":"...","object":"model",...}]}`
once the model is loaded into memory.

### Shutdown

`SIGTERM` exits immediately; there is no graceful drain because requests
are sequential and short-lived at the server layer.

```bash
kill -TERM <pid>
```

### Warm-up

30B 4-bit MLX on Mac Studio M3 Ultra 96GB: ~12 seconds (weight mmap +
first Metal kernel compile). On Mac mini M4 64GB: ~18 seconds. First
request after warm-up pays another ~2 seconds of kernel specialization.

### On-the-fly safetensors -> MLX conversion

When `mlx-community` does not publish a pre-converted 4-bit variant,
convert from the original HF safetensors:

```bash
mlx_lm.convert \
  --hf-path Qwen/Qwen2.5-Coder-32B-Instruct \
  --mlx-path /Users/venkat/models/qwen2.5-coder-32b-mlx-4bit \
  --quantize \
  --q-bits 4
```

Flags:

| Flag          | Purpose                                         |
|---------------|-------------------------------------------------|
| `--hf-path`   | HF repo id or local safetensors directory       |
| `--mlx-path`  | Output directory for the MLX-format weights     |
| `--quantize`  | Enable weight quantization                      |
| `--q-bits`    | Bit width: 2, 3, 4, 6, or 8 (use 4 for 30B)     |
| `--q-group-size` | Defaults to 64; lower = higher quality, larger file |

Conversion time for a 32B model on M3 Ultra: ~4 minutes. Output
directory is ~17 GB at 4-bit. After conversion, launch with
`mlx_lm.server --model /Users/venkat/models/qwen2.5-coder-32b-mlx-4bit`.

---

## 3. vLLM (NVIDIA CUDA)

### Install

```bash
pip install vllm
```

Requires CUDA 12.1+ and a compatible PyTorch. On the DGX Sparks use the
NGC PyTorch base image.

### Launch command

```bash
vllm serve Qwen/Qwen2.5-Coder-32B-Instruct \
  --host 0.0.0.0 \
  --port 8000 \
  --max-model-len 32768 \
  --max-num-seqs 16 \
  --tensor-parallel-size 1 \
  --gpu-memory-utilization 0.92 \
  --dtype auto \
  --served-model-name qwen2.5-coder-32b
```

For a local weight directory:

```bash
vllm serve /models/qwen2.5-coder-32b-instruct \
  --host 0.0.0.0 --port 8000 --max-model-len 32768 --max-num-seqs 16
```

Flag mapping:

| Concept           | Flag                          |
|-------------------|-------------------------------|
| Model path        | positional arg (HF id or dir) |
| Bind host         | `--host`                      |
| Port              | `--port`                      |
| Context size      | `--max-model-len`             |
| Parallel requests | `--max-num-seqs`              |
| GPU layers        | N/A — vLLM always full-GPU; split across GPUs with `--tensor-parallel-size N` |
| Mem headroom      | `--gpu-memory-utilization` (0.0-1.0) |
| Model alias       | `--served-model-name`         |

### Health endpoint

```
GET http://<host>:8000/health
```

Returns HTTP 200 with empty body when the engine is ready. Also exposes
`GET /v1/models`.

```bash
curl -sf http://dgx-1:8000/health && echo ok
curl -s  http://dgx-1:8000/v1/models
```

### Shutdown

vLLM handles `SIGTERM` gracefully: it stops the HTTP listener,
finishes batches currently in the scheduler, then tears down the engine.
Drain typically completes in under 5 seconds at `--max-num-seqs 16`.

```bash
kill -TERM <pid>          # graceful, drains in-flight
kill -KILL <pid>          # force; only if stuck in CUDA
```

### Warm-up

30B FP16 on a single H100 80GB: ~45 seconds (weight load + CUDA graph
capture). 30B AWQ-INT4 on an RTX 4090 24GB: ~25 seconds. On DGX Spark
(Grace Hopper unified memory): ~35 seconds.

---

## Quick reference matrix

| Runtime   | Binary / Entry        | Health path       | Clean signal | 30B warm-up |
|-----------|-----------------------|-------------------|--------------|-------------|
| llama.cpp | `llama-server`        | `/health`         | SIGTERM      | 8-15 s      |
| MLX       | `mlx_lm.server`       | `/v1/models`      | SIGTERM      | 12-18 s     |
| vLLM      | `vllm serve`          | `/health`         | SIGTERM      | 25-45 s     |

All three speak the same OpenAI REST dialect at `/v1/chat/completions`,
so the `ff` router treats them as interchangeable endpoints once the
health probe passes.
