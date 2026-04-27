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
#   {{GITHUB_OWNER}}           — e.g. "venkatyarl"
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

# Resolve USER_HOME upfront — multiple later stages reference it (install
# target, vllm venv path, ssh keypair, sub-agent workspaces). Leaving this
# until later caused $USER_HOME expansion to empty and silent path breakage.
USER_HOME="$(eval echo ~${SUDO_INVOKER})"

# Pre-create directories the script writes to later. `install -m 755` does
# NOT auto-create the parent; a fresh Ubuntu box has no ~/.local/bin.
run_as_user mkdir -p "$USER_HOME/.local/bin" "$USER_HOME/.forgefleet/logs"

say "ForgeFleet onboarding for $NAME ($IP) — runtime hint: $RUNTIME_HINT"
report "start" running

# ─── 1. OS detection ──────────────────────────────────────────────────────

OS_FULL="unknown"
OS_ID="unknown"
if [ -f /etc/os-release ]; then
  # Source in a subshell so /etc/os-release's NAME=Ubuntu can't clobber
  # our operator-supplied $NAME (which is this node's fleet name, e.g. "sia").
  # Previous bug: Sia enrolled as "ubuntu" because $NAME got overwritten here.
  OS_FULL="$(. /etc/os-release; printf '%s' "${PRETTY_NAME:-${NAME:-linux}}")"
  OS_ID="$(. /etc/os-release; printf '%s' "${ID:-linux}")"
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

# ─── 5b. gh auth login via PAT stored in fleet_secrets ───────────────────
# Prereq: operator ran `ff secrets set github.venkat_pat ghp_xxx` on taylor.
# If the secret isn't set we skip — `git clone` will still work for PUBLIC
# repos, and `gh auth status` will fail at verify-time so the operator knows.
report "gh_auth" running
# We don't have ff installed yet on this new box, so fetch the secret via HTTP.
# The enrollment token doubles as auth for this one-time lookup.
PAT_VALUE="$(curl -fsS -m 10 \
  "$LEADER/api/fleet/secret-peek?token=$TOKEN&key=$GITHUB_PAT_SECRET_KEY" \
  2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin).get("value",""))' 2>/dev/null || true)"

if [ -n "$PAT_VALUE" ]; then
  if run_as_user bash -lc "echo \"$PAT_VALUE\" | gh auth login --with-token >/dev/null 2>&1"; then
    if run_as_user bash -lc 'gh auth status --hostname github.com >/dev/null 2>&1'; then
      GH_USER="$(run_as_user bash -lc 'gh api user -q .login 2>/dev/null' || echo unknown)"
      report "gh_auth" ok "logged in as $GH_USER"
    else
      report "gh_auth" failed "login accepted but auth status verification failed"
    fi
  else
    report "gh_auth" failed "gh auth login --with-token rejected PAT"
  fi
else
  report "gh_auth" ok "no PAT on fleet (public repo clone will still work)"
fi

# ─── 6. Clone forge-fleet + build ff ─────────────────────────────────────
#
# Canonical source-tree location (per reference_source_tree_locations.md +
# the V31 `computers.source_tree_path` backfill):
#   - Taylor (leader / dev workstation):      ~/projects/forge-fleet
#   - Every other fleet member:               ~/.forgefleet/sub-agent-0/forge-fleet
#
# The sub-agent-0 path is the canonical workspace for dispatched fleet-LLM
# work (`ff supervise` / `ff run`). Keeping the bootstrap clone there
# eliminates a wasted re-clone on the first auto-upgrade tick (V32's
# playbook would otherwise clone to the canonical path separately).

report "clone" running
if [ "$OS_ID" = "macos" ]; then
  # macOS nodes are rare in the current fleet and usually the leader (Taylor).
  # Keep the legacy path — the macOS Ace box isn't a dispatch target.
  REPO_DIR="/Users/${SUDO_INVOKER}/projects/forge-fleet"
elif [ "$ROLE" = "leader" ]; then
  REPO_DIR="/home/${SUDO_INVOKER}/projects/forge-fleet"
else
  REPO_DIR="/home/${SUDO_INVOKER}/.forgefleet/sub-agent-0/forge-fleet"
fi

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
run_as_user install -m 755 "$REPO_DIR/target/release/ff" "$USER_HOME/.local/bin/ff"
# CLI aliases so external agents (Codex, Claude Code, OpenClaw, third-party
# tools) can resolve the binary by project name without hardcoding "ff".
run_as_user ln -sf "$USER_HOME/.local/bin/ff" "$USER_HOME/.local/bin/forgefleet"
run_as_user ln -sf "$USER_HOME/.local/bin/ff" "$USER_HOME/.local/bin/ForgeFleet"
report "build" ok

# ─── 6a. Node 22 + real dashboard build + forgefleetd ─────────────────────
# Pulse publishing lives in forgefleetd (not ff daemon). Sia's first
# enrollment skipped this and stayed dark in `ff fleet health`.
# The `forge-fleet` crate's ff-gateway uses `#[derive(RustEmbed)]` pointing
# at `dashboard/dist/` — the folder must exist at build time with the
# compiled React assets. Operator directive: NEVER stub the dashboard —
# every node must serve the real UI. Vite needs Node ≥ 20.19 / 22.12;
# Ubuntu 24.04 apt ships Node 18 (too old), so we install Node 22 from
# NodeSource on Linux and assume brew on macOS.
case "$OS_ID" in
  macos)
    if ! command -v node >/dev/null 2>&1 || [ "$(node --version | cut -dv -f2 | cut -d. -f1)" -lt 20 ] 2>/dev/null; then
      report "nodejs" running
      run_as_user bash -lc 'command -v brew >/dev/null && brew install node@22 && brew link --overwrite --force node@22' \
        || die "install node@22 via brew failed (install homebrew first)"
      report "nodejs" ok "$(node --version)"
    fi ;;
  *)
    NEED_NODE=0
    if ! command -v node >/dev/null 2>&1; then NEED_NODE=1; fi
    if command -v node >/dev/null 2>&1 && [ "$(node --version | cut -dv -f2 | cut -d. -f1)" -lt 20 ] 2>/dev/null; then NEED_NODE=1; fi
    if [ "$NEED_NODE" = "1" ]; then
      report "nodejs" running
      # Ubuntu's default nodejs is 18 on 24.04; wipe it first so NodeSource's
      # install doesn't conflict.
      apt-get remove -y nodejs npm libnode-dev >/dev/null 2>&1 || true
      curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null 2>&1 \
        || die "NodeSource setup_22 failed"
      apt-get install -y nodejs >/dev/null 2>&1 \
        || die "apt-get install nodejs (NodeSource) failed"
      report "nodejs" ok "$(node --version)"
    fi ;;
esac

report "dashboard_build" running
run_as_user bash -lc "cd '$REPO_DIR/dashboard' && npm install --no-audit --no-fund --silent 2>&1 | tail -2 && npm run build 2>&1 | tail -3" \
  || die "dashboard build failed"
[ -f "$REPO_DIR/dashboard/dist/index.html" ] || die "dashboard build produced no dist/index.html"
report "dashboard_build" ok

report "forgefleetd_build" running
run_as_user bash -lc "cd '$REPO_DIR' && cargo build -p forge-fleet --release 2>&1 | tail -2" \
  || die "forgefleetd cargo build failed"
run_as_user install -m 755 "$REPO_DIR/target/release/forgefleetd" "$USER_HOME/.local/bin/forgefleetd"
report "forgefleetd_build" ok

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

KERNEL_REL="$(uname -r 2>/dev/null || echo unknown)"
ENROLL_PAYLOAD="$(cat <<EOF
{
  "token": "$TOKEN",
  "name": "$NAME",
  "hostname": "$(hostname)",
  "ip": "$IP",
  "os": "$OS_FULL",
  "os_id": "$OS_ID",
  "kernel": "$KERNEL_REL",
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

# ─── 10b. fleet.toml — Postgres + Redis URL pointing at the leader ──────
# The daemon refuses to start without this file. Self-heal gap surfaced on
# Sia's first enrollment (Apr 21 2026): daemon crashed-looped with
# `connect Postgres: read fleet.toml: No such file or directory`.
report "fleet_toml" running
FLEET_TOML="$USER_HOME/.forgefleet/fleet.toml"
run_as_user mkdir -p "$USER_HOME/.forgefleet"
if [ ! -f "$FLEET_TOML" ]; then
  run_as_user bash -c "cat > '$FLEET_TOML' <<EOF
[database]
mode = \"postgres_full\"
cutover_evidence = \"phase38-cutover-validated-2026-04-05\"
host = \"{{LEADER_HOST}}\"
port = 55432
name = \"forgefleet\"
user = \"forgefleet\"
password = \"forgefleet\"
url = \"postgresql://forgefleet:forgefleet@{{LEADER_HOST}}:55432/forgefleet\"

[redis]
url = \"redis://{{LEADER_HOST}}:6380\"
prefix = \"pulse\"

[loops.self_heal]
enabled = true
interval_secs = 30
auto_adopt = true
max_health_failures = 3
stop_timeout_secs = 10
health_probe_timeout_secs = 3
EOF"
  report "fleet_toml" ok
else
  report "fleet_toml" ok "already exists"
fi

# ─── 11. systemd unit ────────────────────────────────────────────────────

if [ "$OS_ID" != "macos" ]; then
  # Sweep legacy user-scope units that ship the `forgefleetd --node-name <h>
  # start` ExecStart pattern. When they coexist with the canonical
  # `forgefleetd.service`, both fire on boot and the one with --node-name
  # creates a "shell-launcher → forgefleetd" pair that looks like an
  # orphan to the wave dispatcher. Discovered 2026-04-27 — present on 9
  # of 13 Linux fleet hosts at that point. Idempotent: noop when absent.
  USER_SYSTEMD_DIR="$USER_HOME/.config/systemd/user"
  if [ -d "$USER_SYSTEMD_DIR" ]; then
    for legacy in forgefleet-node.service forgefleet-agent.service; do
      if [ -f "$USER_SYSTEMD_DIR/$legacy" ]; then
        run_as_user systemctl --user stop "$legacy" 2>/dev/null || true
        run_as_user systemctl --user disable "$legacy" 2>/dev/null || true
        rm -f "$USER_SYSTEMD_DIR/$legacy"
        rm -f "$USER_SYSTEMD_DIR/default.target.wants/$legacy"
        report "legacy_unit_swept" ok "$legacy"
      fi
    done
    run_as_user systemctl --user daemon-reload 2>/dev/null || true
  fi

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
  # macOS: install LaunchAgent plist so `launchctl kickstart -k` works
  # for the wave dispatcher's Phase-2 restart. Skipping this step left
  # ace stranded with no registered service on 2026-04-27 — every
  # launchctl-domain probe failed and the wave's pkill+nohup fallback
  # had to handle the restart instead. Bootstrap should install the
  # supervisor unit unconditionally; the fallback is for crash-recovery,
  # not normal operation.
  PLIST_TEMPLATE="$REPO_DIR/deploy/launchd/com.forgefleet.forgefleetd.template.plist"
  PLIST_TARGET_DIR="$USER_HOME/Library/LaunchAgents"
  PLIST_TARGET="$PLIST_TARGET_DIR/com.forgefleet.forgefleetd.plist"
  if [ -f "$PLIST_TEMPLATE" ]; then
    USER_UID="$(run_as_user id -u)"
    GUI_DOMAIN="gui/$USER_UID/com.forgefleet.forgefleetd"
    run_as_user mkdir -p "$PLIST_TARGET_DIR" "$USER_HOME/.forgefleet/logs"
    run_as_user bash -c "sed -e 's|__USER_HOME__|$USER_HOME|g' -e 's|__NODE_NAME__|$NAME|g' '$PLIST_TEMPLATE' > '$PLIST_TARGET'"
    # Bootstrap into the GUI domain so live `launchctl kickstart -k` works.
    run_as_user launchctl bootstrap "gui/$USER_UID" "$PLIST_TARGET" 2>/dev/null || true
    run_as_user launchctl enable "$GUI_DOMAIN" 2>/dev/null || true
    run_as_user launchctl kickstart -k "$GUI_DOMAIN" 2>/dev/null || true
    sleep 2
    if run_as_user launchctl print "$GUI_DOMAIN" >/dev/null 2>&1; then
      report "service" ok "launchd plist registered"
    else
      report "service" warn "launchd plist installed but not yet registered (may need user re-login)"
    fi
  else
    report "service" failed "missing $PLIST_TEMPLATE"
  fi
fi

# ─── 12. CLI MCP auto-config ─────────────────────────────────────────────
#
# Wire each vendor CLI (Claude Code, Codex, Gemini) to the local ff-mcp
# server at port 50001 so the agent loops in those CLIs see ff's brain
# tools (brain_search, brain_write_to_inbox, etc.) and the standard 36
# fleet MCP tools. Per the multi-LLM roadmap (PR-A4), this closes the
# manual `claude mcp add ...` gap.
#
# Each CLI has its own config-file convention; we write only when the
# CLI binary itself is installed (gated by `command -v <cli>`). Idempotent
# via merge-or-create logic.
report "mcp-config" running

MCP_URL="http://127.0.0.1:50001/mcp"

# Claude Code: ~/.claude/.mcp-servers.json (JSON array of server objs)
if run_as_user bash -lc 'command -v claude >/dev/null 2>&1'; then
  CLAUDE_MCP_DIR="$USER_HOME/.claude"
  CLAUDE_MCP_FILE="$CLAUDE_MCP_DIR/.mcp-servers.json"
  run_as_user mkdir -p "$CLAUDE_MCP_DIR"
  if [ ! -f "$CLAUDE_MCP_FILE" ]; then
    run_as_user bash -c "cat > '$CLAUDE_MCP_FILE' <<EOF
{\"mcpServers\":{\"forgefleet\":{\"url\":\"$MCP_URL\"}}}
EOF"
    report "mcp-config" ok "wrote claude mcp config"
  else
    # File exists — try `claude mcp add` if available, else leave alone.
    run_as_user bash -lc "claude mcp add forgefleet $MCP_URL 2>/dev/null" || true
  fi
fi

# Codex: ~/.codex/config.toml (TOML; append [mcp_servers.forgefleet] block)
if run_as_user bash -lc 'command -v codex >/dev/null 2>&1'; then
  CODEX_CONFIG_DIR="$USER_HOME/.codex"
  CODEX_CONFIG_FILE="$CODEX_CONFIG_DIR/config.toml"
  run_as_user mkdir -p "$CODEX_CONFIG_DIR"
  if ! run_as_user bash -lc "grep -q 'mcp_servers.forgefleet' '$CODEX_CONFIG_FILE' 2>/dev/null"; then
    run_as_user bash -c "cat >> '$CODEX_CONFIG_FILE' <<EOF

[mcp_servers.forgefleet]
url = \"$MCP_URL\"
EOF"
    report "mcp-config" ok "appended codex mcp config"
  fi
fi

# Gemini CLI: ~/.gemini/settings.json (JSON; mcpServers map, similar shape)
if run_as_user bash -lc 'command -v gemini >/dev/null 2>&1'; then
  GEMINI_DIR="$USER_HOME/.gemini"
  GEMINI_FILE="$GEMINI_DIR/settings.json"
  run_as_user mkdir -p "$GEMINI_DIR"
  if [ ! -f "$GEMINI_FILE" ]; then
    run_as_user bash -c "cat > '$GEMINI_FILE' <<EOF
{\"mcpServers\":{\"forgefleet\":{\"url\":\"$MCP_URL\"}}}
EOF"
    report "mcp-config" ok "wrote gemini mcp config"
  fi
fi

report "mcp-config" ok

# ─── Done ────────────────────────────────────────────────────────────────

report "done" ok "$NAME is now a ForgeFleet node"
say "✓ Onboarding complete: $NAME"
