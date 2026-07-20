# Fleet-wide LLM node placement audit

Date: 2026-07-20  
Scope: interim three-cable layout and eventual two-triangle ConnectX-7 layout

## Decision

Use the six DGX Sparks as three disjoint pairs now, then regroup them into a code/reasoning triangle and a production/multimodal triangle when six cables are available. Keep routine work on single-node services and reserve distributed deployments for models that materially benefit from more than 128 GB. Route to the smallest capable model; the large services are escalation capacity, not defaults.

This plan is based on the live `computers`, `fleet_model_catalog`, `fleet_model_deployments`, and `fleet_model_library` records. The six Sparks each report about 127.6 GB unified GPU memory. The four 123 GB Linux nodes report only about 2 GB of GPU memory and are therefore treated as CPU/RAM inference nodes until ROCm detection and usable memory are demonstrated.

## Interim: three cables

Cable the nodes as `adele—beyonce`, `rihanna—shakira`, and `sia—thalia`. A pair runs either one distributed service or two independent single-Spark services, never both at assumed full capacity.

| Node | Normal single-node placement | Paired placement / company role |
|---|---|---|
| adele | Qwen3-Coder-30B-A3B (coding and tool use) | With beyonce: Qwen3-Coder-480B-A35B at a validated low-bit quant for repository-scale coding |
| beyonce | Mistral Large 123B Q4 if it passes the memory gate; otherwise Qwen3.5-35B-A3B | With adele: Qwen3-Coder-480B; reuse and checksum the existing stale artifact before serving |
| rihanna | Qwen3-235B at a quant/context that stays below the memory gate; fallback Qwen3.5-35B | With shakira: DeepSeek-V3 Q2 only after the acceptance gate below; hard research/reasoning |
| shakira | Qwen3-VL-30B-A3B plus the required vision projector (documents, vision, marketing review) | With rihanna: DeepSeek-V3 experimental pair; otherwise Qwen3-235B distributed |
| sia | MiniMax-M2.7 only after manifest/runtime validation; fallback Mistral Large 123B | With thalia: Qwen3.5-397B low-bit (council reasoning and long-form research) |
| thalia | Stable Diffusion 3.5 Large (creative and marketing images) | With sia: Qwen3.5-397B; unload image generation while the pair is reserved |
| taylor | Llama 3.3 70B Q4 via Metal | Qwen3.5-9B as the fast task/tool fallback; interactive review and overflow reasoning |
| duncan | DeepSeek-Coder-V2-Lite 16B, CPU/RAM | BGE-M3 embedding replica; asynchronous code jobs, not latency-sensitive serving |
| lily | Qwen3.6-35B-A3B Q4, CPU/RAM | Qwen3-Embedding-8B for multilingual retrieval and document indexing |
| logan | Mistral Large 123B Q4 if benchmarked acceptably; otherwise Qwen3.6-35B | Qwen3-Coder-30B fallback; marketing copy and general synthesis |
| veronica | Existing Qwen3.6-35B-A3B | Keep existing BGE-M3 and BGE-Reranker-v2-M3 as the canonical retrieval services |
| james | Existing Qwen3-VL-30B-A3B for batch document/vision work | Qwen3.5-9B task-agent service; retain the existing 61 GB host as vision overflow |
| marcus | Qwen3-Coder-30B-A3B Q4 | Coding concurrency and CI/test diagnosis |
| priya | Qwen3.5-9B | General task/tool agents and control-plane fallback; do not force a 30B model into 31 GB without measured headroom |
| sophie | Qwen3-Coder-30B-A3B Q4 | Coding concurrency; large local disk is an artifact cache, not a reason to overcommit RAM |
| aura | Whisper Large v3 | Kokoro 82M; transcription, audio research, and speech generation |
| ace | Qwen3.5-9B Q4 via Metal | Lightweight interactive task agents; no large-model duty on 16 GB |
| sarah | No resident generative model | Scheduler, health, queue, and routing duties only on 3 GB RAM |

The existing 30B duplication should be reduced only after the specialized services are healthy. Keep at least two independent coding replicas outside the Sparks so pair reservations do not remove baseline coding capacity.

## Eventual: two fully connected triangles

Six cables make two three-edge triangles. Each triangle can run a three-node shard, any one two-node shard plus one independent node, or three independent nodes. It cannot run multiple overlapping pair deployments simultaneously.

### Triangle A — code and frontier reasoning

Nodes: `adele`, `beyonce`, `rihanna`.

- Three-node primary: DeepSeek-V3 671B Q2 (existing artifact is about 246.6 GB), or Qwen3-Coder-480B when larger context/concurrency needs three-way headroom.
- Two-node primary: `adele—beyonce` Qwen3-Coder-480B; rihanna serves Qwen3-235B or a 35B reasoning model.
- Alternate edges: `beyonce—rihanna` Qwen3.5-397B and `adele—rihanna` Qwen3-235B/high-context coding.
- Mission: difficult coding, architecture, council review, deep research, and overflow from Triangle B.

### Triangle B — production and multimodal

Nodes: `shakira`, `sia`, `thalia`.

- Three-node recovery/overflow: DeepSeek-V3 or Qwen3-Coder-480B, using a checksummed replica rather than an untracked copy.
- Normal singles: shakira runs Qwen3-VL-30B; sia runs Mistral Large 123B or a validated MiniMax service; thalia runs Stable Diffusion 3.5.
- Two-node escalation: `sia—thalia` Qwen3.5-397B while shakira preserves vision service; `shakira—sia` a large research model while thalia preserves image generation.
- Mission: document/vision research, marketing text and images, production content, and second-site flagship failover.

## Routing, failover, and admission rules

1. Route by capability and then cost: 9B task agent → 30/35B specialist → 70/123B reviewer → 235B → paired/three-node flagship.
2. Default coding to Qwen3-Coder-30B and research/marketing to 35B or 70/123B. Escalate only on a failed review, repository-scale context, or an explicitly high-value request.
3. Keep distributed flagships cold until a queue threshold is met; batch work, reserve all participating nodes, drain their single-node services, and unload after an idle timeout.
4. A node or ConnectX link failure disables that distributed placement. Do not silently relaunch it over ordinary Ethernet and advertise equivalent service. Restore the survivor's single-node model and route flagship work to the other triangle or cloud.
5. Pin one verified giant-model artifact plus one checksummed recovery copy. Keep smaller specialist replicas on separate failure domains; avoid copying every giant model to every Spark.
6. Preserve retrieval on veronica with duncan as a replica, coding on at least two non-Spark nodes, vision on both shakira and james, and task-agent capacity on taylor/ace/priya.
7. Admission uses measured resident weights, runtime buffers, KV cache at the advertised context, and requested concurrency. Keep steady-state memory at or below 85% unless a model-specific soak test justifies more.

## Acceptance gates and uncertainties

- The current DeepSeek-V3 Q2 artifact is about 246.6 GB. Two Sparks expose only about 255.2 GB combined, leaving roughly 8.6 GB before runtime and KV cache. Treat two-Spark service as experimental and reject it unless startup, target context, and concurrency pass with at least 10% measured headroom. Three-Spark placement is the intended eventual configuration.
- Qwen3-Coder-480B and Qwen3.5-397B quant sizes are not recorded in the live library. Select a quant only after downloaded size plus runtime/KV calculations pass the same gate; names and parameter counts alone are not capacity evidence.
- A ConnectX-7 cable is not proof of usable distributed inference. Before enabling a placement, verify link state and bandwidth, firmware symmetry, RDMA/GPUDirect behavior, runtime tensor/pipeline parallel support, correct shard loading, and failure fencing.
- Verify the model manifests, licenses, chat templates, tool-call behavior, and runtime support for Qwen3.6-35B-A3B and MiniMax-M2.7 before making them defaults.
- Benchmark latency and quality per workload. CPU/RAM nodes are appropriate for queues and redundancy, but interactive routing should prefer Metal or Spark services.
- The catalog currently labels Qwen3-VL-30B-A3B as `72B`; reconcile that metadata with the actual artifact before capacity automation relies on it.

## Rollout order

1. Benchmark and health-check the existing small/30B, retrieval, vision, and audio services; establish the non-Spark fallbacks.
2. Cable and validate all three interim links, then prove reservation/drain/fencing with a smaller distributed model.
3. Bring up Qwen3-Coder-480B and Qwen3.5-397B pairs one at a time. Attempt the DeepSeek-V3 pair only under its stricter gate.
4. When three additional cables arrive, form the two triangles, validate every edge, and qualify three-node DeepSeek-V3 first.
5. Record benchmark scores and usable context in the existing deployment/catalog tables so the current fleet router, rather than a separate placement mechanism, makes production choices.

## Council note

The required `ff council --members codex,kimi` review was run. Codex returned a placement proposal consistent with the three-pair/two-triangle design; Kimi exited without a usable answer, so no two-model consensus is claimed. The memory gates and live artifact-size caveat above were applied after the council response.
