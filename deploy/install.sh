#!/usr/bin/env bash
# ForgeFleet Installation Script
# ===============================
# Detects OS, installs binary, sets up directories, and configures
# process supervision (launchctl on macOS, systemd on Linux).
#
# Usage:
#   ./install.sh                    # Install from local build
#   ./install.sh /path/to/binary    # Install specific binary
#   ./install.sh --uninstall        # Remove everything
#
# Requirements:
#   macOS: No special requirements
#   Linux: systemd

set -euo pipefail

# ─── Configuration ────────────────────────────────────────────────────────────

BINARY_NAME="forgefleetd"
INSTALL_DIR="/usr/local/bin"
HOME_DIR="${FORGEFLEET_HOME:-$HOME/.forgefleet}"
LOG_DIR="${HOME_DIR}/logs"
CONFIG_DIR="${HOME_DIR}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# ─── Helpers ──────────────────────────────────────────────────────────────────

info()  { echo -e "${BLUE}[info]${NC}  $*"; }
ok()    { echo -e "${GREEN}[ok]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[warn]${NC}  $*"; }
err()   { echo -e "${RED}[error]${NC} $*" >&2; }
die()   { err "$@"; exit 1; }

detect_os() {
    case "$(uname -s)" in
        Darwin) echo "macos" ;;
        Linux)  echo "linux" ;;
        *)      die "Unsupported OS: $(uname -s). ForgeFleet supports macOS and Linux." ;;
    esac
}

# Find the binary — either from argument, local build, or PATH
find_binary() {
    local binary="${1:-}"

    if [[ -n "$binary" && -f "$binary" ]]; then
        echo "$binary"
        return
    fi

    # Check local cargo build
    local cargo_bin="./target/release/${BINARY_NAME}"
    if [[ -f "$cargo_bin" ]]; then
        echo "$cargo_bin"
        return
    fi

    local cargo_debug="./target/debug/${BINARY_NAME}"
    if [[ -f "$cargo_debug" ]]; then
        warn "Using debug build — consider 'cargo build --release' for production"
        echo "$cargo_debug"
        return
    fi

    die "No ${BINARY_NAME} binary found. Build with 'cargo build --release' first."
}

# ─── Directory Setup ─────────────────────────────────────────────────────────

setup_directories() {
    info "Creating ForgeFleet directories..."
    mkdir -p "${HOME_DIR}"
    mkdir -p "${LOG_DIR}"
    mkdir -p "${CONFIG_DIR}"
    ok "Directories created: ${HOME_DIR}"
}

# ─── Binary Installation ─────────────────────────────────────────────────────

install_binary() {
    local src="$1"
    local dest="${INSTALL_DIR}/${BINARY_NAME}"

    info "Installing ${BINARY_NAME} to ${INSTALL_DIR}/"

    if [[ -w "${INSTALL_DIR}" ]]; then
        cp "$src" "$dest"
    else
        info "Need sudo to install to ${INSTALL_DIR}/"
        sudo cp "$src" "$dest"
    fi

    chmod +x "$dest"
    ok "Binary installed: ${dest}"
}

# ─── macOS: LaunchAgent ───────────────────────────────────────────────────────

install_macos() {
    local plist_name="com.forgefleet.daemon.plist"
    local plist_src
    local plist_dest="${HOME}/Library/LaunchAgents/${plist_name}"

    # Find plist — either next to this script or in deploy/macos/
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

    if [[ -f "${script_dir}/macos/${plist_name}" ]]; then
        plist_src="${script_dir}/macos/${plist_name}"
    elif [[ -f "${script_dir}/../deploy/macos/${plist_name}" ]]; then
        plist_src="${script_dir}/../deploy/macos/${plist_name}"
    else
        die "Cannot find ${plist_name}. Run from the deploy/ directory."
    fi

    info "Installing LaunchAgent..."
    mkdir -p "${HOME}/Library/LaunchAgents"
    cp "$plist_src" "$plist_dest"

    # Unload first if already loaded (ignore errors)
    launchctl unload "$plist_dest" 2>/dev/null || true

    launchctl load "$plist_dest"
    ok "LaunchAgent installed and loaded"

    info "Check status: launchctl list | grep forgefleet"
    info "View logs:    tail -f ${LOG_DIR}/${BINARY_NAME}.log"
}

uninstall_macos() {
    local plist_dest="${HOME}/Library/LaunchAgents/com.forgefleet.daemon.plist"

    if [[ -f "$plist_dest" ]]; then
        launchctl unload "$plist_dest" 2>/dev/null || true
        rm -f "$plist_dest"
        ok "LaunchAgent removed"
    else
        info "LaunchAgent not installed"
    fi
}

# ─── Linux: systemd ──────────────────────────────────────────────────────────

install_linux() {
    local service_name="forgefleet.service"
    local service_src
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

    if [[ -f "${script_dir}/linux/${service_name}" ]]; then
        service_src="${script_dir}/linux/${service_name}"
    elif [[ -f "${script_dir}/../deploy/linux/${service_name}" ]]; then
        service_src="${script_dir}/../deploy/linux/${service_name}"
    else
        die "Cannot find ${service_name}. Run from the deploy/ directory."
    fi

    info "Installing systemd service (system-wide)..."

    # If dedicated forgefleet user doesn't exist, run service as current user.
    local effective_user="forgefleet"
    if ! id -u forgefleet >/dev/null 2>&1; then
        effective_user="$(whoami)"
        warn "User 'forgefleet' not found; using current user: ${effective_user}"
    fi

    local rendered_service
    rendered_service="$(mktemp)"
    sed "s/^User=.*/User=${effective_user}/" "$service_src" > "$rendered_service"

    sudo cp "$rendered_service" "/etc/systemd/system/${service_name}"
    rm -f "$rendered_service"

    sudo systemctl daemon-reload
    sudo systemctl enable forgefleet
    sudo systemctl restart forgefleet

    ok "System service installed and started"
    info "Check status: systemctl status forgefleet"
    info "View logs:    journalctl -u forgefleet -f"
}

uninstall_linux() {
    if systemctl is-enabled forgefleet 2>/dev/null; then
        sudo systemctl stop forgefleet 2>/dev/null || true
        sudo systemctl disable forgefleet 2>/dev/null || true
        sudo rm -f /etc/systemd/system/forgefleet.service
        sudo systemctl daemon-reload
        ok "System service removed"
    else
        info "systemd service not installed"
    fi
}

# ─── Uninstall ────────────────────────────────────────────────────────────────

do_uninstall() {
    local os
    os="$(detect_os)"

    info "Uninstalling ForgeFleet..."

    case "$os" in
        macos) uninstall_macos ;;
        linux) uninstall_linux ;;
    esac

    # Remove binary
    local bin_path="${INSTALL_DIR}/${BINARY_NAME}"
    if [[ -f "$bin_path" ]]; then
        if [[ -w "${INSTALL_DIR}" ]]; then
            rm -f "$bin_path"
        else
            sudo rm -f "$bin_path"
        fi
        ok "Binary removed: ${bin_path}"
    fi

    warn "Data directory preserved: ${HOME_DIR}"
    warn "Remove manually with: rm -rf ${HOME_DIR}"
    ok "ForgeFleet uninstalled"
}

# ─── Main ─────────────────────────────────────────────────────────────────────

main() {
    echo ""
    echo "  ⚡ ForgeFleet Installer"
    echo "  ======================"
    echo ""

    # Handle --uninstall flag
    if [[ "${1:-}" == "--uninstall" || "${1:-}" == "-u" ]]; then
        do_uninstall
        exit 0
    fi

    local os
    os="$(detect_os)"
    info "Detected OS: ${os}"

    # Find and install binary
    local binary
    binary="$(find_binary "${1:-}")"
    info "Using binary: ${binary}"

    setup_directories
    install_binary "$binary"

    # Install OS-specific supervision
    case "$os" in
        macos) install_macos ;;
        linux) install_linux ;;
    esac

    echo ""
    ok "ForgeFleet installation complete! 🚀"
    echo ""
}

main "$@"
