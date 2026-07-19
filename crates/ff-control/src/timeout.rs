//! Build timeout killer.
//!
//! Provides a small helper to terminate a build that has exceeded its timeout.

use tracing::warn;

/// Send SIGTERM to a hung build process.
///
/// Logs the kill attempt and ignores ESRCH (process already gone), matching the
/// behaviour of the legacy daemon reaper's backstop SIGTERM.
#[cfg(unix)]
pub fn kill_hung_build(pid: u32) {
    warn!(pid, "sending SIGTERM to hung build");
    // SAFETY: `kill(2)` on a positive pid targets that process. SIGTERM asks
    // the build to shut down gracefully. A dead pid returns ESRCH, which we
    // discard because the goal (process is gone) is already achieved.
    unsafe {
        let _ = libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// No-op on non-Unix platforms.
#[cfg(not(unix))]
pub fn kill_hung_build(pid: u32) {
    let _ = pid;
    warn!("kill_hung_build is a no-op on non-Unix platforms");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    /// Spawn a long-running process, send SIGTERM through `kill_hung_build`, and
    /// verify the child exits within a reasonable window.
    #[cfg(unix)]
    #[test]
    fn kills_a_hung_build_process() {
        let mut child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("failed to spawn sleep");

        let pid = child.id();
        kill_hung_build(pid);

        // Poll until the process exits, up to 5 seconds.
        let mut alive = true;
        for _ in 0..50 {
            thread::sleep(Duration::from_millis(100));
            match child.try_wait() {
                Ok(Some(_)) => {
                    alive = false;
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    alive = false;
                    break;
                }
            }
        }

        if alive {
            // Best-effort cleanup if the test is about to fail.
            let _ = child.kill();
        }

        assert!(!alive, "hung build process should have been killed");
    }
}
