//! Per-tool upgrade playbook snippets.

/// Resolve a tool's upgrade playbook for a given OS family.
///
/// Tries an exact `(tool, os_family)` match first (so specialised arms like
/// `linux-dgx` win), then falls back to the base family (`linux-ubuntu` →
/// `linux`, `macos-15` → `macos`). Without the fallback, every host whose
/// `os_family` is a sub-family (e.g. `linux-ubuntu`) failed with
/// "no playbook for os=linux-ubuntu" — the defer-worker / `ff daemon`
/// upgrade path (which uses this fn) couldn't build forgefleetd/ff on any
/// Linux host, stalling fleet self-upgrade. The DB-driven `auto_upgrade`
/// path already did this via its own `base_family`; this mirrors it.
/// (2026-05-31.)
pub fn playbook_for(tool: &str, os_family: &str) -> Option<String> {
    if let Some(p) = playbook_exact(tool, os_family) {
        return Some(p);
    }
    match base_family(os_family) {
        Some(base) if base != os_family => playbook_exact(tool, base),
        _ => None,
    }
}

/// Build a shell snippet that installs a freshly-built `target/release/{bin}`
/// to `{dest}` **atomically** and only after proving the result runs.
///
/// Why this exists: a plain `install -m 755 target/release/ff $DEST` writes
/// straight into PATH, so a disk-full / interrupted copy leaves a truncated,
/// unrunnable binary *there*. Observed on ace 2026-06-14: a 304-byte garbage
/// `~/.local/bin/ff` from an ENOSPC `install` (the disk was at 100%). Every
/// `ff` invocation then died with a shell syntax error, the host could not run
/// any `ff` verb, and — because forgefleetd kept heartbeating on its stale
/// binary — nothing detected the CLI was dead. The `&&` chain that followed
/// (`codesign`, restart) aborted, so the upgrade task "failed" yet still left
/// the poisoned binary in PATH.
///
/// This installs to `{dest}.new`, code-signs it (macOS, so the temp itself is
/// validatable), proves it executes via `--version`, then atomically renames
/// it over `{dest}`. `mv` within one filesystem is a single rename(2), so PATH
/// only ever sees the old (working) binary or the new (validated) one — never a
/// half-written one. On ANY failure the temp is removed and the snippet
/// `exit 1`s, so the upgrade is recorded as FAILED (loud + retryable by the
/// version-drift machinery) instead of silently bricking the host's CLI.
pub fn atomic_install_cmd(bin: &str, dest: &str, codesign: bool) -> String {
    let sign = if codesign {
        format!("codesign --force --sign - \"{dest}.new\" && ")
    } else {
        String::new()
    };
    format!(
        "{{ install -m 755 target/release/{bin} \"{dest}.new\" && \
         {sign}\"{dest}.new\" --version >/dev/null 2>&1 && \
         mv -f \"{dest}.new\" \"{dest}\"; }} || \
         {{ rm -f \"{dest}.new\"; \
         echo \"upgrade: install/validate of {dest} failed; kept existing binary\" >&2; \
         exit 1; }}"
    )
}

/// Normalise an `os_family` to its base family, or `None` if unrecognised.
fn base_family(os_family: &str) -> Option<&'static str> {
    if os_family.starts_with("linux") {
        Some("linux")
    } else if os_family.starts_with("macos") {
        Some("macos")
    } else if os_family.starts_with("windows") {
        Some("windows")
    } else {
        None
    }
}

fn playbook_exact(tool: &str, os_family: &str) -> Option<String> {
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
        ("ff_git" | "ff", "macos") => Some(format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git pull --ff-only && \
             cargo build -p ff-terminal --release && {install}",
            install = atomic_install_cmd("ff", "$HOME/.local/bin/ff", true),
        )),
        ("ff_git" | "ff", "linux") => Some(format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git reset --hard HEAD && \
             git clean -fdx graphify-out node-compile-cache && \
             git pull --ff-only && \
             cargo build -p ff-terminal --release && {install}",
            install = atomic_install_cmd("ff", "$HOME/.local/bin/ff", false),
        )),
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
        //
        // SELF-KILL FIX (2026-06-14): the restart MUST be detached. The
        // deferred worker runs this playbook as a child of the very
        // forgefleetd it's about to restart, spawned with `process_group(0)`
        // (task_runner.rs). The old playbook ran a FOREGROUND
        // `pkill -f 'forgefleetd --worker-name'` before the restart, which
        // tore down the daemon → the worker's process group → the playbook
        // shell itself (exit -1) before `systemctl restart` / the nohup
        // respawn ever ran. Result: every Linux forgefleetd_git upgrade
        // reported failure (14/15 stuck drifted) and no-systemd hosts could
        // be left with the daemon down. Fix mirrors the wave restart
        // (task_runner.rs "fix C"): wrap the whole kill+restart in a
        // `setsid` session (escapes the worker's process-group reap — see the
        // task_runner.rs:process_group(0) comment), background+disown it so
        // the orchestrator returns and the worker records SUCCESS first, then
        // restart via `systemctl --no-block` (or a detached pkill+nohup
        // respawn). The leading 2s sleep guarantees the success write lands
        // before the daemon is bounced.
        ("forgefleetd_git" | "forgefleetd", "macos") => Some(format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git pull --ff-only && \
             cargo build --bin forgefleetd --release && {install} && \
             USER_ID=$(stat -f %u \"$HOME\" 2>/dev/null || id -u); \
             launchctl kickstart -k \"gui/${{USER_ID}}/com.forgefleet.forgefleetd\" 2>/dev/null \
               || launchctl kickstart -k \"user/${{USER_ID}}/com.forgefleet.forgefleetd\" 2>/dev/null \
               || (pkill -TERM -f \"$HOME/.local/bin/forgefleetd\" 2>/dev/null; sleep 1; \
                   nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                   </dev/null >/tmp/forgefleetd.log 2>&1 & disown)",
            install = atomic_install_cmd("forgefleetd", "$HOME/.local/bin/forgefleetd", true),
        )),
        ("forgefleetd_git" | "forgefleetd", "linux") => Some(format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git reset --hard HEAD && \
             git clean -fdx graphify-out node-compile-cache && \
             git pull --ff-only && \
             cargo build --bin forgefleetd --release && {install} && \
             export XDG_RUNTIME_DIR=\"${{XDG_RUNTIME_DIR:-/run/user/$(id -u)}}\"; \
             setsid bash -c 'sleep 2; \
               systemctl --user reset-failed forgefleetd.service 2>/dev/null; \
               systemctl --user restart --no-block forgefleetd.service </dev/null >/dev/null 2>&1 \
                 || ( pkill -TERM -f \"$HOME/.local/bin/forgefleetd\" 2>/dev/null; sleep 1; \
                      nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                      </dev/null >/tmp/forgefleetd.log 2>&1 & disown )' \
               </dev/null >/tmp/forgefleetd-restart.log 2>&1 & \
             disown; \
             echo \"build+install OK; restart dispatched detached (setsid + --no-block; survives worker self-kill)\"",
            install = atomic_install_cmd("forgefleetd", "$HOME/.local/bin/forgefleetd", false),
        )),
        // DGX Sparks: aarch64 + 4 cores. Default cargo parallelism uses all
        // cores which OOMs LLVM passes during ff-gateway codegen (sia +
        // beyonce both died with exit -1 on 2026-05-19). -j 2 keeps RAM
        // pressure manageable. Same daemon-restart sequence as plain linux.
        // (DGX.1, 2026-05-19.)
        ("forgefleetd_git" | "forgefleetd", "linux-dgx") => Some(format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             cd ~/projects/forge-fleet && git reset --hard HEAD && \
             git clean -fdx graphify-out node-compile-cache && \
             git pull --ff-only && \
             cargo build --bin forgefleetd --release -j 2 && {install} && \
             export XDG_RUNTIME_DIR=\"${{XDG_RUNTIME_DIR:-/run/user/$(id -u)}}\"; \
             setsid bash -c 'sleep 2; \
               systemctl --user reset-failed forgefleetd.service 2>/dev/null; \
               systemctl --user restart --no-block forgefleetd.service </dev/null >/dev/null 2>&1 \
                 || ( pkill -TERM -f \"$HOME/.local/bin/forgefleetd\" 2>/dev/null; sleep 1; \
                      nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                      </dev/null >/tmp/forgefleetd.log 2>&1 & disown )' \
               </dev/null >/tmp/forgefleetd-restart.log 2>&1 & \
             disown; \
             echo \"build+install OK; restart dispatched detached (setsid + --no-block; survives worker self-kill)\"",
            install = atomic_install_cmd("forgefleetd", "$HOME/.local/bin/forgefleetd", false),
        )),
        ("os", "linux") => Some("sudo apt-get update && sudo apt-get -y upgrade".into()),
        ("os", "macos") => Some("softwareupdate -i -a".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_family_falls_back_to_base() {
        // The regression: linux-ubuntu / linux-dgx etc. must resolve the
        // generic `linux` playbook when no specialised arm exists.
        for fam in ["linux-ubuntu", "linux", "linux-fedora"] {
            let p = playbook_for("forgefleetd_git", fam)
                .unwrap_or_else(|| panic!("no playbook for {fam}"));
            assert!(p.contains("cargo build --bin forgefleetd"), "fam={fam}");
        }
    }

    #[test]
    fn specialised_arm_wins_over_base() {
        // linux-dgx has its own `-j 2` arm — exact match must take priority.
        let dgx = playbook_for("forgefleetd_git", "linux-dgx").unwrap();
        assert!(dgx.contains("-j 2"), "dgx arm should be the -j 2 variant");
    }

    #[test]
    fn macos_sub_family_falls_back() {
        let p = playbook_for("forgefleetd_git", "macos-15").unwrap();
        assert!(p.contains("launchctl kickstart"));
    }

    #[test]
    fn unknown_os_is_none() {
        assert!(playbook_for("forgefleetd_git", "plan9").is_none());
    }

    #[test]
    fn atomic_install_uses_temp_validate_then_rename() {
        // The ace 2026-06-14 brick: a disk-full `install` straight into
        // ~/.local/bin/ff left a 304-byte garbage binary in PATH. The install
        // must go to a temp, prove it runs, then atomically rename — and on
        // failure remove the temp + exit non-zero so PATH keeps the old binary.
        let mac = atomic_install_cmd("ff", "$HOME/.local/bin/ff", true);
        assert!(mac.contains("install -m 755 target/release/ff \"$HOME/.local/bin/ff.new\""));
        assert!(mac.contains("codesign --force --sign - \"$HOME/.local/bin/ff.new\""));
        assert!(mac.contains("\"$HOME/.local/bin/ff.new\" --version"));
        assert!(mac.contains("mv -f \"$HOME/.local/bin/ff.new\" \"$HOME/.local/bin/ff\""));
        assert!(mac.contains("rm -f \"$HOME/.local/bin/ff.new\""));
        assert!(mac.contains("exit 1"));

        // Linux build has no code-signing step.
        let lin = atomic_install_cmd("forgefleetd", "$HOME/.local/bin/forgefleetd", false);
        assert!(!lin.contains("codesign"));
        assert!(lin.contains("\"$HOME/.local/bin/forgefleetd.new\" --version"));
        assert!(lin.contains("mv -f \"$HOME/.local/bin/forgefleetd.new\""));
    }

    #[test]
    fn cargo_binary_playbooks_install_atomically() {
        // Every cargo-binary upgrade arm must validate-then-rename (never write
        // straight into PATH) so an interrupted/disk-full copy can't brick the
        // host's CLI or daemon binary.
        for (tool, fam) in [
            ("ff_git", "macos"),
            ("ff_git", "linux"),
            ("forgefleetd_git", "macos"),
            ("forgefleetd_git", "linux"),
            ("forgefleetd_git", "linux-dgx"),
        ] {
            let p = playbook_for(tool, fam).unwrap_or_else(|| panic!("no playbook {tool}/{fam}"));
            assert!(
                p.contains(".new\""),
                "{tool}/{fam}: not installing to a temp"
            );
            assert!(
                p.contains(".new\" --version"),
                "{tool}/{fam}: not validated"
            );
            assert!(p.contains("mv -f"), "{tool}/{fam}: not atomically renamed");
            // Must NOT write the final binary directly (the old poisoning path).
            assert!(
                !p.contains("install -m 755 target/release/ff \"$HOME/.local/bin/ff\"")
                    && !p.contains(
                        "install -m 755 target/release/forgefleetd \"$HOME/.local/bin/forgefleetd\""
                    ),
                "{tool}/{fam}: still installs directly into PATH"
            );
        }
    }

    #[test]
    fn linux_restart_is_detached_and_not_self_killing() {
        // SELF-KILL FIX (2026-06-14): the deferred worker runs this playbook as
        // a child of the forgefleetd it restarts. The restart MUST be `setsid`
        // detached (escapes the worker's process-group reap) and MUST NOT run a
        // foreground `pkill -f 'forgefleetd --worker-name'` — that killed the
        // orchestrating shell before the restart ran (exit -1, fleet stuck
        // drifted). `--no-block` returns immediately so the worker records
        // success first.
        for fam in ["linux", "linux-dgx"] {
            let p = playbook_for("forgefleetd_git", fam)
                .unwrap_or_else(|| panic!("no playbook for {fam}"));
            assert!(
                p.contains("setsid bash -c"),
                "fam={fam}: restart not detached"
            );
            assert!(
                p.contains("systemctl --user restart --no-block forgefleetd.service"),
                "fam={fam}: restart must be --no-block"
            );
            assert!(
                !p.contains("pkill -f 'forgefleetd --worker-name'"),
                "fam={fam}: foreground daemon-pkill self-kills the worker"
            );
        }
    }
}
