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
        // openclaw ships via npm on this fleet (npm-global on macOS, Linux,
        // and DGX alike — never brew/apt despite the binary living under a
        // package-manager bin dir). Homebrew's npm prefix (/opt/homebrew) is
        // user-owned, so macOS needs no sudo; `sudo npm` would corrupt ~/.npm
        // with root-owned cache files and silently break later sudo-free
        // upgrades (cost a 26-day Taylor gateway outage to diagnose).
        ("openclaw", "macos") => {
            Some("export PATH=/opt/homebrew/bin:$PATH && npm install -g openclaw@latest".into())
        }
        ("openclaw", _) => Some("sudo npm install -g openclaw@latest".into()),
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
        // forgefleetd build + install + RESTART. Without the restart step
        // the upgrade only refreshes the binary on disk; the running daemon
        // keeps executing old code in memory. Discovered 2026-05-16: after
        // fleet-wide forgefleetd_git upgrades succeeded, `ff fleet versions`
        // still showed stale SHAs because no daemon got bounced.
        //
        // macOS: launchd manages the daemon via com.forgefleet.forgefleetd;
        // launchctl kickstart -k loads the fresh binary. Per
        // [[macos-launchd-kickstart]] pkill alone doesn't work because
        // launchd respawns from cached state.
        //
        // Linux: prefer systemd --user (forgefleetd.service if present),
        // fall back to pkill + nohup + disown. The fallback is the only
        // safe path for hosts whose bootstrap never installed a systemd
        // unit ([[bootstrap-missing-systemd]]).
        ("forgefleetd_git" | "forgefleetd", "macos") => Some(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git pull --ff-only && \
             cargo build --bin forgefleetd --release && \
             install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && \
             codesign --force --sign - ~/.local/bin/forgefleetd && \
             USER_ID=$(stat -f %u \"$HOME\" 2>/dev/null || id -u); \
             launchctl kickstart -k \"gui/${USER_ID}/com.forgefleet.forgefleetd\" 2>/dev/null \
               || launchctl kickstart -k \"user/${USER_ID}/com.forgefleet.forgefleetd\" 2>/dev/null \
               || (pkill -TERM -f \"$HOME/.local/bin/forgefleetd\" 2>/dev/null; sleep 1; \
                   nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                   </dev/null >/tmp/forgefleetd.log 2>&1 & disown)"
                .into(),
        ),
        ("forgefleetd_git" | "forgefleetd", "linux") => Some(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git reset --hard HEAD && \
             git clean -fdx graphify-out node-compile-cache && \
             git pull --ff-only && \
             cargo build --bin forgefleetd --release && \
             install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && \
             export XDG_RUNTIME_DIR=\"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}\"; \
             pkill -f 'forgefleetd --worker-name' 2>/dev/null; \
             sleep 1; \
             ( systemctl --user reset-failed forgefleetd.service 2>/dev/null; \
               systemctl --user restart forgefleetd.service 2>/dev/null ) \
               || ( pkill -TERM -f \"$HOME/.local/bin/forgefleetd\" 2>/dev/null; sleep 1; \
                    nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                    </dev/null >/tmp/forgefleetd.log 2>&1 & disown )"
                .into(),
        ),
        // DGX Sparks: aarch64 + 4 cores. Default cargo parallelism uses all
        // cores which OOMs LLVM passes during ff-gateway codegen (sia +
        // beyonce both died with exit -1 on 2026-05-19). -j 2 keeps RAM
        // pressure manageable. Same daemon-restart sequence as plain linux.
        // (DGX.1, 2026-05-19.)
        ("forgefleetd_git" | "forgefleetd", "linux-dgx") => Some(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git reset --hard HEAD && \
             git clean -fdx graphify-out node-compile-cache && \
             git pull --ff-only && \
             cargo build --bin forgefleetd --release -j 2 && \
             install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && \
             export XDG_RUNTIME_DIR=\"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}\"; \
             pkill -f 'forgefleetd --worker-name' 2>/dev/null; \
             sleep 1; \
             ( systemctl --user reset-failed forgefleetd.service 2>/dev/null; \
               systemctl --user restart forgefleetd.service 2>/dev/null ) \
               || ( pkill -TERM -f \"$HOME/.local/bin/forgefleetd\" 2>/dev/null; sleep 1; \
                    nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                    </dev/null >/tmp/forgefleetd.log 2>&1 & disown )"
                .into(),
        ),
        ("os", "linux") => Some("sudo apt-get update && sudo apt-get -y upgrade".into()),
        ("os", "macos") => Some("softwareupdate -i -a".into()),
        _ => None,
    }
}
