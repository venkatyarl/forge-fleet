#!/usr/bin/env bash
set -eo pipefail

# Fleet bring-up preflight checker (read-only / safe).
# - Verifies local artifacts and config prerequisites
# - Verifies required worker nodes exist in fleet.toml or Postgres
# - Optionally verifies SSH reachability + disk free on each node

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONFIG_PATH="${FORGEFLEET_CONFIG:-$HOME/.forgefleet/fleet.toml}"
SKIP_SSH=0

RELEASE_BIN="${REPO_ROOT}/target/release/forgefleetd"
REQUIRED_NODES=(james marcus sophie priya ace)

usage() {
  cat <<'EOF'
Usage: tools/fleet_preflight.sh [--config <path>] [--skip-ssh]

Options:
  --config <path>   Path to fleet.toml (default: $FORGEFLEET_CONFIG or ~/.forgefleet/fleet.toml)
  --skip-ssh        Skip SSH/disk checks (local checks only)
  -h, --help        Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)
      CONFIG_PATH="$2"
      shift 2
      ;;
    --skip-ssh)
      SKIP_SSH=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

ok() { printf "[ok] %s\n" "$*"; }
warn() { printf "[warn] %s\n" "$*"; }
fail() { printf "[fail] %s\n" "$*"; }

require_cmd() {
  local cmd="$1"
  if command -v "$cmd" >/dev/null 2>&1; then
    ok "command available: $cmd"
  else
    fail "missing required command: $cmd"
    return 1
  fi
}

FAILURES=0

echo "== ForgeFleet preflight =="
echo "repo:   ${REPO_ROOT}"
echo "config: ${CONFIG_PATH}"
echo

# 1) Local tooling checks
for cmd in python3 git ssh curl; do
  if ! require_cmd "$cmd"; then
    FAILURES=$((FAILURES + 1))
  fi
done

# cargo is only mandatory if no prebuilt release binary exists
if command -v cargo >/dev/null 2>&1; then
  ok "command available: cargo"
else
  if [[ -x "$RELEASE_BIN" ]]; then
    warn "cargo missing, but prebuilt binary exists: $RELEASE_BIN"
  else
    fail "missing required command: cargo (or provide prebuilt $RELEASE_BIN)"
    FAILURES=$((FAILURES + 1))
  fi
fi

echo
# 2) Local file/artifact checks
for path in \
  "${CONFIG_PATH}" \
  "${REPO_ROOT}/deploy/install.sh" \
  "${REPO_ROOT}/deploy/linux/forgefleet.service" \
  "${REPO_ROOT}/deploy/macos/com.forgefleet.daemon.plist"; do
  if [[ -f "$path" ]]; then
    ok "found: $path"
  else
    fail "missing: $path"
    FAILURES=$((FAILURES + 1))
  fi
done

if [[ -x "$RELEASE_BIN" ]]; then
  ok "release binary present: $RELEASE_BIN"
else
  warn "release binary missing: $RELEASE_BIN (run cargo build --release --bin forgefleetd)"
fi

echo
# 3) Parse node records from fleet.toml (fallback: query Postgres)
API_PORT=""
NODE_LINES=()

PARSED="$({ python3 - "$CONFIG_PATH" <<'PY'
import sys, tomllib
from pathlib import Path

config_path = Path(sys.argv[1]).expanduser()
with config_path.open('rb') as f:
    cfg = tomllib.load(f)

api_port = int(cfg.get('general', {}).get('api_port', 51800))
print(f"__API_PORT__\t{api_port}")

nodes = cfg.get('nodes', {})
for name, node in nodes.items():
    ip = node.get('ip', '')
    user = node.get('ssh_user', '')
    os_name = node.get('os', '')
    models = node.get('models', {})
    model_port = ''
    if isinstance(models, dict) and models:
        for _model_slug, model_cfg in models.items():
            if isinstance(model_cfg, dict) and 'port' in model_cfg:
                model_port = str(model_cfg.get('port', ''))
                break
    print(f"{name}\t{ip}\t{user}\t{os_name}\t{model_port}")
PY
} 2>/dev/null)" || PARSED=""

if [[ -n "$PARSED" ]]; then
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    IFS=$'\t' read -r col1 col2 _rest <<< "$line"
    if [[ "$col1" == "__API_PORT__" ]]; then
      API_PORT="$col2"
    else
      NODE_LINES+=("$line")
    fi
  done <<< "$PARSED"
fi

# Fallback: query Postgres when fleet.toml has no nodes (DB-first inventory)
if [[ ${#NODE_LINES[@]} -eq 0 ]]; then
  PG_NODES="$({ python3 - "$CONFIG_PATH" <<'PY'
import sys, tomllib
from pathlib import Path
from urllib.parse import urlparse

config_path = Path(sys.argv[1]).expanduser()
with config_path.open('rb') as f:
    cfg = tomllib.load(f)

db = cfg.get('database', {})
url = db.get('url', '')
if not url:
    exit(0)

u = urlparse(url)
host = u.hostname or 'localhost'
port = u.port or 5432
user = u.username or ''
password = u.password or ''
dbname = u.path.lstrip('/') or 'forgefleet'

try:
    import psycopg2
    conn = psycopg2.connect(host=host, port=port, dbname=dbname, user=user, password=password)
    cur = conn.cursor()
    cur.execute("SELECT name, ip_address, os_family, model_port FROM computers WHERE status != 'offline' ORDER BY name")
    for row in cur.fetchall():
        name, ip, osf, mport = row
        print(f"{name}\t{ip or ''}\t{name}\t{osf or ''}\t{mport or ''}")
    conn.close()
except Exception:
    exit(0)
PY
} 2>/dev/null)"

  if [[ -n "$PG_NODES" ]]; then
    while IFS= read -r line; do
      [[ -z "$line" ]] && continue
      NODE_LINES+=("$line")
    done <<< "$PG_NODES"
  fi
fi

if [[ -z "$API_PORT" ]]; then
  API_PORT="51800"
fi
ok "fleet api_port: ${API_PORT}"

find_node_line() {
  local wanted="$1"
  local line node_name
  local -a node_lines_safe=()
  if [[ ${#NODE_LINES[@]} -gt 0 ]]; then
    node_lines_safe=("${NODE_LINES[@]}")
  fi
  for line in "${node_lines_safe[@]}"; do
    node_name="${line%%$'	'*}"
    if [[ "$node_name" == "$wanted" ]]; then
      printf "%s\n" "$line"
      return 0
    fi
  done
  return 1
}

echo
for node in "${REQUIRED_NODES[@]}"; do
  line="$(find_node_line "$node" || true)"
  if [[ -n "$line" ]]; then
    IFS=$'\t' read -r _name ip user os_name model_port <<< "$line"
    ok "node in config: ${node} (${user}@${ip}, os=${os_name:-unknown}, model_port=${model_port:-n/a})"
  else
    warn "required node missing in config or DB: ${node}"
  fi
done

if [[ "$SKIP_SSH" -eq 1 ]]; then
  warn "skipping SSH checks (--skip-ssh)"
else
  echo
  echo "== SSH reachability + disk checks =="
  for node in "${REQUIRED_NODES[@]}"; do
    line="$(find_node_line "$node" || true)"
    [[ -z "$line" ]] && continue

    IFS=$'\t' read -r _name ip user os_name model_port <<< "$line"
    target="${user}@${ip}"

    if ssh -o BatchMode=yes -o ConnectTimeout=6 "$target" "echo ok" >/dev/null 2>&1; then
      ok "ssh reachable: ${target}"

      disk_line="$(ssh -o BatchMode=yes -o ConnectTimeout=6 "$target" "df -h / | tail -n 1" 2>/dev/null || true)"
      if [[ -n "$disk_line" ]]; then
        ok "${node} disk: ${disk_line}"
      else
        warn "${node}: could not read disk usage"
      fi
    else
      fail "ssh unreachable: ${target}"
      FAILURES=$((FAILURES + 1))
    fi
  done
fi

echo
if [[ "$FAILURES" -eq 0 ]]; then
  ok "preflight passed"
  exit 0
fi

fail "preflight failed (${FAILURES} issue(s))"
exit 1
