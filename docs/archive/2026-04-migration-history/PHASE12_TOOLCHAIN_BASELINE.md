# Phase 12 — Toolchain Baseline (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Repo: `/Users/venkat/projects/forge-fleet`  
Scope: Local developer setup baseline for Phase 12 quality gates

## 1) Goal

Define a single, repeatable Rust toolchain baseline so local development matches CI quality gates (`fmt`, `clippy`, `check`, `test --lib`).

---

## 2) Recommended baseline versions

| Area | Recommendation | Why |
|---|---|---|
| Rust installer | `rustup` (latest stable release) | Standard Rust toolchain manager across macOS/Linux |
| Rust channel/toolchain | `1.85.0` (stable) | Matches CI (`dtolnay/rust-toolchain@1.85.0`) and supports workspace `edition = "2024"` |
| Default toolchain policy | Use `1.85.0` as default for this repo | Prevent local/CI drift |
| Formatting | `rustfmt` component | Required for `cargo fmt --check` gate |
| Linting | `clippy` component | Required for `cargo clippy --workspace -- -D warnings` gate |

> Notes:
> - CI currently enforces Rust `1.85.0` in `.github/workflows/rust-quality-gates.yml`.
> - Keep local runs pinned with `cargo +1.85.0 ...` when validating before PR.

---

## 3) Rustup + cargo utility setup

1. Install `rustup` (if missing).
2. Install toolchain `1.85.0`.
3. Add required components: `rustfmt`, `clippy`.
4. Verify with the same commands used in CI.

Validation command set (same as CI gates):

```bash
cargo +1.85.0 fmt --check
cargo +1.85.0 clippy --workspace -- -D warnings
cargo +1.85.0 check --workspace
cargo +1.85.0 test --workspace --lib
```

---

## 4) OS-specific notes (dev nodes)

### macOS

- Install Xcode Command Line Tools (required for C toolchain/linker):
  - `xcode-select --install`
- If native TLS/OpenSSL build issues appear, install optional helpers:
  - `brew install pkg-config openssl@3`
- If Homebrew OpenSSL is needed by local environment, export:
  - `export PKG_CONFIG_PATH="$(brew --prefix openssl@3)/lib/pkgconfig:${PKG_CONFIG_PATH:-}"`

### Linux (Ubuntu/Debian)

- Install base build dependencies:
  - `sudo apt-get update`
  - `sudo apt-get install -y build-essential pkg-config libssl-dev ca-certificates curl`
- `libssl-dev` is commonly required for crates that rely on native TLS/OpenSSL.

---

## 5) Copy-paste bootstrap block (new contributors)

```bash
#!/usr/bin/env bash
set -euo pipefail

# 0) OS prereqs (best-effort)
case "$(uname -s)" in
  Darwin)
    xcode-select -p >/dev/null 2>&1 || xcode-select --install || true
    if command -v brew >/dev/null 2>&1; then
      brew install pkg-config openssl@3 || true
    fi
    ;;
  Linux)
    if command -v apt-get >/dev/null 2>&1; then
      sudo apt-get update
      sudo apt-get install -y build-essential pkg-config libssl-dev ca-certificates curl
    fi
    ;;
esac

# 1) Install rustup if missing
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi

# shellcheck disable=SC1091
source "$HOME/.cargo/env"

# 2) Install/pin Rust toolchain + required components
rustup toolchain install 1.85.0
rustup component add rustfmt clippy --toolchain 1.85.0
rustup default 1.85.0

# 3) Verify locally with the same quality gates as CI
cargo +1.85.0 fmt --check
cargo +1.85.0 clippy --workspace -- -D warnings
cargo +1.85.0 check --workspace
cargo +1.85.0 test --workspace --lib

echo "ForgeFleet Rust toolchain bootstrap complete (1.85.0 + fmt/clippy)."
```

---

## 6) Contributor workflow recommendation

Before opening a PR, run:

1. `cargo +1.85.0 fmt --check`
2. `cargo +1.85.0 clippy --workspace -- -D warnings`
3. `cargo +1.85.0 check --workspace`
4. `cargo +1.85.0 test --workspace --lib`

This keeps local results aligned with CI and reduces avoidable PR churn.
