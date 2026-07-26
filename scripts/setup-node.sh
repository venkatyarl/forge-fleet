#!/usr/bin/env bash
# Node setup hook for Kimi CLI authentication.
set -euo pipefail

export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

log() {
  printf '>> %s\n' "$*" >&2
}

die() {
  printf '!! %s\n' "$*" >&2
  exit 1
}

has_kimi_credentials() {
  [ -n "${KIMI_LOGIN_STDIN:-}" ] ||
    [ -n "${KIMI_API_KEY:-}" ] ||
    [ -n "${KIMI_MODEL_API_KEY:-}" ] ||
    [ -n "${MOONSHOT_API_KEY:-}" ]
}

run_with_timeout() {
  local secs=$1
  shift

  if command -v timeout >/dev/null 2>&1; then
    timeout "$secs" "$@"
    return
  fi

  perl -e '
    my $secs = shift @ARGV;
    local $SIG{ALRM} = sub { die "TIMEOUT\n" };
    alarm $secs;
    exec @ARGV;
  ' "$secs" "$@"
}

ensure_kimi() {
  if command -v kimi >/dev/null 2>&1; then
    command -v kimi
    return
  fi

  log "kimi missing; installing kimi-cli"
  if ! command -v uv >/dev/null 2>&1; then
    if command -v curl >/dev/null 2>&1; then
      curl -LsSf https://astral.sh/uv/install.sh | sh
    elif command -v wget >/dev/null 2>&1; then
      wget -qO- https://astral.sh/uv/install.sh | sh
    else
      python3 -m pip install --user --break-system-packages uv >/dev/null
    fi
    export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
  fi

  uv tool install kimi-cli --force >/dev/null
  command -v kimi || die "kimi install completed but kimi is not on PATH"
}

kimi_login_input() {
  if [ -n "${KIMI_LOGIN_STDIN:-}" ]; then
    printf '%s\n' "$KIMI_LOGIN_STDIN"
    return
  fi

  local api_key="${KIMI_API_KEY:-${KIMI_MODEL_API_KEY:-${MOONSHOT_API_KEY:-}}}"
  [ -n "$api_key" ] || die "Kimi credentials were expected but no API key was set"

  # The Kimi login wizard is interactive. Operators can override the selector
  # values when the installed CLI changes its menu order.
  printf '%s\n%s\n%s\n' \
    "${KIMI_LOGIN_PLATFORM:-3}" \
    "$api_key" \
    "${KIMI_LOGIN_MODEL:-${KIMI_MODEL_NAME:-kimi-k2-0711-preview}}"
}

authenticate_kimi() {
  local kimi_bin=$1

  log "running kimi login from environment credentials"
  kimi_login_input | run_with_timeout "${KIMI_LOGIN_TIMEOUT_SECS:-120}" "$kimi_bin" login
}

verify_kimi_auth() {
  local kimi_bin=$1
  local raw

  log "verifying kimi authentication"
  raw="$(run_with_timeout "${KIMI_VERIFY_TIMEOUT_SECS:-120}" \
    "$kimi_bin" -p "Reply with only the word: PONG" 2>&1 || true)"

  if printf '%s' "$raw" | grep -q 'PONG'; then
    log "kimi authentication verified"
    return
  fi

  printf '%s\n' "$raw" >&2
  die "kimi authentication verification failed"
}

main() {
  if ! has_kimi_credentials; then
    log "no Kimi credentials in environment; skipping kimi login"
    return
  fi

  local kimi_bin
  kimi_bin="$(ensure_kimi)"
  authenticate_kimi "$kimi_bin"
  verify_kimi_auth "$kimi_bin"
}

main "$@"
