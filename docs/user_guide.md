# ForgeFleet user guide

This guide walks through the full lifecycle of running an LLM on ForgeFleet:
provisioning a node, downloading a model, loading it, using it, and maintaining
it over time. Everything below assumes the `ff` binary is installed at
`~/.local/bin/ff` and Postgres is reachable (check with `ff status`).

## 1. One-time setup

### a. Install the HF token (once per fleet)

Hugging Face downloads are free but gated models (Gemma, Llama) require a
free HF access token. Create one at https://huggingface.co/settings/tokens
with **Read** scope, then:

```bash
ff secrets set huggingface.token hf_xxx --description "HF PAT read-only"
```

Stored in Postgres; every fleet node reads it automatically.

### b. Sync the catalog

The curated catalog at `config/model_catalog.toml` lists ~30 models we know
how to provision. Sync it into Postgres once (re-run after edits):

```bash
ff model sync-catalog
# → Synced 32 catalog entries from TOML to Postgres
```

### c. Tag each node's runtime

ForgeFleet supports three inference runtimes. Each node has ONE based on
hardware. Set via the `runtime` column in `fleet_nodes`:

| Hardware                | Runtime     |
|-------------------------|-------------|
| Apple Silicon (Mac)     | `mlx`       |
| Intel/AMD Linux (CPU/CUDA) | `llama.cpp` |
| NVIDIA DGX / heavy GPU  | `vllm`      |

### d. Scan existing models

If you already have models in `~/models/`, reconcile them into the library:

```bash
ff model scan
#  added: 10, updated: 0, removed: 0, total: 548.0GiB
```

### e. Start the daemon

Run `ff daemon` on every node (see `deploy/README.md` for launchd/systemd
service files). Exactly one node (the leader) should run with `--scheduler`.

---

## 2. Common operations

### Download a model

**On this node:**
```bash
ff model download qwen3-coder-30b
# picks the variant matching this node's runtime, streams from HF with progress
```

**On another node (via defer queue):**
```bash
ff model download qwen3-coder-30b --node marcus
# Enqueued cross-node download as deferred task {uuid}.
# It will run on marcus when a defer-worker there claims it.
```

**Batch onto a node (provisioning):**
```bash
ff model download-batch --node marcus \
    qwen3-coder-30b qwen35-35b-a3b gemma3-27b
```

### Load a model (start inference server)

```bash
# Find the library row for the GGUF you want.
ff model library --node marcus

# Load it on port 51001 with 32k context.
ssh marcus "ff model load <library-uuid> --port 51001"

# See what's running.
ssh marcus "ff model ps"
```

### Search + discover

```bash
ff model search qwen
ff model catalog
ff model library              # across all nodes
ff model deployments          # running inference servers
```

### Delete + reclaim disk

```bash
# Dry-run: see what smart LRU would evict on Marcus.
ff model prune --node marcus

# Actually delete one.
ssh marcus "ff model delete <library-uuid> --yes"
```

### Swap runtime variants

Taylor has both a GGUF and an MLX Gemma-4 — the GGUF is unused on Taylor
(MLX is Taylor's runtime). Smart-LRU will flag the GGUF for eviction first:

```bash
ff model prune --node taylor
# → Gemma-4 GGUF flagged with reason "wrong-runtime(llama.cpp/mlx)"
```

### Transfer between nodes (LAN rsync)

Save HF bandwidth — copy from a node that already has it:

```bash
ff model transfer --library-id <uuid-on-source> --from marcus --to sophie
# uses ssh+rsync from target; only works within the same runtime family
```

### Convert safetensors → MLX

Apple Silicon only. Fallback when mlx-community lacks a pre-converted variant:

```bash
ssh taylor "ff model convert <safetensors-lib-uuid> --q-bits 4"
# spawns mlx_lm.convert; output logged to ~/.forgefleet/logs/
```

---

## 3. Deferred + scheduled work

Anything you want to happen "when conditions are met" goes through the
deferred queue:

```bash
# Run when Ace comes back online.
ff defer add-shell \
    --title "Ollama cleanup on ace" \
    --run "rm -rf ~/.ollama" \
    --when-node-online ace \
    --on-node ace

# Run at 9am tomorrow UTC.
ff defer add-shell \
    --title "Re-scan marcus models after download batch" \
    --run "ff model scan" \
    --when-at "2026-04-15T09:00:00Z" \
    --on-node marcus

ff defer list
ff defer get <uuid>
ff defer retry <uuid>   # re-queue a failed task
ff defer cancel <uuid>
```

The scheduler promotes tasks to `dispatchable` when triggers fire; workers
claim atomically (`FOR UPDATE SKIP LOCKED`), execute, and record result.

---

## 4. Everyday hygiene

Run on the leader:

```bash
ff status                     # full system health
ff model disk                 # latest disk samples per node
ff defer list --status failed # anything stuck?
ff model deployments          # what's loaded fleet-wide?
```

---

## 5. MCP integration (for Claude)

The forgefleet MCP server exposes model lifecycle tools to Claude Code and
Desktop:

| Tool                        | Purpose                           |
|-----------------------------|-----------------------------------|
| `fleet_models_catalog`      | list downloadable models          |
| `fleet_models_search`       | fuzzy search the catalog          |
| `fleet_models_library`      | what's on disk per node           |
| `fleet_models_deployments`  | what's running per node           |
| `fleet_models_disk_usage`   | disk snapshots per node           |
| `fleet_models_db`           | legacy — use `library`+`deployments` |
| `fleet_nodes_db`            | node inventory                    |
| `fleet_status`              | fleet health overview             |

Claude can call these to make decisions about which model to route a task to,
or to provision new capacity on demand.

---

## 6. Troubleshooting

**`ff` hangs or exits 137 after updating the binary**
macOS code-signing — always reinstall with:
```bash
install -m 755 target/release/ff ~/.local/bin/ff
codesign --force --sign - ~/.local/bin/ff
```

**Model picker (`/model` in TUI) shows model as offline but it should be running**
The 30 s health refresh may not have run yet. Wait or restart `ff`.

**`ff model scan` finds 0 entries**
Set `FORGEFLEET_SCAN_DEBUG=1` to see per-file classification decisions:
```bash
FORGEFLEET_SCAN_DEBUG=1 ff model scan
```

**Daemon not claiming a task**
Check trigger type. For `node_online`, the scheduler probes SSH port 22 every
15s. If the node's SSH is slow or filtered, adjust timeout or use `manual` /
`now` triggers.
