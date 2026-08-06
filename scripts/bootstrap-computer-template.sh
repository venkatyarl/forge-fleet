#!/usr/bin/env bash
# ForgeFleet computer bootstrap script (rendered from template at serve time).
#
# Placeholders substituted by crates/ff-gateway/src/onboard.rs::render_bootstrap:
#   {{LEADER_HOST}}            — e.g. "192.168.5.100"
#   {{LEADER_PORT}}            — dedicated TLS enrollment port (51443)
#   {{TLS_SERVER_NAME}}        — certificate DNS name (not request-derived)
#   {{TLS_CA_PEM_B64}}         — public fleet CA, base64 encoded
#   {{TLS_SPKI_PIN}}           — curl sha256// SPKI pin
#   {{COMPUTER_NAME}}              — desired fleet_workers.name (from form)
#   {{COMPUTER_IP}}                — computer's LAN IP (from form / server remote_addr)
#   {{SSH_USER}}               — ssh_user for this computer
#   {{ROLE}}                   — "builder" | "gateway" | "testbed"
#   {{RUNTIME}}                — "auto" | "llama.cpp" | "mlx" | "vllm"
#   {{GITHUB_OWNER}}           — e.g. "venkatyarl"
#   {{IS_VINNY}}              — "true" or "false" (controls passwordless sudo)
#
# This script expects to be run with sudo on the new machine:
#   read -rsp 'Enrollment token: ' FORGEFLEET_ENROLLMENT_TOKEN; echo
#   export FORGEFLEET_ENROLLMENT_TOKEN
#   (use the `ff onboard show` command; it pins both CA and SPKI)
# The one-time credential is accepted only through the inherited environment;
# no 1Password service-account token or other fleet-wide credential is ever
# accepted by, rendered into, or forwarded through this joining-node script.
#
# It is intentionally bash, self-contained, and idempotent: re-running it on
# a computer that's already partially set up just advances to the next unfinished
# step.

set -eu
set -o pipefail

LEADER_IP="{{LEADER_HOST}}"
TLS_SERVER_NAME="{{TLS_SERVER_NAME}}"
TLS_CA_PEM_B64="{{TLS_CA_PEM_B64}}"
TLS_SPKI_PIN="{{TLS_SPKI_PIN}}"
LEADER="https://${TLS_SERVER_NAME}:{{LEADER_PORT}}"
TOKEN="${FORGEFLEET_ENROLLMENT_TOKEN:-}"
NAME="{{COMPUTER_NAME}}"
IP="{{COMPUTER_IP}}"
SSH_USER="{{SSH_USER}}"
ROLE="{{ROLE}}"
RUNTIME_HINT="{{RUNTIME}}"
GITHUB_OWNER="{{GITHUB_OWNER}}"
IS_VINNY="{{IS_VINNY}}"

# Reject malformed input before it can enter curl's stdin configuration. The
# server accepts exactly a 32-byte random token encoded as ffe1_<base64url>.
if [[ ! "$TOKEN" =~ ^ffe1_[A-Za-z0-9_-]{43}$ ]]; then
  echo "ERROR: a valid one-time FORGEFLEET_ENROLLMENT_TOKEN is required" >&2
  exit 1
fi

# Retain the one-time value as shell-local state and stop exporting it to every
# child process. Enrollment requests receive it through curl's anonymous stdin
# configuration; only the final sudo hop explicitly preserves it.
export -n FORGEFLEET_ENROLLMENT_TOKEN 2>/dev/null || true

# ─── Helpers ──────────────────────────────────────────────────────────────

say() { printf '▶ %s\n' "$*"; }
decode_tls_ca() {
  if [ "$(uname -s)" = "Darwin" ]; then
    printf '%s' "$TLS_CA_PEM_B64" | base64 -D
  else
    printf '%s' "$TLS_CA_PEM_B64" | base64 -d
  fi
}
tls_curl() {
  # CA bytes and SPKI are public trust anchors embedded by the authenticated
  # renderer. Process substitution keeps even the CA out of durable temp files;
  # --resolve pins the certificate name to the elected leader IP without DNS.
  curl --proto '=https' --tlsv1.3 \
    --resolve "${TLS_SERVER_NAME}:{{LEADER_PORT}}:${LEADER_IP}" \
    --cacert <(decode_tls_ca) \
    --pinnedpubkey "$TLS_SPKI_PIN" \
    "$@"
}
enrollment_auth_config() {
  # TOKEN is expanded by bash's builtin printf into an anonymous pipe, not a
  # child argv, URL, environment, or durable file.
  printf 'header = "Authorization: Bearer %s"\n' "$TOKEN"
}
# JSON-escape a detail string WITHOUT python3 (the bootstrap's report/die/trap
# paths must never depend on a binary that can hang: vinny 2026-08-04).
json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | tr '\n' ' '
}
report() {
  # POST progress event to the leader so the dashboard can show live status.
  local step="$1" status="${2:-running}" detail="${3:-}"
  tls_curl -fsS -m 5 -X POST \
    --config <(enrollment_auth_config) \
    -H "Content-Type: application/json" \
    --data "$(printf '{"name":"%s","step":"%s","status":"%s","detail":"%s"}' \
      "$NAME" "$step" "$status" "$(json_escape "$detail")")" \
    "$LEADER/api/fleet/enrollment-progress" >/dev/null 2>&1 || true
}

die() {
  local msg="$*"
  FF_FATAL_REPORTED=1
  report "fatal" failed "$msg"
  echo "ERROR: $msg" >&2
  exit 1
}

# Run as the target user (not as root). Used for cargo, git, etc. When
# invoked by `sudo bash`, we drop to the real invoker; when invoked directly
# as the user (the sudo-less path), we run commands straight through.
SUDO_INVOKER="${SUDO_USER:-$SSH_USER}"
run_as_user() {
  if [ "$(id -un)" = "$SUDO_INVOKER" ]; then
    "$@"
  else
    sudo -u "$SUDO_INVOKER" -H "$@"
  fi
}

# Run a command with root privileges: directly when already root (legacy
# `curl | sudo bash` flow), or via sudo when run as a normal user (sudo-less
# flow — sudo prompts once and caches). Root is only needed for a handful of
# Linux steps (apt, /etc/hosts, sudoers, loginctl); the macOS path never
# touches this.
as_root() {
  if [ "$(id -u)" = "0" ]; then
    "$@"
  else
    sudo "$@"
  fi
}

# Report unexpected early exits — previously a truncated/aborted run died
# silently and the operator stared at a stuck "running" step forever.
FF_COMPLETED=""
FF_FATAL_REPORTED=""
on_bootstrap_exit() {
  local rc=$?
  if [ "$rc" -ne 0 ] && [ -z "$FF_COMPLETED" ] && [ -z "$FF_FATAL_REPORTED" ]; then
    report "fatal" failed "bootstrap aborted (exit $rc) — re-run to resume; every phase is idempotent"
  fi
}
trap on_bootstrap_exit EXIT

# Resolve USER_HOME upfront — multiple later stages reference it (install
# target, vllm venv path, ssh keypair, sub-agent workspaces). Leaving this
# until later caused $USER_HOME expansion to empty and silent path breakage.
USER_HOME="$(eval echo ~${SUDO_INVOKER})"

# Pre-create directories the script writes to later. `install -m 755` does
# NOT auto-create the parent; a fresh Ubuntu box has no ~/.local/bin.
run_as_user mkdir -p "$USER_HOME/.local/bin" "$USER_HOME/.forgefleet/logs"

say "ForgeFleet onboarding for $NAME ($IP) — runtime hint: $RUNTIME_HINT"

# ─── Preflight: the leader must be reachable BEFORE doing any work ────────
# A wrong subnet/VLAN (e.g. Wi-Fi on 192.168.4.x vs fleet LAN 192.168.5.x)
# used to surface as a dead curl with no explanation an hour in.
if ! tls_curl -fsS -m 5 "$LEADER/health" >/dev/null 2>&1; then
  echo "ERROR: cannot reach the ForgeFleet leader at $LEADER" >&2
  echo "  → check this computer is on the fleet LAN (correct subnet/VLAN; on a Mac, try turning Wi-Fi off)" >&2
  exit 1
fi

report "start" running

# ─── 0. Non-interactive PATH header in ~/.bashrc ─────────────────────────
#
# Ubuntu's stock ~/.bashrc opens with a `case $- in *i*)` guard that
# `return`s for non-interactive shells, so the distro's own ~/.local/bin
# PATH setup further down never runs for `ssh <host> 'ff ...'` or the
# dispatch harness's `bash -c` invocations — they can't resolve
# ~/.local/bin/ff or ~/.cargo/bin/cargo. The export must be PREPENDED
# above that guard; appending would land after the early `return` and
# never execute. Idempotent via the marker line.
report "path_header" running
BASHRC="$USER_HOME/.bashrc"
PATH_MARKER="# forgefleet: non-interactive PATH (must stay above the interactive-only guard)"
if grep -qFx "$PATH_MARKER" "$BASHRC" 2>/dev/null; then
  report "path_header" ok "already present"
else
  run_as_user bash -c "
    tmp='$BASHRC.ffpath.tmp'
    {
      printf '%s\n' '$PATH_MARKER'
      printf 'export PATH=\"\$HOME/.local/bin:\$HOME/.cargo/bin:\$PATH\"\n\n'
      cat '$BASHRC' 2>/dev/null || true
    } > \"\$tmp\"
    mv \"\$tmp\" '$BASHRC'
  "
  report "path_header" ok "prepended"
fi

# ─── 1. OS detection ──────────────────────────────────────────────────────

OS_FULL="unknown"
OS_ID="unknown"
if [ -f /etc/os-release ]; then
  # Source in a subshell so /etc/os-release's NAME=Ubuntu can't clobber
  # our operator-supplied $NAME (which is this computer's fleet name, e.g. "sia").
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

# ─── 1b. /etc/hosts hostname entry ─────────────────────────────────────────
# Debian/Ubuntu convention: 127.0.1.1 must map to this computer's hostname.
# Fleet images are sometimes renamed after imaging, leaving the old entry (or
# none at all), which breaks `sudo` and hostname-sensitive daemons. Make the
# bootstrap idempotent: rewrite the 127.0.1.1 line to NAME, or append one.
if [ "$OS_ID" != "macos" ]; then
  report "hosts" running
  HOSTS_BACKUP="/etc/hosts.forgefleet-before-$(date +%s)"
  as_root cp /etc/hosts "$HOSTS_BACKUP"

  if ! awk -v name="$NAME" '
    BEGIN { found=0 }
    {
      if ($1 == "127.0.1.1") {
        $2 = name
        found=1
      }
      print
    }
    END { if (!found) print "127.0.1.1 " name }
  ' /etc/hosts > /tmp/forgefleet-hosts.new; then
    rm -f /tmp/forgefleet-hosts.new "$HOSTS_BACKUP"
    die "failed to stage /etc/hosts update"
  fi

  if ! awk -v name="$NAME" '$1 == "127.0.1.1" && $2 == name { found=1 } END { exit !found }' /tmp/forgefleet-hosts.new; then
    as_root cp "$HOSTS_BACKUP" /etc/hosts
    rm -f /tmp/forgefleet-hosts.new "$HOSTS_BACKUP"
    die "failed to ensure 127.0.1.1 $NAME in /etc/hosts"
  fi

  as_root mv /tmp/forgefleet-hosts.new /etc/hosts
  rm -f "$HOSTS_BACKUP"
  report "hosts" ok
fi

# ─── 1c. GDM desktop autologin ────────────────────────────────────────────
# Linger keeps user services alive but does not start the graphical desktop
# session after reboot. Match established fleet desktop nodes while leaving
# headless Linux nodes alone (they do not have this GDM configuration file).
if [ -f /etc/gdm3/custom.conf ]; then
  report "gdm_autologin" running
  GDM_CONFIG="/etc/gdm3/custom.conf"
  GDM_TMP="$(mktemp /tmp/forgefleet-gdm.XXXXXX)"
  if ! awk -v user="$SUDO_INVOKER" '
    BEGIN { in_daemon=0; saw_daemon=0; enable_set=0; user_set=0 }
    function add_missing() {
      if (!enable_set) print "AutomaticLoginEnable=True"
      if (!user_set) print "AutomaticLogin=" user
    }
    /^\[daemon\][[:space:]]*$/ {
      saw_daemon=1
      in_daemon=1
      print
      next
    }
    /^\[/ {
      if (in_daemon) add_missing()
      in_daemon=0
    }
    in_daemon && /^[[:space:]]*AutomaticLoginEnable[[:space:]]*=/ {
      if (!enable_set) print "AutomaticLoginEnable=True"
      enable_set=1
      next
    }
    in_daemon && /^[[:space:]]*AutomaticLogin[[:space:]]*=/ {
      if (!user_set) print "AutomaticLogin=" user
      user_set=1
      next
    }
    { print }
    END {
      if (in_daemon) add_missing()
      if (!saw_daemon) {
        print ""
        print "[daemon]"
        print "AutomaticLoginEnable=True"
        print "AutomaticLogin=" user
      }
    }
  ' "$GDM_CONFIG" > "$GDM_TMP"; then
    rm -f "$GDM_TMP"
    die "failed to configure GDM autologin"
  fi
  as_root install -m 0644 "$GDM_TMP" "$GDM_CONFIG"
  rm -f "$GDM_TMP"
  report "gdm_autologin" ok "$SUDO_INVOKER"
fi

# ─── 1d. macOS Remote Login (SSH server) ──────────────────────────────────
# Fresh macOS ships with Remote Login OFF, leaving the node unreachable for
# fleet ops (deploy/ssh/mesh) until someone toggles it by hand — vinny
# 2026-08-04/05: the whole enrollment had to be driven manually over Telegram
# because of exactly this. Linux fleet images have sshd on by default, so this
# step is macOS-only. Never blocks enrollment: if we can't enable it
# automatically (no TTY for the sudo prompt, or macOS demands Full Disk
# Access), report failed with the manual instruction instead of dying.
if [ "$OS_ID" = "macos" ]; then
  report "remote_login" running
  if lsof -nP -iTCP:22 -sTCP:LISTEN 2>/dev/null | grep -q LISTEN; then
    report "remote_login" ok "already on"
  elif [ -t 0 ] && sudo systemsetup -setremotelogin on 2>/dev/null; then
    report "remote_login" ok "enabled"
  else
    report "remote_login" failed "could not enable automatically — operator: System Settings → General → Sharing → Remote Login → On"
  fi
fi

# ─── 2. Prerequisites ─────────────────────────────────────────────────────

report "prereqs" running
case "$OS_ID" in
  macos)
    # Homebrew presumed installed manually (mac setup is interactive).
    if ! run_as_user bash -lc 'command -v op >/dev/null 2>&1'; then
      run_as_user bash -lc 'command -v brew >/dev/null && brew install --cask 1password-cli' \
        || die "1Password CLI install failed"
    fi
    ;;
  *)
    # Ubuntu/DGX OS/Debian — install build toolchain.
    export DEBIAN_FRONTEND=noninteractive
    as_root apt-get update -y >/dev/null 2>&1 || die "apt-get update failed"
    as_root apt-get install -y --no-install-recommends \
      build-essential pkg-config libssl-dev git curl ca-certificates gnupg openssh-client openssh-server \
      >/dev/null 2>&1 || die "apt-get install (prereqs) failed"

    # Install the signed 1Password CLI package from its official APT
    # repository. Installing the client is harmless, but enrollment deliberately
    # performs no vault authentication and receives no service-account token.
    if ! command -v op >/dev/null 2>&1; then
      as_root install -d -m 0755 \
        /usr/share/keyrings \
        /etc/apt/sources.list.d \
        /etc/debsig/policies/AC2D62742012EA22 \
        /usr/share/debsig/keyrings/AC2D62742012EA22
      curl -fsSL https://downloads.1password.com/linux/keys/1password.asc \
        | as_root gpg --dearmor --yes --output /usr/share/keyrings/1password-archive-keyring.gpg \
        || die "1Password signing-key install failed"
      OP_DEB_ARCH="$(dpkg --print-architecture)"
      printf 'deb [arch=%s signed-by=/usr/share/keyrings/1password-archive-keyring.gpg] https://downloads.1password.com/linux/debian/%s stable main\n' \
        "$OP_DEB_ARCH" "$OP_DEB_ARCH" \
        | as_root tee /etc/apt/sources.list.d/1password.list >/dev/null
      curl -fsSL https://downloads.1password.com/linux/debian/debsig/1password.pol \
        | as_root tee /etc/debsig/policies/AC2D62742012EA22/1password.pol >/dev/null \
        || die "1Password debsig policy install failed"
      curl -fsSL https://downloads.1password.com/linux/keys/1password.asc \
        | as_root gpg --dearmor --yes --output /usr/share/debsig/keyrings/AC2D62742012EA22/debsig.gpg \
        || die "1Password debsig key install failed"
      as_root apt-get update -y >/dev/null 2>&1 || die "1Password apt update failed"
      as_root apt-get install -y 1password-cli >/dev/null 2>&1 \
        || die "1Password CLI install failed"
    fi
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

# ─── 4. Passwordless sudo (except on Vinny) ─────────────────────────────

if [ "$IS_VINNY" != "true" ]; then
  report "sudoers" running
  SUDOERS_FILE="/etc/sudoers.d/forgefleet-${SUDO_INVOKER}"
  echo "${SUDO_INVOKER} ALL=(ALL) NOPASSWD:ALL" | as_root tee "$SUDOERS_FILE" >/dev/null
  as_root chmod 0440 "$SUDOERS_FILE"
  as_root visudo -c -f "$SUDOERS_FILE" >/dev/null 2>&1 || die "sudoers syntax invalid"
  # Verify from the user's shell.
  run_as_user sudo -n true || die "passwordless sudo not working"
  report "sudoers" ok
else
  report "sudoers" ok "skipped (vinny)"
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
             as_root apt-get update -y >/dev/null 2>&1
             as_root apt-get install -y gh >/dev/null 2>&1 || die "gh install failed"
           fi ;;
esac
report "gh" ok

# ─── 5b. GitHub API credential policy ────────────────────────────────────
# Do not fetch, interpolate, or persist a GitHub PAT during enrollment.
# Repository clone/push authority is the fleet-owned SSH identity installed
# below. Commands that require the GitHub API receive GH_TOKEN on demand from
# the fleet/1Password authority after ff is installed.
report "gh_auth" running
report "gh_auth" ok "deferred: API token is injected on demand from fleet/1Password authority"

# ─── 5c. Git identity ────────────────────────────────────────────────────
# Commits made on this computer (sub-agent worktrees, dispatched builds) need
# a consistent author identity. Configure it for the invoking user, not root.
report "git_identity" running
run_as_user git config --global user.name 'Venkat Yarlagadda'
run_as_user git config --global user.email 'venkatyarl@users.noreply.github.com'
report "git_identity" ok

# ─── 5d. Repository/bootstrap credential boundary ─────────────────────────
# forge-fleet is readable over HTTPS, so first install needs no GitHub private
# key. Push/API/cloud credentials are distributed only after the node has been
# admitted and is under fleet policy; a joining node never receives the
# vault-wide 1Password authority or centralized OAuth documents.
report "github_deploy_key" ok "deferred until after authenticated enrollment"

# Pin GitHub's published host keys in a dedicated trust file.  These values and
# fingerprints come from https://docs.github.com/en/authentication/
# keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints.  Never
# turn an unauthenticated network scan directly into trust.
GITHUB_KNOWN_HOSTS="$USER_HOME/.ssh/known_hosts.github"
GITHUB_KNOWN_HOSTS_TMP="$(run_as_user mktemp "$USER_HOME/.ssh/.known_hosts.github.XXXXXX")" \
  || die "failed to stage pinned GitHub SSH host keys"
if ! run_as_user sh -c 'umask 077; cat > "$1"' \
  forgefleet-github-hosts "$GITHUB_KNOWN_HOSTS_TMP" <<'EOF'
github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl
github.com ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=
github.com ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCj7ndNxQowgcQnjshcLrqPEiiphnt+VTTvDP6mHBL9j1aNUkY4Ue1gvwnGLVlOhGeYrnZaMgRK6+PKCUXaDbC7qtbW8gIkhL7aGCsOr/C56SJMy/BCZfxd1nWzAOxSDPgVsmerOBYfNqltV9/hWCqBywINIR+5dIg6JTJ72pcEpEjcYgXkE2YEFXV1JHnsKgbLWNlhScqb2UmyRkQyytRLtL+38TGxkxCflmO+5Z8CSSNY7GidjMIZ7Q4zMjA2n1nGrlTDkzwDCsw+wqFPGQA179cnfGWOWRVruj16z6XyvxvjJwbz0wQZ75XK5tKSb7FNyeIEs4TT4jk+S4dhPeAUC5y+bDYirYgM4GC7uEnztnZyaVWQ7B381AK4Qdrwt51ZqExKbQpTUNn+EjqoTwvqNj4kqx5QUCI0ThS/YkOxJCXmPUWZbhjpCg56i+2aB6CmK2JGhn57K5mj0MNdBXA4/WnwH6XoPWJzK5Nyu2zB3nAZp+S5hpQs+p1vN1/wsjk=
EOF
then
  run_as_user rm -f "$GITHUB_KNOWN_HOSTS_TMP"
  die "failed to write pinned GitHub SSH host keys"
fi
GITHUB_HOST_FINGERPRINTS="$(run_as_user ssh-keygen -lf "$GITHUB_KNOWN_HOSTS_TMP" -E sha256 2>/dev/null)" \
  || {
    run_as_user rm -f "$GITHUB_KNOWN_HOSTS_TMP"
    die "pinned GitHub SSH host keys are malformed"
  }
for github_fp in \
  'SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s' \
  'SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM' \
  'SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU'; do
  if ! printf '%s\n' "$GITHUB_HOST_FINGERPRINTS" | grep -Fq "$github_fp"; then
    unset GITHUB_HOST_FINGERPRINTS github_fp
    run_as_user rm -f "$GITHUB_KNOWN_HOSTS_TMP"
    die "pinned GitHub SSH host-key fingerprint validation failed"
  fi
done
unset GITHUB_HOST_FINGERPRINTS github_fp
run_as_user chmod 644 "$GITHUB_KNOWN_HOSTS_TMP" \
  && run_as_user mv -f "$GITHUB_KNOWN_HOSTS_TMP" "$GITHUB_KNOWN_HOSTS" \
  || {
    run_as_user rm -f "$GITHUB_KNOWN_HOSTS_TMP"
    die "failed to atomically install pinned GitHub SSH host keys"
  }

# Put the managed alias in a dedicated include that is evaluated before any
# legacy Host block, so older bootstrap output cannot weaken pinned trust.
SSH_CONFIG_D="$USER_HOME/.ssh/config.d"
SSH_GITHUB_CONFIG="$SSH_CONFIG_D/forgefleet-github.conf"
run_as_user mkdir -p "$SSH_CONFIG_D"
SSH_GITHUB_CONFIG_TMP="$(run_as_user mktemp "$SSH_CONFIG_D/.forgefleet-github.XXXXXX")" \
  || die "failed to stage the GitHub SSH configuration"
if ! run_as_user sh -c 'umask 077; cat > "$1"' \
  forgefleet-github-config "$SSH_GITHUB_CONFIG_TMP" <<'EOF'
Host github.com-venkat
  HostName github.com
  User git
  IdentityFile ~/.ssh/id_venkat
  IdentitiesOnly yes
  StrictHostKeyChecking yes
  UserKnownHostsFile ~/.ssh/known_hosts.github
  GlobalKnownHostsFile /dev/null
EOF
then
  run_as_user rm -f "$SSH_GITHUB_CONFIG_TMP"
  die "failed to write the GitHub SSH configuration"
fi
run_as_user chmod 600 "$SSH_GITHUB_CONFIG_TMP" \
  && run_as_user mv -f "$SSH_GITHUB_CONFIG_TMP" "$SSH_GITHUB_CONFIG" \
  || {
    run_as_user rm -f "$SSH_GITHUB_CONFIG_TMP"
    die "failed to atomically install the GitHub SSH configuration"
  }
SSH_CONFIG="$USER_HOME/.ssh/config"
if ! run_as_user grep -qFx 'Include ~/.ssh/config.d/forgefleet-github.conf' "$SSH_CONFIG" 2>/dev/null; then
  SSH_CONFIG_TMP="$(run_as_user mktemp "$USER_HOME/.ssh/.config.XXXXXX")" \
    || die "failed to stage the SSH configuration include"
  if ! run_as_user sh -c 'umask 077; { printf "%s\n" "Include ~/.ssh/config.d/forgefleet-github.conf"; cat "$2" 2>/dev/null || true; } > "$1"' \
    forgefleet-ssh-config "$SSH_CONFIG_TMP" "$SSH_CONFIG"; then
    run_as_user rm -f "$SSH_CONFIG_TMP"
    die "failed to stage the SSH configuration include"
  fi
  run_as_user chmod 600 "$SSH_CONFIG_TMP" \
    && run_as_user mv -f "$SSH_CONFIG_TMP" "$SSH_CONFIG" \
    || {
      run_as_user rm -f "$SSH_CONFIG_TMP"
      die "failed to atomically install the SSH configuration include"
    }
fi
run_as_user chmod 600 "$SSH_CONFIG"
report "github_host_trust" ok "pinned GitHub SSH host keys installed for later post-enrollment auth"

# ─── 6. Clone forge-fleet + build ff ─────────────────────────────────────
#
# Canonical source-tree location (per reference_source_tree_locations.md +
# the V31 `computers.source_tree_path` backfill):
# Builder-role nodes use ~/projects/forge-fleet because auto-upgrade and the
# config loaders treat it as the canonical source tree. Never stage a second
# checkout at ~/forge-fleet or under a sub-agent workspace during onboarding.

report "clone" running
if [ "$ROLE" = "builder" ] || [ "$ROLE" = "leader" ]; then
  REPO_DIR="$USER_HOME/projects/forge-fleet"
else
  REPO_DIR="/home/${SUDO_INVOKER}/.forgefleet/sub-agents/sub-agent-0/forge-fleet"
fi

run_as_user mkdir -p "$(dirname "$REPO_DIR")"
if [ ! -d "$REPO_DIR/.git" ]; then
  CLONE_URL="https://github.com/${GITHUB_OWNER}/forge-fleet.git"
  run_as_user git clone --depth 50 "$CLONE_URL" "$REPO_DIR" \
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
# CLI aliases so external agents (Codex, Claude Code, third-party
# tools) can resolve the binary by project name without hardcoding "ff".
run_as_user ln -sf "$USER_HOME/.local/bin/ff" "$USER_HOME/.local/bin/forgefleet"
run_as_user ln -sf "$USER_HOME/.local/bin/ff" "$USER_HOME/.local/bin/ForgeFleet"
report "build" ok

# ─── 6a. Node 22 + real web-forge-fleet build + forgefleetd ───────────────
# Pulse publishing lives in forgefleetd (not ff daemon). Sia's first
# enrollment skipped this and stayed dark in `ff fleet health`.
# The `forge-fleet` crate's ff-gateway uses `#[derive(RustEmbed)]` pointing
# at `web-forge-fleet/out/` — the folder must exist at build time with the
# compiled Next.js static export. Operator directive: NEVER stub the web
# console — every computer must serve the real UI. Next.js needs
# Node ≥ 20.19 / 22.12; Ubuntu 24.04 apt ships Node 18 (too old), so we
# install Node 22 from NodeSource on Linux and assume brew on macOS.
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
      as_root apt-get remove -y nodejs npm libnode-dev >/dev/null 2>&1 || true
      curl -fsSL https://deb.nodesource.com/setup_22.x | as_root bash - >/dev/null 2>&1 \
        || die "NodeSource setup_22 failed"
      as_root apt-get install -y nodejs >/dev/null 2>&1 \
        || die "apt-get install nodejs (NodeSource) failed"
report "nodejs" ok "$(node --version)"
    fi ;;
esac

# ─── 6b. Cloud coding CLIs; authentication remains post-enrollment ────────
# Install every supported cloud CLI as the target user. Centralized OAuth
# documents are deliberately not copied during bootstrap; the authenticated
# fleet distributor owns that lifecycle after admission.
report "cloud_clis" running
if ! run_as_user bash -lc 'command -v claude >/dev/null 2>&1'; then
  run_as_user bash -lc 'curl -fsSL https://claude.ai/install.sh | bash' \
    || die "Claude Code install failed"
fi
if ! run_as_user bash -lc 'command -v codex >/dev/null 2>&1'; then
  run_as_user bash -lc 'npm install -g @openai/codex' || die "Codex install failed"
fi
CODEX_BIN="$(run_as_user bash -lc 'npm prefix -g')/bin/codex"
if [ -x "$CODEX_BIN" ]; then
  run_as_user ln -sf "$CODEX_BIN" "$USER_HOME/.local/bin/codex"
fi
if ! run_as_user bash -lc 'command -v uv >/dev/null 2>&1'; then
  run_as_user bash -lc 'curl -LsSf https://astral.sh/uv/install.sh | sh' || die "uv install failed"
fi
if ! run_as_user bash -lc 'command -v kimi >/dev/null 2>&1'; then
  run_as_user bash -lc 'uv tool install kimi-cli' || die "Kimi CLI install failed"
fi

report "cloud_clis" ok "claude, codex, kimi installed; auth deferred to fleet distributor"

report "web_build" running
run_as_user bash -lc "cd '$REPO_DIR/web-forge-fleet' && npm install --no-audit --no-fund --silent 2>&1 | tail -2 && npm run build 2>&1 | tail -3" \
  || die "web-forge-fleet build failed"
[ -f "$REPO_DIR/web-forge-fleet/out/index.html" ] || die "web-forge-fleet build produced no out/index.html"
report "web_build" ok

report "forgefleetd_build" running
run_as_user bash -lc "cd '$REPO_DIR' && cargo build -p forge-fleet --release 2>&1 | tail -2" \
  || die "forgefleetd cargo build failed"
run_as_user install -m 755 "$REPO_DIR/target/release/forgefleetd" "$USER_HOME/.local/bin/forgefleetd"
report "forgefleetd_build" ok

# ─── 6c. vLLM venv (GPU nodes only) ──────────────────────────────────────
if [ "$RUNTIME" = "vllm" ]; then
  report "vllm_venv" running
  VENV="$USER_HOME/.forgefleet/vllm-venv"
  if [ ! -d "$VENV" ]; then
    run_as_user mkdir -p "$USER_HOME/.forgefleet"
    if ! run_as_user python3 -m venv "$VENV" >/dev/null 2>&1; then
      # DGX / Ubuntu often need python3-venv installed separately.
      as_root apt-get install -y python3-venv >/dev/null 2>&1 || true
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
# Read the public key we just generated (or that already existed). Redirect
# stderr so a failed `cat` can never leak an error message into the enrollment
# payload, which would otherwise be written to peer authorized_keys files.
USER_PUBKEY="$(cat "${KEY_PATH}.pub" 2>/dev/null || true)"
if [ -z "$USER_PUBKEY" ] || ! printf '%s\n' "$USER_PUBKEY" | grep -qE '^ssh-(rsa|ed25519|ecdsa|dsa) '; then
  die "no valid user SSH public key found at ${KEY_PATH}.pub"
fi

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
  run_as_user mkdir -p "$FF_HOME/sub-agents/sub-agent-${i}/scratch" "$FF_HOME/sub-agents/sub-agent-${i}/checkpoints" "$FF_HOME/sub-agents/sub-agent-${i}/cache"
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

ENROLL_RESP="$(printf '%s' "$ENROLL_PAYLOAD" | tls_curl -fsS -m 30 -X POST \
  --config <(enrollment_auth_config) \
  -H "Content-Type: application/json" \
  --data-binary @- \
  "$LEADER/api/fleet/self-enroll")" || die "self-enroll HTTPS request failed"

say "Enrolled: $ENROLL_RESP"
report "enroll" ok
unset ENROLL_PAYLOAD TOKEN FORGEFLEET_ENROLLMENT_TOKEN

# ─── 10. Import peer SSH identities ──────────────────────────────────────

report "mesh_import" running
# Parse peer_ssh_identities from the enrollment response and merge into
# ~/.ssh/authorized_keys and ~/.ssh/known_hosts. Runs from a temp file (not a
# heredoc) under a 90s watchdog with captured output: the 2026-08-03 vinny
# onboard hung silently INSIDE this step with zero diagnostics — a hang the
# EXIT trap cannot see — so now it logs visibly and times out loudly.
# Suffix-free mktemp templates — GNU mktemp accepts `XXXXXX.py` suffixes but
# stock macOS (BSD) mktemp rejects them, killing the step instantly.
MESH_PY="$(mktemp /tmp/forgefleet-mesh-import-py.XXXXXX)"
MESH_LOG="$(mktemp /tmp/forgefleet-mesh-import-log.XXXXXX)"
cat > "$MESH_PY" <<PY
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
    # Reject anything that does not look like an OpenSSH public key line.
    # This prevents shell error messages (e.g. from a failed `cat` on the
    # peer) from being appended to authorized_keys.
    if not upk:
        continue
    parts = upk.split(None, 2)
    if len(parts) < 2 or not parts[0].startswith("ssh-"):
        print(f"skipping bogus peer key from {p.get('name', '?')}: {upk[:40]!r}")
        continue
    if upk not in existing_authz:
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
try:
    uid = pwd.getpwnam("$SUDO_INVOKER").pw_uid
    gid = pwd.getpwnam("$SUDO_INVOKER").pw_gid
    os.chown(str(authz), uid, gid)
    os.chown(str(known), uid, gid)
except PermissionError:
    # Sudo-less runs already own these files; chown is only needed when the
    # script runs as root (legacy sudo flow).
    pass
print(f"imported: +{added_user} authorized_keys, +{added_host} known_hosts")
PY
# Hard, un-hangable cap: perl's alarm survives exec and always fires — no
# polling loop, no zombie semantics, no ps portability roulette (the 2026-08-04
# vinny runs proved every shell-watchdog variant can itself wedge). Peer keys
# are best-effort: a failure here must NEVER block enrollment.
perl -e 'alarm 60; exec @ARGV' python3 "$MESH_PY" > "$MESH_LOG" 2>&1
mesh_rc=$?
cat "$MESH_LOG"
if [ "$mesh_rc" -ne 0 ]; then
  report "mesh_import" failed "peer key import failed/timeout (rc=$mesh_rc) — continuing without it"
else
  report "mesh_import" ok
fi
rm -f "$MESH_PY" "$MESH_LOG"

# ─── 10b. fleet.toml — Postgres + Redis URL pointing at the DB host ──────
# The daemon refuses to start without this file. Self-heal gap surfaced on
# Sia's first enrollment (Apr 21 2026): daemon crashed-looped with
# `connect Postgres: read fleet.toml: No such file or directory`.
# DB host is rendered separately from the leader host: Postgres/Redis do NOT
# necessarily live on the serving gateway (vinny 2026-08-04 — a fleet.toml
# pointing at the leader's IP would crash-loop the fresh daemon).
report "fleet_toml" running
FLEET_TOML="$USER_HOME/.forgefleet/fleet.toml"
run_as_user mkdir -p "$USER_HOME/.forgefleet"
if [ ! -f "$FLEET_TOML" ]; then
  # Create secret-bearing configuration under a restrictive umask so there is
  # no permissive-mode window before the unconditional chmod below.
  run_as_user bash -c "umask 077
cat > '$FLEET_TOML' <<EOF
[database]
mode = \"postgres_full\"
cutover_evidence = \"phase38-cutover-validated-2026-04-05\"
host = \"{{DB_HOST}}\"
port = {{DB_PORT}}
name = \"forgefleet\"
user = \"forgefleet\"
password = \"forgefleet\"
url = \"postgresql://forgefleet:forgefleet@{{DB_HOST}}:{{DB_PORT}}/forgefleet\"

[redis]
url = \"redis://{{REDIS_HOST}}:{{REDIS_PORT}}\"
prefix = \"pulse\"

[loops.self_heal]
enabled = true
interval_secs = 30
auto_adopt = true
max_health_failures = 3
stop_timeout_secs = 10
health_probe_timeout_secs = 3
EOF"
  FLEET_TOML_RESULT="created"
else
  FLEET_TOML_RESULT="already existed"
fi
run_as_user chmod 600 "$FLEET_TOML" \
  || die "failed to restrict $FLEET_TOML to mode 0600"
report "fleet_toml" ok "$FLEET_TOML_RESULT; mode=0600"

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

  # Legacy system-scope `ff daemon` units: stop + remove so they can't
  # double-run alongside the canonical forgefleetd user unit below.
  if [ -f /etc/systemd/system/forgefleet-daemon@.service ]; then
    systemctl disable --now "forgefleet-daemon@${SUDO_INVOKER}.service" >/dev/null 2>&1 || true
    rm -f /etc/systemd/system/forgefleet-daemon@.service
    systemctl daemon-reload
    report "legacy_unit_swept" ok "forgefleet-daemon@.service"
  fi
  if [ -f /etc/systemd/system/forgefleet-daemon.service ]; then
    systemctl disable --now forgefleet-daemon.service >/dev/null 2>&1 || true
    rm -f /etc/systemd/system/forgefleet-daemon.service
    systemctl daemon-reload
    report "legacy_unit_swept" ok "forgefleet-daemon.service"
  fi

  report "service" running
  # Canonical forgefleetd USER unit (deploy/systemd/forgefleetd.service).
  # enable-linger starts the user manager at boot (and creates
  # /run/user/<uid>, which `systemctl --user` from a sudo context needs).
  USER_UID="$(run_as_user id -u)"
  [ "$USER_UID" -ne 0 ] || die "refusing to install the user service for root; set SUDO_USER or SSH_USER to the fleet account"
  as_root loginctl enable-linger "$SUDO_INVOKER" \
    || die "loginctl enable-linger failed for $SUDO_INVOKER"
  [ "$(loginctl show-user "$SUDO_INVOKER" -p Linger --value 2>/dev/null)" = "yes" ] \
    || die "lingering is not enabled for $SUDO_INVOKER; run: sudo loginctl enable-linger $SUDO_INVOKER"
  user_systemctl() {
    run_as_user env XDG_RUNTIME_DIR="/run/user/$USER_UID" \
      DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$USER_UID/bus" \
      systemctl --user "$@"
  }
  run_as_user mkdir -p "$USER_SYSTEMD_DIR" "$USER_HOME/.forgefleet/logs"
  run_as_user bash -c "sed 's|__COMPUTER_NAME__|$NAME|g' '$REPO_DIR/deploy/systemd/forgefleetd.service' > '$USER_SYSTEMD_DIR/forgefleetd.service'"
  run_as_user cp "$REPO_DIR/deploy/systemd/forgefleet-mcp.service" "$USER_SYSTEMD_DIR/forgefleet-mcp.service"
  user_systemctl daemon-reload \
    || die "systemctl --user daemon-reload failed for $SUDO_INVOKER"
  user_systemctl enable forgefleetd.service forgefleet-mcp.service \
    || die "failed to enable ForgeFleet services for reboot persistence"
  # restart (not just enable --now): an idempotent re-run must pick up the
  # freshly built binary, not keep the old process.
  user_systemctl restart forgefleetd.service \
    || die "failed to restart forgefleetd.service"
  user_systemctl restart forgefleet-mcp.service \
    || die "failed to restart DB-independent forgefleet-mcp.service"
  sleep 2
  if ! user_systemctl is-enabled forgefleetd.service >/dev/null 2>&1; then
    die "systemctl --user reports forgefleetd.service disabled; run: systemctl --user enable forgefleetd.service"
  elif ! user_systemctl is-active forgefleetd.service >/dev/null 2>&1; then
    die "systemctl --user reports forgefleetd.service inactive; inspect: systemctl --user status forgefleetd.service"
  fi
  if ! user_systemctl is-enabled forgefleet-mcp.service >/dev/null 2>&1; then
    die "systemctl --user reports forgefleet-mcp.service disabled; run: systemctl --user enable forgefleet-mcp.service"
  elif ! user_systemctl is-active forgefleet-mcp.service >/dev/null 2>&1; then
    die "systemctl --user reports forgefleet-mcp.service inactive; inspect: systemctl --user status forgefleet-mcp.service"
  fi
  report "service" ok "forgefleetd and separate MCP user units active"
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
  MCP_PLIST_TARGET="$PLIST_TARGET_DIR/com.forgefleet.forgefleet-mcp.plist"
  if [ -f "$PLIST_TEMPLATE" ]; then
    USER_UID="$(run_as_user id -u)"
    GUI_DOMAIN="gui/$USER_UID/com.forgefleet.forgefleetd"
    MCP_GUI_DOMAIN="gui/$USER_UID/com.forgefleet.forgefleet-mcp"
    run_as_user mkdir -p "$PLIST_TARGET_DIR" "$USER_HOME/.forgefleet/logs"
    TG_TOKEN="${TELEGRAM_BOT_TOKEN:-${FORGEFLEET_TELEGRAM_BOT_TOKEN:-}}"
    run_as_user bash -c "sed -e 's|__USER_HOME__|$USER_HOME|g' -e 's|__COMPUTER_NAME__|$NAME|g' -e 's|__TELEGRAM_BOT_TOKEN__|$TG_TOKEN|g' '$PLIST_TEMPLATE' > '$PLIST_TARGET'"
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

    # MCP is a separate, restartable process just like the Linux
    # forgefleet-mcp.service. Keeping it out of the main daemon means client
    # transports survive a forgefleetd rollout and reconnect to a stable port.
    # Stage in the LaunchAgents directory, validate the XML with plutil, and
    # atomically publish it before touching the currently running agent.  A
    # malformed generated plist therefore leaves both the existing file and
    # existing launchd job intact.
    MCP_PLIST_TMP="$(run_as_user mktemp "$PLIST_TARGET_DIR/.com.forgefleet.forgefleet-mcp.XXXXXX")" \
      || die "failed to stage the separate ForgeFleet MCP LaunchAgent"
    if ! run_as_user bash -c "umask 077; cat > '$MCP_PLIST_TMP' <<EOF
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>Label</key>
  <string>com.forgefleet.forgefleet-mcp</string>
  <key>ProgramArguments</key>
  <array>
    <string>$USER_HOME/.local/bin/forgefleetd</string>
    <string>mcp</string>
    <string>--listen</string>
    <string>0.0.0.0:50001</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>$USER_HOME</string>
    <key>PATH</key>
    <string>$USER_HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin</string>
  </dict>
  <key>WorkingDirectory</key>
  <string>$USER_HOME</string>
  <key>StandardOutPath</key>
  <string>$USER_HOME/.forgefleet/logs/forgefleet-mcp.log</string>
  <key>StandardErrorPath</key>
  <string>$USER_HOME/.forgefleet/logs/forgefleet-mcp.log</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>2</integer>
</dict>
</plist>
EOF"; then
      run_as_user rm -f "$MCP_PLIST_TMP"
      die "failed to write the staged ForgeFleet MCP LaunchAgent"
    fi
    if ! run_as_user plutil -lint "$MCP_PLIST_TMP" >/dev/null 2>&1; then
      run_as_user rm -f "$MCP_PLIST_TMP"
      die "generated ForgeFleet MCP LaunchAgent failed plutil validation"
    fi
    run_as_user chmod 600 "$MCP_PLIST_TMP" \
      && run_as_user mv -f "$MCP_PLIST_TMP" "$MCP_PLIST_TARGET" \
      || {
        run_as_user rm -f "$MCP_PLIST_TMP"
        die "failed to atomically install the ForgeFleet MCP LaunchAgent"
      }
    run_as_user launchctl bootout "gui/$USER_UID" "$MCP_PLIST_TARGET" 2>/dev/null || true
    run_as_user launchctl bootstrap "gui/$USER_UID" "$MCP_PLIST_TARGET" \
      || die "failed to bootstrap the separate ForgeFleet MCP LaunchAgent"
    run_as_user launchctl enable "$MCP_GUI_DOMAIN" \
      || die "failed to enable the separate ForgeFleet MCP LaunchAgent"
    run_as_user launchctl kickstart -k "$MCP_GUI_DOMAIN" \
      || die "failed to start the separate ForgeFleet MCP LaunchAgent"
    sleep 2
    run_as_user launchctl print "$MCP_GUI_DOMAIN" >/dev/null 2>&1 \
      || die "launchd did not register the separate ForgeFleet MCP agent"
    report "mcp-service" ok "separate launchd MCP agent registered"
  else
    die "missing canonical launchd template: $PLIST_TEMPLATE"
  fi
fi

# Do not install any client configuration until the independently supervised
# MCP listener both answers health and exposes a non-empty tools catalog.
report "mcp-ready" running
MCP_HEALTHY=""
MCP_ATTEMPT=0
while [ "$MCP_ATTEMPT" -lt 20 ]; do
  if curl -fsS -m 2 http://127.0.0.1:50001/mcp/health 2>/dev/null | grep -q '^ok$'; then
    MCP_HEALTHY=1
    break
  fi
  MCP_ATTEMPT=$((MCP_ATTEMPT + 1))
  sleep 1
done
[ -n "$MCP_HEALTHY" ] \
  || die "separate ForgeFleet MCP listener did not become healthy on 127.0.0.1:50001"
if ! MCP_TOOLS_RESPONSE="$(curl -fsS -m 10 \
  -H 'Content-Type: application/json' \
  --data '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}' \
  http://127.0.0.1:50001/mcp 2>/dev/null)"; then
  die "separate ForgeFleet MCP tools/list request failed"
fi
if ! printf '%s' "$MCP_TOOLS_RESPONSE" | python3 -c '
import json, sys
payload = json.load(sys.stdin)
tools = payload.get("result", {}).get("tools", [])
raise SystemExit(0 if isinstance(tools, list) and tools else 1)
'; then
  die "separate ForgeFleet MCP returned no tools"
fi
unset MCP_TOOLS_RESPONSE MCP_HEALTHY MCP_ATTEMPT
report "mcp-ready" ok "listener healthy with tools available"

# ─── 12. CLI MCP auto-config ─────────────────────────────────────────────
#
# Delegate every client-specific config shape and transport choice to ff's
# typed, idempotent installer. This is the single authority for current Claude
# Code/Desktop, Codex, Gemini, Kimi Code/Desktop, and the other supported
# clients. In particular, do not call `claude mcp add <name> <url>` here: that
# CLI form can interpret the URL as a stdio command instead of a native HTTP
# endpoint. --no-instructions limits bootstrap to MCP config; the shared
# project/user instructions are managed separately.
report "mcp-config" running

if MCP_CONFIG_OUTPUT="$(run_as_user "$USER_HOME/.local/bin/ff" mcp install --for all --no-instructions 2>&1)"; then
  printf '%s\n' "$MCP_CONFIG_OUTPUT"
  # `ff mcp install` continues across per-client errors and reports each one
  # with a cross marker. Surface partial failure in the bootstrap workstream
  # even though the aggregate CLI invocation intentionally exits successfully.
  if grep -Fq '✗' <<<"$MCP_CONFIG_OUTPUT"; then
    MCP_CONFIG_FAILURES="$(grep -Fc '✗' <<<"$MCP_CONFIG_OUTPUT")"
    report "mcp-config" failed "canonical installer reported $MCP_CONFIG_FAILURES client failure(s); inspect bootstrap output"
    die "ff mcp install reported one or more client configuration failures"
  else
    report "mcp-config" ok "canonical ff mcp install --for all completed"
  fi
else
  MCP_CONFIG_RC=$?
  printf '%s\n' "$MCP_CONFIG_OUTPUT" >&2
  report "mcp-config" failed "ff mcp install exited $MCP_CONFIG_RC; inspect bootstrap output"
  die "ff mcp install failed"
fi

# ─── Skills catalog sync (V105) ──────────────────────────────────────────
# Materialize the DB skills catalog onto disk under ~/.forgefleet/skills/
# so the runtime skill_catalog.rs reader has a populated catalog from this
# node's very first session, instead of the operator having to remember to
# run `ff skills sync` by hand after every new-node bootstrap.
report "skills-sync" running
if run_as_user bash -lc 'command -v ff >/dev/null 2>&1'; then
  if run_as_user bash -lc 'ff skills sync 2>&1'; then
    report "skills-sync" ok "materialized skills catalog from DB"
  else
    report "skills-sync" warn "ff skills sync failed — run manually after bootstrap"
  fi
else
  report "skills-sync" warn "ff not on PATH — skipping skills sync"
fi

# ─── Done ────────────────────────────────────────────────────────────────

report "done" ok "$NAME is now a ForgeFleet computer"
FF_COMPLETED=1
say "✓ Onboarding complete: $NAME"
