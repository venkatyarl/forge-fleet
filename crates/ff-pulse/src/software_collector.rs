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
                "forgefleetd",
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
                // os-macos / os-ubuntu-22.04 / os-ubuntu-24.04 / os-dgx.
                "os-macos",
                "os-ubuntu-22.04",
                "os-ubuntu-24.04",
                "os-dgx",
            ],
        }
    }

    /// Detect installed software on this machine. Entries that can't be
    /// resolved are simply omitted.
    pub fn detect(&self) -> Vec<InstalledSoftware> {
        let mut out: Vec<InstalledSoftware> = Vec::new();

        // ── ff ──────────────────────────────────────────────────────────
        if let Some(path) = which("ff") {
            if let Some(raw) = run("ff", &["--version"]) {
                if let Some(ver) = regex_capture(&raw, r"ff\s+(\S+)") {
                    out.push(InstalledSoftware {
                        id: "ff".into(),
                        version: ver,
                        install_source: Some(classify_ff_source(&path)),
                        install_path: Some(path),
                    });
                }
            }
        }

        // ── forgefleetd ─────────────────────────────────────────────────
        if let Some(path) = which("forgefleetd") {
            if let Some(raw) = run("forgefleetd", &["--version"]) {
                let ver = regex_capture(&raw, r"(\S+)$").unwrap_or(raw.clone());
                out.push(InstalledSoftware {
                    id: "forgefleetd".into(),
                    version: ver.trim().to_string(),
                    install_source: Some(classify_ff_source(&path)),
                    install_path: Some(path),
                });
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
                });
            }
        }

        // ── mlx_lm (macOS only, via python3 -c) ────────────────────────
        if std::env::consts::OS == "macos" {
            if let Some(ver) = run_allow_nonzero(
                "python3",
                &["-c", "import mlx_lm; print(mlx_lm.__version__)"],
            ) {
                let ver = ver.trim().to_string();
                if !ver.is_empty() {
                    out.push(InstalledSoftware {
                        id: "mlx_lm".into(),
                        version: ver,
                        install_source: Some("pip".to_string()),
                        install_path: None,
                    });
                }
            }
        }

        // ── vllm (Linux only, via python3 -c) ──────────────────────────
        if std::env::consts::OS == "linux" {
            if let Some(ver) = run_allow_nonzero(
                "python3",
                &["-c", "import vllm; print(vllm.__version__)"],
            ) {
                let ver = ver.trim().to_string();
                if !ver.is_empty() {
                    out.push(InstalledSoftware {
                        id: "vllm".into(),
                        version: ver,
                        install_source: Some("pip".to_string()),
                        install_path: None,
                    });
                }
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

/// Run a command and return trimmed stdout on success (non-empty only).
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Like `run`, but accept non-zero exit status and use stdout+stderr. Useful
/// for tools that report their version on stderr or exit non-zero on
/// `--version`.
fn run_allow_nonzero(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    if s.trim().is_empty() {
        s = String::from_utf8_lossy(&out.stderr).to_string();
    }
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
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
fn classify_pkg_source(path: &str) -> Option<String> {
    if path.starts_with("/opt/homebrew/bin/") || path.starts_with("/usr/local/bin/") {
        Some("brew".to_string())
    } else if std::env::consts::OS == "linux" && path.starts_with("/usr/bin/") {
        Some("apt".to_string())
    } else {
        None
    }
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
