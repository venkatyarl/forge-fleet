#!/usr/bin/env bash
# Fleet-wide ForgeFleet deployment script
# ========================================
# Builds release binary and deploys to all reachable fleet nodes.
#
# Node discovery is delegated to scripts/lib/fleet.sh (canonical resolver).
# Usage:
#   ./scripts/deploy-to-fleet.sh
#   ./scripts/deploy-to-fleet.sh --dry-run
#   ./scripts/deploy-to-fleet.sh --nodes n1,n2,n3

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
source "$SCRIPT_DIR/lib/fleet.sh"

BINARY_NAME="forgefleetd"
INSTALL_DIR=".local/bin"
SSH_USER="${FORGEFLEET_SSH_USER:-venkat}"
SSH_OPTS="-o ConnectTimeout=5 -o StrictHostKeyChecking=no -o BatchMode=yes"
DRY_RUN=false
NODES=""

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info()  { echo -e "${BLUE}[deploy]${NC}  $*"; }
ok()    { echo -e "${GREEN}[deploy]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[deploy]${NC}  $*"; }
err()   { echo -e "${RED}[deploy]${NC}   $*" >&2; }

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --dry-run          Show what would be deployed without doing it"
    echo "  --nodes n1,n2,...  Deploy only to specified nodes (IPs or hostnames)"
    echo "  --help             Show this help"
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --nodes)
            NODES="$2"
            shift 2
            ;;
        --help|-h)
            usage
            ;;
        *)
            err "Unknown option: $1"
            usage
            ;;
    esac
done

# ─── Build release binary ────────────────────────────────────────────────────

info "Building release binary..."
if [[ "$DRY_RUN" == "false" ]]; then
    cd "$PROJECT_ROOT"
    cargo build --release --bin "$BINARY_NAME" 2>&1 | tail -5
fi

BINARY_PATH="$PROJECT_ROOT/target/release/$BINARY_NAME"
if [[ ! -f "$BINARY_PATH" ]]; then
    err "Binary not found: $BINARY_PATH"
    exit 1
fi

BINARY_SIZE=$(du -h "$BINARY_PATH" | cut -f1)
ok "Binary ready: $BINARY_PATH ($BINARY_SIZE)"

# ─── Discover fleet nodes ────────────────────────────────────────────────────

NODE_LIST=()
if [[ -n "$NODES" ]]; then
    IFS=',' read -ra NODE_LIST <<< "$NODES"
else
    while IFS='|' read -r name ip user os role; do
        [[ -n "$ip" ]] && NODE_LIST+=("$ip")
    done < <(discover_fleet_computers)
fi

if [[ ${#NODE_LIST[@]} -eq 0 ]]; then
    warn "No fleet nodes discovered. Use --nodes to specify targets."
    exit 0
fi

info "Deploying to ${#NODE_LIST[@]} node(s)"

# ─── Deploy to each node ─────────────────────────────────────────────────────

SUCCESS=0
FAILED=0
SKIPPED=0

for node in "${NODE_LIST[@]}"; do
    echo ""
    info "[$node] Starting deployment..."

    # Skip local node
    if [[ "$node" == "127.0.0.1" || "$node" == "192.168.5.100" ]]; then
        warn "[$node] Skipping local leader node (deploy manually)"
        ((SKIPPED++)) || true
        continue
    fi

    # Check SSH connectivity
    if ! ssh $SSH_OPTS "$SSH_USER@$node" "echo ok" >/dev/null 2>&1; then
        warn "[$node] SSH unreachable — skipping"
        ((SKIPPED++)) || true
        continue
    fi
    ok "[$node] SSH reachable"

    if [[ "$DRY_RUN" == "true" ]]; then
        info "[$node] Would copy $BINARY_NAME ($BINARY_SIZE) to $INSTALL_DIR"
        info "[$node] Would run: sudo systemctl restart forgefleet || launchctl restart com.forgefleet.forgefleetd"
        continue
    fi

    # Copy binary
    if scp $SSH_OPTS "$BINARY_PATH" "$SSH_USER@$node:/tmp/$BINARY_NAME" >/dev/null 2>&1; then
        ok "[$node] Binary copied to /tmp"
    else
        err "[$node] SCP failed — skipping"
        ((FAILED++)) || true
        continue
    fi

    # Install and restart
    if ssh $SSH_OPTS "$SSH_USER@$node" "
        mkdir -p ~/$INSTALL_DIR && \
        install -m 755 /tmp/$BINARY_NAME ~/$INSTALL_DIR/$BINARY_NAME && \
        rm /tmp/$BINARY_NAME && \
        (sudo systemctl restart forgefleet 2>/dev/null || \
         launchctl kickstart -k gui/\$(id -u)/com.forgefleet.forgefleetd 2>/dev/null || \
         echo 'manual restart needed') && \
        echo 'ok'
    " >/dev/null 2>&1; then
        ok "[$node] Installed and restarted"
        ((SUCCESS++)) || true
    else
        err "[$node] Install/restart failed"
        ((FAILED++)) || true
    fi
done

# ─── Summary ─────────────────────────────────────────────────────────────────

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
info "Deployment complete"
echo "  Success: $SUCCESS"
echo "  Failed:  $FAILED"
echo "  Skipped: $SKIPPED"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [[ $FAILED -gt 0 ]]; then
    exit 1
fi
