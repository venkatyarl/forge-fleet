#!/usr/bin/env bash
# Centralized fleet computer discovery library.
# ═══════════════════════════════════════════════════════════════════════════════
#
# Source this file in any script that needs fleet computer information:
#   source "$(dirname "$0")/../lib/fleet.sh"
#
# All fleet-related scripts should use this library instead of implementing
# their own discovery. The canonical resolution chain is:
#   1. Postgres fleet_workers table  (via `ff fleet computers --format json`)
#   2. fleet.toml [nodes.*]          (fallback)
#   3. ~/.ssh/config                 (fallback)
#   4. ~/.forgefleet/fleet.json      (last resort)
#
# Provided functions:
#   discover_fleet_computers      → outputs "name|ip|ssh_user" lines
#   discover_fleet_computers_json → outputs JSON array
#   discover_fleet_ips            → outputs IPs only (for simple SCP/SSH loops)
#   fleet_computers_linux         → filters to Linux only
#   fleet_computers_exclude_macos → filters out macOS
#
# Legacy aliases (discover_fleet_nodes, fleet_nodes_linux, etc.) remain
# during the node→computer rename window — they delegate to the new names.

set -euo pipefail

# ─── Configuration ───────────────────────────────────────────────────────────

FORGEFLEET_HOME="${FORGEFLEET_HOME:-$HOME/.forgefleet}"
FLEET_TOML="${FORGEFLEET_HOME}/fleet.toml"
SSH_CONFIG="${HOME}/.ssh/config"
FLEET_JSON="${FORGEFLEET_HOME}/fleet.json"

# ─── Internal: try the canonical Rust resolver ───────────────────────────────

_ff_fleet_nodes_json() {
    # Try the compiled `ff` CLI first — this is the canonical resolver that
    # uses the same code path as the daemon (Postgres → config → SSH → JSON).
    # Try the new `computers` verb first, then fall back to the legacy `nodes`
    # name so mixed-fleet upgrade windows still resolve.
    if command -v ff >/dev/null 2>&1; then
        ff fleet computers --format json 2>/dev/null && return 0
        ff fleet nodes --format json 2>/dev/null && return 0
    fi
    # Also try local dev build
    local local_ff
    local_ff="$(dirname "${BASH_SOURCE[0]}")/../../target/release/ff"
    if [[ -x "$local_ff" ]]; then
        "$local_ff" fleet computers --format json 2>/dev/null && return 0
        "$local_ff" fleet nodes --format json 2>/dev/null && return 0
    fi
    return 1
}

# ─── Internal: fallback Python+shell resolver ────────────────────────────────

_discover_from_postgres() {
    if ! command -v python3 >/dev/null 2>&1; then
        return 1
    fi
    local pgurl="${PGURL:-postgresql://forgefleet:forgefleet@192.168.5.100:55432/forgefleet}"
    python3 -c "
import psycopg2, os, sys
try:
    conn = psycopg2.connect(os.environ.get('PGURL','$pgurl'))
    cur = conn.cursor()
    cur.execute('SELECT name, ip, ssh_user, os, role FROM fleet_workers ORDER BY election_priority, name')
    for r in cur.fetchall():
        print(f'{r[0]}|{r[1]}|{r[2]}|{r[3]}|{r[4]}')
except Exception as e:
    sys.exit(1)
" 2>/dev/null
}

_discover_from_fleet_toml() {
    if ! command -v python3 >/dev/null 2>&1 || [[ ! -f "$FLEET_TOML" ]]; then
        return 1
    fi
    python3 -c "
import toml, sys
try:
    cfg = toml.load('$FLEET_TOML')
    for name, node in cfg.get('nodes', {}).items():
        ip = node.get('ip', node.get('host', ''))
        user = node.get('ssh_user', 'venkat')
        os_name = node.get('os', '')
        role = node.get('role', '')
        if ip:
            print(f'{name}|{ip}|{user}|{os_name}|{role}')
except Exception:
    sys.exit(1)
" 2>/dev/null
}

_discover_from_ssh_config() {
    if [[ ! -f "$SSH_CONFIG" ]]; then
        return 1
    fi
    awk '
    /^Host[ \t]+/ {
        host = $2
        hostname = ""
        user = "venkat"
    }
    /[ \t]*HostName[ \t]+/ { hostname = $2 }
    /[ \t]*User[ \t]+/ { user = $2 }
    host && hostname && host !~ /\*/ && host !~ /^github/ {
        print host "|" hostname "|" user "||"
        host = ""
    }
    ' "$SSH_CONFIG" 2>/dev/null
}

_discover_from_fleet_json() {
    if ! command -v python3 >/dev/null 2>&1 || [[ ! -f "$FLEET_JSON" ]]; then
        return 1
    fi
    python3 -c "
import json, sys
try:
    with open('$FLEET_JSON') as f:
        data = json.load(f)
    for n in data.get('nodes', []):
        name = n.get('name','')
        ip = n.get('ip','')
        user = n.get('ssh_user','venkat')
        os_name = n.get('os','')
        role = n.get('role','')
        if name and ip:
            print(f'{name}|{ip}|{user}|{os_name}|{role}')
except Exception:
    sys.exit(1)
" 2>/dev/null
}

# ─── Internal: resolve with fallbacks ────────────────────────────────────────

_resolve_fleet_nodes() {
    local output=""

    # 1. Canonical Rust resolver (preferred)
    if output=$(_ff_fleet_nodes_json 2>/dev/null) && [[ -n "$output" ]]; then
        echo "$output" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for n in data:
    print(f\"{n['name']}|{n['ip']}|{n.get('ssh_user','venkat')}|{n.get('os','')}|{n.get('role','')}\")
" 2>/dev/null
        return 0
    fi

    # 2. Postgres direct query
    if output=$(_discover_from_postgres 2>/dev/null) && [[ -n "$output" ]]; then
        echo "$output"
        return 0
    fi

    # 3. fleet.toml
    if output=$(_discover_from_fleet_toml 2>/dev/null) && [[ -n "$output" ]]; then
        echo "$output"
        return 0
    fi

    # 4. ~/.ssh/config
    if output=$(_discover_from_ssh_config 2>/dev/null) && [[ -n "$output" ]]; then
        echo "$output"
        return 0
    fi

    # 5. ~/.forgefleet/fleet.json
    if output=$(_discover_from_fleet_json 2>/dev/null) && [[ -n "$output" ]]; then
        echo "$output"
        return 0
    fi

    return 1
}

# ─── Public API ──────────────────────────────────────────────────────────────

# Discover fleet computers and output one "name|ip|ssh_user|os|role" line each.
# Usage: while IFS='|' read -r name ip user os role; do ...; done < <(discover_fleet_computers)
discover_fleet_computers() {
    _resolve_fleet_nodes
}

# Legacy alias retained during the node→computer rename window.
discover_fleet_nodes() {
    discover_fleet_computers
}

# Discover fleet computers as a JSON array (delegates to `ff fleet computers --format json`).
# Falls back to inline JSON generation if `ff` is not available.
discover_fleet_computers_json() {
    if output=$(_ff_fleet_nodes_json 2>/dev/null) && [[ -n "$output" ]]; then
        echo "$output"
        return 0
    fi

    # Fallback: build JSON from the pipe-delimited output
    local lines=()
    while IFS= read -r line; do
        lines+=("$line")
    done < <(_resolve_fleet_nodes)

    if [[ ${#lines[@]} -eq 0 ]]; then
        echo "[]"
        return 1
    fi

    python3 -c "
import json
lines = $(printf '%s\n' "${lines[@]}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')
out = []
for line in lines:
    parts = line.split('|')
    out.append({
        'name': parts[0] if len(parts) > 0 else '',
        'ip': parts[1] if len(parts) > 1 else '',
        'ssh_user': parts[2] if len(parts) > 2 else 'venkat',
        'os': parts[3] if len(parts) > 3 else '',
        'role': parts[4] if len(parts) > 4 else '',
    })
print(json.dumps(out, indent=2))
" 2>/dev/null
}

# Output IPs only (one per line), useful for simple SSH/SCP loops.
discover_fleet_ips() {
    discover_fleet_computers | awk -F'|' '{print $2}'
}

# Legacy alias for discover_fleet_computers_json.
discover_fleet_nodes_json() {
    discover_fleet_computers_json
}

# Filter fleet computers to Linux only (excludes macOS and empty OS).
fleet_computers_linux() {
    discover_fleet_computers | awk -F'|' '
        BEGIN { IGNORECASE=1 }
        $4 ~ /linux/ { print }
    '
}

# Legacy alias retained during the rename window.
fleet_nodes_linux() {
    fleet_computers_linux
}

# Filter fleet computers excluding macOS.
fleet_computers_exclude_macos() {
    discover_fleet_computers | awk -F'|' '
        BEGIN { IGNORECASE=1 }
        $4 !~ /macos|darwin/ { print }
    '
}

# Legacy alias.
fleet_nodes_exclude_macos() {
    fleet_computers_exclude_macos
}

# Count fleet computers.
fleet_computer_count() {
    discover_fleet_computers | wc -l | tr -d ' '
}

# Legacy alias.
fleet_node_count() {
    fleet_computer_count
}
