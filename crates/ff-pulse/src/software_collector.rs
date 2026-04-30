//! Software inventory collector — probes the local machine for installed
//! developer/runtime software and reports `(id, version, install_source,
//! install_path)` tuples suitable for inclusion in [`PulseBeatV2::installed_software`].
//!
//! V66+ (data-driven): the set of probed software_ids and the detection
//! method per id come from `software_registry.detection JSONB`, loaded
//! into a process-wide cache by [`crate::detection_registry::spawn_refresher`].
//! The collector below is a pure dispatcher — for each rule it runs the
//! method's probe and emits an `InstalledSoftware` entry on success.
//!
//! Adding a new tool to fleet inventory requires zero Rust code: insert
//! a `software_registry` row with a `detection` JSONB. Pre-V66 hardcoded
//! detection blocks are removed; the helpers below (`run`, `which`,
//! `regex_capture`, `parse_ff_version_line`, etc.) are now generic
//! primitives the dispatcher composes.

use regex::Regex;

use crate::beat_v2::InstalledSoftware;
use crate::detection_registry::{self, DetectionRule};

/// Probes the local machine for installed software using rules loaded
/// from `software_registry.detection` (V66+).
pub struct SoftwareCollector {
    rules: Vec<DetectionRule>,
}

impl Default for SoftwareCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftwareCollector {
    /// Snapshot the registry cache. Empty when the cache hasn't been
    /// populated yet (early daemon startup, tests without DB plumbing) —
    /// `detect()` returns Vec::new() in that case.
    pub fn new() -> Self {
        Self {
            rules: detection_registry::current_rules(),
        }
    }

    /// `software_id`s the collector currently knows how to probe — derived
    /// from the loaded registry rules.
    pub fn known_ids(&self) -> Vec<String> {
        self.rules.iter().map(|r| r.software_id.clone()).collect()
    }

    /// Detect installed software on this machine. Iterates the registry
    /// snapshot and dispatches each rule by `detection.method`. Per-rule
    /// failures (missing binary, regex no-match, etc.) are silently
    /// omitted — same semantics the hardcoded path used.
    pub fn detect(&self) -> Vec<InstalledSoftware> {
        if self.rules.is_empty() {
            tracing::debug!(
                "software_collector: detection registry empty — \
                 returning empty inventory (cache not yet refreshed?)"
            );
            return Vec::new();
        }

        let mut out: Vec<InstalledSoftware> = Vec::new();
        for rule in &self.rules {
            if let Some(entry) = dispatch_rule(rule) {
                out.push(entry);
            }
        }
        out
    }
}

/// Top-level dispatcher: read `detection.method` and call the matching
/// probe helper. Returns `None` on any per-rule failure (missing binary,
/// no-match, IO error). The collector's contract is best-effort.
fn dispatch_rule(rule: &DetectionRule) -> Option<InstalledSoftware> {
    let method = rule.detection.get("method")?.as_str()?;
    match method {
        "ff_version_pair" => detect_ff_version_pair(rule),
        "binary_version" => detect_binary_version(rule),
        "git_checkout" => detect_git_checkout(rule),
        "python_module" => detect_python_module(rule),
        "os_release" => detect_os_release(rule),
        other => {
            tracing::debug!(
                software_id = %rule.software_id,
                method = %other,
                "software_collector: unknown detection method, skipping"
            );
            None
        }
    }
}

/// `ff_version_pair`: run `<binary> --version`, parse with the shared
/// `parse_ff_version_line`, emit either the semver/build-version
/// (`field="version"`) or the SHA (`field="sha"`) per the rule.
fn detect_ff_version_pair(rule: &DetectionRule) -> Option<InstalledSoftware> {
    let binary = rule.detection.get("binary")?.as_str()?;
    let field = rule.detection.get("field")?.as_str()?;
    let path = which(binary)?;
    let raw = run(binary, &["--version"])?;
    let parsed = parse_ff_version_line(&raw);
    let version = match field {
        "version" => parsed.version.clone()?,
        "sha" => parsed.sha.clone()?,
        _ => return None,
    };
    let metadata = parsed
        .git_state
        .as_ref()
        .map(|s| serde_json::json!({ "git_state": s }));
    Some(InstalledSoftware {
        id: rule.software_id.clone(),
        version,
        install_source: Some(classify_ff_source(&path)),
        install_path: Some(path),
        metadata,
    })
}

/// `binary_version`: `which <binary>`, run with optional args, regex
/// capture group 1. `install_source_hint`:
///   - `"auto"` → classify by path (brew/apt/npm/direct).
///   - explicit string (e.g. `"direct"`, `"pip"`) → use it verbatim.
fn detect_binary_version(rule: &DetectionRule) -> Option<InstalledSoftware> {
    let binary = rule.detection.get("binary")?.as_str()?;
    let args: Vec<&str> = rule
        .detection
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_else(|| vec!["--version"]);
    let pattern = rule.detection.get("regex").and_then(|v| v.as_str())?;

    let path = which(binary)?;
    let raw = if rule
        .detection
        .get("fallback_via_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        run_allow_nonzero(binary, &args)?
    } else {
        run(binary, &args)?
    };
    let version = regex_capture(&raw, pattern)?;
    let install_source = classify_install_source(rule, &path);
    Some(InstalledSoftware {
        id: rule.software_id.clone(),
        version,
        install_source,
        install_path: Some(path),
        metadata: None,
    })
}

/// `git_checkout`: if `<path>/.git` exists, report
/// `git -C <path> rev-parse HEAD`. Optional `truncate` clamps the SHA.
fn detect_git_checkout(rule: &DetectionRule) -> Option<InstalledSoftware> {
    let path_template = rule.detection.get("path")?.as_str()?;
    let truncate = rule
        .detection
        .get("truncate")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let install_source = rule
        .detection
        .get("install_source")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let resolved = expand_home(path_template);
    let p = std::path::PathBuf::from(&resolved);
    if !p.join(".git").exists() {
        return None;
    }
    let sha_raw = run("git", &["-C", p.to_str()?, "rev-parse", "HEAD"])?;
    let sha_trimmed = sha_raw.trim();
    let sha = if truncate > 0 {
        sha_trimmed.chars().take(truncate).collect()
    } else {
        sha_trimmed.to_string()
    };
    if sha.is_empty() {
        return None;
    }
    Some(InstalledSoftware {
        id: rule.software_id.clone(),
        version: sha,
        install_source,
        install_path: p.to_str().map(str::to_string),
        metadata: None,
    })
}

/// `python_module`: `python3 -c "import <m>; print(<m>.__version__)"`.
/// `os_filter` ("macos" or "linux") gates so we don't probe vllm on
/// macOS or mlx_lm on Linux.
fn detect_python_module(rule: &DetectionRule) -> Option<InstalledSoftware> {
    let module = rule.detection.get("module")?.as_str()?;
    if let Some(filter) = rule.detection.get("os_filter").and_then(|v| v.as_str()) {
        if std::env::consts::OS != filter {
            return None;
        }
    }
    let version = run_python_version_probe(module)?;
    let install_source = rule
        .detection
        .get("install_source_hint")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(InstalledSoftware {
        id: rule.software_id.clone(),
        version,
        install_source,
        install_path: None,
        metadata: None,
    })
}

/// `os_release`: emit ONLY when this host's OS markers match the rule's
/// `expected_*` fields. Lets every os-* registry row fire its own probe
/// — only one matches per host, so the collector emits exactly one OS
/// entry without coordination.
fn detect_os_release(rule: &DetectionRule) -> Option<InstalledSoftware> {
    let det = &rule.detection;
    let expected_id = det.get("expected_id").and_then(|v| v.as_str());
    let expected_version_prefix = det
        .get("expected_version_prefix")
        .and_then(|v| v.as_str());
    let expected_kernel_contains = det
        .get("expected_kernel_contains")
        .and_then(|v| v.as_str());

    match std::env::consts::OS {
        "macos" => {
            if expected_id != Some("macos") {
                return None;
            }
            let ver = run("sw_vers", &["-productVersion"])?;
            Some(InstalledSoftware {
                id: rule.software_id.clone(),
                version: ver,
                install_source: Some("system".into()),
                install_path: None,
                metadata: None,
            })
        }
        "linux" => {
            // Kernel-string match (DGX OS layers atop Ubuntu).
            if let Some(needle) = expected_kernel_contains {
                let kernel = run("uname", &["-r"]).unwrap_or_default();
                if !kernel.contains(needle) {
                    return None;
                }
                let ver = std::fs::read_to_string("/etc/dgx-release")
                    .ok()
                    .and_then(|raw| regex_capture(&raw, r#"DGX_OS_VERSION\s*=\s*"?([^"\n]+)"?"#))
                    .unwrap_or_else(|| kernel.clone());
                return Some(InstalledSoftware {
                    id: rule.software_id.clone(),
                    version: ver,
                    install_source: Some("system".into()),
                    install_path: None,
                    metadata: None,
                });
            }
            // /etc/os-release ID + VERSION_ID match.
            let osr = std::fs::read_to_string("/etc/os-release").ok()?;
            let id = regex_capture(&osr, r#"^ID\s*=\s*"?([^"\n]+)"?"#)
                .or_else(|| regex_capture(&osr, r#"\nID\s*=\s*"?([^"\n]+)"?"#))
                .unwrap_or_default();
            if let Some(want) = expected_id {
                if id != want {
                    return None;
                }
            }
            let version_id =
                regex_capture(&osr, r#"VERSION_ID\s*=\s*"([^"]+)""#).unwrap_or_default();
            if let Some(prefix) = expected_version_prefix {
                if !version_id.starts_with(prefix) {
                    return None;
                }
            }
            Some(InstalledSoftware {
                id: rule.software_id.clone(),
                version: version_id,
                install_source: Some("system".into()),
                install_path: None,
                metadata: None,
            })
        }
        "windows" => {
            if expected_id != Some("windows") {
                return None;
            }
            let ver = run(
                "powershell",
                &[
                    "-NoProfile",
                    "-Command",
                    "(Get-CimInstance Win32_OperatingSystem).Version",
                ],
            )?;
            Some(InstalledSoftware {
                id: rule.software_id.clone(),
                version: ver,
                install_source: Some("system".into()),
                install_path: None,
                metadata: None,
            })
        }
        _ => None,
    }
}

/// Expand `$HOME` (and bare `~`) in path templates loaded from JSONB.
fn expand_home(template: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    template
        .replace("$HOME", &home)
        .replace("${HOME}", &home)
        .replacen("~/", &format!("{home}/"), 1)
}

/// Resolve the rule's `install_source_hint`:
///   - `"auto"` → classify by path (brew/apt/etc.) via `classify_pkg_source`.
///   - explicit string → use it verbatim.
///   - missing → None.
fn classify_install_source(rule: &DetectionRule, path: &str) -> Option<String> {
    let hint = rule
        .detection
        .get("install_source_hint")
        .and_then(|v| v.as_str())?;
    match hint {
        "auto" => classify_pkg_source(path).or_else(|| {
            // npm-installed CLIs typically land in /opt/homebrew/lib/node_modules
            // or /usr/local/lib/node_modules — classify_pkg_source returns
            // brew for these (which is technically true: brew's npm)
            // but operators read it as "npm" for the CLI. Surface npm
            // explicitly when path matches the node_modules pattern.
            if path.contains("/node_modules/") {
                Some("npm".to_string())
            } else {
                None
            }
        }),
        other => Some(other.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_with_empty_registry_returns_empty_inventory() {
        // V66+: collector reads from the global detection_registry cache,
        // which is empty in unit tests (no DB plumbing). Empty rules in →
        // empty inventory out (no false negatives, no panics).
        let items = SoftwareCollector::new().detect();
        assert!(
            items.is_empty(),
            "expected empty inventory with empty registry; got {:?}",
            items.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn known_ids_reflects_loaded_rules() {
        // With an empty registry cache, known_ids() is also empty.
        let c = SoftwareCollector::new();
        assert!(c.known_ids().is_empty());
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
        assert_eq!(
            parse_ff_version_line("ff 2026.4.7 (build unknown)").sha,
            None
        );
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
