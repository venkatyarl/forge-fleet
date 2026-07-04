//! Make per-user tool directories reachable from a daemon whose PATH is the
//! bare system default.
//!
//! A `systemd --user` unit (Linux) or a `launchd` agent (macOS) starts
//! forgefleetd with a minimal PATH — typically `/usr/local/bin:/usr/bin:/bin:…`
//! — that does NOT include `~/.local/bin` or `~/.cargo/bin`. But the daemon
//! spawns child processes by BARE NAME: `ff cli codex` (Lane-2 dispatch),
//! `cargo check` (Lane-1 codegen), `kimi`, etc. Those live in the per-user dirs,
//! so the bare spawn fails with `No such file or directory (os error 2)` and the
//! whole dispatch reports the misleading "no dispatchable backend on this node".
//!
//! Prepending the per-user bin dirs to the daemon's own PATH fixes every such
//! spawn at once (children inherit the process environment), without having to
//! edit a PATH override into each host's unit file.

use std::path::PathBuf;

/// Prepend `~/.local/bin` and `~/.cargo/bin` to `PATH` when they exist and are
/// not already present. Idempotent; safe to call once at process startup before
/// any child is spawned. No-op when `HOME` is unset or the dirs are missing.
pub fn ensure_user_tool_path() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    // Front-of-PATH order: .local/bin then .cargo/bin (both win over any stale
    // copies further down). Listed here in final priority order.
    let want = [home.join(".local/bin"), home.join(".cargo/bin")];

    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&current).collect();

    let mut changed = false;
    // Insert in reverse so the first `want` entry ends up first in PATH.
    for dir in want.iter().rev() {
        if dir.is_dir() && !dirs.iter().any(|p| p == dir) {
            dirs.insert(0, dir.clone());
            changed = true;
        }
    }

    if changed {
        if let Ok(joined) = std::env::join_paths(&dirs) {
            // SAFETY: intended to be called at process startup before any
            // subsystem thread or child process is spawned, mirroring the other
            // `set_var` env setup in the daemon's `main`.
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var("PATH", joined);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_user_tool_path_is_idempotent_and_no_panic() {
        // Whatever the test env's PATH is, calling twice must not panic and must
        // leave PATH stable on the second call (idempotent).
        ensure_user_tool_path();
        let after_first = std::env::var_os("PATH");
        ensure_user_tool_path();
        let after_second = std::env::var_os("PATH");
        assert_eq!(after_first, after_second);
    }
}
