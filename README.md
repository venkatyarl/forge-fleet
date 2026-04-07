# ForgeFleet

ForgeFleet is a Rust-first fleet orchestrator for AI infrastructure.

It combines:
- fleet discovery and health monitoring
- leader election and replication
- OpenAI-compatible LLM routing
- MCP tooling for external agents and control planes
- Mission Control-style work management
- self-update, rollout, audit, and observability systems

The active canonical repo is:
- `~/taylorProjects/forge-fleet`

Legacy repos are frozen:
- `~/taylorProjects/forge-fleet-py-legacy`
- `~/taylorProjects/mission-control-legacy`

---

## What ForgeFleet does

### Fleet operations
- discover nodes from `fleet.toml`
- track node health and model health
- elect a leader and fail over when needed
- replicate SQLite state from leader to followers
- expose fleet status over HTTP and MCP

### LLM routing
- OpenAI-compatible `/v1/chat/completions`
- OpenAI-compatible `/v1/models`
- tier-based routing and fallback
- adaptive routing based on task type, quality, and latency
- streaming support
- llama.cpp runtime integration

### Work orchestration
- task decomposition
- pipeline execution
- agent-team / crew execution
- work items, epics, sprints, kanban board
- autonomous agent mode in `forgefleetd`
- ownership / lease / handoff tracking

### Operations and safety
- API key auth + node-to-node HMAC
- Prometheus metrics
- tracing + trace IDs
- audit logging
- canary rollouts + rollback
- synthetic health checks and scorecards
- launchd/systemd install support

---

## Repository layout

```text
forge-fleet/
├── crates/                # Rust workspace crates
├── src/                   # Root daemon entrypoint (forgefleetd)
├── dashboard/             # Node/Vite/React frontend
├── deploy/                # launchd/systemd installer artifacts
├── docs/                  # audits, cutover docs, release docs
├── tests/                 # integration tests
└── tools/                 # migration / support binaries
```

Important crates:
- `ff-core` — config, types, health, audit, monitoring
- `ff-api` — LLM routing, OpenAI compatibility, adaptive routing
- `ff-discovery` — node scanning, registry, health
- `ff-gateway` — HTTP gateway, dashboard serving, WebSocket, APIs
- `ff-db` — embedded SQLite, migrations, replication, ownership
- `ff-mcp` — MCP server and tools
- `ff-mc` — Mission Control feature set inside ForgeFleet
- `ff-agent` — embedded autonomous/heartbeat agent runtime
- `ff-pipeline` — DAG execution engine
- `ff-updater` — updater, canary rollout, rollback
- `ff-runtime` — model/runtime/process management

---

## Current status

ForgeFleet is the active canonical implementation.

What is true right now:
- Rust workspace builds cleanly
- workspace lib tests pass
- integration boot test passes
- the repo has already absorbed substantial behavior from old Python ForgeFleet and Mission Control
- legacy repos are frozen, not yet deleted

Important docs:
- `docs/CONSOLIDATED_PARITY_AND_CUTOVER.md`
- `docs/PYTHON_FORGEFLEET_PARITY_AUDIT.md`
- `docs/MISSION_CONTROL_PARITY_AUDIT.md`
- `docs/DELETE_OR_ARCHIVE_RECOMMENDATION.md`

---

## Requirements

### Backend / daemon
- Rust toolchain (stable)
- `cargo`
- macOS or Linux

### Optional runtime dependencies
- `llama-server` / `llama.cpp` binaries for local model serving
- Docker if you want auxiliary services in containers
- systemd (Linux) or launchd (macOS) for service installation

### Frontend
The frontend currently uses Node/Vite/React.

If you want to build the dashboard locally, install:
- Node.js 20+
- npm

---

## Build

### Build the daemon
```bash
cd ~/taylorProjects/forge-fleet
cargo build --release --bin forgefleetd
```

### Run workspace verification
```bash
cd ~/taylorProjects/forge-fleet
cargo check --workspace
cargo test --workspace --lib
cargo test -p forge-fleet --test integration_boot
```

### Build the dashboard
```bash
cd ~/taylorProjects/forge-fleet/dashboard
npm install
npm run build
```

---

## Configuration

ForgeFleet reads config from:
- `~/.forgefleet/fleet.toml`

You can also pass a custom config path:
```bash
forgefleetd --config /path/to/fleet.toml start
```

### Important config areas
Common sections in `fleet.toml` include:
- `[general]`
- `[nodes.<name>]`
- `[nodes.<name>.models.<model_id>]`
- `[llm]`
- `[ports]`
- `[notifications.telegram]`
- `[transport.telegram]`
- `[services.*]`
- `[scheduling]`
- `[mcp.*]`
- `[enrollment]`
- `[database]`
- `[agent]`

### Agent settings
ForgeFleet supports an agent config section such as:
- `autonomous_mode = true|false`
- `poll_interval_secs = 8`
- `ownership_api_base_url = "..."`

### Database mode
ForgeFleet supports config-driven DB mode in `[database]`:
- `mode = "embedded_sqlite"` (default)
- `mode = "postgres_runtime"` (runtime registry + enrollment events in Postgres, legacy tables still SQLite)
- `mode = "postgres_full"` (target end-state mode; startup preflight currently blocks activation until all SQLite-only dependencies are removed and cutover evidence is recorded)

See:
- `docs/POSTGRES_RUNTIME_MODE.md` for runtime-mode setup and Docker verification against `forgefleet-postgres`
- `docs/checklists/POSTGRES_FULL_CUTOVER_CHECKLIST.md` for safe full cutover/rollback procedure

### Example usage model
- local models run on configured node/model ports
- gateway serves APIs and dashboard
- daemon uses config to seed discovery, routing, and fleet behavior

---

## Run

### Start in foreground
```bash
cd ~/taylorProjects/forge-fleet
./target/release/forgefleetd start
```

### Show version
```bash
./target/release/forgefleetd version
```

### Run with explicit config
```bash
./target/release/forgefleetd --config ~/.forgefleet/fleet.toml start
```

---

## Service installation

ForgeFleet includes install artifacts for both macOS and Linux.

### Install from local build
```bash
cd ~/taylorProjects/forge-fleet/deploy
./install.sh ~/taylorProjects/forge-fleet/target/release/forgefleetd
```

### Uninstall service
```bash
cd ~/taylorProjects/forge-fleet/deploy
./install.sh --uninstall
```

### macOS
Uses:
- `deploy/macos/com.forgefleet.daemon.plist`

Installed as a LaunchAgent.

Useful commands:
```bash
launchctl list | grep forgefleet

tail -f ~/.forgefleet/logs/forgefleetd.log
```

### Linux
Uses:
- `deploy/linux/forgefleet.service`

Useful commands:
```bash
systemctl status forgefleet
journalctl -u forgefleet -f
```

---

## HTTP API

Common endpoints:

### Core
- `GET /health`
- `GET /api/config`
- `POST /api/config`
- `GET /api/fleet/status`
- `GET /api/transports/telegram/status`
- `GET /metrics`
- `GET /api/traces/recent`

### Telegram
- Setup + BotFather + polling config: `docs/TELEGRAM_SETUP.md`

### LLM
- `GET /v1/models`
- `POST /v1/chat/completions`

### Mission Control
- `GET /api/mc/board`
- work item / epic / sprint APIs under `/api/mc/*`

### MCP
- `POST /mcp`
- `GET /mcp/health`

### Replication
- `GET /api/fleet/replicate/sequence`
- `POST /api/fleet/replicate/pull`
- `POST /api/fleet/replicate/snapshot`

---

## Dashboard

The dashboard lives in `dashboard/` and is served by ForgeFleet.

Current major screens include:
- fleet overview
- node detail
- model inventory
- config editor
- mission control
- LLM proxy
- topology
- audit log
- updates
- metrics

If the frontend is built, the gateway serves it directly.

---

## LLM runtime notes

ForgeFleet can route to local and remote model endpoints.

Typical flow:
1. llama.cpp / model endpoints come online
2. discovery sees healthy nodes
3. `/v1/models` reflects available routing targets
4. `/v1/chat/completions` routes using tier/adaptive logic

If model endpoints are unavailable, the API may correctly return `503` or upstream failure responses depending on the path.

---

## Migration / legacy state

We are in active cutover from:
- Python ForgeFleet
- Mission Control

Legacy repos are frozen but retained while final parity gaps are closed.

Do **not** assume all historical docs are still active runtime truth.
Use these docs first:
- `docs/CONSOLIDATED_PARITY_AND_CUTOVER.md`
- `docs/PYTHON_FORGEFLEET_PARITY_AUDIT.md`
- `docs/MISSION_CONTROL_PARITY_AUDIT.md`

---

## Which docs still matter?

### Keep and actively use
- `docs/INDEX.md`
- `docs/FINAL_COMPLETION_CHECKLIST.md`
- `docs/CONSOLIDATED_PARITY_AND_CUTOVER.md`
- `docs/PYTHON_FORGEFLEET_PARITY_AUDIT.md`
- `docs/MISSION_CONTROL_PARITY_AUDIT.md`
- `docs/DELETE_OR_ARCHIVE_RECOMMENDATION.md`
- `docs/FINAL_STATUS.md`
- `docs/FLEET_BRINGUP_PLAYBOOK.md`

### Historical migration / phase docs
Historical `PHASE*` materials have been moved into:
- `docs/archive/2026-04-migration-history/`

They remain useful as migration evidence and audit trail, but they are **not** the primary operational source of truth.

---

## Recommended next cleanup

1. Keep canonical docs at top level
2. Archive older phase-by-phase migration docs into a history folder
3. Continue closing remaining parity items before deleting legacy repos
4. Update GitHub repo description / README badges / release notes to match the canonical repo name

---

## Quick start

### 1. Build
```bash
cd ~/taylorProjects/forge-fleet
cargo build --release --bin forgefleetd
```

### 2. Configure
Make sure this exists:
```bash
~/.forgefleet/fleet.toml
```

### 3. Start
```bash
./target/release/forgefleetd start
```

### 4. Check health
```bash
curl http://127.0.0.1:51801/health
```

### 5. Check models
```bash
curl http://127.0.0.1:51801/v1/models
```

### 6. Open dashboard
Visit the gateway URL in your browser.

---

## License
MIT
