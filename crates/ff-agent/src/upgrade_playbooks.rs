//! Per-tool upgrade playbook snippets.
pub fn playbook_for(tool: &str, os_family: &str) -> Option<String> {
    match (tool, os_family) {
        ("gh", "linux") => {
            Some("sudo apt-get update && sudo apt-get install --only-upgrade -y gh".into())
        }
        ("gh", "macos") => Some("brew upgrade gh".into()),
        ("op", "linux") => Some(
            "sudo apt-get update && sudo apt-get install --only-upgrade -y 1password-cli".into(),
        ),
        ("op", "macos") => Some("brew upgrade --cask 1password-cli".into()),
        ("openclaw", _) => Some("curl -fsSL https://openclaw.ai/install.sh | bash".into()),
        ("mlx_lm", _) => Some("pip install -U mlx-lm".into()),
        ("vllm", _) => Some("pip install -U vllm".into()),
        ("llama.cpp", _) => {
            Some("cd ~/llama.cpp && git pull && cmake --build build --config Release -j".into())
        }
        // Cargo binaries (ff CLI + forgefleetd daemon). Playbooks source
        // ~/.cargo/env because they execute under `sh` (Ubuntu /bin/sh =
        // dash) without the operator's interactive PATH — the rustup-managed
        // cargo at $HOME/.cargo/bin would otherwise fall back to PATH and
        // fail with `cargo: not found`. Tracking down that one-line error
        // cost a fleet-wide upgrade attempt 2026-05-16. Use `. <file>`
        // (POSIX `source`) so dash + bash both load it.
        ("ff_git" | "ff", "macos") => Some(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git pull --ff-only && \
             cargo build -p ff-terminal --release && \
             install -m 755 target/release/ff ~/.local/bin/ff && \
             codesign --force --sign - ~/.local/bin/ff"
                .into(),
        ),
        ("ff_git" | "ff", "linux") => Some(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git reset --hard HEAD && \
             git clean -fdx graphify-out node-compile-cache && \
             git pull --ff-only && \
             cargo build -p ff-terminal --release && \
             install -m 755 target/release/ff ~/.local/bin/ff"
                .into(),
        ),
        ("forgefleetd_git" | "forgefleetd", "macos") => Some(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git pull --ff-only && \
             cargo build --bin forgefleetd --release && \
             install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && \
             codesign --force --sign - ~/.local/bin/forgefleetd"
                .into(),
        ),
        ("forgefleetd_git" | "forgefleetd", "linux") => Some(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git reset --hard HEAD && \
             git clean -fdx graphify-out node-compile-cache && \
             git pull --ff-only && \
             cargo build --bin forgefleetd --release && \
             install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd"
                .into(),
        ),
        ("os", "linux") => Some("sudo apt-get update && sudo apt-get -y upgrade".into()),
        ("os", "macos") => Some("softwareupdate -i -a".into()),
        _ => None,
    }
}
