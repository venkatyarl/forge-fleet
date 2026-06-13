//! Periodic reaper for leaked **orphan** processes left behind by the task
//! runner before PR #215's process-group kill landed — and a permanent
//! backstop for any future gap (e.g. a grandchild that `setsid`s into its own
//! session and so escapes the group kill).
//!
//! ## The leak this cleans up
//!
//! Before #215, `run_shell_payload` SIGKILLed only the **direct** child shell
//! on timeout; every grandchild it had spawned (ssh / git / rsync / cargo) was
//! reparented to pid 1 and ran forever. On 2026-06-13 sophie carried **430**
//! such orphans (26-day-old `git-upload-pack` ssh-to-GitHub processes, stuck
//! multi-hour `rsync --timeout=3600` HA-backup fetches) which saturated the
//! host so every task it then CLAIMED wedged. #215 stops NEW local leaks, but
//! it cannot retroactively reap the orphans already accumulated fleet-wide, nor
//! the orphans still being produced by hosts not yet rolled onto #215.
//!
//! ## Why this is safe to run on every node, SIGKILL and all
//!
//! A process is only reaped when **all three** hold:
//!   1. **`PPID == 1`** — it was reparented to init/launchd, i.e. its real
//!      parent already died. A process Vinny (or any live tool) started has its
//!      parent shell/daemon alive, so it is never PPID 1. The HA-backup rsync
//!      that forgefleetd itself drives runs under the task runner (PPID ≠ 1)
//!      while healthy, so an in-flight legitimate backup is never touched.
//!   2. **command matches a tight allow-list** of the exact short-lived tools
//!      the task runner spawns (rsync, git fetch over ssh/https). This excludes
//!      everything else that is *legitimately* PPID 1 — forgefleetd itself,
//!      sshd, the deliberately-`setsid`'d model servers (llama-server /
//!      mlx_lm.server / vllm), etc. We never kill a server.
//!   3. **age ≥ `FORGEFLEET_ORPHAN_REAP_SECS`** (default 7200s = 2h, matching
//!      the stale-task reaper) — well past any legitimate rsync/git run, so a
//!      brief orphan caught between a parent's death and its own exit is spared.
//!
//! Filesystem/`ps`-only, no DB, no leader gate — like the disk sampler and
//! log-rotation ticks. Set `FORGEFLEET_ORPHAN_REAP_SECS=0` to disable.

/// Default minimum orphan age before reaping. 2h is safely above the ~45min
/// cold cargo build and the HA `rsync --timeout=3600`, so only genuinely stuck
/// orphans qualify. Matches the stale-task reaper's default.
const DEFAULT_MIN_AGE_SECS: u64 = 7200;

/// Resolve the minimum-age threshold, honoring `FORGEFLEET_ORPHAN_REAP_SECS`.
/// Returns `None` (reaper disabled) when the override is `0`.
pub fn min_age_secs() -> Option<u64> {
    match std::env::var("FORGEFLEET_ORPHAN_REAP_SECS") {
        Ok(s) => match s.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(DEFAULT_MIN_AGE_SECS),
        },
        Err(_) => Some(DEFAULT_MIN_AGE_SECS),
    }
}

/// A leaked orphan we decided to reap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub pid: i32,
    pub age_secs: u64,
    /// Short label for *why* it matched the allow-list (logged).
    pub kind: &'static str,
    /// The full command line (logged for the operator).
    pub command: String,
}

/// Parse a `ps` elapsed-time field (`[[DD-]HH:]MM:SS`) into seconds. This
/// format is emitted identically by BSD `ps` (macOS) and GNU/procps `ps`
/// (Linux), so one parser covers the whole fleet.
pub fn parse_etime(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0u64, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (
            h.parse::<u64>().ok()?,
            m.parse::<u64>().ok()?,
            s.parse::<u64>().ok()?,
        ),
        [m, s] => (0u64, m.parse::<u64>().ok()?, s.parse::<u64>().ok()?),
        _ => return None,
    };
    if m >= 60 || sec >= 60 {
        return None;
    }
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
}

/// The basename of the first whitespace-delimited token of a command line
/// (the program), lower-cased for matching. `/usr/bin/rsync -az` → `rsync`.
fn program_basename(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Decide whether an orphaned command line is one of the task-runner's
/// short-lived spawns and therefore safe to reap. Returns a static label for
/// logging, or `None` to spare it.
///
/// Deliberately an *allow*-list, not a deny-list: anything not explicitly the
/// rsync / git-fetch tooling the task runner spawns is left alone, so the
/// model servers (which legitimately `setsid` and reparent to pid 1) and every
/// real daemon are never candidates.
pub fn reapable_kind(command: &str) -> Option<&'static str> {
    let prog = program_basename(command);
    match prog.as_str() {
        // HA backup fan-out: `rsync -az ... --timeout=3600 ...`.
        "rsync" => Some("ha-backup-rsync"),
        // Build playbook `git fetch` over HTTPS (the git-remote-https helper)
        // or a bare `git fetch`/`git-remote-*` left hanging.
        "git" | "git-remote-https" | "git-remote-http" | "git-remote-http-curl" => {
            Some("build-git-fetch")
        }
        // `git fetch` over the github SSH alias spawns an `ssh` whose remote
        // command is `git-upload-pack`/`git-receive-pack` — sophie's 26-day-old
        // orphans were exactly this shape. Only reap ssh that is a git tunnel,
        // never an arbitrary ssh session.
        "ssh" if command.contains("git-upload-pack") || command.contains("git-receive-pack") => {
            Some("build-git-ssh")
        }
        _ => None,
    }
}

/// Pure core: given raw `ps` output (lines of `pid ppid etime command...`),
/// the age threshold, and our own pid, return the orphans to reap.
///
/// Selection requires PPID==1 **and** an allow-listed command **and**
/// age ≥ `min_age_secs`. `self_pid` is excluded defensively (forgefleetd is
/// never an allow-listed command anyway, but belt-and-suspenders).
pub fn find_orphans(ps_output: &str, min_age_secs: u64, self_pid: i32) -> Vec<Orphan> {
    let mut out = Vec::new();
    for line in ps_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // `pid ppid etime command...` — columns are separated by runs of
        // spaces (ps pads numerically), so split on whitespace runs, take the
        // three leading fields, and rejoin the rest as the command line.
        let mut it = line.split_whitespace();
        let Some(pid) = it.next().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Some(ppid) = it.next().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Some(etime) = it.next().and_then(parse_etime) else {
            continue;
        };
        let command = it.collect::<Vec<_>>().join(" ");
        if command.is_empty() || ppid != 1 || pid == self_pid || etime < min_age_secs {
            continue;
        }
        let Some(kind) = reapable_kind(&command) else {
            continue;
        };
        out.push(Orphan {
            pid,
            age_secs: etime,
            kind,
            command,
        });
    }
    out
}

/// Snapshot every process as `pid ppid etime command`. `-A` (all processes)
/// and the `etime`/`command` columns are POSIX and behave identically on macOS
/// and Linux; the trailing `=` on each column suppresses the header row.
#[cfg(unix)]
fn ps_snapshot() -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-Ao", "pid=,ppid=,etime=,command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// SIGKILL a single pid. ESRCH (already gone) is ignored.
#[cfg(unix)]
fn sigkill(pid: i32) {
    // SAFETY: `kill(2)` on a positive pid targets that process; SIGKILL cannot
    // be caught or ignored. A dead pid returns ESRCH, which we discard.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// Run one reaper pass: snapshot processes, select leaked orphans, SIGKILL
/// each, and return how many were reaped. Logs each kill loudly. No-op (returns
/// 0) when `ps` is unavailable or nothing qualifies.
#[cfg(unix)]
pub fn reap_once(min_age_secs: u64) -> usize {
    let Some(snapshot) = ps_snapshot() else {
        return 0;
    };
    let self_pid = std::process::id() as i32;
    let orphans = find_orphans(&snapshot, min_age_secs, self_pid);
    for o in &orphans {
        tracing::warn!(
            pid = o.pid,
            age_secs = o.age_secs,
            kind = o.kind,
            command = %o.command,
            "reaping leaked orphan process (PPID=1, allow-listed, aged past threshold)"
        );
        sigkill(o.pid);
    }
    orphans.len()
}

#[cfg(not(unix))]
pub fn reap_once(_min_age_secs: u64) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etime_formats() {
        assert_eq!(parse_etime("05"), None); // SS alone is not a valid ps etime
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("01:30"), Some(90));
        assert_eq!(parse_etime("02:03:04"), Some(2 * 3600 + 3 * 60 + 4));
        assert_eq!(
            parse_etime("3-04:05:06"),
            Some(3 * 86400 + 4 * 3600 + 5 * 60 + 6)
        );
        assert_eq!(parse_etime("26-00:00:00"), Some(26 * 86400));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("garbage"), None);
        assert_eq!(parse_etime("00:99"), None); // out-of-range seconds
        assert_eq!(parse_etime("99:00:01"), Some(99 * 3600 + 1)); // hours may exceed 23
    }

    #[test]
    fn allow_list_matches_only_runner_spawns() {
        assert_eq!(
            reapable_kind("rsync -az --timeout=3600 src dst"),
            Some("ha-backup-rsync")
        );
        assert_eq!(
            reapable_kind("/usr/bin/rsync foo bar"),
            Some("ha-backup-rsync")
        );
        assert_eq!(
            reapable_kind("git-remote-https origin https://github.com/x/y"),
            Some("build-git-fetch")
        );
        assert_eq!(reapable_kind("git fetch --all"), Some("build-git-fetch"));
        assert_eq!(
            reapable_kind("ssh git@github.com-venkat git-upload-pack 'venkat/forge-fleet.git'"),
            Some("build-git-ssh")
        );
        // ssh that is NOT a git tunnel is spared.
        assert_eq!(reapable_kind("ssh user@host some-command"), None);
        // Servers and daemons that legitimately reparent to pid 1 are spared.
        assert_eq!(
            reapable_kind("llama-server --port 55000 --parallel 4"),
            None
        );
        assert_eq!(reapable_kind("/opt/mlx/bin/mlx_lm.server --model x"), None);
        assert_eq!(reapable_kind("vllm serve Qwen/Qwen3-30B"), None);
        assert_eq!(reapable_kind("forgefleetd start"), None);
        assert_eq!(reapable_kind("cargo build --release"), None);
        assert_eq!(reapable_kind(""), None);
    }

    #[test]
    fn find_orphans_requires_ppid1_and_age_and_allowlist() {
        // Columns: pid ppid etime command
        let ps = "\
  101     1   03:00:00 rsync -az --timeout=3600 a b
  102     1   00:10:00 rsync -az a b
  103   999   05:00:00 rsync -az a b
  104     1   04:00:00 llama-server --port 55000
  105     1   26-00:00:00 ssh git@github.com-venkat git-upload-pack 'x.git'
  106     1   02:30:00 git fetch --all
  107     1   01:59:59 git fetch --all
";
        // min_age = 2h (7200s). self_pid arbitrary (200).
        let got = find_orphans(ps, 7200, 200);
        let pids: Vec<i32> = got.iter().map(|o| o.pid).collect();
        // 101: ppid1, rsync, 3h     -> reap
        // 102: ppid1, rsync, 10m    -> too young, spared
        // 103: ppid999, rsync, 5h   -> live parent, spared
        // 104: ppid1, llama-server  -> not allow-listed, spared
        // 105: ppid1, ssh git tunnel, 26d -> reap
        // 106: ppid1, git fetch, 2h30m    -> reap
        // 107: ppid1, git fetch, 1h59m59s -> just under threshold, spared
        assert_eq!(pids, vec![101, 105, 106]);
        assert_eq!(got[0].kind, "ha-backup-rsync");
        assert_eq!(got[1].kind, "build-git-ssh");
        assert_eq!(got[2].kind, "build-git-fetch");
    }

    #[test]
    fn find_orphans_excludes_self_pid() {
        let ps = "  500     1   10:00:00 rsync -az a b\n";
        assert!(find_orphans(ps, 7200, 500).is_empty()); // self
        assert_eq!(find_orphans(ps, 7200, 999).len(), 1); // other
    }

    #[test]
    fn min_age_env_parsing() {
        // We can't safely mutate process env in parallel tests; just assert the
        // default constant is the documented 2h.
        assert_eq!(DEFAULT_MIN_AGE_SECS, 7200);
    }
}
