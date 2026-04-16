#!/usr/bin/env bash
# ForgeFleet node bootstrap script (rendered from template at serve time).
#
# Placeholders substituted by crates/ff-gateway/src/onboard.rs::render_bootstrap:
#   {{LEADER_HOST}}            — e.g. "192.168.5.100"
#   {{LEADER_PORT}}            — e.g. "51002"
#   {{TOKEN}}                  — one-use-ish enrollment token
#   {{NODE_NAME}}              — desired fleet_nodes.name (from form)
#   {{NODE_IP}}                — node's LAN IP (from form / server remote_addr)
#   {{SSH_USER}}               — ssh_user for this node
#   {{ROLE}}                   — "builder" | "gateway" | "testbed"
#   {{RUNTIME}}                — "auto" | "llama.cpp" | "mlx" | "vllm"
#   {{GITHUB_OWNER}}           — e.g. "venkat-oclaw"
#   {{GITHUB_PAT_SECRET_KEY}}  — "github.venkat_pat"
#   {{IS_TAYLOR}}              — "true" or "false" (controls passwordless sudo)
#
# This script expects to be run with sudo on the new machine:
#   curl -fsSL 'http://...' | sudo bash
#
# It is intentionally bash, self-contained, and idempotent: re-running it on
# a node that's already partially set up just advances to the next unfinished
# step.

set -eu
set -o pipefail

LEADER="http://{{LEADER_HOST}}:{{LEADER_PORT}}"
TOKEN="{{TOKEN}}"
NAME="{{NODE_NAME}}"
IP="{{NODE_IP}}"
SSH_USER="{{SSH_USER}}"
ROLE="{{ROLE}}"
RUNTIME_HINT="{{RUNTIME}}"
GITHUB_OWNER="{{GITHUB_OWNER}}"
GITHUB_PAT_SECRET_KEY="{{GITHUB_PAT_SECRET_KEY}}"
IS_TAYLOR="{{IS_TAYLOR}}"

# ─── Helpers ──────────────────────────────────────────────────────────────

say() { printf '▶ %s\n' "$*"; }
report() {
  # POST progress event to the leader so the dashboard can show live status.
  local step="$1" status="${2:-running}" detail="${3:-}"
  curl -fsS -m 5 -X POST \
    -H "Content-Type: application/json" \
    --data "$(printf '{"name":"%s","step":"%s","status":"%s","detail":%s}' \
      "$NAME" "$step" "$status" \
      "$(printf '%s' "$detail" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || echo '""')")" \
    "$LEADER/api/fleet/enrollment-progress" >/dev/null 2>&1 || true
}

die() {
  local msg="$*"
  report "fatal" failed "$msg"
  echo "ERROR: $msg" >&2
  exit 1
}

# Run as the target user (not as root). Used for cargo, git, etc. When
# invoked by `sudo bash`, we drop to the real invoker.
SUDO_INVOKER="${SUDO_USER:-$SSH_USER}"
run_as_user() {
  if [ "$(id -un)" = "$SUDO_INVOKER" ]; then
    "$@"
  else
    sudo -u "$SUDO_INVOKER" -H "$@"
  fi
}

say "ForgeFleet onboarding for $NAME ($IP) — runtime hint: $RUNTIME_HINT"
report "start" running

# ─── 1. OS detection ──────────────────────────────────────────────────────

OS_FULL="unknown"
OS_ID="unknown"
if [ -f /etc/os-release ]; then
  . /etc/os-release
  OS_FULL="${PRETTY_NAME:-${NAME:-linux}}"
  OS_ID="${ID:-linux}"
elif [ "$(uname)" = "Darwin" ]; then
  OS_FULL="macOS $(sw_vers -productVersion 2>/dev/null || echo unknown)"
  OS_ID="macos"
fi
say "OS: $OS_FULL (id=$OS_ID)"

# Detect NVIDIA GPU for vllm runtime decision.
HAS_NVIDIA="false"
if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
  HAS_NVIDIA="true"
fi

RUNTIME="$RUNTIME_HINT"
if [ "$RUNTIME" = "auto" ]; then
  case "$OS_ID" in
    dgx*)                                         RUNTIME="vllm" ;;
    macos)    RUNTIME="mlx" ;;
    *)        if [ "$HAS_NVIDIA" = "true" ]; then RUNTIME="vllm"; else RUNTIME="llama.cpp"; fi ;;
  esac
fi
say "Runtime resolved: $RUNTIME"
report "detect_os" ok "$OS_FULL / $RUNTIME"

# ─── 2. Prerequisites ─────────────────────────────────────────────────────

report "prereqs" running
case "$OS_ID" in
  macos)
    # Homebrew presumed installed manually (mac setup is interactive).
    ;;
  *)
    # Ubuntu/DGX OS/Debian — install build toolchain.
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -y >/dev/null 2>&1 || die "apt-get update failed"
    apt-get install -y --no-install-recommends \
      build-essential pkg-config libssl-dev git curl ca-certificates openssh-client openssh-server \
      >/dev/null 2>&1 || die "apt-get install (prereqs) failed"
    systemctl enable --now ssh >/dev/null 2>&1 || true
    ;;
esac
report "prereqs" ok

# ─── 3. Rust toolchain (as the invoking user) ─────────────────────────────

report "rust" running
if ! run_as_user bash -lc 'command -v cargo >/dev/null'; then
  say "Installing rustup for $SUDO_INVOKER..."
  run_as_user bash -lc 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal' \
    || die "rustup install failed"
fi
report "rust" ok

# ─── 4. Passwordless sudo (except on Taylor) ─────────────────────────────

if [ "$IS_TAYLOR" != "true" ]; then
  report "sudoers" running
  SUDOERS_FILE="/etc/sudoers.d/forgefleet-${SUDO_INVOKER}"
  echo "${SUDO_INVOKER} ALL=(ALL) NOPASSWD:ALL" > "$SUDOERS_FILE"
  chmod 0440 "$SUDOERS_FILE"
  visudo -c -f "$SUDOERS_FILE" >/dev/null 2>&1 || die "sudoers syntax invalid"
  # Verify from the user's shell.
  run_as_user sudo -n true || die "passwordless sudo not working"
  report "sudoers" ok
else
  report "sudoers" ok "skipped (taylor)"
fi

# ─── 5. GitHub CLI + auth ────────────────────────────────────────────────

report "gh" running
case "$OS_ID" in
  macos)   run_as_user bash -lc 'command -v brew >/dev/null && (command -v gh >/dev/null || brew install gh)' ;;
  *)       if ! command -v gh >/dev/null 2>&1; then
             # Official GitHub CLI apt repo.
             curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
               | gpg --dearmor -o /usr/share/keyrings/githubcli-archive-keyring.gpg 2>/dev/null
             chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg 2>/dev/null
             echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] \
               https://cli.github.com/packages stable main" \
               | tee /etc/apt/sources.list.d/github-cli.list >/dev/null
             apt-get update -y >/dev/null 2>&1
             apt-get install -y gh >/dev/null 2>&1 || die "gh install failed"
           fi ;;
esac
report "gh" ok

# ─── 6. Clone forge-fleet + build ff ─────────────────────────────────────

report "clone" running
REPO_DIR="/home/${SUDO_INVOKER}/taylorProjects/forge-fleet"
[ "$OS_ID" = "macos" ] && REPO_DIR="/Users/${SUDO_INVOKER}/taylorProjects/forge-fleet"

run_as_user mkdir -p "$(dirname "$REPO_DIR")"
if [ ! -d "$REPO_DIR/.git" ]; then
  run_as_user git clone --depth 50 "https://github.com/${GITHUB_OWNER}/forge-fleet.git" "$REPO_DIR" \
    || die "git clone failed"
else
  run_as_user bash -c "cd '$REPO_DIR' && git fetch origin main && git reset --hard origin/main" \
    || die "git fetch/reset failed"
fi
report "clone" ok

report "build" running
run_as_user bash -lc "cd '$REPO_DIR' && cargo build -p ff-terminal --release 2>&1 | tail -2" \
  || die "cargo build failed"
run_as_user install -m 755 "$REPO_DIR/target/release/ff" "$(eval echo ~${SUDO_INVOKER})/.local/bin/ff"
report "build" ok

# ─── 6b. OpenClaw ────────────────────────────────────────────────────────
# Installs OpenClaw via npm (matches deploy/provision-node.sh). Failure here
# does NOT abort enrollment — the node can still work as a ForgeFleet member
# and a deferred task is available to retry later.
report "openclaw" running
if command -v npm >/dev/null 2>&1; then
  run_as_user bash -lc 'command -v openclaw >/dev/null || npm install -g openclaw' >/dev/null 2>&1 \
    || npm install -g openclaw >/dev/null 2>&1 || true
  if run_as_user bash -lc 'command -v openclaw >/dev/null'; then
    report "openclaw" ok "$(run_as_user bash -lc 'openclaw --version 2>&1 | head -1')"
  else
    report "openclaw" failed "install failed — retry later"
  fi
else
  report "openclaw" failed "npm not present — install node/npm and rerun"
fi

# ─── 6c. vLLM venv (GPU nodes only) ──────────────────────────────────────
if [ "$RUNTIME" = "vllm" ]; then
  report "vllm_venv" running
  VENV="$USER_HOME/.forgefleet/vllm-venv"
  if [ ! -d "$VENV" ]; then
    run_as_user mkdir -p "$USER_HOME/.forgefleet"
    if ! run_as_user python3 -m venv "$VENV" >/dev/null 2>&1; then
      # DGX / Ubuntu often need python3-venv installed separately.
      apt-get install -y python3-venv >/dev/null 2>&1 || true
      run_as_user python3 -m venv "$VENV" || die "python3 -m venv failed (install python3-venv)"
    fi
  fi
  # pip install vllm (takes a while on first run — safe to re-run, pip is idempotent).
  run_as_user bash -lc "source '$VENV/bin/activate' && pip install --quiet --upgrade pip && pip install --quiet vllm" \
    && report "vllm_venv" ok "$VENV" \
    || report "vllm_venv" failed "pip install vllm failed — retry after resolving CUDA issues"
fi

# ─── 7. SSH keypair + host keys ──────────────────────────────────────────

report "sshkey" running
USER_HOME="$(eval echo ~${SUDO_INVOKER})"
KEY_PATH="$USER_HOME/.ssh/id_ed25519"
if [ ! -f "$KEY_PATH" ]; then
  run_as_user mkdir -p "$USER_HOME/.ssh"
  run_as_user chmod 700 "$USER_HOME/.ssh"
  run_as_user ssh-keygen -t ed25519 -N "" -f "$KEY_PATH" -C "${SUDO_INVOKER}@${NAME}" >/dev/null
fi
USER_PUBKEY="$(cat "${KEY_PATH}.pub")"

# Collect host keys (created automatically by sshd on first start).
HOST_PUBKEYS=""
for f in /etc/ssh/ssh_host_*_key.pub; do
  [ -f "$f" ] || continue
  HOST_PUBKEYS="${HOST_PUBKEYS}$(cat "$f")"$'\n'
done
report "sshkey" ok

# ─── 8. Hardware detection ───────────────────────────────────────────────

CORES="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1)"
RAM_KB="$(awk '/MemTotal/ {print $2; exit}' /proc/meminfo 2>/dev/null || echo 0)"
if [ "$RAM_KB" = "0" ] && [ "$OS_ID" = "macos" ]; then
  RAM_BYTES="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
  RAM_KB=$((RAM_BYTES / 1024))
fi
RAM_GB=$(( (RAM_KB + 524288) / 1048576 ))
[ "$RAM_GB" -lt 1 ] && RAM_GB=1

# Sub-agent count formula: max(1, min(cores/2, ram/16, 4))
COUNT_FROM_CORES=$((CORES / 2))
COUNT_FROM_RAM=$((RAM_GB / 16))
SUB_AGENTS=4
[ "$COUNT_FROM_CORES" -lt "$SUB_AGENTS" ] && SUB_AGENTS="$COUNT_FROM_CORES"
[ "$COUNT_FROM_RAM"   -lt "$SUB_AGENTS" ] && SUB_AGENTS="$COUNT_FROM_RAM"
[ "$SUB_AGENTS" -lt 1 ] && SUB_AGENTS=1
# Big-GPU boost
if [ "$HAS_NVIDIA" = "true" ] && [ "$RAM_GB" -ge 64 ]; then
  DGX_MAX=8
  [ "$COUNT_FROM_CORES" -lt "$DGX_MAX" ] && DGX_MAX="$COUNT_FROM_CORES"
  SUB_AGENTS="$DGX_MAX"
fi
say "Sub-agents: $SUB_AGENTS (cores=$CORES, ram=${RAM_GB}G)"

# Create sub-agent workspaces
FF_HOME="$USER_HOME/.forgefleet"
run_as_user mkdir -p "$FF_HOME/logs"
i=0
while [ "$i" -lt "$SUB_AGENTS" ]; do
  run_as_user mkdir -p "$FF_HOME/sub-agent-${i}/scratch" "$FF_HOME/sub-agent-${i}/checkpoints" "$FF_HOME/sub-agent-${i}/cache"
  i=$((i + 1))
done
report "sub_agents" ok "count=$SUB_AGENTS"

# ─── 9. Self-enroll ──────────────────────────────────────────────────────

report "enroll" running
# Escape newlines in host pubkeys for JSON.
HOST_KEYS_JSON="$(printf '%s' "$HOST_PUBKEYS" | python3 -c '
import json,sys
lines = [l for l in sys.stdin.read().splitlines() if l.strip()]
print(json.dumps(lines))
' 2>/dev/null || echo '[]')"

ENROLL_PAYLOAD="$(cat <<EOF
{
  "token": "$TOKEN",
  "name": "$NAME",
  "hostname": "$(hostname)",
  "ip": "$IP",
  "os": "$OS_FULL",
  "os_id": "$OS_ID",
  "runtime": "$RUNTIME",
  "ram_gb": $RAM_GB,
  "cpu_cores": $CORES,
  "role": "$ROLE",
  "ssh_user": "$SUDO_INVOKER",
  "sub_agent_count": $SUB_AGENTS,
  "gh_account": "$GITHUB_OWNER",
  "has_nvidia": $HAS_NVIDIA,
  "ssh_identity": {
    "user_public_key": $(printf '%s' "$USER_PUBKEY" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().strip()))'),
    "host_public_keys": $HOST_KEYS_JSON
  }
}
EOF
)"

ENROLL_RESP="$(curl -fsS -m 30 -X POST \
  -H "Content-Type: application/json" \
  --data "$ENROLL_PAYLOAD" \
  "$LEADER/api/fleet/self-enroll")" || die "self-enroll HTTP request failed"

say "Enrolled: $ENROLL_RESP"
report "enroll" ok

# ─── 10. Import peer SSH identities ──────────────────────────────────────

report "mesh_import" running
# Parse peer_ssh_identities from the enrollment response and merge into
# ~/.ssh/authorized_keys and ~/.ssh/known_hosts.
python3 <<PY || die "failed to import peer SSH identities"
import json, os, sys, pathlib
data = json.loads('''$ENROLL_RESP''')
peers = data.get("peer_ssh_identities", [])
home = pathlib.Path(os.path.expanduser("~$SUDO_INVOKER"))
ssh = home / ".ssh"
ssh.mkdir(mode=0o700, exist_ok=True)
authz = ssh / "authorized_keys"
known = ssh / "known_hosts"
existing_authz = authz.read_text() if authz.exists() else ""
existing_known = known.read_text() if known.exists() else ""
added_user, added_host = 0, 0
for p in peers:
    upk = (p.get("user_public_key") or "").strip()
    if upk and upk not in existing_authz:
        existing_authz += upk + "\n"
        added_user += 1
    ip = p.get("ip", "")
    name = p.get("name", "")
    for hk in p.get("host_public_keys", []):
        hk = hk.strip()
        if not hk:
            continue
        # known_hosts line format: "ip,name <type> <key>"
        parts = hk.split(None, 2)
        if len(parts) >= 2:
            line = f"{ip},{name} {hk}"
            if line not in existing_known:
                existing_known += line + "\n"
                added_host += 1
authz.write_text(existing_authz)
authz.chmod(0o600)
known.write_text(existing_known)
known.chmod(0o644)
import pwd
uid = pwd.getpwnam("$SUDO_INVOKER").pw_uid
gid = pwd.getpwnam("$SUDO_INVOKER").pw_gid
os.chown(str(authz), uid, gid)
os.chown(str(known), uid, gid)
print(f"imported: +{added_user} authorized_keys, +{added_host} known_hosts")
PY
report "mesh_import" ok

# ─── 11. systemd unit ────────────────────────────────────────────────────

if [ "$OS_ID" != "macos" ]; then
  report "service" running
  UNIT=/etc/systemd/system/forgefleet-daemon@.service
  cp "$REPO_DIR/deploy/systemd/forgefleet-daemon.service" "$UNIT"
  systemctl daemon-reload
  systemctl enable --now "forgefleet-daemon@${SUDO_INVOKER}.service" >/dev/null 2>&1 || true
  sleep 2
  if systemctl is-active "forgefleet-daemon@${SUDO_INVOKER}.service" >/dev/null 2>&1; then
    report "service" ok
  else
    report "service" failed "systemctl reports inactive"
  fi
else
  # macOS: leave launchd setup to the operator per CLAUDE.md.
  report "service" ok "macOS — operator installs launchd plist"
fi

# ─── Done ────────────────────────────────────────────────────────────────

report "done" ok "$NAME is now a ForgeFleet node"
say "✓ Onboarding complete: $NAME"
