#!/usr/bin/env bash
# Deploy the `ff` binary to every Linux node in the fleet.
#
# Strategy: SSH to each node, git pull, cargo build -p ff-terminal --release,
# install to ~/.local/bin/ff, deploy the systemd service for ff daemon.
#
# Runs builds in parallel. Apple Silicon nodes are skipped — those must be
# built locally on each Mac.
#
# Node discovery is delegated to scripts/lib/fleet.sh (canonical resolver).

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/fleet.sh"

# ─── Resolve node list ──────────────────────────────────────────────────────

NODES=()
while IFS='|' read -r name ip user os role; do
    # Skip macOS nodes — this script builds on Linux nodes only
    if [[ "${os,,}" == *"macos"* || "${os,,}" == *"darwin"* ]]; then
        continue
    fi
    [[ -n "$name" && -n "$ip" && -n "$user" ]] && NODES+=("${name}|${ip}|${user}")
done < <(discover_fleet_nodes)

if [[ ${#NODES[@]} -eq 0 ]]; then
    echo "No Linux fleet nodes found. Is Postgres reachable and are nodes registered?" >&2
    exit 1
fi

echo "[deploy-ff] Discovered ${#NODES[@]} Linux node(s) from fleet resolver" >&2

# ─── Deploy to one node ───────────────────────────────────────────────────

deploy_one() {
    local name=$1 ip=$2 user=$3
    local prefix="[$name]"
    echo "$prefix starting build..."

    ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
        -o BatchMode=yes "$user@$ip" bash -l <<'REMOTE' 2>&1 | sed "s/^/$prefix /"
set -e

# 1. Ensure Rust toolchain is available.
if ! command -v cargo >/dev/null; then
    if [ -f ~/.cargo/env ]; then
        source ~/.cargo/env
    fi
fi
command -v cargo >/dev/null || { echo "ERROR: no cargo on PATH (install rustup first)"; exit 1; }

# 2. Ensure repo is present and up-to-date.
mkdir -p ~/projects
if [ ! -d ~/projects/forge-fleet/.git ]; then
    echo "no .git — cloning fresh"
    rm -rf ~/projects/forge-fleet
    git clone --depth 50 https://github.com/venkatyarl/forge-fleet.git ~/projects/forge-fleet 2>&1 | tail -3
fi
cd ~/projects/forge-fleet
git fetch origin main 2>&1 | tail -2
git reset --hard origin/main 2>&1 | tail -1

# 3. Build.
cargo build -p ff-terminal --release 2>&1 | tail -2

# 4. Install — fail loudly if build didn't produce a binary.
if [ ! -x target/release/ff ]; then
    echo "ERROR: target/release/ff missing after build — see compile errors above"
    exit 1
fi
mkdir -p ~/.local/bin
install -m 755 target/release/ff ~/.local/bin/ff
~/.local/bin/ff --version

# 5. Install systemd service if available and not already present.
if command -v systemctl >/dev/null; then
    UNIT=/etc/systemd/system/forgefleet-daemon@.service
    if [ ! -f "$UNIT" ]; then
        echo "systemd: installing unit template (requires sudo)..."
        sudo cp deploy/systemd/forgefleet-daemon.service "$UNIT" || echo "SUDO_FAILED"
        sudo systemctl daemon-reload || true
    fi
    # Auto-enable at boot so the daemon comes back after a restart. Idempotent.
    if sudo systemctl enable "forgefleet-daemon@$USER.service" >/dev/null 2>&1; then
        echo "systemd: enabled forgefleet-daemon@$USER.service for boot"
    else
        echo "systemd: enable step failed (may need manual: sudo systemctl enable forgefleet-daemon@$USER.service)"
    fi
fi
echo "OK"
REMOTE
    local rc=$?
    if [[ $rc -eq 0 ]]; then
        echo "$prefix ✓ deployed"
    else
        echo "$prefix ✗ failed (rc=$rc)"
    fi
    return $rc
}

export -f deploy_one

# ─── Run in parallel ──────────────────────────────────────────────────────

failed=0
pids=()
for entry in "${NODES[@]}"; do
    IFS='|' read -r name ip user <<<"$entry"
    deploy_one "$name" "$ip" "$user" &
    pids+=($!)
done

for pid in "${pids[@]}"; do
    wait "$pid" || failed=$((failed + 1))
done

echo
if [[ $failed -eq 0 ]]; then
    echo "✓ All ${#NODES[@]} nodes deployed successfully."
else
    echo "✗ $failed of ${#NODES[@]} node(s) failed — check log above."
    exit 1
fi
