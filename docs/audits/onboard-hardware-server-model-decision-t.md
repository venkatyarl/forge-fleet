# Audit: Onboard hardware → server → model decision table (auto-select runtime + models)

- **Date:** 2026-07-19
- **Work item:** Onboard: hardware→server→model decision table (auto-select runtime + models)
- **Branch:** `feature/onboard-hardware-server-model-decision-t-0d66`
- **Type:** research / plan audit (no code changes; this report is the deliverable)

## TL;DR

The fleet already has **four independent, partially-contradictory runtime decision points**
and full model-lifecycle plumbing (catalog → library → jobs → defer queue). What is
missing is (a) one canonical decision table consumed by all of them, (b) two input
signals (`arch`, `cpu_flags`) that are detected nowhere or detected but not persisted,
(c) three of the seven models the 2026-07-18 research names (`gpt-oss-120b`,
`glm-4.5-air`, `qwen3-4b-2507` are absent from the DB-seeded catalog), and (d) any
onboard-time seeding of `fleet_model_jobs`. Recommendation: implement the table as a
pure function server-side in the self-enroll path (`ff-gateway::onboard::self_enroll`),
delegating to an extended `ff_runtime::detector::recommend`, and seed model downloads
through the **existing** cross-node deferred-download pattern from
`crates/ff-terminal/src/model_cmd.rs`. Keep the three existing runtimes; encode
backend flavor (CUDA / Vulkan / ROCm / OpenVINO / CPU / Metal) as metadata, not as new
runtimes.

---

## 1. What exists today (discovery-first inventory)

### 1.1 Hardware detection

| Signal | Where detected | Where persisted |
|---|---|---|
| `gpu_kind` | `ff_pulse::heartbeat_v2::detect_gpu_kind` (crates/ff-pulse/src/heartbeat_v2.rs:716) — macOS+aarch64 → `apple_silicon`; `nvidia-smi` ok → `nvidia_cuda`; `rocm-smi` ok → `amd_rocm`; else `none` | `computers.gpu_kind` via `UPSERT_COMPUTER_ROW_SQL` (crates/ff-pulse/src/materializer.rs:43) |
| OS family (incl. DGX) | `detect_os_info` (heartbeat_v2.rs:637) — kernel ending `-nvidia` → `linux-dgx`; same rule server-side in `derive_os_family` (crates/ff-gateway/src/onboard.rs:381) | `computers.os_family` |
| GPU VRAM / unified pool | `detect_gpu_vram_gb` (heartbeat_v2.rs:772) — on GB10/DGX Spark `nvidia-smi` reports `N/A`, falls back to total system RAM | `computers.gpu_vram_gb`, `computers.gpu_total_vram_gb` |
| CPU arch | `OsInfo.arch` in every beat (`std::env::consts::ARCH`) | **NOT persisted** — `computers` has no `arch` column and the materializer upsert drops it |
| CPU flags (AVX2/AVX-512) | **Not detected anywhere** (no `cpu_flags`/`avx` hits in ff-pulse or ff-agent) | — |
| RAM / cores | bootstrap script + heartbeat | `computers.total_ram_gb`, `fleet_workers.ram_gb` |

### 1.2 The four existing runtime decision points

1. **Bootstrap script** (`scripts/bootstrap-computer-template.sh:131-139`) — the one
   onboarding actually uses today. Shell `case`:
   `dgx*` → `vllm`; `macos` → `mlx`; else `has_nvidia ? vllm : llama.cpp`.
   Result is POSTed to self-enroll and written to `fleet_workers.runtime`
   (crates/ff-gateway/src/onboard.rs:528).
2. **`ff_runtime::detector::recommend`** (crates/ff-runtime/src/detector.rs:36) — the
   richest table: Apple Silicon → llama.cpp Metal (MLX alt); Linux+NVIDIA → vLLM (DGX
   Spark special-cased via `GB10`/`Blackwell` model string, tensor-parallel aware);
   Linux+AMD RDNA → llama.cpp **Vulkan**; Intel GPU → llama.cpp Vulkan; CPU-only →
   llama.cpp CPU. Backend flavor comes from `LlamaCppBackend::detect`
   (tests at crates/ff-runtime/src/llamacpp.rs:417). **Not called by onboarding.**
3. **Autoscaler placement** — `runtime_compatible` (crates/ff-agent/src/autoscaler.rs:404):
   `mlx` ⇒ macOS; `vllm` ⇒ `nvidia_cuda | gb10`; else anywhere. `usable_pool_gb`
   (autoscaler.rs:417) already implements the **AMD GTT-unified heuristic**: `amd_rocm`
   with discrete VRAM < 8 GB → treat 75 % of system RAM as the pool (test
   `amd_rocm_gtt_unified_uses_full_ram_pool`, autoscaler.rs:1648).
4. **Placement guard V118** — `check_runtime_placement`
   (crates/ff-agent/src/model_runtime.rs:225) re-states the same policy at load time,
   keyed off `fleet_workers.runtime`.

Plus a fifth mapping in the gateway: `map_gpu_kind`
(crates/ff-gateway/src/orchestrator_adapter.rs:200) translating heartbeat strings to
`ff_core::GpuType` (`amd_rocm` → `AmdRdna`, `intel_gpu` → `IntelGpu`).

### 1.3 Hardware classification precedent

`ff_agent::conformance::classify_hardware` already buckets machines into hardware
profiles — `amd_rocm`/`gfx1151`/`Radeon 8060S` → `"strix-halo"`, else `"generic"`
(test at crates/ff-agent/src/conformance.rs:861). This is the natural naming precedent
for decision-table row keys.

### 1.4 Model lifecycle plumbing (all reusable)

- **Catalog:** the TOML is retired — `config/model_catalog.toml` no longer exists;
  the canonical seed is `SCHEMA_V39_RETIRE_MODEL_CATALOG_TOML`
  (crates/ff-db/src/schema.rs:2561) populating the `model_catalog` table.
  `sync_catalog` is a no-op kept for compat (crates/ff-agent/src/model_catalog.rs).
- **Library / jobs:** `fleet_model_library` (one row per file per node) and
  `fleet_model_jobs` (schema.rs:658; insert at crates/ff-db/src/queries.rs:3054).
- **Cross-node download:** `ff model download --node <n>` and `download-batch` enqueue
  through the deferred-task queue (crates/ff-terminal/src/model_cmd.rs:2458 — "a
  defer-worker on the target claims it"). This is exactly the mechanism onboard-time
  model seeding should reuse.
- **Runtimes:** `model_runtime.rs` launches exactly three servers — `llama-server`,
  `mlx_lm.server`, `vllm serve` — matching the work item's "keep the 3 existing
  runtimes" constraint.

---

## 2. Gap analysis vs the proposed decision table

The researched table (2026-07-18) keys on
`(arch, gpu_kind, has_discrete_vram, ram, cpu_flags)`. Signal availability:

| Input | Status |
|---|---|
| `arch` | Detected every beat; **not persisted** (no `computers.arch` column) and not in the self-enroll payload |
| `gpu_kind` | ✅ persisted, but `intel_gpu` is mapped downstream yet **never emitted** by `detect_gpu_kind`; `gb10` is accepted by `runtime_compatible` but **never written** by any detector |
| `has_discrete_vram` | Derivable: the autoscaler GTT heuristic (`gpu_total_vram_gb < 8` on `amd_rocm`) and the GB10 unified-RAM fallback already encode it, but only implicitly and in two different places |
| `ram` | ✅ `total_ram_gb` / `ram_gb` |
| `cpu_flags` | ❌ not collected anywhere (needed for row 6, AVX2 vs OpenVINO) |

Row-by-row against what the code does today:

| # | Hardware class | Proposed | Today | Gap |
|---|---|---|---|---|
| 1 | DGX Spark (aarch64 CUDA unified) | vLLM (**aarch64/sm_121 image**) + llama-server; gpt-oss-120B / GLM-4.5-Air / Qwen3-Coder-30B | bootstrap → `vllm` (via `has_nvidia`, since Sparks report `os_id=ubuntu`); no image/arch channel encoded anywhere | encode vLLM image/build channel per arch; add missing models |
| 2 | x86 + NVIDIA discrete | vLLM | bootstrap → `vllm` ✅ | none (runtime); model floor missing |
| 3 | x86 + AMD ROCm discrete | vLLM-ROCm | `runtime_compatible` **rejects** vllm on `amd_rocm`; bootstrap → llama.cpp | new decision row + relax placement for ROCm-discrete; no such hardware in fleet today (`usable_pool_gb` comment: "none in fleet today") |
| 4 | AMD unified / Strix Halo | llama-server **Vulkan** (ROCm for long-ctx) | bootstrap → llama.cpp (right answer, wrong reason — falls out of `has_nvidia=false`); Vulkan backend exists only in ff-runtime's unused `recommend` path; conformance already names the `strix-halo` class | make Vulkan-vs-ROCm an explicit recorded choice |
| 5 | Apple Silicon | mlx_lm.server | bootstrap → `mlx` ✅ | none |
| 6 | Intel CPU | llama-server + OpenVINO (2026.1) or AVX2 | falls to generic llama.cpp CPU; no `cpu_flags`, no OpenVINO support at all | new detection (cpu flags, `intel_gpu`) + optional OpenVINO backend metadata |
| 7 | Tiny ≤ 8 GB | llama-server CPU + Qwen3-4B-2507 | no RAM threshold anywhere in runtime choice | new row; add model |

**Model catalog gaps** (checked against the V39 seed's id list):
`qwen3-coder-30b` ✅ exists (llama.cpp Q4_K_M / mlx 4bit / vllm fp16 variants).
`gpt-oss-120b` ❌, `glm-4.5-air` ❌, `qwen3-4b-2507` ❌ — no `gpt-oss` string anywhere
in `crates/`. These need a new catalog seed migration before any decision table can
reference them.

**No onboard-time model seeding exists.** Self-enroll writes `fleet_workers` +
`computers` + SSH mesh propagation and stops; nothing enqueues downloads.

---

## 3. Proposed design

### 3.1 One decision function, one owner

Implement the table as a **pure function** so every current decision point can
delegate to it:

```rust
// ff-runtime (extends detector.rs) or a new ff-core module
pub struct HardwareProfile {
    pub arch: Arch,                 // x86_64 | aarch64
    pub os: OsType,
    pub gpu_kind: GpuKind,          // incl. IntelGpu; gb10 folded into NvidiaCuda+unified
    pub gpu_model: Option<String>,
    pub unified_memory: bool,       // = !has_discrete_vram (GTT/GB10/Apple heuristics)
    pub ram_gb: u32,
    pub cpu_flags: CpuFlags,        // avx2, avx512, amx…
}

pub struct RuntimePlan {
    pub runtime: Runtime,               // LlamaCpp | Mlx | Vllm (unchanged enum)
    pub backend: BackendFlavor,         // Cuda|CudaAarch64Sm121|Rocm|Vulkan|Metal|OpenVino|CpuAvx2|Cpu
    pub secondary: Option<Runtime>,     // e.g. llama-server beside vllm on Spark
    pub slm_floor: &'static str,        // catalog id, e.g. "qwen3-4b-2507"
    pub larger: &'static [&'static str],// optional bigger models if RAM allows
    pub reason: String,
}
```

`ff_runtime::detector::recommend` is the natural home — it already has 5 of the 7
rows and the DGX/GB10 special-casing; extend rather than fork (existing tests
`test_recommend_nvidia` / `test_create_engine_nvidia` at detector.rs:237/267 extend
naturally). `BackendFlavor` is metadata (stored, logged, used to pick install
artifacts) — **not** a new runtime, satisfying the keep-3-runtimes constraint.

### 3.2 Decision table (target state)

| Row key | Match on | runtime / backend | SLM floor | Larger (RAM-gated) |
|---|---|---|---|---|
| `dgx-spark` | aarch64 + nvidia_cuda + unified | vllm / **cuda-aarch64 (sm_121 image!)** + llama-server secondary | qwen3-coder-30b | gpt-oss-120b, glm-4.5-air |
| `nvidia-discrete` | x86_64 + nvidia_cuda + discrete VRAM | vllm / cuda | qwen3-4b-2507 (VRAM-scaled) | per-VRAM |
| `amd-discrete` | x86_64 + amd_rocm + discrete VRAM ≥ 8 GB | vllm / rocm | qwen3-4b-2507 | per-VRAM |
| `strix-halo` | amd_rocm + unified (GTT: dVRAM < 8 GB) — reuse `classify_hardware` | llama.cpp / vulkan (rocm alt for long-ctx) | qwen3-coder-30b | glm-4.5-air if RAM ≥ 96 GB |
| `apple-silicon` | macOS + aarch64 | mlx / metal | qwen3-coder-30b (mlx 4bit) | RAM-gated |
| `intel-cpu` | x86_64, no GPU, avx2 | llama.cpp / openvino-2026.1 if available else cpu-avx2 | qwen3-4b-2507 | — |
| `tiny` | ram_gb ≤ 8 | llama.cpp / cpu | qwen3-4b-2507 | — |

Row order matters: `tiny` wins over everything except a discrete-GPU match; unified
checks run before discrete.

### 3.3 Where it hooks in (onboarding flow)

Decide **server-side in `self_enroll`**, not in the bootstrap shell script. The
script keeps its current crude `case` only as the pre-`ff`-install hint (it decides
whether to install the vllm toolchain, line 365); the payload grows `arch`,
`cpu_flags`, `gpu_model`, `gpu_vram_gb` fields (all cheap to collect in sh), and the
gateway:

1. Runs the decision function → `RuntimePlan`.
2. Writes `fleet_workers.runtime` (existing `pg_upsert_node` path, onboard.rs:545)
   and stores `{backend, hardware_class, plan_reason}` in the `computers.metadata` /
   `fleet_workers.capabilities` JSONB — **no new columns needed** for backend flavor.
3. Enqueues the SLM-floor (and RAM-permitting larger models) as cross-node download
   deferred tasks — the exact `pg_enqueue_deferred` pattern `ff model download --node`
   already uses (model_cmd.rs:2458), targeted at the new node with `trigger_type =
   node_online` so downloads start when the daemon comes up. That fulfills "writes
   fleet_workers.runtime + seeds fleet_model_library jobs" with zero new machinery.

Existing nodes get the same via a new `ff model plan [--apply] [--node <n>]` verb
(dogfooding rule: add the verb, don't route around it) that runs the same function
against `computers` rows and shows/applies the diff.

### 3.4 Consumers to converge (follow-up, same function)

- `runtime_compatible` (autoscaler.rs:404): add `amd-discrete → vllm` allowance;
  derive from the shared table instead of a private match.
- `check_runtime_placement` (model_runtime.rs:225): same.
- Resolve the `gb10` inconsistency: either make `detect_gpu_kind` emit `gb10` on
  Spark (kernel `-nvidia` + aarch64) or drop the string and key on
  `nvidia_cuda && unified` — recommend the latter (one less magic string; the
  heartbeat VRAM fallback already flags unified).
- Make `detect_gpu_kind` emit `intel_gpu` (probe `lspci`/`vainfo` or
  `/sys/class/drm`) so `map_gpu_kind`'s existing `IntelGpu` arm stops being dead.

---

## 4. Required schema / catalog changes

All forward-only, appended after the current highest migration (**V179** as of this
branch — re-check across branches before assigning V180+ to avoid collisions):

1. **Catalog seed migration** adding `gpt-oss-120b`, `glm-4.5-air`, `qwen3-4b-2507`
   to `model_catalog` with per-runtime variants (vllm bf16/fp8 for Spark rows;
   llama.cpp GGUF quants; mlx 4bit where published). Follow the V39 JSONB shape.
2. **`computers.arch` column** (`ALTER TABLE ... ADD COLUMN IF NOT EXISTS arch TEXT`)
   + materializer upsert extension, and optionally `cpu_flags JSONB`. Alternative:
   stash both in `computers.metadata` and add no columns — acceptable for v1; a real
   column is better long-term since the decision table and placement will filter on it.
3. No new tables. `fleet_model_library` / `fleet_model_jobs` / `deferred_tasks` /
   `model_catalog` cover everything (confirmed in source; **live-DB confirmation via
   `ff db query` was not possible from this sandbox** — per the two-schema-systems
   caveat, re-verify `model_catalog` vs legacy `fleet_model_catalog` column lists
   against the live DB before writing the seed migration).

---

## 5. Implementation plan (phased, fleet-parallelizable)

| Phase | Work | Where | Tests |
|---|---|---|---|
| 1 | `HardwareProfile`/`RuntimePlan` + 7-row table fn | ff-runtime `detector.rs` (+ ff-core types: `Arch`, `CpuFlags`, `BackendFlavor`) | table-driven unit tests, one per row + boundary (8 GB, dVRAM 8 GB); extend detector.rs:237-271 tests |
| 2 | Detection: `arch`+`cpu_flags` in beat + self-enroll payload; `intel_gpu` probe; persist arch | ff-pulse heartbeat_v2 + materializer, bootstrap templates (sh **and** ps1 — both exist), onboard.rs payload | heartbeat unit tests; `derive_os_family`-style table tests |
| 3 | Catalog seed migration (3 models) + arch column migration | ff-db schema.rs/migrations.rs (append-only) | migration registers; **DB tests must early-return without `FORGEFLEET_POSTGRES_URL`/`FORGEFLEET_DATABASE_URL`** |
| 4 | self_enroll: call plan → write runtime → enqueue `node_online` download tasks | ff-gateway onboard.rs | unit test the plan→deferred-payload mapping (pure part) |
| 5 | `ff model plan [--apply]` verb for existing nodes | ff-terminal model_cmd.rs | snapshot test of plan output |
| 6 | Converge `runtime_compatible` / `check_runtime_placement` onto the shared fn | ff-agent | keep autoscaler.rs:1648 GTT test green; add amd-discrete-vllm case |

Phases 1–3 are independent and fleet-parallelizable as separate work items; 4–6
depend on 1.

## 6. Risks / open questions

- **vLLM on aarch64/sm_121 (DGX Spark)** needs the arch-specific wheel/container;
  nothing in-repo encodes which artifact to install — the bootstrap script's vllm
  branch (line 365) must become arch-aware. Verify the current install path on a
  real Spark before flipping default runtime writes.
- **OpenVINO 2026.1** is a net-new backend for llama-server; treat as optional
  (`BackendFlavor::OpenVino` falls back to `CpuAvx2` when the runtime isn't
  installed) so row 6 degrades gracefully.
- **ROCm-discrete row is speculative** — no such box in the fleet today
  (autoscaler comment). Land the row but mark it untested-on-hardware.
- **Re-planning existing nodes** may change `fleet_workers.runtime` under running
  deployments; `--apply` must refuse while `fleet_model_deployments` has active rows
  for the node (or drain first).
- **gpt-oss-120b / GLM-4.5-Air sizing**: 120B/106B-class models fit only the
  Spark-class (128 GB unified) and biggest Macs; the RAM-gating thresholds in row 1
  need real numbers from `fleet_disk_usage`/benchmark runs (`model_benchmark.rs`
  `gpu_priority` ordering already prefers apple → cuda → rocm → cpu for placement).
