//! Convergent onboarding — idempotent self-heal for the per-machine
//! onboarding checklist (`config/onboarding-checklist.json`).
//!
//! Motivation: a fresh macOS enroll died mid-bootstrap (2026-08) and left the
//! node without MCP wiring, skills sync, and desktop installers. Instead of
//! re-running the whole bootstrap by hand, each node re-applies the drifting
//! items on a cadence: the `ff converge` CLI verb runs it on demand, and the
//! daemon's self-heal loop runs it daily.
//!
//! Every step is idempotent and non-fatal: failures are collected into the
//! returned `Vec<ConvergeResult>`, never bail the set. This runs in daemon
//! context, so it logs via `tracing::info` per item — the CLI verb renders
//! the table.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::info;

/// How one checklist item ended up after a converge pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvergeStatus {
    /// Already in the desired state (no change needed).
    Ok,
    /// Was missing/drifted; this pass (re)applied it.
    Installed,
    /// Not applicable here (e.g. desktop installers on Linux) or a soft
    /// dependency was unavailable (no DB for skills sync).
    Skipped,
    /// Attempted and failed — see `detail`.
    Failed,
}

impl ConvergeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConvergeStatus::Ok => "ok",
            ConvergeStatus::Installed => "installed",
            ConvergeStatus::Skipped => "skipped",
            ConvergeStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConvergeResult {
    pub item: String,
    pub status: ConvergeStatus,
    pub detail: String,
}

impl ConvergeResult {
    fn new(item: impl Into<String>, status: ConvergeStatus, detail: impl Into<String>) -> Self {
        let r = Self {
            item: item.into(),
            status,
            detail: detail.into(),
        };
        info!(item = %r.item, status = r.status.as_str(), detail = %r.detail, "converge item");
        r
    }
}

/// Cloud CLI vendor installers (checklist `install` entries for macOS/Linux).
/// Windows uses the `.ps1` variants and is handled by the bootstrap template,
/// not by converge.
const CLOUD_CLIS: &[(&str, &str, &str)] = &[
    // (binary, installer URL, shell)
    ("claude", "https://claude.ai/install.sh", "bash"),
    ("codex", "https://chatgpt.com/codex/install.sh", "sh"),
    ("kimi", "https://code.kimi.com/kimi-code/install.sh", "bash"),
];

/// Desktop apps: (macOS .app bundle, brew cask). Operator installs the
/// downloaded dmg by hand (operator preference 2026-08-06) — converge only
/// stages the installer into ~/Downloads/fleet-desktop-apps.
const DESKTOP_APPS: &[(&str, &str)] = &[
    ("/Applications/Claude.app", "claude"),
    ("/Applications/Kimi.app", "kimi"),
    ("/Applications/ChatGPT.app", "chatgpt"),
];

const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(300);

/// Re-apply the local onboarding checklist. Idempotent; collects per-item
/// results instead of bailing on the first failure.
pub async fn run_converge() -> Vec<ConvergeResult> {
    let mut results = Vec::new();
    results.push(converge_mcp_wiring().await);
    results.push(converge_skills_sync().await);
    for (bin, url, shell) in CLOUD_CLIS {
        results.push(converge_cloud_cli(bin, url, shell).await);
    }
    results.extend(converge_desktop_apps().await);
    results
}

// ─── mcp wiring ────────────────────────────────────────────────────────────

/// The `ff mcp install --for all` logic lives in ff-terminal (`mcp_cmd.rs`,
/// ~1000 lines covering 11 client configs); ff-terminal depends on ff-agent,
/// so the reusable part can't move down without dragging clap/serde writers
/// with it. Shell out to the installed `ff` instead — same dogfooding path
/// the operator runs by hand, and the install map is itself idempotent.
async fn converge_mcp_wiring() -> ConvergeResult {
    let item = "mcp wiring";
    let Some(ff) = resolve_ff_binary() else {
        return ConvergeResult::new(item, ConvergeStatus::Failed, "no ff binary found");
    };
    match run_quiet(&ff, &["mcp", "install", "--for", "all"]).await {
        Ok(true) => ConvergeResult::new(
            item,
            ConvergeStatus::Ok,
            format!("ff mcp install --for all ({})", ff.display()),
        ),
        Ok(false) => ConvergeResult::new(
            item,
            ConvergeStatus::Failed,
            "ff mcp install exited non-zero",
        ),
        Err(e) => ConvergeResult::new(item, ConvergeStatus::Failed, e),
    }
}

/// Prefer the `ff` sitting next to the current executable (works for both the
/// `ff` CLI itself and `forgefleetd` in ~/.local/bin), then ~/.local/bin/ff,
/// then PATH.
fn resolve_ff_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(exe_name("ff"));
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    let local = home_dir().join(".local").join("bin").join(exe_name("ff"));
    if local.is_file() {
        return Some(local);
    }
    resolve_on_path("ff")
}

#[cfg(windows)]
fn exe_name(base: &str) -> String {
    format!("{base}.exe")
}

#[cfg(not(windows))]
fn exe_name(base: &str) -> String {
    base.to_string()
}

// ─── skills sync ───────────────────────────────────────────────────────────

/// Same path as `ff skills sync` (no `--prune`: converge never deletes):
/// the materializer already lives in ff-agent (`skills_db`), so reuse it
/// directly rather than subprocess.
async fn converge_skills_sync() -> ConvergeResult {
    let item = "skills sync";
    let pool = match crate::fleet_info::get_fleet_pool().await {
        Ok(p) => p,
        Err(e) => {
            return ConvergeResult::new(
                item,
                ConvergeStatus::Skipped,
                format!("Postgres unavailable: {e}"),
            );
        }
    };
    match crate::skills_db::materialize_all(&pool).await {
        Ok((written, skipped)) => ConvergeResult::new(
            item,
            ConvergeStatus::Ok,
            format!(
                "materialized {written}, skipped {skipped}, root={}",
                crate::skills_db::skills_root().display()
            ),
        ),
        Err(e) => ConvergeResult::new(item, ConvergeStatus::Failed, format!("{e}")),
    }
}

// ─── cloud CLIs ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliAction {
    /// Not on PATH at all — install from the vendor script.
    InstallMissing,
    /// Active binary is npm-global (under a node_modules dir or an
    /// npm-global bin like /usr/bin, /usr/local/bin). npm-global installs
    /// can't self-update when root-owned — migrate to the native installer.
    MigrateNpm,
    /// Active binary is under ~/.local/bin — the native installer's home.
    SkipLocal,
    /// Anything else (e.g. /opt/homebrew/bin brew-managed codex) — managed by
    /// its own package manager, leave it alone.
    SkipOther,
}

/// Pure source classification — unit-tested. `resolved` is the on-PATH binary
/// location (from `which`), if any.
fn classify_cli_source(resolved: Option<&Path>, home: &Path) -> CliAction {
    let Some(path) = resolved else {
        return CliAction::InstallMissing;
    };
    if path.starts_with(home.join(".local").join("bin")) {
        return CliAction::SkipLocal;
    }
    let s = path.to_string_lossy();
    if s.contains("node_modules")
        || path.starts_with("/usr/bin")
        || path.starts_with("/usr/local/bin")
    {
        return CliAction::MigrateNpm;
    }
    CliAction::SkipOther
}

async fn converge_cloud_cli(bin: &str, url: &str, shell: &str) -> ConvergeResult {
    let item = format!("cloud cli: {bin}");
    let home = home_dir();
    let action = classify_cli_source(resolve_on_path(bin).as_deref(), &home);
    match action {
        CliAction::SkipLocal => ConvergeResult::new(
            item,
            ConvergeStatus::Ok,
            "native install under ~/.local/bin",
        ),
        CliAction::SkipOther => ConvergeResult::new(
            item,
            ConvergeStatus::Ok,
            format!(
                "present at {} (externally managed, left alone)",
                resolve_on_path(bin)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
        ),
        CliAction::InstallMissing | CliAction::MigrateNpm => {
            let why = if action == CliAction::InstallMissing {
                "missing"
            } else {
                "npm-global install"
            };
            let cmd = format!("curl -fsSL {url} | {shell}");
            match run_shell(shell, &cmd).await {
                Ok(true) => ConvergeResult::new(
                    item,
                    ConvergeStatus::Installed,
                    format!("vendor installer applied ({why})"),
                ),
                Ok(false) => ConvergeResult::new(
                    item,
                    ConvergeStatus::Failed,
                    format!("vendor installer exited non-zero ({why})"),
                ),
                Err(e) => ConvergeResult::new(item, ConvergeStatus::Failed, e),
            }
        }
    }
}

// ─── desktop installers (macOS) ────────────────────────────────────────────

/// Staging dir for operator-hand-installed desktop apps.
fn desktop_download_dir(home: &Path) -> PathBuf {
    home.join("Downloads").join("fleet-desktop-apps")
}

async fn converge_desktop_apps() -> Vec<ConvergeResult> {
    if !cfg!(target_os = "macos") {
        return vec![ConvergeResult::new(
            "desktop installers",
            ConvergeStatus::Skipped,
            "macOS only (no official Linux builds; Windows via bootstrap template)",
        )];
    }
    let dl = desktop_download_dir(&home_dir());
    let mut out = Vec::new();
    for (app_dir, cask) in DESKTOP_APPS {
        let item = format!("desktop app: {cask}");
        if Path::new(app_dir).exists() {
            out.push(ConvergeResult::new(
                item,
                ConvergeStatus::Ok,
                format!("{app_dir} present"),
            ));
            continue;
        }
        // brew fetch resolves the current vendor URL (no hardcoded versioned
        // links), then copy the cached dmg into the staging dir for the
        // operator to install by hand.
        let fetch = format!(
            "mkdir -p '{dl}' && brew fetch --cask {cask} >/dev/null 2>&1 \
             && cp \"$(brew --cache --cask {cask})\" '{dl}/'",
            dl = dl.display()
        );
        match run_shell("bash", &fetch).await {
            Ok(true) => out.push(ConvergeResult::new(
                item,
                ConvergeStatus::Installed,
                format!("installer staged in {} (operator installs)", dl.display()),
            )),
            Ok(false) => out.push(ConvergeResult::new(
                item,
                ConvergeStatus::Failed,
                "brew fetch --cask failed — fetch from the vendor site by hand",
            )),
            Err(e) => out.push(ConvergeResult::new(item, ConvergeStatus::Failed, e)),
        }
    }
    out
}

// ─── helpers ───────────────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Locate a binary on PATH via `which` (synchronous, cheap).
fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    if p.as_os_str().is_empty() {
        None
    } else {
        Some(p)
    }
}

/// Run `argv` quietly (stdout/stderr captured), with a timeout. Ok(bool) is
/// the exit status; Err is spawn/timeout failure.
async fn run_quiet(program: &Path, args: &[&str]) -> Result<bool, String> {
    let fut = tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match tokio::time::timeout(SUBPROCESS_TIMEOUT, fut).await {
        Ok(Ok(out)) => Ok(out.status.success()),
        Ok(Err(e)) => Err(format!("spawn {}: {e}", program.display())),
        Err(_) => Err(format!("{} timed out", program.display())),
    }
}

/// Run a shell pipeline (`curl ... | bash` style) with a timeout.
async fn run_shell(shell: &str, cmd: &str) -> Result<bool, String> {
    let fut = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match tokio::time::timeout(SUBPROCESS_TIMEOUT, fut).await {
        Ok(Ok(status)) => Ok(status.success()),
        Ok(Err(e)) => Err(format!("spawn {shell}: {e}")),
        Err(_) => Err("shell command timed out".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/test")
    }

    #[test]
    fn classify_missing_installs() {
        assert_eq!(
            classify_cli_source(None, &home()),
            CliAction::InstallMissing
        );
    }

    #[test]
    fn classify_local_bin_skips() {
        let p = home().join(".local/bin/claude");
        assert_eq!(classify_cli_source(Some(&p), &home()), CliAction::SkipLocal);
    }

    #[test]
    fn classify_npm_global_migrates() {
        for p in [
            "/usr/bin/claude",
            "/usr/local/bin/codex",
            "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            "/usr/local/lib/node_modules/@openai/codex/bin/codex.js",
        ] {
            let path = PathBuf::from(p);
            assert_eq!(
                classify_cli_source(Some(&path), &home()),
                CliAction::MigrateNpm,
                "expected MigrateNpm for {p}"
            );
        }
    }

    #[test]
    fn classify_brew_managed_left_alone() {
        let p = PathBuf::from("/opt/homebrew/bin/codex");
        assert_eq!(classify_cli_source(Some(&p), &home()), CliAction::SkipOther);
    }

    #[test]
    fn desktop_dir_under_downloads() {
        assert_eq!(
            desktop_download_dir(&home()),
            PathBuf::from("/home/test/Downloads/fleet-desktop-apps")
        );
    }
}
