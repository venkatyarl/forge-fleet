#!/bin/bash
# External fleet alerter — runs on Taylor OUTSIDE forgefleetd.
#
# The in-daemon alert_evaluator dies when forgefleetd dies. This script
# queries Postgres directly for stale computer.last_seen_at and pages via
# Telegram if any member's beat is too old. It only depends on:
#   * Postgres container (forgefleet-postgres) being up on localhost:55432
#   * `curl` for the Telegram webhook
# so it survives forgefleetd death — which is exactly the scenario where
# the in-daemon alerter fails silently.
#
# Install as a launchctl agent:
#   cp deploy/launchd/com.forgefleet.external-alerter.plist ~/Library/LaunchAgents/
#   launchctl load ~/Library/LaunchAgents/com.forgefleet.external-alerter.plist
#
# Manual run:
#   FORGEFLEET_STALE_THRESHOLD_SECS=1800 ./scripts/external-alerter.sh

set -euo pipefail

PGCONTAINER="${FORGEFLEET_PG_CONTAINER:-forgefleet-postgres}"
STALE_WARN_SECS="${FORGEFLEET_STALE_WARN_SECS:-300}"      # 5 min
STALE_CRIT_SECS="${FORGEFLEET_STALE_CRIT_SECS:-1800}"     # 30 min
STATE_DIR="${FORGEFLEET_ALERTER_STATE:-$HOME/.forgefleet/state/external-alerter}"
COOLDOWN_SECS="${FORGEFLEET_ALERT_COOLDOWN:-600}"         # don't re-page for 10 min

mkdir -p "$STATE_DIR"

# Secrets come from fleet_secrets table (falls back to env for bootstrap).
fetch_secret() {
  local key="$1"
  docker exec -i "$PGCONTAINER" psql -U forgefleet forgefleet -tA \
    -c "SELECT value FROM fleet_secrets WHERE key='$key' LIMIT 1" 2>/dev/null \
    | head -1 | tr -d '\r'
}

TELEGRAM_BOT_TOKEN="${TELEGRAM_BOT_TOKEN:-$(fetch_secret telegram.bot_token)}"
TELEGRAM_CHAT_ID="${TELEGRAM_CHAT_ID:-$(fetch_secret telegram.chat_id)}"

page_telegram() {
  local severity="$1"
  local text="$2"
  if [ -z "${TELEGRAM_BOT_TOKEN:-}" ] || [ -z "${TELEGRAM_CHAT_ID:-}" ]; then
    echo "[$(date -u +%FT%TZ)] no telegram creds; would page: [$severity] $text" >&2
    return 0
  fi
  curl -fsS --max-time 10 \
    "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendMessage" \
    -d "chat_id=${TELEGRAM_CHAT_ID}" \
    -d "text=[ff external-alerter / ${severity}] ${text}" \
    >/dev/null || echo "[$(date -u +%FT%TZ)] telegram webhook failed" >&2
}

# Cooldown: one file per (computer,severity). Touch when we page; skip if
# mtime is within COOLDOWN_SECS.
on_cooldown() {
  local key="$1"
  local file="$STATE_DIR/${key//\//_}.lastpaged"
  [ -f "$file" ] || return 1
  local last=$(stat -f %m "$file" 2>/dev/null || stat -c %Y "$file" 2>/dev/null || echo 0)
  local now=$(date +%s)
  (( now - last < COOLDOWN_SECS ))
}
mark_paged() {
  local key="$1"
  touch "$STATE_DIR/${key//\//_}.lastpaged"
}

# Pull every computer with a last_seen_at; compute age in seconds. Exclude
# status='offline' (intentional sleep/power-off — operator knows).
ROWS=$(docker exec -i "$PGCONTAINER" psql -U forgefleet forgefleet -tA -F '|' -c "
  SELECT name,
         COALESCE(EXTRACT(EPOCH FROM (NOW() - last_seen_at))::BIGINT, 999999),
         COALESCE(status, 'unknown')
  FROM computers
  WHERE status NOT IN ('offline','decommissioned')
  ORDER BY name
")

echo "[$(date -u +%FT%TZ)] checking $(echo "$ROWS" | grep -c . || echo 0) computers"

while IFS='|' read -r NAME AGE STATUS; do
  [ -z "$NAME" ] && continue
  if [ "$AGE" -gt "$STALE_CRIT_SECS" ]; then
    KEY="$NAME/critical"
    if ! on_cooldown "$KEY"; then
      page_telegram critical "$NAME silent for $((AGE/60)) min (status=$STATUS, threshold=${STALE_CRIT_SECS}s)"
      mark_paged "$KEY"
    fi
  elif [ "$AGE" -gt "$STALE_WARN_SECS" ]; then
    KEY="$NAME/warning"
    if ! on_cooldown "$KEY"; then
      page_telegram warning "$NAME beat stale ${AGE}s (status=$STATUS, threshold=${STALE_WARN_SECS}s)"
      mark_paged "$KEY"
    fi
  fi
done <<< "$ROWS"

# Bonus: alert if forgefleetd itself is dead on Taylor (this script runs
# here, so a local check is cheap and catches "daemon died but we didn't
# notice for N minutes" — the exact 9h DGX outage scenario in reverse).
if ! pgrep -f 'forgefleetd.*start' >/dev/null 2>&1; then
  KEY="taylor/daemon_dead"
  if ! on_cooldown "$KEY"; then
    page_telegram critical "forgefleetd is NOT running on taylor — no process matches 'forgefleetd.*start'"
    mark_paged "$KEY"
  fi
fi
