//! Software inventory collector — probes the local machine for installed
//! developer/runtime software and reports `(id, version, install_source,
//! install_path)` tuples suitable for inclusion in [`PulseBeatV2::installed_software`].
//!
//! Detection is best-effort: each probe runs a small shell command
//! (`which <bin>`, `<bin> --version`, etc.), and any failure silently omits
//! that entry from the returned `Vec`. No errors are surfaced to the caller.
//!
//! The set of probed IDs matches what the ForgeFleet config seeds in
//! `config/software.toml` — so the materializer can join on ID.

use regex::Regex;

use crate::beat_v2::InstalledSoftware;

/// Probes the local machine for installed software.
pub struct SoftwareCollector {
    /// IDs that this collector knows how to probe for. Used by callers who
    /// want to cross-check against the catalog.
    pub known_ids: Vec<&'static str>,
}

impl Default for SoftwareCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftwareCollector {
    pub fn new() -> Self {
        Self {
            known_ids: vec![
                "ff",
                "ff_git",
                "forgefleetd",
                "forgefleetd_git",
                "openclaw",
                "gh",
                "op",
                "rustup",
                "llama.cpp",
                "mlx_lm",
                "vllm",
                "ollama",
                "node",
                "python",
                "docker",
                // OS is derived at runtime: one of
                // os-macos / os-ubuntu-22.04 / os-ubuntu-24.04 / os-dgx / os-windows.
                "os-macos",
                "os-ubuntu-22.04",
                "os-ubuntu-24.04",
                "os-dgx",
                "os-windows",
            ],
        }
    }

    /// Detect installed software on this machine. Entries that can't be
    /// resolved are simply omitted.
    pub fn detect(&self) -> Vec<InstalledSoftware> {
        let mut out: Vec<InstalledSoftware> = Vec::new();

        // ── ff ──────────────────────────────────────────────────────────
        //
        // `ff --version` prints one of two shapes, both supported here:
        //   - legacy:  `ff 2026.4.7 (build 8355028d1)`
        //   - current: `ff 2026.4.21_5 (pushed 8355028d12)`
        //
        // Semver / build-version goes to `ff`; the SHA goes to `ff_git`.
        // When the new shape is detected, the git-state token is stashed
        // in the `metadata` JSONB field on BOTH rows so the auto-upgrade
        // gate can read it without re-probing the leader.
        if let Some(path) = which("ff") {
            if let Some(raw) = run("ff", &["--version"]) {
                let parsed = parse_ff_version_line(&raw);
                if let Some(ver) = parsed.version.clone() {
                    let meta = parsed.git_state.clone().map(|s| {
                        serde_json::json!({ "git_state": s })
                    });
                    out.push(InstalledSoftware {
                        id: "ff".into(),
                        version: ver,
                        install_source: Some(classify_ff_source(&path)),
                        install_path: Some(path.clone()),
                        metadata: meta,
                    });
                }
                if let Some(sha) = parsed.sha {
                    let meta = parsed.git_state.map(|s| {
                        serde_json::json!({ "git_state": s })
                    });
                    out.push(InstalledSoftware {
                        id: "ff_git".into(),
                        version: sha,
                        install_source: Some(classify_ff_source(&path)),
                        install_path: Some(path),
                        metadata: meta,
                    });
                } else {
                    tracing::debug!(
                        raw = %raw,
                        "ff --version has no SHA suffix — skipping ff_git row"
                    );
                }
            }
        }

        // ── forgefleetd ─────────────────────────────────────────────────
        //
        // Same dual-shape parse as `ff` above: semver/build-version →
        // `forgefleetd`, SHA → `forgefleetd_git`, plus `git_state` in
        // `metadata` when present. Banner word is "forgefleet", not
        // "forgefleetd" — the parser normalizes either prefix.
        if let Some(path) = which("forgefleetd") {
            if let Some(raw) = run("forgefleetd", &["--version"]) {
                let parsed = parse_ff_version_line(&raw);
                if let Some(ver) = parsed.version.clone() {
                    let meta = parsed.git_state.clone().map(|s| {
                        serde_json::json!({ "git_state": s })
                    });
                    out.push(InstalledSoftware {
                        id: "forgefleetd".into(),
                        version: ver,
                        install_source: Some(classify_ff_source(&path)),
                        install_path: Some(path.clone()),
                        metadata: meta,
                    });
                }
                if let Some(sha) = parsed.sha {
                    let meta = parsed.git_state.map(|s| {
                        serde_json::json!({ "git_state": s })
                    });
                    out.push(InstalledSoftware {
                        id: "forgefleetd_git".into(),
                        version: sha,
                        install_source: Some(classify_ff_source(&path)),
                        install_path: Some(path),
                        metadata: meta,
                    });
                } else {
                    tracing::debug!(
                        raw = %raw,
                        "forgefleetd --version has no SHA suffix — skipping forgefleetd_git row"
                    );
                }
            }
        }

        // ── openclaw ────────────────────────────────────────────────────
        if let Some(path) = which("openclaw") {
            if let Some(raw) = run("openclaw", &["--version"]) {
                let first = raw.lines().next().unwrap_or("").to_string();
                if let Some(ver) = regex_capture(&first, r"OpenClaw\s+(\S+)") {
                    let src = if path.starts_with("/opt/homebrew/") || path.starts_with("/usr/local/") {
                        Some("npm".to_string())
                    } else {
                        None
                    };
                    out.push(InstalledSoftware {
                        id: "openclaw".into(),
                        version: ver,
                        install_source: src,
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── gh ──────────────────────────────────────────────────────────
        if let Some(path) = which("gh") {
            if let Some(raw) = run("gh", &["--version"]) {
                let first = raw.lines().next().unwrap_or("").to_string();
                if let Some(ver) = regex_capture(&first, r"gh version\s+(\S+)") {
                    out.push(InstalledSoftware {
                        id: "gh".into(),
                        version: ver,
                        install_source: classify_pkg_source(&path),
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── op ──────────────────────────────────────────────────────────
        if let Some(path) = which("op") {
            if let Some(raw) = run("op", &["--version"]) {
                let ver = raw.trim().to_string();
                if !ver.is_empty() {
                    out.push(InstalledSoftware {
                        id: "op".into(),
                        version: ver,
                        install_source: classify_pkg_source(&path),
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── rustup ──────────────────────────────────────────────────────
        if let Some(path) = which("rustup") {
            if let Some(raw) = run("rustup", &["--version"]) {
                let first = raw.lines().next().unwrap_or("").to_string();
                if let Some(ver) = regex_capture(&first, r"rustup\s+(\S+)") {
                    out.push(InstalledSoftware {
                        id: "rustup".into(),
                        version: ver,
                        install_source: Some("direct".to_string()),
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── llama.cpp (llama-server) ────────────────────────────────────
        if let Some(path) = which("llama-server") {
            let ver = run("llama-server", &["--version"])
                .and_then(|raw| {
                    let first = raw.lines().next().unwrap_or("").to_string();
                    regex_capture(&first, r"version:\s*(\S+)")
                })
                .or_else(|| {
                    // Fallback: try git log in ~/llama.cpp
                    let home = std::env::var("HOME").ok()?;
                    let llama_dir = format!("{home}/llama.cpp");
                    run(
                        "git",
                        &["-C", &llama_dir, "log", "-1", "--format=%h"],
                    )
                });
            if let Some(ver) = ver {
                out.push(InstalledSoftware {
                    id: "llama.cpp".into(),
                    version: ver,
                    install_source: Some("direct".to_string()),
                    install_path: Some(path),
                    metadata: None,
                });
            }
        }

        // ── mlx_lm (macOS only, via python3 -c) ────────────────────────
        // Only record a version when `python3 -c "import mlx_lm; ..."` exits 0
        // AND stdout is a clean short version string. If the module isn't
        // installed, the command exits non-zero and we silently omit the row.
        if std::env::consts::OS == "macos" {
            if let Some(ver) = run_python_version_probe("mlx_lm") {
                out.push(InstalledSoftware {
                    id: "mlx_lm".into(),
                    version: ver,
                    install_source: Some("pip".to_string()),
                    install_path: None,
                    metadata: None,
                });
            }
        }

        // ── vllm (Linux only, via python3 -c) ──────────────────────────
        // Same guardrail as mlx_lm above — must exit 0 with a clean version.
        if std::env::consts::OS == "linux" {
            if let Some(ver) = run_python_version_probe("vllm") {
                out.push(InstalledSoftware {
                    id: "vllm".into(),
                    version: ver,
                    install_source: Some("pip".to_string()),
                    install_path: None,
                    metadata: None,
                });
            }
        }

        // ── ollama ──────────────────────────────────────────────────────
        if let Some(path) = which("ollama") {
            if let Some(raw) = run("ollama", &["--version"]) {
                let first = raw.lines().next().unwrap_or("").to_string();
                let ver = regex_capture(&first, r"ollama version is\s+(\S+)")
                    .or_else(|| regex_capture(&first, r"v(\S+)"))
                    .or_else(|| regex_capture(&first, r"(\d+\.\d+\.\d+)"));
                if let Some(ver) = ver {
                    out.push(InstalledSoftware {
                        id: "ollama".into(),
                        version: ver,
                        install_source: classify_pkg_source(&path),
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── node ────────────────────────────────────────────────────────
        if let Some(path) = which("node") {
            if let Some(raw) = run("node", &["--version"]) {
                // Strip leading 'v'
                let ver = raw.trim().trim_start_matches('v').to_string();
                if !ver.is_empty() {
                    let src = if std::env::consts::OS == "macos" {
                        if path.starts_with("/opt/homebrew/") {
                            Some("brew".to_string())
                        } else {
                            None
                        }
                    } else {
                        Some("apt".to_string())
                    };
                    out.push(InstalledSoftware {
                        id: "node".into(),
                        version: ver,
                        install_source: src,
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── python3 ─────────────────────────────────────────────────────
        if let Some(path) = which("python3") {
            if let Some(raw) = run("python3", &["--version"]) {
                if let Some(ver) = regex_capture(&raw, r"Python\s+(\S+)") {
                    let src = if std::env::consts::OS == "macos" {
                        Some("brew".to_string())
                    } else {
                        Some("apt".to_string())
                    };
                    out.push(InstalledSoftware {
                        id: "python".into(),
                        version: ver,
                        install_source: src,
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── docker ──────────────────────────────────────────────────────
        if let Some(path) = which("docker") {
            if let Some(raw) = run("docker", &["--version"]) {
                if let Some(ver) = regex_capture(&raw, r"Docker version\s+(\S+)") {
                    let src = if std::env::consts::OS == "macos" {
                        Some("brew-cask".to_string())
                    } else {
                        Some("apt".to_string())
                    };
                    // docker --version sometimes prints with a trailing comma.
                    let ver = ver.trim_end_matches(',').to_string();
                    out.push(InstalledSoftware {
                        id: "docker".into(),
                        version: ver,
                        install_source: src,
                        install_path: Some(path),
                        metadata: None,
                    });
                }
            }
        }

        // ── OS ──────────────────────────────────────────────────────────
        if let Some(os_entry) = detect_os() {
            out.push(os_entry);
        }

        out
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Build a PATH that always includes the user-level install dirs the
/// daemon's inherited `$PATH` frequently omits. Systemd user units on
/// Ubuntu get a minimal PATH (no `$HOME/.local/bin`, no `$HOME/.cargo/bin`),
/// so `which ff` and `Command::new("ff")` silently fail to find binaries
/// that `ff onboard` installed there. Noticed 2026-04-24 on sia/adele —
/// both had ff+forgefleetd at ~/.local/bin/ but reported empty software
/// inventory. Prepend the user dirs and keep the daemon's own PATH after.
fn augmented_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut parts: Vec<String> = vec![
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        "/opt/bin".into(),
    ];
    let current = std::env::var("PATH").unwrap_or_default();
    for p in current.split(':') {
        if !p.is_empty() && !parts.iter().any(|e| e == p) {
            parts.push(p.to_string());
        }
    }
    parts.join(":")
}

/// Run a command and return trimmed stdout on success (non-empty only).
/// The PATH is augmented with user-level bin dirs (`~/.local/bin`,
/// `~/.cargo/bin`, etc.) so probes succeed even when the daemon inherited
/// a minimal systemd PATH.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .env("PATH", augmented_path())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Like `run`, but accept non-zero exit status and use stdout+stderr. Useful
/// for tools that report their version on stderr or exit non-zero on
/// `--version`.
#[allow(dead_code)]
fn run_allow_nonzero(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .env("PATH", augmented_path())
        .output()
        .ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    if s.trim().is_empty() {
        s = String::from_utf8_lossy(&out.stderr).to_string();
    }
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Probe a Python module's `__version__` via `python3 -c`. Returns
/// `Some(version)` ONLY when:
/// 1. `python3` exits 0 (module imports successfully), AND
/// 2. stdout passes [`sanitize_version`] (non-empty, single-line, no error
///    markers, ≤ 64 chars).
///
/// Returns `None` in every other case — in particular, when the module isn't
/// installed (which causes a `ModuleNotFoundError` traceback on stderr with
/// exit code 1). This stops garbage like `"Traceback (most r..."` from leaking
/// into the `computer_software.installed_version` column.
fn run_python_version_probe(module: &str) -> Option<String> {
    let script = format!("import {module}; print({module}.__version__)");
    let output = std::process::Command::new("python3")
        .args(["-c", &script])
        .env("PATH", augmented_path())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    sanitize_version(&stdout)
}

/// Accept only sane-looking version strings. Rejects:
/// - empty / whitespace-only
/// - multi-line output (versions are one line)
/// - anything > 64 chars (versions are always short)
/// - substrings that indicate a failed probe: `Traceback`, `Error`, `error:`,
///   `No module named`, `command not found`.
fn sanitize_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    if trimmed.len() > 64 {
        return None;
    }
    const BAD: &[&str] = &[
        "Traceback",
        "Error",
        "error:",
        "No module named",
        "command not found",
    ];
    for marker in BAD {
        if trimmed.contains(marker) {
            return None;
        }
    }
    Some(trimmed.to_string())
}

/// `which <bin>` — returns the resolved path on success.
fn which(bin: &str) -> Option<String> {
    run("which", &[bin])
}

/// Extract the first capture group from `pattern` applied to `s`.
fn regex_capture(s: &str, pattern: &str) -> Option<String> {
    let re = Regex::new(pattern).ok()?;
    re.captures(s)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Parsed result of an `ff --version` / `forgefleetd --version` line.
#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedVersionLine {
    /// The primary version token — either the legacy semver (`2026.4.7`)
    /// or the current build-version (`2026.4.21_5`).
    version: Option<String>,
    /// Short SHA when the line carries a `(build <sha>)` or
    /// `(STATE <sha>)` suffix. `"unknown"` is dropped.
    sha: Option<String>,
    /// `pushed` / `unpushed` / `dirty` / `unknown` when the new shape is
    /// detected. `None` for pre-upgrade binaries.
    git_state: Option<String>,
}

/// Parse `ff`/`forgefleetd` version output in either shape:
///   legacy:  `ff 2026.4.7 (build 8355028d1)`
///   current: `ff 2026.4.21_5 (pushed 8355028d12)`
///
/// The current shape strips the `STATE` token into `git_state`; the
/// legacy shape leaves `git_state = None` so callers know this is an
/// older fleet node that can't be safety-gated.
fn parse_ff_version_line(raw: &str) -> ParsedVersionLine {
    let mut out = ParsedVersionLine::default();
    // Version token follows the leading binary name ("ff"/"forgefleet"/"forgefleetd").
    out.version = regex_capture(raw, r"(?:ff|forgefleet\S*)\s+(\S+)");

    // Suffix — current shape first, then legacy.
    if let Some(caps) = Regex::new(r"\((pushed|unpushed|dirty|unknown)\s+([0-9a-f]{6,40}\+?\S*)\)")
        .ok()
        .and_then(|re| re.captures(raw))
    {
        out.git_state = caps.get(1).map(|m| m.as_str().to_string());
        let sha = caps.get(2).map(|m| m.as_str().to_string());
        out.sha = sha.filter(|s| s != "unknown");
    } else if let Some(sha) = regex_capture(raw, r"\(build\s+([0-9a-f]{6,40})\)") {
        if sha != "unknown" {
            out.sha = Some(sha);
        }
    }
    out
}

/// `ff` and `forgefleetd` are usually direct installs — either in
/// `~/.local/bin/` or `~/.cargo/bin/`.
fn classify_ff_source(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let local_bin = format!("{home}/.local/bin/");
    let cargo_bin = format!("{home}/.cargo/bin/");
    if path.starts_with(&local_bin) || path.starts_with(&cargo_bin) {
        "direct".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Classify install_source for a package-manager-installed binary based on
/// its install path + current OS.
///
/// Windows: distinguishes `winget` (paths under `%LOCALAPPDATA%\Microsoft\WinGet\`
/// or `%ProgramFiles%\WindowsApps\`) from `choco` (paths under
/// `%ProgramData%\chocolatey\`). Falls through to `None` when unclear.
/// UNTESTED — gated on a real Windows node joining the fleet.
fn classify_pkg_source(path: &str) -> Option<String> {
    // Normalize — on Windows paths come back with backslashes; treat
    // lowercase substrings to avoid case-sensitivity headaches.
    let p = path.to_lowercase().replace('\\', "/");

    if path.starts_with("/opt/homebrew/bin/") || path.starts_with("/usr/local/bin/") {
        return Some("brew".to_string());
    }
    if std::env::consts::OS == "linux" && path.starts_with("/usr/bin/") {
        return Some("apt".to_string());
    }
    if std::env::consts::OS == "windows" {
        if p.contains("/chocolatey/") {
            return Some("choco".to_string());
        }
        if p.contains("/winget/") || p.contains("/windowsapps/") {
            return Some("winget".to_string());
        }
        if p.contains("/scoop/") {
            return Some("scoop".to_string());
        }
    }
    None
}

/// Derive an `InstalledSoftware` entry for the host OS.
fn detect_os() -> Option<InstalledSoftware> {
    match std::env::consts::OS {
        "macos" => {
            let ver = run("sw_vers", &["-productVersion"])?;
            Some(InstalledSoftware {
                id: "os-macos".into(),
                version: ver,
                install_source: Some("system".to_string()),
                install_path: None,
                metadata: None,
            })
        }
        "linux" => {
            // DGX OS takes priority.
            if std::path::Path::new("/etc/dgx-release").exists() {
                let ver = std::fs::read_to_string("/etc/dgx-release")
                    .ok()
                    .and_then(|raw| {
                        regex_capture(&raw, r#"DGX_OS_VERSION\s*=\s*"?([^"\n]+)"?"#)
                    })
                    .unwrap_or_else(|| "unknown".to_string());
                return Some(InstalledSoftware {
                    id: "os-dgx".into(),
                    version: ver,
                    install_source: Some("system".to_string()),
                    install_path: None,
                    metadata: None,
                });
            }
            // Parse /etc/os-release for Ubuntu version.
            let osr = std::fs::read_to_string("/etc/os-release").ok()?;
            let pretty = regex_capture(&osr, r#"PRETTY_NAME\s*=\s*"([^"]+)""#)
                .unwrap_or_default();
            let version_id = regex_capture(&osr, r#"VERSION_ID\s*=\s*"([^"]+)""#)
                .unwrap_or_default();

            let (id, ver) = if pretty.contains("Ubuntu") {
                let id = match version_id.as_str() {
                    "22.04" => "os-ubuntu-22.04",
                    "24.04" => "os-ubuntu-24.04",
                    _ => return None,
                };
                (id.to_string(), version_id)
            } else {
                return None;
            };

            Some(InstalledSoftware {
                id,
                version: ver,
                install_source: Some("system".to_string()),
                install_path: None,
                metadata: None,
            })
        }
        "windows" => {
            // Best-effort probe via PowerShell. UNTESTED — Windows is
            // future fleet hardware. Two attempts:
            //   1. `Get-CimInstance Win32_OperatingSystem` — modern + fast.
            //   2. Registry fallback for stripped-down SKUs.
            let ver = run(
                "powershell",
                &[
                    "-NoProfile",
                    "-Command",
                    "(Get-CimInstance Win32_OperatingSystem).Version",
                ],
            )
            .or_else(|| {
                run(
                    "powershell",
                    &[
                        "-NoProfile",
                        "-Command",
                        "(Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion').DisplayVersion",
                    ],
                )
            })?;
            Some(InstalledSoftware {
                id: "os-windows".into(),
                version: ver,
                install_source: Some("system".to_string()),
                install_path: None,
                metadata: None,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_populates_known_ids() {
        let c = SoftwareCollector::new();
        assert!(c.known_ids.contains(&"ff"));
        assert!(c.known_ids.contains(&"docker"));
    }

    #[test]
    fn detect_returns_host_marker() {
        // Running the ForgeFleet test suite on a host means at minimum the OS
        // entry should resolve, and in most dev environments `ff` is on PATH
        // too. Require at least ONE of:
        //   - id == "ff"
        //   - id starts with "os-"
        let items = SoftwareCollector::new().detect();
        let has_marker = items.iter().any(|i| {
            i.id == "ff" || i.id.starts_with("os-")
        });
        assert!(
            has_marker,
            "expected at least one entry with id=ff or id=os-*; got: {:?}",
            items.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn sanitize_version_accepts_clean_versions() {
        assert_eq!(sanitize_version("0.6.3"), Some("0.6.3".to_string()));
        assert_eq!(sanitize_version("  0.19.1\n"), Some("0.19.1".to_string()));
        assert_eq!(sanitize_version("3.11.9"), Some("3.11.9".to_string()));
    }

    #[test]
    fn sanitize_version_rejects_tracebacks_and_errors() {
        // Python ModuleNotFoundError traceback — the exact bug we're fixing.
        let tb = "Traceback (most recent call last):\n  File \"<string>\", line 1, in <module>\nModuleNotFoundError: No module named 'vllm'";
        assert_eq!(sanitize_version(tb), None);
        assert_eq!(sanitize_version("Error: something broke"), None);
        assert_eq!(sanitize_version("error: bad thing"), None);
        assert_eq!(sanitize_version("No module named 'foo'"), None);
        assert_eq!(sanitize_version("bash: vllm: command not found"), None);
    }

    #[test]
    fn sanitize_version_rejects_multiline_and_long() {
        assert_eq!(sanitize_version(""), None);
        assert_eq!(sanitize_version("   "), None);
        assert_eq!(sanitize_version("line1\nline2"), None);
        let long = "x".repeat(65);
        assert_eq!(sanitize_version(&long), None);
        let sixty_four = "y".repeat(64);
        assert_eq!(sanitize_version(&sixty_four), Some(sixty_four));
    }

    #[test]
    fn parse_ff_version_line_legacy_shape() {
        let p = parse_ff_version_line("ff 2026.4.7 (build 8355028d1a)");
        assert_eq!(p.version.as_deref(), Some("2026.4.7"));
        assert_eq!(p.sha.as_deref(), Some("8355028d1a"));
        assert_eq!(p.git_state, None);

        let p = parse_ff_version_line("forgefleet 2026.4.7 (build abcdef0123)");
        assert_eq!(p.sha.as_deref(), Some("abcdef0123"));

        // Pre-vergen binary (no suffix) — no SHA, no state.
        let p = parse_ff_version_line("ff 2026.4.7");
        assert_eq!(p.version.as_deref(), Some("2026.4.7"));
        assert_eq!(p.sha, None);
        assert_eq!(p.git_state, None);

        // `(build unknown)` — dropped so we don't seed "unknown" SHAs.
        assert_eq!(parse_ff_version_line("ff 2026.4.7 (build unknown)").sha, None);
    }

    #[test]
    fn parse_ff_version_line_current_shape() {
        let p = parse_ff_version_line("ff 2026.4.21_5 (pushed 8355028d12)");
        assert_eq!(p.version.as_deref(), Some("2026.4.21_5"));
        assert_eq!(p.sha.as_deref(), Some("8355028d12"));
        assert_eq!(p.git_state.as_deref(), Some("pushed"));

        let p = parse_ff_version_line("ff 2026.4.21_6 (unpushed 8355028d12)");
        assert_eq!(p.git_state.as_deref(), Some("unpushed"));

        let p = parse_ff_version_line("ff 2026.4.21_7 (dirty 8355028d12+local)");
        assert_eq!(p.git_state.as_deref(), Some("dirty"));
        assert_eq!(p.sha.as_deref(), Some("8355028d12+local"));

        let p = parse_ff_version_line("forgefleet 2026.4.21_5 (pushed abcdef0123)");
        assert_eq!(p.git_state.as_deref(), Some("pushed"));
        assert_eq!(p.sha.as_deref(), Some("abcdef0123"));
    }

    #[test]
    fn regex_capture_extracts_first_group() {
        assert_eq!(
            regex_capture("ff 2026.4.7", r"ff\s+(\S+)"),
            Some("2026.4.7".to_string())
        );
        assert_eq!(
            regex_capture("Python 3.11.9", r"Python\s+(\S+)"),
            Some("3.11.9".to_string())
        );
        assert_eq!(regex_capture("no match here", r"ff\s+(\S+)"), None);
    }
}
