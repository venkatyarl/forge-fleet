#!/usr/bin/env bash
# ForgeFleet node setup: authenticate the kimi CLI (Moonshot) with credentials
# from environment variables and verify the login succeeded before proceeding.
# Setup FAILS (non-zero exit, no SETUP_NODE_OK) unless kimi authentication is
# confirmed — a node must never report itself as set up without a working,
# authenticated kimi CLI.
set -euo pipefail
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

fail() { echo "!! $*" >&2; exit 1; }

# ---- kimi CLI must be present ----------------------------------------------
# `uv tool install kimi-cli` exposes the binary as `kimi`; some nodes only
# carry the `kimi-cli` / `kimi-legacy` entry points.
KIMI_BIN="$(command -v kimi || command -v kimi-cli || command -v kimi-legacy || true)"
[ -n "$KIMI_BIN" ] || fail "kimi CLI not installed — run scripts/install_uv_kimi.sh first"

# ---- credentials from environment ------------------------------------------
# KIMI_CREDENTIALS_JSON carries the OAuth credential blob (fleet_secrets key
# moonshot.oauth_token.credentials — same materialization as
# scripts/bootstrap-computer-template.sh). KIMI_API_KEY / MOONSHOT_API_KEY are
# the API-key form kimi-cli reads from the environment. Never log the values.
CRED_FILE="$HOME/.kimi/credentials/kimi-code.json"
if [ -n "${KIMI_CREDENTIALS_JSON:-}" ]; then
  mkdir -p "$(dirname "$CRED_FILE")"
  tmp="$(mktemp)"
  printf '%s' "$KIMI_CREDENTIALS_JSON" | python3 -m json.tool > "$tmp" \
    || { rm -f "$tmp"; fail "KIMI_CREDENTIALS_JSON is not valid JSON"; }
  install -m 600 "$tmp" "$CRED_FILE"
  rm -f "$tmp"
fi
if [ -z "${KIMI_API_KEY:-}" ] && [ -n "${MOONSHOT_API_KEY:-}" ]; then
  export KIMI_API_KEY="$MOONSHOT_API_KEY"
fi
if [ -z "${KIMI_API_KEY:-}" ] && [ ! -s "$CRED_FILE" ]; then
  fail "no kimi credentials: set KIMI_CREDENTIALS_JSON or KIMI_API_KEY/MOONSHOT_API_KEY (or pre-provision $CRED_FILE)"
fi

# ---- authenticate and verify ------------------------------------------------
echo ">> authenticating kimi CLI ($KIMI_BIN)"
# stdin closed + timeout so an unauthorized headless node fails instead of
# hanging forever on the OAuth device prompt.
timeout 120 "$KIMI_BIN" login </dev/null \
  || fail "kimi login failed (exit $?) — aborting node setup"
[ -s "$CRED_FILE" ] || fail "kimi login exited 0 but $CRED_FILE is missing/empty"
echo "KIMI_AUTH_OK"

echo "SETUP_NODE_OK"
