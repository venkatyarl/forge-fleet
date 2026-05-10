//! Shared CLI utilities — formatting helpers, color constants, etc.

use std::path::PathBuf;

// ─── Color constants ───────────────────────────────────────────────────────

pub const GREEN: &str = "\x1b[32m";
pub const CYAN: &str = "\x1b[36m";
pub const YELLOW: &str = "\x1b[33m";
pub const RED: &str = "\x1b[31m";
pub const RESET: &str = "\x1b[0m";

// ─── Formatting helpers ────────────────────────────────────────────────────

pub fn human_bytes(n: u64) -> String {
    let (unit, v) = if n >= 1 << 40 {
        ("TiB", n as f64 / (1u64 << 40) as f64)
    } else if n >= 1 << 30 {
        ("GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        ("MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        ("KiB", n as f64 / (1u64 << 10) as f64)
    } else {
        return format!("{n}B");
    };
    format!("{v:.1}{unit}")
}

pub fn human_bytes_i64(n: i64) -> String {
    if n < 0 {
        return "0 B".into();
    }
    human_bytes(n as u64)
}

/// Truncate a string for inline status display, with a leading ellipsis.
pub fn trunc_for_status(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let suffix: String = s
        .chars()
        .rev()
        .take(take)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{suffix}")
}

/// Truncate a string to `n` characters, appending an ellipsis if truncated.
pub fn truncate_for_col(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

// ─── Shell helpers ─────────────────────────────────────────────────────────

/// POSIX shell single-quote escape: wraps the argument in single quotes and
/// escapes any embedded single quotes. Safe for pasting into `sh -c`.
pub fn shell_escape_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ─── Identity helpers ──────────────────────────────────────────────────────

/// Best-effort tag for `updated_by`: `user@host`.
static WHOAMI_TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn whoami_tag() -> String {
    WHOAMI_TAG
        .get_or_init(|| {
            let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
            let host = std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".into());
            format!("{user}@{host}")
        })
        .clone()
}

// ─── Path helpers ──────────────────────────────────────────────────────────

/// Expand a leading `~` to `$HOME` so config strings like "~/models" resolve to absolute paths.
pub fn expand_tilde(p: &str, home: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else if p == "~" {
        PathBuf::from(home)
    } else {
        PathBuf::from(p)
    }
}

// ─── Pulse helpers ─────────────────────────────────────────────────────────

pub fn resolve_pulse_redis_url() -> String {
    if let Ok(url) = std::env::var("FORGEFLEET_REDIS_URL")
        && !url.trim().is_empty()
    {
        return url;
    }
    const FALLBACK: &str = "redis://localhost:6380";
    let Some(home) = dirs::home_dir() else {
        return FALLBACK.to_string();
    };
    let path = home.join(".forgefleet/fleet.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return FALLBACK.to_string();
    };
    let Ok(val) = toml::from_str::<toml::Value>(&text) else {
        return FALLBACK.to_string();
    };
    val.get("redis")
        .and_then(|r| r.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| FALLBACK.to_string())
}

pub fn pulse_reader() -> anyhow::Result<ff_pulse::reader::PulseReader> {
    let url = resolve_pulse_redis_url();
    ff_pulse::reader::PulseReader::new(&url)
        .map_err(|e| anyhow::anyhow!("pulse: connect {url}: {e}"))
}

/// Alias for `truncate_for_col` — same semantics, different name used in legacy code.
pub fn truncate_str(s: &str, max: usize) -> String {
    truncate_for_col(s, max)
}

/// Parse a duration like "5m", "1h", "24h", "30s" into seconds.
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = match s.chars().position(|c| !c.is_ascii_digit() && c != '.') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "s"),
    };
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "s" | "" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        _ => return None,
    };
    Some((n * mult).round() as u64)
}
