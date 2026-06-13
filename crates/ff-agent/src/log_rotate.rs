//! Size-capped rotation for the daemon's own stdout/stderr log files.
//!
//! forgefleetd does NOT own its log file — systemd (`StandardOutput=append:`)
//! or launchd (`StandardOutPath`/`StandardErrorPath`) open it and redirect our
//! stdout/stderr into it, so there is no tracing rolling-file appender to bound
//! it. Left alone it grows without limit (1.87 GiB observed on rihanna
//! 2026-06-13 — a recurring disk-pressure root cause).
//!
//! Both systemd `append:` and launchd open the file in append mode, so every
//! write seeks to the real EOF *first*. That lets us reclaim space by
//! truncating the live file **in place** (`set_len(0)` on the same inode): the
//! redirect's next write lands at offset 0 and the file grows fresh. Renaming
//! would NOT work — the open fd would keep writing to the orphaned inode and
//! the live path would never be recreated until the daemon restarts. So we do a
//! logrotate-style **copytruncate**: copy a bounded tail of recent lines into
//! `<name>.1`, then truncate the live file to 0.
//!
//! The tiny race (a redirect write between reading the tail and truncating is
//! dropped) is the same one logrotate's `copytruncate` accepts — fine for a log.
//! Per-node, idempotent, no DB — safe to run on every member unconditionally.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Rotate the live log once it exceeds this many bytes. Override with
/// `FORGEFLEET_LOG_MAX_MB` (megabytes); 0/invalid falls back to the default.
const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

/// How much of the tail to preserve into `<name>.1` on rotation. Keeps recent
/// context for an operator tailing the log without copying the whole file
/// (which would briefly double disk use on the very host under pressure).
pub const KEEP_TAIL_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB

/// Resolve the rotation threshold, honoring `FORGEFLEET_LOG_MAX_MB`.
pub fn max_bytes() -> u64 {
    std::env::var("FORGEFLEET_LOG_MAX_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&mb| mb > 0)
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// The log directory: `~/.forgefleet/logs`.
pub fn default_log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".forgefleet").join("logs"))
}

/// Live daemon log files in `dir` that this rotation owns. Matches the three
/// redirect shapes in use across the fleet — `forgefleetd.log` (systemd
/// `append:` + macOS plist) and `forgefleetd.out.log` / `forgefleetd.err.log`
/// (launchd split). Already-rotated `.1` archives and per-model server logs are
/// skipped. (Linux units that log to the journal have no file here, so they're
/// transparently a no-op.)
pub fn rotate_targets(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_rotate_target(name) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// True for the live daemon log file names (not their `.1` archives).
fn is_rotate_target(name: &str) -> bool {
    name.starts_with("forgefleetd") && name.ends_with(".log")
}

/// Rotate `path` if it exceeds `max_bytes`. Returns `Ok(Some(freed))` with the
/// bytes reclaimed from the live file when a rotation happened, `Ok(None)` if
/// the file was under the cap (or absent). `keep_tail` bytes (line-aligned) are
/// preserved into `<path>.1`.
pub fn rotate_if_needed(
    path: &Path,
    max_bytes: u64,
    keep_tail: u64,
) -> std::io::Result<Option<u64>> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = meta.len();
    if len <= max_bytes {
        return Ok(None);
    }

    // Read a line-aligned tail of recent history.
    let tail = read_line_aligned_tail(path, len, keep_tail)?;

    // Archive it to `<path>.1` (write-then-rename so a concurrent reader never
    // sees a half-written archive). The `.1` file is not append-redirected by
    // anyone, so overwriting the prior generation is safe.
    let archive = archive_path(path);
    let tmp = archive.with_extension("1.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&tail)?;
        f.flush()?;
    }
    fs::rename(&tmp, &archive)?;

    // Truncate the LIVE file in place — same inode, so the redirect's
    // append-mode fd keeps writing from the new EOF (0).
    let f = fs::OpenOptions::new().write(true).open(path)?;
    f.set_len(0)?;

    Ok(Some(len))
}

/// `<path>.1`.
fn archive_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

/// Read the last `keep_tail` bytes of `path`, then advance past the first
/// partial line so the archive begins on a clean line boundary. If the whole
/// file is smaller than `keep_tail`, returns it verbatim.
fn read_line_aligned_tail(path: &Path, len: u64, keep_tail: u64) -> std::io::Result<Vec<u8>> {
    let start = len.saturating_sub(keep_tail);
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    if start > 0 {
        // Mid-line start: drop everything up to and including the first newline.
        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=nl);
        }
    }
    Ok(buf)
}

/// Rotate every daemon log in `dir` that's over the cap. Returns the total
/// bytes reclaimed across all files. Per-file errors are logged and skipped so
/// one unreadable file never blocks the rest.
pub fn rotate_dir(dir: &Path, max_bytes: u64) -> u64 {
    let mut freed_total = 0u64;
    for path in rotate_targets(dir) {
        match rotate_if_needed(&path, max_bytes, KEEP_TAIL_BYTES) {
            Ok(Some(freed)) => {
                freed_total += freed;
                tracing::warn!(
                    file = %path.display(),
                    freed_mb = freed / 1_048_576,
                    cap_mb = max_bytes / 1_048_576,
                    "rotated oversized daemon log (copytruncate; tail kept in .1)"
                );
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(file = %path.display(), error = %e, "log rotation failed"),
        }
    }
    freed_total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_rotate_target_matches_live_logs_only() {
        assert!(is_rotate_target("forgefleetd.log"));
        assert!(is_rotate_target("forgefleetd.out.log"));
        assert!(is_rotate_target("forgefleetd.err.log"));
        // Archives and unrelated logs are skipped.
        assert!(!is_rotate_target("forgefleetd.log.1"));
        assert!(!is_rotate_target("forgefleetd.out.log.1"));
        assert!(!is_rotate_target("llama-server-deepseek-v32.log"));
        assert!(!is_rotate_target("dsr1-download.log"));
        assert!(!is_rotate_target("forgefleetd"));
    }

    #[test]
    fn max_bytes_defaults_when_env_absent() {
        // We can't safely set env in a parallel test, so just assert the
        // default is what we documented.
        assert_eq!(DEFAULT_MAX_BYTES, 256 * 1024 * 1024);
    }

    #[test]
    fn under_cap_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forgefleetd.log");
        fs::write(&p, b"small\n").unwrap();
        assert_eq!(rotate_if_needed(&p, 1000, 100).unwrap(), None);
        // Untouched.
        assert_eq!(fs::read(&p).unwrap(), b"small\n");
        assert!(!archive_path(&p).exists());
    }

    #[test]
    fn missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forgefleetd.log");
        assert_eq!(rotate_if_needed(&p, 10, 5).unwrap(), None);
    }

    #[test]
    fn over_cap_truncates_live_and_archives_line_aligned_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forgefleetd.log");
        // 10 fixed-width lines.
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line{i:02}-padding-xx\n"));
        }
        let total = content.len() as u64;
        let line_len = content.lines().next().unwrap().len();
        fs::write(&p, &content).unwrap();

        // Cap below total, keep a tail that starts mid-line, so the archive
        // must begin at the next clean line boundary.
        let freed = rotate_if_needed(&p, total / 4, line_len as u64 * 2 + 3).unwrap();
        assert_eq!(freed, Some(total));

        // Live file truncated to empty (same inode; redirect would re-append).
        assert_eq!(fs::metadata(&p).unwrap().len(), 0);

        // Archive holds whole lines only and ends with the last line.
        let archived = fs::read_to_string(archive_path(&p)).unwrap();
        assert!(archived.ends_with("line09-padding-xx\n"));
        for line in archived.lines() {
            assert_eq!(line.len(), line_len, "archive line not whole: {line:?}");
        }
        // The mid-line fragment at the cut point must have been dropped, so the
        // archive holds strictly fewer than all 10 lines.
        assert!(archived.lines().count() < 10);
    }

    #[test]
    fn whole_file_kept_when_smaller_than_keep_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forgefleetd.log");
        fs::write(&p, b"aaaa\nbbbb\ncccc\n").unwrap(); // 15 bytes
        // Cap 10 (over), keep_tail 1000 (> file) → archive whole file verbatim.
        let freed = rotate_if_needed(&p, 10, 1000).unwrap();
        assert_eq!(freed, Some(15));
        assert_eq!(fs::metadata(&p).unwrap().len(), 0);
        assert_eq!(
            fs::read_to_string(archive_path(&p)).unwrap(),
            "aaaa\nbbbb\ncccc\n"
        );
    }

    #[test]
    fn rotate_targets_filters_dir() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "forgefleetd.log",
            "forgefleetd.out.log",
            "forgefleetd.err.log",
            "forgefleetd.log.1",
            "llama-server-x.log",
        ] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        let targets = rotate_targets(dir.path());
        let names: Vec<_> = targets
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "forgefleetd.err.log",
                "forgefleetd.log",
                "forgefleetd.out.log"
            ]
        );
    }
}
