#!/usr/bin/env bash
# ForgeFleet Node Provisioning Script
# ====================================
# Installs all required software on a fleet node.
# Run via SSH: ssh user@node "bash -s" < deploy/provision-node.sh
#
# Installs:
#   - Node.js + npm (if missing)
#   - Python3 + pip (if missing)
#   - Claude Code (@anthropic-ai/claude-code)
#   - OpenClaw
#   - Codex (OpenAI)
#   - code-review-graph (MCP server)
#   - Docker (if missing)
#   - Rust/Cargo (if missing)

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log() { echo -e "${GREEN}[provision]${NC} $1"; }
warn() { echo -e "${YELLOW}[provision]${NC} $1"; }
err() { echo -e "${RED}[provision]${NC} $1"; }

OS=$(uname -s)
ARCH=$(uname -m)

log "Provisioning $(hostname) — $OS $ARCH"

# ─── Node.js + npm ────────────────────────────────────────
if command -v node &>/dev/null; then
    log "Node.js: $(node --version) ✓"
else
    log "Installing Node.js..."
    if [ "$OS" = "Darwin" ]; then
        brew install node 2>/dev/null || true
    else
        curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - 2>/dev/null
        sudo apt-get install -y nodejs 2>/dev/null
    fi
    log "Node.js: $(node --version) ✓"
fi

# ─── Python3 + pip ────────────────────────────────────────
if command -v pip3 &>/dev/null; then
    log "pip3: ✓"
else
    log "Installing pip3..."
    sudo apt-get install -y python3-pip 2>/dev/null || true
fi

# ─── Claude Code ──────────────────────────────────────────
if command -v claude &>/dev/null; then
    log "Claude Code: $(claude --version 2>/dev/null || echo 'installed') ✓"
else
    log "Installing Claude Code..."
    sudo npm install -g @anthropic-ai/claude-code 2>/dev/null || npm install -g @anthropic-ai/claude-code 2>/dev/null || true
    if command -v claude &>/dev/null; then
        log "Claude Code: installed ✓"
    else
        warn "Claude Code: install failed (may need manual setup)"
    fi
fi

# ─── OpenClaw ─────────────────────────────────────────────
if command -v openclaw &>/dev/null; then
    log "OpenClaw: $(openclaw --version 2>/dev/null || echo 'installed') ✓"
else
    log "Installing OpenClaw..."
    sudo npm install -g openclaw 2>/dev/null || npm install -g openclaw 2>/dev/null || true
    if command -v openclaw &>/dev/null; then
        log "OpenClaw: installed ✓"
    else
        warn "OpenClaw: install failed (may need manual setup)"
    fi
fi

# ─── Codex (OpenAI) ───────────────────────────────────────
if command -v codex &>/dev/null; then
    log "Codex: $(codex --version 2>/dev/null || echo 'installed') ✓"
else
    log "Installing Codex..."
    sudo npm install -g @openai/codex 2>/dev/null || npm install -g @openai/codex 2>/dev/null || true
    if command -v codex &>/dev/null; then
        log "Codex: installed ✓"
    else
        warn "Codex: install failed (may need manual setup)"
    fi
fi

# ─── code-review-graph ────────────────────────────────────
if command -v code-review-graph &>/dev/null; then
    log "code-review-graph: $(code-review-graph --version 2>/dev/null || echo 'installed') ✓"
else
    log "Installing code-review-graph..."
    pip3 install --break-system-packages code-review-graph 2>/dev/null || pip3 install code-review-graph 2>/dev/null || true
    if command -v code-review-graph &>/dev/null; then
        log "code-review-graph: installed ✓"
    else
        warn "code-review-graph: install failed"
    fi
fi

# ─── Docker ───────────────────────────────────────────────
if command -v docker &>/dev/null; then
    log "Docker: $(docker --version 2>/dev/null) ✓"
else
    log "Installing Docker..."
    if [ "$OS" = "Linux" ]; then
        curl -fsSL https://get.docker.com | sudo sh 2>/dev/null
        sudo usermod -aG docker "$(whoami)" 2>/dev/null || true
    else
        warn "Docker: install manually on macOS (Docker Desktop)"
    fi
fi

# ─── Rust/Cargo ───────────────────────────────────────────
if command -v cargo &>/dev/null || [ -f "$HOME/.cargo/env" ]; then
    source "$HOME/.cargo/env" 2>/dev/null || true
    log "Rust: $(rustc --version 2>/dev/null || echo 'installed') ✓"
else
    log "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y 2>/dev/null
    source "$HOME/.cargo/env" 2>/dev/null || true
    log "Rust: $(rustc --version 2>/dev/null) ✓"
fi

log "Provisioning complete for $(hostname)"
