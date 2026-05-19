#!/usr/bin/env bash
# lint_hardcoded_ports.sh — fail-on-find for 4-digit hardcoded ports in
# Rust sources. The convention: every ForgeFleet-owned port is 5-digit
# and registered in port_registry. The only acceptable 4-digit hits are:
#
#   - External tools we don't control on the host port AT THE INTERNAL
#     CONTAINER LEVEL ONLY (e.g. NATS internal 4222 inside its container's
#     own config). After Phase B, those should NOT appear in Rust source
#     either — Rust code reads the host-side mapping (5-digit).
#   - Vite dev server (5173) — frontend tooling, external to fleet.
#   - Test fixtures on 192.168.1.x — different subnet, clearly not real.
#   - Doc-comment examples that mention historical behaviour.
#
# Anything else is a bug — see commit 2026-05-18 fixing the :4000 collision
# with Obsidian.
#
# Run: scripts/lint_hardcoded_ports.sh
# Exit 0 = clean. Exit 1 = violators found (paths printed).

set -u

VIOLATIONS_FILE=$(mktemp)
trap 'rm -f "$VIOLATIONS_FILE"' EXIT

# Match http(s)://<host>:NNNN where NNNN is exactly 4 digits.
# `host` can be IP, localhost, 127.0.0.1, etc.
# Also catches `redis://`, `nats://`, `postgres://` schemes.
PATTERN='(https?|redis|nats|postgres)://[A-Za-z0-9.-]+:[0-9]{4}\b'

# Allowlist regex — any line that matches ANY of these is OK to ignore.
# Keep this list tight; every entry needs a stated reason in this file.
ALLOWLIST_REGEX='(5173|192\.168\.1\.|172\.[0-9]+\.[0-9]+\.[0-9]+|11434|26380|bad-node:8080|localhost:8080|localhost:3000|localhost:3100|127\.0\.0\.1:5000)'
# Allowlist entries:
#   5173          — Vite frontend dev server (external tool)
#   192.168.1.*   — test/example fixtures on a different subnet
#   172.*         — docker bridge network references
#   11434         — ollama default (5 digits actually — kept on allowlist for
#                   explicitness in case anything matches the bare number)
#   26380         — Redis Sentinel deprecated entry (5 digits — kept for
#                   explicitness)
#   bad-node:8080  — test fixture in ff-gateway::orchestrate; intentionally
#                    a fake unreachable host for negative-path tests.
#   localhost:8080 — example URLs in ff-benchmark + ff-pipeline test templates
#                    (representative HTTP service in deploy_pipeline examples).
#   localhost:3000 — ff-skills test default MCP endpoint.
#   localhost:3100 — example MCP server URL in ff-agent::mcp_tools doc + test
#                    (third-party MCP convention, not a ForgeFleet port).
#   127.0.0.1:5000 — ff-mcp federation normalize_endpoint test input.

scan_dir() {
    local dir="$1"
    while IFS= read -r line; do
        # Strip leading filename:line# (grep -n format) for allowlist check.
        # But keep the full line in the violation log.
        if echo "$line" | grep -qE "$ALLOWLIST_REGEX"; then
            continue
        fi
        # Filter doc-comment "examples" — `/// - http://...:NNNN` patterns.
        # The grep -n line format is `file:lineno:CONTENT`, so the code
        # portion lives after the second `:`. Strip that prefix before
        # the doc-comment + module-doc + plain-comment checks.
        content="${line#*:*:}"
        if echo "$content" | grep -qE '^\s*(///|//!|//)'; then
            continue
        fi
        echo "$line" >> "$VIOLATIONS_FILE"
    done < <(grep -rnE "$PATTERN" "$dir" 2>/dev/null)
}

scan_dir crates/

if [ -s "$VIOLATIONS_FILE" ]; then
    echo "✗ Hardcoded 4-digit ports found in Rust source:"
    echo "  (canonical ForgeFleet ports must be 5 digits and registered in port_registry)"
    echo
    cat "$VIOLATIONS_FILE"
    echo
    echo "Fixes:"
    echo "  - If this is a ForgeFleet service, use a 5-digit port registered via a schema migration."
    echo "  - If it's an external tool default, add to ALLOWLIST_REGEX with a reason in this script."
    echo "  - If it's documentation, move the example to a /// doc-comment."
    exit 1
fi

echo "✓ no 4-digit hardcoded ports in Rust source (allowlist applied)"
exit 0
