# Fleet Bring-Up Playbook (Phase 26)

Date: 2026-04-05  
Repo: `/Users/venkat/projects/forge-fleet`

Goal: make real multi-node bring-up repeatable so James, Marcus, Sophie, Priya, and Ace can be brought online quickly once reachable.

---

## 1) Current state inspection (what this playbook is based on)

### Config source of truth
- Repo-local `fleet.toml`: **not present**.
- Active config used by daemon/CLI: `~/.forgefleet/fleet.toml`.
- On this machine, current config file: `/Users/venkat/.forgefleet/fleet.toml`.

### Service/install paths (from current repo artifacts)
- Installer script: `deploy/install.sh`
  - Binary name: `forgefleetd`
  - Install dir: `/usr/local/bin/forgefleetd`
  - Home/config dir: `~/.forgefleet`
  - Logs dir: `~/.forgefleet/logs`
- Linux unit template: `deploy/linux/forgefleet.service`
  - `ExecStart=/usr/local/bin/forgefleetd start`
  - `WorkingDirectory=%h/.forgefleet`
  - `Environment=FORGEFLEET_HOME=%h/.forgefleet`
- macOS LaunchAgent template: `deploy/macos/com.forgefleet.daemon.plist`
  - ProgramArguments: `/usr/local/bin/forgefleetd start`
  - WorkingDirectory: `~/.forgefleet`
  - Log file: `~/.forgefleet/logs/forgefleetd.log`

### Current target nodes from active fleet config
- James: `james@192.168.5.108` (Ubuntu), model port `55001`
- Marcus: `marcus@192.168.5.102` (Ubuntu), model port `55002`
- Sophie: `sophie@192.168.5.103` (Ubuntu), model port `55003`
- Priya: `priya@192.168.5.106` (Ubuntu), model port `55004`
- Ace: `ace@192.168.5.104` (macOS), model port `55005`

---

## 2) Preflight checklist (must pass before node setup starts)

Use this checklist as a hard gate.

### A. Reachability + access
- [ ] Passwordless SSH works to each target node.
- [ ] Correct SSH user per node is known.
- [ ] `sudo` access exists on Linux nodes (James/Marcus/Sophie/Priya).
- [ ] Network path allows access to expected ports (`51800`, `51801`, node model port).

### B. Disk + OS readiness
- [ ] Node has enough free disk for repo + Rust build artifacts (recommend >= 20GB free).
- [ ] Node OS confirmed (`uname -s`) and matches install expectations (Linux/macOS).

### C. Toolchain + binaries
- [ ] `git`, `curl`, `ssh` available on nodes.
- [ ] Rust toolchain and `cargo` installed on nodes (or binary copy strategy ready).
- [ ] Local release artifact available (or build plan set):
  - `target/release/forgefleetd`

### D. Config consistency
- [ ] Canonical config file exists locally: `~/.forgefleet/fleet.toml`.
- [ ] Config includes all 5 target nodes with correct IPs/users/ports.
- [ ] Config can be copied to each node at: `~/.forgefleet/fleet.toml`.

### E. Runtime/model readiness
- [ ] llama.cpp / runtime process plan is defined on each node.
- [ ] Model ports in config are reserved and not occupied by unrelated services.

### F. Service artifacts
- [ ] `deploy/install.sh` exists and executable.
- [ ] `deploy/linux/forgefleet.service` present.
- [ ] `deploy/macos/com.forgefleet.daemon.plist` present.

### Preflight automation command

Run from repo root:

```bash
tools/fleet_preflight.sh
```

(Use `tools/fleet_preflight.sh --skip-ssh` for local-only checks.)

---

## 3) Standard bring-up sequence (controller machine)

```bash
export FLEET_REPO=/Users/venkat/projects/forge-fleet
export FLEET_CONFIG=/Users/venkat/.forgefleet/fleet.toml
cd "$FLEET_REPO"

git fetch origin
git checkout main
git pull --ff-only origin main

cargo build --release --bin forgefleetd
```

If a node cannot build locally, copy prebuilt binary + deploy artifacts from controller.

---

## 4) Node procedure template (repeat for each node)

For each node, execute:

1. **Repo sync/build/install path**
   - Ensure repo exists at `~/projects/forge-fleet`
   - Sync to `main`
   - Build `forgefleetd` release binary

2. **Service install/start**
   - `cd ~/projects/forge-fleet/deploy`
   - `./install.sh ~/projects/forge-fleet/target/release/forgefleetd`

3. **Config verification**
   - Confirm `~/.forgefleet/fleet.toml` exists
   - Confirm node entry exists in config
   - Confirm `forgefleetd --config ~/.forgefleet/fleet.toml status` works

4. **llama.cpp/runtime verification**
   - Check runtime process (`llama-server`, `llama.cpp`, or `ollama`)
   - Check model port listener
   - Probe runtime endpoint (`/health` or `/v1/models`)

5. **Health endpoint verification**
   - `http://<node-ip>:51800/health` (API)
   - `http://<node-ip>:51801/health` (gateway)
   - `http://<node-ip>:51801/api/fleet/status` (discovery view)

6. **Leader election / replication validation**
   - Verify election logs show initial leader and/or leader change events
   - Check replicate endpoints from elected leader

---

## 5) Per-node concrete steps

> Run these from controller unless noted.

### 5.1 James (`james@192.168.5.108`, Linux, model port `55001`)

#### Repo sync/build/install path
```bash
ssh james@192.168.5.108 '
  set -euo pipefail
  mkdir -p ~/projects
  if [ ! -d ~/projects/forge-fleet/.git ]; then
    echo "Repo missing at ~/projects/forge-fleet; clone or copy it first" >&2
    exit 1
  fi
  cd ~/projects/forge-fleet
  git fetch origin
  git checkout main
  git pull --ff-only origin main
  cargo build --release --bin forgefleetd
'
```

#### Service install/start
```bash
scp "$FLEET_CONFIG" james@192.168.5.108:~/.forgefleet/fleet.toml
ssh james@192.168.5.108 '
  set -euo pipefail
  cd ~/projects/forge-fleet/deploy
  ./install.sh ~/projects/forge-fleet/target/release/forgefleetd
  sudo systemctl status forgefleet --no-pager --lines=30
'
```

#### Config verification
```bash
ssh james@192.168.5.108 '
  test -f ~/.forgefleet/fleet.toml
  grep -n "^\[nodes.james\]" ~/.forgefleet/fleet.toml
  /usr/local/bin/forgefleetd --config ~/.forgefleet/fleet.toml status
'
```

#### llama.cpp/runtime verification
```bash
ssh james@192.168.5.108 '
  pgrep -af "llama-server|llama.cpp|ollama" || true
  ss -ltnp 2>/dev/null | grep ":55001" || lsof -nP -iTCP:55001 -sTCP:LISTEN || true
  curl -fsS --max-time 4 http://127.0.0.1:55001/health || curl -fsS --max-time 4 http://127.0.0.1:55001/v1/models
'
```

#### Health endpoint verification
```bash
curl -fsS http://192.168.5.108:51800/health
curl -fsS http://192.168.5.108:51801/health
curl -fsS http://192.168.5.108:51801/api/fleet/status
```

#### Leader election / replication validation
```bash
ssh james@192.168.5.108 'journalctl -u forgefleet --no-pager -n 200 | egrep "initial leader election completed|leader change detected|leader announcement" || true'
curl -sS http://192.168.5.108:51801/api/fleet/replicate/sequence
```

---

### 5.2 Marcus (`marcus@192.168.5.102`, Linux, model port `55002`)

#### Repo sync/build/install path
```bash
ssh marcus@192.168.5.102 '
  set -euo pipefail
  mkdir -p ~/projects
  if [ ! -d ~/projects/forge-fleet/.git ]; then
    echo "Repo missing at ~/projects/forge-fleet; clone or copy it first" >&2
    exit 1
  fi
  cd ~/projects/forge-fleet
  git fetch origin
  git checkout main
  git pull --ff-only origin main
  cargo build --release --bin forgefleetd
'
```

#### Service install/start
```bash
scp "$FLEET_CONFIG" marcus@192.168.5.102:~/.forgefleet/fleet.toml
ssh marcus@192.168.5.102 '
  set -euo pipefail
  cd ~/projects/forge-fleet/deploy
  ./install.sh ~/projects/forge-fleet/target/release/forgefleetd
  sudo systemctl status forgefleet --no-pager --lines=30
'
```

#### Config verification
```bash
ssh marcus@192.168.5.102 '
  test -f ~/.forgefleet/fleet.toml
  grep -n "^\[nodes.marcus\]" ~/.forgefleet/fleet.toml
  /usr/local/bin/forgefleetd --config ~/.forgefleet/fleet.toml status
'
```

#### llama.cpp/runtime verification
```bash
ssh marcus@192.168.5.102 '
  pgrep -af "llama-server|llama.cpp|ollama" || true
  ss -ltnp 2>/dev/null | grep ":55002" || lsof -nP -iTCP:55002 -sTCP:LISTEN || true
  curl -fsS --max-time 4 http://127.0.0.1:55002/health || curl -fsS --max-time 4 http://127.0.0.1:55002/v1/models
'
```

#### Health endpoint verification
```bash
curl -fsS http://192.168.5.102:51800/health
curl -fsS http://192.168.5.102:51801/health
curl -fsS http://192.168.5.102:51801/api/fleet/status
```

#### Leader election / replication validation
```bash
ssh marcus@192.168.5.102 'journalctl -u forgefleet --no-pager -n 200 | egrep "initial leader election completed|leader change detected|leader announcement" || true'
curl -sS http://192.168.5.102:51801/api/fleet/replicate/sequence
```

---

### 5.3 Sophie (`sophie@192.168.5.103`, Linux, model port `55003`)

#### Repo sync/build/install path
```bash
ssh sophie@192.168.5.103 '
  set -euo pipefail
  mkdir -p ~/projects
  if [ ! -d ~/projects/forge-fleet/.git ]; then
    echo "Repo missing at ~/projects/forge-fleet; clone or copy it first" >&2
    exit 1
  fi
  cd ~/projects/forge-fleet
  git fetch origin
  git checkout main
  git pull --ff-only origin main
  cargo build --release --bin forgefleetd
'
```

#### Service install/start
```bash
scp "$FLEET_CONFIG" sophie@192.168.5.103:~/.forgefleet/fleet.toml
ssh sophie@192.168.5.103 '
  set -euo pipefail
  cd ~/projects/forge-fleet/deploy
  ./install.sh ~/projects/forge-fleet/target/release/forgefleetd
  sudo systemctl status forgefleet --no-pager --lines=30
'
```

#### Config verification
```bash
ssh sophie@192.168.5.103 '
  test -f ~/.forgefleet/fleet.toml
  grep -n "^\[nodes.sophie\]" ~/.forgefleet/fleet.toml
  /usr/local/bin/forgefleetd --config ~/.forgefleet/fleet.toml status
'
```

#### llama.cpp/runtime verification
```bash
ssh sophie@192.168.5.103 '
  pgrep -af "llama-server|llama.cpp|ollama" || true
  ss -ltnp 2>/dev/null | grep ":55003" || lsof -nP -iTCP:55003 -sTCP:LISTEN || true
  curl -fsS --max-time 4 http://127.0.0.1:55003/health || curl -fsS --max-time 4 http://127.0.0.1:55003/v1/models
'
```

#### Health endpoint verification
```bash
curl -fsS http://192.168.5.103:51800/health
curl -fsS http://192.168.5.103:51801/health
curl -fsS http://192.168.5.103:51801/api/fleet/status
```

#### Leader election / replication validation
```bash
ssh sophie@192.168.5.103 'journalctl -u forgefleet --no-pager -n 200 | egrep "initial leader election completed|leader change detected|leader announcement" || true'
curl -sS http://192.168.5.103:51801/api/fleet/replicate/sequence
```

---

### 5.4 Priya (`priya@192.168.5.106`, Linux, model port `55004`)

#### Repo sync/build/install path
```bash
ssh priya@192.168.5.106 '
  set -euo pipefail
  mkdir -p ~/projects
  if [ ! -d ~/projects/forge-fleet/.git ]; then
    echo "Repo missing at ~/projects/forge-fleet; clone or copy it first" >&2
    exit 1
  fi
  cd ~/projects/forge-fleet
  git fetch origin
  git checkout main
  git pull --ff-only origin main
  cargo build --release --bin forgefleetd
'
```

#### Service install/start
```bash
scp "$FLEET_CONFIG" priya@192.168.5.106:~/.forgefleet/fleet.toml
ssh priya@192.168.5.106 '
  set -euo pipefail
  cd ~/projects/forge-fleet/deploy
  ./install.sh ~/projects/forge-fleet/target/release/forgefleetd
  sudo systemctl status forgefleet --no-pager --lines=30
'
```

#### Config verification
```bash
ssh priya@192.168.5.106 '
  test -f ~/.forgefleet/fleet.toml
  grep -n "^\[nodes.priya\]" ~/.forgefleet/fleet.toml
  /usr/local/bin/forgefleetd --config ~/.forgefleet/fleet.toml status
'
```

#### llama.cpp/runtime verification
```bash
ssh priya@192.168.5.106 '
  pgrep -af "llama-server|llama.cpp|ollama" || true
  ss -ltnp 2>/dev/null | grep ":55004" || lsof -nP -iTCP:55004 -sTCP:LISTEN || true
  curl -fsS --max-time 4 http://127.0.0.1:55004/health || curl -fsS --max-time 4 http://127.0.0.1:55004/v1/models
'
```

#### Health endpoint verification
```bash
curl -fsS http://192.168.5.106:51800/health
curl -fsS http://192.168.5.106:51801/health
curl -fsS http://192.168.5.106:51801/api/fleet/status
```

#### Leader election / replication validation
```bash
ssh priya@192.168.5.106 'journalctl -u forgefleet --no-pager -n 200 | egrep "initial leader election completed|leader change detected|leader announcement" || true'
curl -sS http://192.168.5.106:51801/api/fleet/replicate/sequence
```

---

### 5.5 Ace (`ace@192.168.5.104`, macOS, model port `55005`)

#### Repo sync/build/install path
```bash
ssh ace@192.168.5.104 '
  set -euo pipefail
  mkdir -p ~/projects
  if [ ! -d ~/projects/forge-fleet/.git ]; then
    echo "Repo missing at ~/projects/forge-fleet; clone or copy it first" >&2
    exit 1
  fi
  cd ~/projects/forge-fleet
  git fetch origin
  git checkout main
  git pull --ff-only origin main
  cargo build --release --bin forgefleetd
'
```

#### Service install/start
```bash
scp "$FLEET_CONFIG" ace@192.168.5.104:~/.forgefleet/fleet.toml
ssh ace@192.168.5.104 '
  set -euo pipefail
  cd ~/projects/forge-fleet/deploy
  ./install.sh ~/projects/forge-fleet/target/release/forgefleetd
  launchctl list | grep -i forgefleet || true
'
```

#### Config verification
```bash
ssh ace@192.168.5.104 '
  test -f ~/.forgefleet/fleet.toml
  grep -n "^\[nodes.ace\]" ~/.forgefleet/fleet.toml
  /usr/local/bin/forgefleetd --config ~/.forgefleet/fleet.toml status
'
```

#### llama.cpp/runtime verification
```bash
ssh ace@192.168.5.104 '
  pgrep -af "llama-server|llama.cpp|ollama" || true
  lsof -nP -iTCP:55005 -sTCP:LISTEN || true
  curl -fsS --max-time 4 http://127.0.0.1:55005/health || curl -fsS --max-time 4 http://127.0.0.1:55005/v1/models
'
```

#### Health endpoint verification
```bash
curl -fsS http://192.168.5.104:51800/health
curl -fsS http://192.168.5.104:51801/health
curl -fsS http://192.168.5.104:51801/api/fleet/status
```

#### Leader election / replication validation
```bash
ssh ace@192.168.5.104 'tail -n 200 ~/.forgefleet/logs/forgefleetd.log | egrep "initial leader election completed|leader change detected|leader announcement" || true'
curl -sS http://192.168.5.104:51801/api/fleet/replicate/sequence
```

---

## 6) Cluster-level validation after all 5 nodes are up

### Leader election consistency
1. Check logs on all 5 nodes for same elected leader.
2. Check each node’s fleet status view:

```bash
for ip in 192.168.5.108 192.168.5.102 192.168.5.103 192.168.5.106 192.168.5.104; do
  echo "=== $ip ==="
  curl -fsS "http://$ip:51801/api/fleet/status" || true
  echo
  echo
done
```

### Replication probes

From controller, test against elected leader gateway (`<leader_ip>:51801`):

```bash
curl -sS http://<leader_ip>:51801/api/fleet/replicate/sequence
curl -sS -X POST http://<leader_ip>:51801/api/fleet/replicate/pull \
  -H 'content-type: application/json' \
  -d '{"since_sequence":0}'
```

Expected outcomes:
- If replication wiring is active: sequence JSON and pull response/snapshot metadata.
- If not yet wired: `503` + `{"type":"not_leader"...}` style error (track as known gap).

---

## 7) Notes / known implementation caveats

- `forgefleetd` currently announces leader changes to `/api/fleet/leader` on peers, but gateway route for `/api/fleet/leader` is not present yet in this repo snapshot. Use log-based validation for election consistency.
- Replication endpoints exist in gateway (`/api/fleet/replicate/*`), but operational readiness depends on leader sync wiring at runtime.

---

## 8) Exit criteria for Phase 26

Phase is complete when:
- [ ] Preflight script passes (or only known/non-blocking warnings remain).
- [ ] All 5 target nodes complete repo sync/build/install path.
- [ ] Service is installed and running on each node.
- [ ] Config verified on each node.
- [ ] Runtime/model checks pass on each node.
- [ ] Health endpoints pass on each node.
- [ ] Leader election and replication probes executed and results captured.
