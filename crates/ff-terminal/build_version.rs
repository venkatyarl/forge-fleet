// Shared build-script helper. `include!`d from both build.rs files so the
// version emission logic stays single-sourced without adding a build-dep.

/// Emit `FF_GIT_SHA`, `FF_BUILD_VERSION`, `FF_GIT_STATE` as rustc env vars.
/// NEVER fails — every probe falls through to sensible defaults.
fn emit_version_env() {
    let sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    let state = detect_git_state(&sha);
    let (date_key, date_human) = today_date_parts();
    let counter = bump_daily_counter(&date_key);
    let build_version = format!("{date_human}_{counter}");

    let built_at = Command::new("date")
        .args(["+%Y-%m-%d %H:%M:%S"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=FF_GIT_SHA={sha}");
    println!("cargo:rustc-env=FF_BUILD_VERSION={build_version}");
    println!("cargo:rustc-env=FF_GIT_STATE={state}");
    println!("cargo:rustc-env=FF_BUILT_AT={built_at}");
}

fn git_short_sha() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Returns one of `"dirty" | "unpushed" | "pushed" | "unknown"`.
/// A build is:
///   - dirty    = working tree OR index has uncommitted changes
///   - unpushed = clean but HEAD is not an ancestor of `origin/main`
///   - pushed   = clean AND HEAD is an ancestor of `origin/main`
///   - unknown  = no git / probe errored
fn detect_git_state(sha: &str) -> String {
    if sha == "unknown" {
        return "unknown".to_string();
    }
    let diff = Command::new("git").args(["diff", "--quiet"]).status();
    let cached = Command::new("git").args(["diff", "--cached", "--quiet"]).status();
    let dirty = matches!(&diff, Ok(s) if !s.success())
        || matches!(&cached, Ok(s) if !s.success());
    if dirty {
        return "dirty".to_string();
    }
    // Clean — is HEAD in origin/main's history?
    let ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", "HEAD", "origin/main"])
        .status();
    match ancestor {
        Ok(s) if s.success() => "pushed".to_string(),
        Ok(_) => "unpushed".to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Returns (`"YYYY-M-D"`, `"YYYY.M.D"`) for today in system-local time.
/// Uses `std::time` + a manual proleptic-Gregorian breakdown (no chrono).
fn today_date_parts() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Best-effort local offset via the `date` shell tool. Falls back to UTC.
    let offset_secs = Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| parse_zone(s.trim()))
        .unwrap_or(0);
    let (y, m, d) = ymd_from_unix(secs + offset_secs);
    (format!("{y}-{m}-{d}"), format!("{y}.{m}.{d}"))
}

fn parse_zone(z: &str) -> Option<i64> {
    // `+HHMM` / `-HHMM`
    if z.len() != 5 {
        return None;
    }
    let sign: i64 = if z.starts_with('+') { 1 } else if z.starts_with('-') { -1 } else { return None };
    let h: i64 = z[1..3].parse().ok()?;
    let m: i64 = z[3..5].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

fn ymd_from_unix(secs: i64) -> (i32, u32, u32) {
    // Days since 1970-01-01 (Thu).
    let days = secs.div_euclid(86_400);
    // Howard Hinnant's civil-date algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d)
}

/// Increment (or create) `~/.forgefleet/builds/YYYY-M-D.count` atomically.
/// ANY error → counter of 1. Never panics, never fails the build.
fn bump_daily_counter(date_key: &str) -> u32 {
    let home = std::env::var("HOME").ok();
    let Some(home) = home else { return 1 };
    let dir = std::path::PathBuf::from(home).join(".forgefleet").join("builds");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{date_key}.count"));
    let current: u32 = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let next = current.saturating_add(1).max(1);
    let _ = std::fs::write(&path, next.to_string());
    next
}
