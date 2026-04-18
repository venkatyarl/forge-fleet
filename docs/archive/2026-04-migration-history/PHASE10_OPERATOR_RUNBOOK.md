# Phase 10 Operator Runbook (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Scope: `forge-fleet-rs` v0.1 internal operations (control plane + agents)

---

## 0) Quick reference

- Control-plane API (ff-api): `http://127.0.0.1:4000` (default)
- Agent health endpoint: `http://<node-host>:<FF_AGENT_HTTP_PORT>/health` (default port `51820`)
- Gateway health endpoint (ff-gateway default): `http://127.0.0.1:8787/health`
- Fleet CLI binary: `forgefleet` (via `cargo run -p ff-cli -- ...`)

---

## 1) Startup sequence (control plane → agents)

Run from repo root:

```bash
cd /Users/venkat/projects/forge-fleet
```

1. **Preflight checks**
```bash
cargo check --workspace
cargo run -p ff-cli -- --help
cargo run -p ff-cli -- config show
```

2. **Start control-plane API** (terminal A)
```bash
RUST_LOG=info cargo run -p ff-api
```

3. **Verify control-plane health**
```bash
curl -fsS http://127.0.0.1:4000/health
cargo run -p ff-cli -- status
cargo run -p ff-cli -- health
```

4. **Start each agent** (one terminal per node)
```bash
FF_AGENT_NODE_ID=node-1 \
FF_LEADER_URL=http://127.0.0.1:51819 \
FF_RUNTIME_URL=http://127.0.0.1:8000 \
FF_AGENT_HTTP_PORT=51820 \
cargo run -p ff-agent
```

5. **Verify each agent**
```bash
curl -fsS http://127.0.0.1:51820/health
curl -fsS http://127.0.0.1:51820/status
cargo run -p ff-cli -- nodes
```

---

## 2) Shutdown sequence (agents → control plane)

1. **Stop agents first** (`Ctrl+C` in each agent terminal).
2. **Then stop control-plane API** (`Ctrl+C` in ff-api terminal).
3. **Confirm listeners are down**
```bash
lsof -nP -iTCP:4000 -sTCP:LISTEN || true
lsof -nP -iTCP:51820 -sTCP:LISTEN || true
```

---

## 3) Health triage flow (first check to deeper checks)

Use this order to reduce time-to-diagnosis:

1. **Global snapshot first**
```bash
cargo run -p ff-cli -- health
cargo run -p ff-cli -- status
```

2. **Control-plane reachable?**
```bash
curl -fsS http://127.0.0.1:4000/health
```

3. **Node/agent reachability**
```bash
cargo run -p ff-cli -- nodes
curl -fsS http://<agent-host>:<agent-port>/health
```

4. **Model/API path sanity**
```bash
cargo run -p ff-cli -- models
curl -fsS http://127.0.0.1:4000/v1/models
```

5. **Gateway path (if enabled)**
```bash
curl -fsS http://127.0.0.1:8787/health
```

---

## 4) Incident playbooks

## A) Node down

1. Detect:
```bash
cargo run -p ff-cli -- nodes
curl -fsS http://<agent-host>:<agent-port>/health
```
2. If agent not reachable, restart agent process on that node.
3. Re-verify:
```bash
curl -fsS http://<agent-host>:<agent-port>/health
cargo run -p ff-cli -- status
```
4. If still down: check host/network first (SSH/connectivity), then inspect agent logs.

## B) Model down

1. Detect:
```bash
cargo run -p ff-cli -- models
curl -fsS http://127.0.0.1:4000/v1/models
```
2. Probe model backend endpoint directly (`<runtime-host>:<runtime-port>`).
3. Restart runtime/model server on affected node.
4. Re-verify model routing:
```bash
curl -fsS http://127.0.0.1:4000/v1/models
```

## C) Gateway outage

1. Detect:
```bash
curl -fsS http://127.0.0.1:8787/health
```
2. Restart gateway service/process.
   - If managed by OpenClaw daemon:
```bash
openclaw gateway status
openclaw gateway restart
```
3. Re-verify:
```bash
curl -fsS http://127.0.0.1:8787/health
```

## D) Cron backlog

1. Detect scheduler pressure/failures (control-plane health + logs):
```bash
cargo run -p ff-cli -- health
# inspect control-plane logs for cron dispatch/tick failures
```
2. Confirm whether failures are transient (runtime unavailable) vs persistent (bad job config).
3. Restore dependency first (runtime/node), then restart control-plane scheduler loop.
4. Re-run critical missed jobs manually if needed, then monitor for new backlog growth.

## E) Deployment rollback

Use when post-merge regression is confirmed.

```bash
git checkout main
git pull --ff-only origin main
git log --oneline --decorate -n 20

# revert the merge commit (preferred over force push)
git revert -m 1 <merge_commit_sha>
git push origin main

# re-run release gates
cargo check --workspace
cargo test --workspace --lib
cargo run -p ff-cli -- --help
```

If `v0.1.0-internal` tag was already pushed and must be withdrawn:

```bash
git tag -d v0.1.0-internal
git push --delete origin v0.1.0-internal
```

---

## 5) Post-incident checklist

- [ ] Incident start/end time captured
- [ ] Impacted services/nodes/models listed
- [ ] Exact detection command + first failing output recorded
- [ ] Mitigation commands recorded in order executed
- [ ] Verification commands recorded with pass evidence
- [ ] Root cause classified (infra / config / code / external dependency)
- [ ] Follow-up ticket(s) created with owner + due date
- [ ] Runbook/docs updated if a gap was found
