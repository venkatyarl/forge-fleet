//! `ff self <subcommand>` — operations the daemon runs against its own
//! process / installation. Specifically [`handle_restart_detached`]
//! solves the "daemon can't suicide-restart from a task that lives
//! inside the daemon" problem.

use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, Result};

/// Schedule a daemon restart that survives the current process group's
/// death. Uses `setsid(2)` to detach a child shell into a new session
/// (and therefore new process group), so that when the parent shell
/// task exits and the daemon eventually receives `SIGTERM`/`SIGKILL`
/// from `launchctl kickstart` / `systemctl restart`, the detached
/// child is not killed alongside its siblings.
///
/// ## Why this is needed
///
/// `forgefleetd` is launched by `launchd` (macOS) or `systemd --user`
/// (Linux). The supervisor owns the daemon's process group and treats
/// every descendant as part of the unit; `kickstart -k` /
/// `systemctl restart` send the kill signal to the **whole group**.
///
/// When a fleet task running as a child of the daemon does
/// `launchctl kickstart -k …` directly, the kill signal arrives
/// before the task can finish writing its result row, so the daemon
/// dies with the task in `running` state. The watchdog re-queues 120s
/// later and the loop repeats.
///
/// Detaching with `setsid` puts the child in a new session that
/// `launchd`/`systemd` no longer owns. The child then sleeps long
/// enough that the parent task (and the daemon) can mark the row
/// `completed` cleanly before the actual restart fires.
///
/// ## Behavior
///
/// 1. Forks a child that calls `setsid()` via `pre_exec`.
/// 2. Child runs `sleep <delay> && <restart-cmd>` with stdio detached.
/// 3. Parent (this verb) returns immediately with the child's PID.
///
/// `<restart-cmd>` is OS-specific:
/// - **macOS** — `launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd`
/// - **Linux** — `systemctl --user restart forgefleetd.service` with the
///   same fallback chain `revive.rs` uses.
pub fn handle_restart_detached(delay_secs: u64) -> Result<()> {
    let restart_cmd = if cfg!(target_os = "macos") {
        // launchd label is fixed by the .plist we ship; safe constant.
        "launchctl kickstart -k gui/$(id -u)/com.forgefleet.forgefleetd".to_string()
    } else {
        // systemd --user restart, falling back through the three known
        // unit names (mirrors revive.rs and V48 playbook fix).
        "export XDG_RUNTIME_DIR=\"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}\" && \
         systemctl --user reset-failed forgefleetd.service forgefleet-node.service \
                                       forgefleet-daemon.service forgefleet-agent.service 2>/dev/null; \
         systemctl --user restart forgefleetd.service \
            || systemctl --user restart forgefleet-node.service \
            || systemctl --user restart forgefleet-daemon.service \
            || systemctl --user restart forgefleet-agent.service"
            .to_string()
    };

    let script = format!(
        "sleep {delay_secs} && {restart_cmd} >/dev/null 2>&1",
    );

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&script);
    // Sever stdio so the child doesn't block on the parent's pipe and
    // doesn't write to a closed terminal once the daemon dies.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // SAFETY: setsid() with no args is signal-safe (per POSIX) and
    // `pre_exec` runs after `fork(2)` but before `execve(2)` — the
    // child has a single thread at this point, so sigsafety is the
    // only requirement.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .context("spawn detached restart child")?;
    println!(
        "ff: scheduled forgefleetd restart in {delay_secs}s (detached pid {})",
        child.id()
    );
    Ok(())
}
