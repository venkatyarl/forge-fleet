//! Periodic reaper for the **legacy `ff daemon` supervisor** processes that
//! linger alongside the production `forgefleetd` daemon.
//!
//! ## The leak this cleans up
//!
//! ForgeFleet historically ran two cooperating processes per host:
//! `forgefleetd` (pulse + leader election + scheduler-pass) and a sibling
//! `ff daemon` CLI process (the worker-pass: defer-worker, disk sampler,
//! deployment reconciler). The worker loops were since **folded into
//! `forgefleetd` itself** (see `ff_agent::defer_worker`, the disk-sampler tick,
//! and the deployment reconciler in `forgefleetd`), so `ff daemon` is now pure
//! legacy — [[feedback_two_daemons]]: any tick added only to `ff daemon`
//! silently no-ops, and worse, a *stale* `ff daemon` keeps running the OLD
//! binary's logic.
//!
//! In practice the old `ff daemon` processes were never stopped when hosts were
//! migrated to `forgefleetd`. On 2026-06-14 **12 of 15** fleet hosts were still
//! running a multi-week-old `ff daemon` (james carried two, 15 days old;
//! several ran `--defer-interval 15 --reconcile-interval 60`). These stale
//! supervisors:
//!   - **race forgefleetd's worker** — both claim `shell`/`http`/`upgrade`
//!     deferred tasks. `FOR UPDATE SKIP LOCKED` stops double-*execution*, but a
//!     15-day-old `ff` binary that wins the claim runs the task with **stale,
//!     pre-fix logic** (pre-atomic-install, pre-process-group-kill, …).
//!   - **duplicate the reconciler / disk sampler**, writing the same rows
//!     forgefleetd already maintains.
//!
//! ## Why this is safe to SIGTERM on every node
//!
//! A process is only reaped when **all** hold:
//!   1. **command is `<…>/ff daemon …`** — the program basename is exactly `ff`
//!      AND its first argument is `daemon`. The production daemon is
//!      `forgefleetd` (a *different* basename), so it can never match; neither
//!      can `ff <any-other-verb>`. This is the same precise allow-list style as
//!      the leaked-orphan reaper — we name exactly what we kill.
//!   2. **not a single-pass run** — a command containing `--once` is the
//!      legitimate cron/one-shot mode that exits on its own; never reaped.
//!   3. **age ≥ `FORGEFLEET_LEGACY_DAEMON_REAP_SECS`** (default 300s = 5min) —
//!      a brief grace so an operator's just-launched debug `ff daemon`, or a
//!      transition window during an upgrade, is spared. A real legacy
//!      supervisor has run for hours-to-weeks.
//!
//! ## Disabling the supervising unit (the part that makes this stick)
//!
//! On most hosts the legacy `ff daemon` is not a stray process — it is
//! supervised by a systemd user/system unit (`forgefleet-daemon.service` or the
//! templated `forgefleet-daemon@<host>.service`, `Restart=on-failure`,
//! `ExecStart=…/ff daemon`). A bare SIGTERM there is pointless: systemd
//! respawns it in `RestartSec`. So for each legacy process we first locate its
//! supervising unit from `/proc/<pid>/cgroup` and `systemctl disable --now` it
//! (honoring user vs. system scope), which both stops it AND prevents the
//! respawn. We disable a unit **only** when the process under it is a confirmed
//! `ff daemon` and the unit is not `forgefleetd.service` — so the production
//! daemon's own unit is never touched. Reversible: an operator can re-enable.
//!
//! After (or instead of, when there is no unit — a `nohup`'d daemon) disabling,
//! we send **SIGTERM** (not SIGKILL): `ff daemon` is a cooperative daemon with a
//! graceful-shutdown handler, so a clean stop lets it release its NATS/PG
//! handles. A daemon that ignores SIGTERM is caught again on the next tick.
//!
//! `ps`/`/proc`-only, no DB, no leader gate — like the orphan reaper and disk
//! sampler. Set `FORGEFLEET_LEGACY_DAEMON_REAP_SECS=0` to disable.

/// Default minimum age before a legacy `ff daemon` is reaped. 5min is long
/// enough to spare an operator's momentary debug invocation or an upgrade
/// transition window, yet trivially below the hours-to-weeks a real leaked
/// supervisor has been running.
const DEFAULT_MIN_AGE_SECS: u64 = 300;

/// Resolve the minimum-age threshold, honoring
/// `FORGEFLEET_LEGACY_DAEMON_REAP_SECS`. Returns `None` (reaper disabled) when
/// the override is `0`.
pub fn min_age_secs() -> Option<u64> {
    match std::env::var("FORGEFLEET_LEGACY_DAEMON_REAP_SECS") {
        Ok(s) => match s.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(DEFAULT_MIN_AGE_SECS),
        },
        Err(_) => Some(DEFAULT_MIN_AGE_SECS),
    }
}

/// A legacy `ff daemon` process we decided to reap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDaemon {
    pub pid: i32,
    pub age_secs: u64,
    /// The full command line (logged for the operator).
    pub command: String,
}

/// Parse a `ps` elapsed-time field (`[[DD-]HH:]MM:SS`) into seconds. Identical
/// format on BSD `ps` (macOS) and GNU/procps `ps` (Linux).
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
/// (the program), lower-cased. `/home/x/.local/bin/ff daemon` → `ff`.
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

/// Decide whether a command line is a long-running legacy `ff daemon`
/// supervisor we should reap.
///
/// True only when the program basename is exactly `ff`, its first argument is
/// `daemon`, and it is NOT a `--once` single-pass run. `forgefleetd` (a
/// different basename) and every other `ff` subcommand are excluded by
/// construction.
pub fn is_legacy_ff_daemon(command: &str) -> bool {
    if program_basename(command) != "ff" {
        return false;
    }
    let mut args = command.split_whitespace().skip(1);
    // First argument must be the `daemon` subcommand.
    if args.next() != Some("daemon") {
        return false;
    }
    // `--once` is the legitimate single-pass/cron mode; leave it alone.
    !command.split_whitespace().any(|t| t == "--once")
}

/// Pure core: given raw `ps` output (lines of `pid ppid etime command...`), the
/// age threshold, and our own pid, return the legacy daemons to reap.
///
/// Selection requires a legacy-`ff-daemon` command **and** age ≥ `min_age_secs`
/// **and** `pid != self_pid`. `forgefleetd` never matches the command test, so
/// the self exclusion is belt-and-suspenders. The `ppid` column is parsed (for
/// format-compatibility with the orphan reaper's `ps` layout) but intentionally
/// not used as a gate: a stale supervisor is wrong regardless of its parent.
pub fn find_legacy_daemons(ps_output: &str, min_age_secs: u64, self_pid: i32) -> Vec<LegacyDaemon> {
    let mut out = Vec::new();
    for line in ps_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(pid) = it.next().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        // ppid column — parsed to keep the same `ps` format as the orphan
        // reaper, but not used as a selection gate.
        let Some(_ppid) = it.next().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Some(etime) = it.next().and_then(parse_etime) else {
            continue;
        };
        let command = it.collect::<Vec<_>>().join(" ");
        if command.is_empty() || pid == self_pid || etime < min_age_secs {
            continue;
        }
        if !is_legacy_ff_daemon(&command) {
            continue;
        }
        out.push(LegacyDaemon {
            pid,
            age_secs: etime,
            command,
        });
    }
    out
}

/// The systemd unit supervising a legacy daemon, resolved from its cgroup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisingUnit {
    /// Unit name, e.g. `forgefleet-daemon.service` or
    /// `forgefleet-daemon@james.service`.
    pub name: String,
    /// `true` when the unit lives in the system manager (needs `sudo
    /// systemctl`), `false` for a `--user` unit.
    pub system_scope: bool,
}

/// Undo systemd cgroup hex-escaping for the characters that appear in unit
/// names (`-` → `\x2d`, `.` → `\x2e`, `@` → `\x40`). Only these three matter
/// for forgefleet unit names; anything else is left verbatim.
fn unescape_systemd(s: &str) -> String {
    s.replace("\\x2d", "-")
        .replace("\\x2e", ".")
        .replace("\\x40", "@")
}

/// Parse `/proc/<pid>/cgroup` contents to find the supervising systemd unit and
/// whether it is system- or user-scoped.
///
/// The cgroup-v2 line looks like
/// `0::/user.slice/user-1000.slice/user@1000.service/app.slice/forgefleet-daemon.service`
/// (user scope) or
/// `0::/system.slice/system-forgefleet\x2ddaemon.slice/forgefleet-daemon@james.service`
/// (system scope). We take the last `*.service` path component as the unit and
/// classify the scope by whether the path is anchored in a `user@` manager.
///
/// Returns `None` when there is no `.service` leaf (a `nohup`'d daemon sitting in
/// a bare slice) or when the leaf is `forgefleetd.service` — we never disable the
/// production daemon's own unit.
pub fn parse_supervising_unit(cgroup: &str) -> Option<SupervisingUnit> {
    // cgroup v2 is a single `0::<path>` line; older v1 has many `n:ctrl:<path>`
    // lines. Scan every line's path for a `.service` leaf and prefer the last.
    let mut best: Option<SupervisingUnit> = None;
    for line in cgroup.lines() {
        let path = line.rsplit_once(':').map(|(_, p)| p).unwrap_or(line);
        let user_scoped = path.contains("/user@") || path.contains("user.slice");
        // Walk components, remember the last that names a `.service` unit.
        for comp in path.split('/') {
            if comp.ends_with(".service") {
                let name = unescape_systemd(comp);
                best = Some(SupervisingUnit {
                    name,
                    system_scope: !user_scoped,
                });
            }
        }
    }
    let unit = best?;
    // Never touch the production daemon's own unit, whatever the process was.
    if unit.name == "forgefleetd.service" || unit.name.starts_with("forgefleetd") {
        return None;
    }
    Some(unit)
}

/// Read `/proc/<pid>/cgroup` and resolve the supervising unit. Linux only;
/// returns `None` on read failure or no resolvable unit.
#[cfg(target_os = "linux")]
fn supervising_unit_of(pid: i32) -> Option<SupervisingUnit> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    parse_supervising_unit(&contents)
}

/// `systemctl [--user] disable --now <unit>` (system scope shells out via
/// `sudo -n`). Logs the outcome. Best-effort: a failure (no passwordless sudo,
/// unit already gone) is logged and the SIGTERM backstop still runs.
#[cfg(target_os = "linux")]
fn disable_unit(unit: &SupervisingUnit) {
    let (program, args): (&str, Vec<String>) = if unit.system_scope {
        (
            "sudo",
            vec![
                "-n".into(),
                "systemctl".into(),
                "disable".into(),
                "--now".into(),
                unit.name.clone(),
            ],
        )
    } else {
        (
            "systemctl",
            vec![
                "--user".into(),
                "disable".into(),
                "--now".into(),
                unit.name.clone(),
            ],
        )
    };
    match std::process::Command::new(program).args(&args).output() {
        Ok(out) if out.status.success() => {
            tracing::warn!(
                unit = %unit.name,
                system_scope = unit.system_scope,
                "disabled legacy ff-daemon supervising unit (systemctl disable --now)"
            );
        }
        Ok(out) => {
            tracing::warn!(
                unit = %unit.name,
                system_scope = unit.system_scope,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "could not disable legacy ff-daemon unit (SIGTERM backstop will still fire)"
            );
        }
        Err(e) => {
            tracing::warn!(unit = %unit.name, error = %e, "systemctl invocation failed");
        }
    }
}

/// Snapshot every process as `pid ppid etime command`. POSIX columns, identical
/// on macOS and Linux; trailing `=` suppresses the header row.
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

/// SIGTERM a single pid. ESRCH (already gone) is ignored.
#[cfg(unix)]
fn sigterm(pid: i32) {
    // SAFETY: `kill(2)` on a positive pid targets that process. SIGTERM is the
    // graceful stop signal; the legacy daemon's shutdown handler releases its
    // NATS/PG handles. A dead pid returns ESRCH, which we discard.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

/// Run one reaper pass: snapshot processes, select legacy `ff daemon`
/// supervisors, SIGTERM each, and return how many were signalled. Logs each
/// kill loudly. No-op (returns 0) when `ps` is unavailable or nothing qualifies.
#[cfg(unix)]
pub fn reap_once(min_age_secs: u64) -> usize {
    let Some(snapshot) = ps_snapshot() else {
        return 0;
    };
    let self_pid = std::process::id() as i32;
    let daemons = find_legacy_daemons(&snapshot, min_age_secs, self_pid);
    for d in &daemons {
        tracing::warn!(
            pid = d.pid,
            age_secs = d.age_secs,
            command = %d.command,
            "reaping legacy `ff daemon` supervisor (superseded by forgefleetd)"
        );
        // On Linux, disable the supervising systemd unit first so it cannot
        // respawn the daemon after we SIGTERM it. `disable --now` also stops
        // the unit, so the SIGTERM below is a backstop for nohup'd daemons.
        #[cfg(target_os = "linux")]
        if let Some(unit) = supervising_unit_of(d.pid) {
            disable_unit(&unit);
        }
        sigterm(d.pid);
    }
    daemons.len()
}

#[cfg(not(unix))]
pub fn reap_once(_min_age_secs: u64) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_long_running_ff_daemon() {
        // The two real shapes seen on the fleet 2026-06-14.
        assert!(is_legacy_ff_daemon("/home/marcus/.local/bin/ff daemon"));
        assert!(is_legacy_ff_daemon(
            "/home/priya/.local/bin/ff daemon --defer-interval 15 --disk-interval 300 --reconcile-interval 60"
        ));
        assert!(is_legacy_ff_daemon("ff daemon --scheduler"));

        // forgefleetd (the production daemon) is a different basename — never matched.
        assert!(!is_legacy_ff_daemon("/home/x/.local/bin/forgefleetd"));
        assert!(!is_legacy_ff_daemon(
            "/home/x/.local/bin/forgefleetd --node-name ace start"
        ));
        // Other ff subcommands are never matched.
        assert!(!is_legacy_ff_daemon("/home/x/.local/bin/ff fleet health"));
        assert!(!is_legacy_ff_daemon("ff run \"do a thing\""));
        assert!(!is_legacy_ff_daemon("ff daemon-helper"));
        // `--once` single-pass mode is spared.
        assert!(!is_legacy_ff_daemon("ff daemon --once"));
        assert!(!is_legacy_ff_daemon(
            "/home/x/.local/bin/ff daemon --scheduler --once"
        ));
        // ffmpeg / lookalikes are not `ff`.
        assert!(!is_legacy_ff_daemon("ffmpeg daemon"));
        assert!(!is_legacy_ff_daemon(""));
    }

    #[test]
    fn find_requires_command_and_age_and_not_self() {
        // Columns: pid ppid etime command
        let ps = "\
  101     1   15-15:07:26 /home/james/.local/bin/ff daemon
  102     1   00:02:00 /home/x/.local/bin/ff daemon
  103  4242   01:00:00 /home/x/.local/bin/ff daemon --reconcile-interval 60
  104     1   10:00:00 /home/x/.local/bin/forgefleetd --node-name x start
  105     1   03:00:00 /home/x/.local/bin/ff daemon --once
  106     1   03:00:00 /home/x/.local/bin/ff fleet health
";
        // min_age = 300s (5min). self_pid = 999.
        let got = find_legacy_daemons(ps, 300, 999);
        let pids: Vec<i32> = got.iter().map(|d| d.pid).collect();
        // 101: ff daemon, 15d            -> reap
        // 102: ff daemon, 2min           -> too young, spared
        // 103: ff daemon (live parent)   -> reap (ppid not a gate)
        // 104: forgefleetd               -> not ff daemon, spared
        // 105: ff daemon --once          -> single-pass, spared
        // 106: ff fleet health           -> not the daemon subcommand, spared
        assert_eq!(pids, vec![101, 103]);
        assert_eq!(got[0].age_secs, 15 * 86_400 + 15 * 3_600 + 7 * 60 + 26);
    }

    #[test]
    fn find_excludes_self_pid() {
        let ps = "  500     1   10:00:00 /home/x/.local/bin/ff daemon\n";
        assert!(find_legacy_daemons(ps, 300, 500).is_empty()); // self
        assert_eq!(find_legacy_daemons(ps, 300, 999).len(), 1); // other
    }

    #[test]
    fn etime_parses_fleet_shapes() {
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("01:00:00"), Some(3600));
        assert_eq!(
            parse_etime("15-15:07:26"),
            Some(15 * 86_400 + 15 * 3_600 + 7 * 60 + 26)
        );
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("00:99"), None);
    }

    #[test]
    fn min_age_default_is_5min() {
        assert_eq!(DEFAULT_MIN_AGE_SECS, 300);
    }

    #[test]
    fn parse_user_scoped_unit() {
        // The real shape from james pid 4753 (user manager).
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/forgefleet-daemon.service\n";
        let u = parse_supervising_unit(cg).expect("unit");
        assert_eq!(u.name, "forgefleet-daemon.service");
        assert!(!u.system_scope);
    }

    #[test]
    fn parse_system_scoped_templated_unit() {
        // The real shape from james pid 5304 (system manager, templated +
        // hex-escaped slice name).
        let cg =
            "0::/system.slice/system-forgefleet\\x2ddaemon.slice/forgefleet-daemon@james.service\n";
        let u = parse_supervising_unit(cg).expect("unit");
        assert_eq!(u.name, "forgefleet-daemon@james.service");
        assert!(u.system_scope);
    }

    #[test]
    fn parse_skips_forgefleetd_own_unit() {
        // We must never disable the production daemon's own unit.
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/forgefleetd.service\n";
        assert_eq!(parse_supervising_unit(cg), None);
    }

    #[test]
    fn parse_no_unit_for_bare_slice() {
        // A nohup'd daemon sitting in a bare user slice has no `.service` leaf.
        let cg = "0::/user.slice/user-1000.slice/session-3.scope\n";
        assert_eq!(parse_supervising_unit(cg), None);
        assert_eq!(parse_supervising_unit(""), None);
    }

    #[test]
    fn unescape_systemd_names() {
        assert_eq!(
            unescape_systemd("forgefleet\\x2ddaemon"),
            "forgefleet-daemon"
        );
        assert_eq!(unescape_systemd("foo\\x40bar"), "foo@bar");
        assert_eq!(unescape_systemd("plain.service"), "plain.service");
    }
}
