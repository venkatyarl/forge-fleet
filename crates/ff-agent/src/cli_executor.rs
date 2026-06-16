//! Spawn a vendor CLI as a subprocess (Layer 2 of the multi-LLM CLI
//! integration roadmap; see `~/.claude/plans/cosmic-splashing-chipmunk.md`).
//!
//! Each backend maps to its CLI binary + the right "non-interactive,
//! plain-text output" invocation. The user runs `ff run --backend claude
//! "fix the bug"` and ff:
//!
//! 1. Validates the binary exists on PATH.
//! 2. Spawns it with the prompt + structured-output flags.
//! 3. Captures stdout (with a configurable timeout — default reuses
//!    `task_runner::MAX_TASK_DURATION`).
//! 4. Returns the captured text.
//!
//! Unlike Layer 1 (direct API), this path preserves each vendor CLI's
//! own agent loop + tool calling — so e.g. `claude -p` will use Claude
//! Code's MCP tools, file reads, etc. The cost is one cold-start per
//! call (~1-3s) and lossy structured output (each CLI's JSON shape is
//! different; we return raw text by default and let callers parse).
//!
//! Authentication is handled by the CLI itself, reading
//! `~/.<vendor>/credentials.json` etc. Layer 4 (PR-A2's
//! `oauth_distributor`) ensures every member has the centralized cred
//! file when ff drives this path.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tracing::{debug, info};

/// Default per-CLI invocation timeout. The old 10-min value killed real
/// multi-minute build/codegen runs mid-flight (a verified codex task completing
/// in ~2min is fine, but a larger refactor legitimately exceeds 10min) — the
/// "wedges at ~600s" symptom was THIS cap, NOT any cred refresh (the codex
/// token is a 10-day TTL and never refreshes mid-run; verified by reproduction).
/// 30min gives real build tasks room; callers cap it lower with `--timeout`.
/// NOTE: a DISPATCHED `ff run` is also bounded by the fleet task worker's
/// `MAX_TASK_DURATION`, which the build verbs raise in tandem.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How a vendor CLI wants its working directory expressed. Even though we
/// always set the spawned process's cwd (so relative paths resolve), the
/// agent CLIs sandbox file access to dirs they were *told* about — so we
/// also pass the vendor's own flag where it exists.
#[derive(Debug, Clone, Copy)]
pub enum CwdMode {
    /// Process cwd is enough; the CLI inherits it (e.g. `claude` reads the
    /// process cwd and additionally honours `--add-dir <dir>`).
    Flag(&'static str),
    /// No dedicated flag — rely solely on the spawned process's cwd.
    ProcessOnly,
}

/// One row in the supported-backend catalog. Keeps the
/// "what-binary-and-flags-for-each-vendor" mapping in one place.
#[derive(Debug, Clone, Copy)]
pub struct CliBackend {
    /// Public name used by `--backend X` / `ff cli <name>`.
    pub name: &'static str,
    /// Binary on PATH. Existence is checked before spawn.
    pub binary: &'static str,
    /// Flags inserted between the binary and the user's prompt. Each
    /// vendor's "non-interactive, print-and-exit" mode lives here.
    pub default_flags: &'static [&'static str],
    /// How to tell this CLI which directory to operate in. The flag (if
    /// any) is emitted as `<flag> <dir>` before the prompt.
    pub cwd_mode: CwdMode,
    /// `true` if the prompt is passed as a positional trailing arg (the
    /// common case). `false` if it rides on a value-flag — for those the
    /// flag lives in `default_flags` and we still append the prompt as the
    /// last arg, so this stays `true` everywhere today. Kept for clarity.
    pub prompt_is_positional: bool,
}

/// Catalog of supported backends. `local` is intentionally absent — that
/// path stays in the existing agent loop, not this module.
///
/// Headless invocations verified on the leader 2026-05-31:
///   claude -p --output-format text "<prompt>"   (cwd via process + --add-dir)
///   codex exec --skip-git-repo-check "<prompt>"  (cwd via -C/--cd)
///   kimi --print --yes --prompt "<prompt>"       (cwd via -w/--work-dir)
pub const BACKENDS: &[CliBackend] = &[
    CliBackend {
        name: "claude",
        binary: "claude",
        // `-p` runs in print-mode (non-interactive); `--output-format
        // text` keeps stdout free of the agent-loop JSON envelope.
        default_flags: &["-p", "--output-format", "text"],
        // Claude Code reads the process cwd; `--add-dir` widens tool access.
        cwd_mode: CwdMode::Flag("--add-dir"),
        prompt_is_positional: true,
    },
    CliBackend {
        name: "codex",
        binary: "codex",
        // Codex `exec` is the headless equivalent. `--skip-git-repo-check`
        // lets it run outside a git repo (matches `ff cli` running anywhere).
        default_flags: &["exec", "--skip-git-repo-check"],
        // `-C/--cd <dir>` sets the agent's working root.
        cwd_mode: CwdMode::Flag("-C"),
        prompt_is_positional: true,
    },
    CliBackend {
        name: "gemini",
        binary: "gemini",
        // Gemini CLI's print-mode flag.
        default_flags: &["-p"],
        cwd_mode: CwdMode::ProcessOnly,
        prompt_is_positional: true,
    },
    CliBackend {
        name: "kimi",
        binary: "kimi",
        // Kimi CLI (Moonshot) headless build form (verified against `kimi
        // --help` 2026-06-16): `--afk` runs the autonomous away-from-keyboard
        // mode (the old `--print` was NOT a full agent run), `--yolo`
        // auto-approves writes, and `-p` (alias `--prompt`) carries the prompt.
        // The previous `--print --yes --prompt` form did not actually execute
        // multi-step build tasks.
        default_flags: &["--afk", "--yolo", "-p"],
        // `-w/--work-dir <dir>` sets the agent's working directory.
        cwd_mode: CwdMode::Flag("-w"),
        prompt_is_positional: true,
    },
    CliBackend {
        name: "grok",
        // No official xAI CLI today — the row is here for symmetry and
        // returns a clear "not installed" error until a `grok` binary ships.
        binary: "grok",
        default_flags: &["-p"],
        cwd_mode: CwdMode::ProcessOnly,
        prompt_is_positional: true,
    },
];

/// Look up a backend by name; case-insensitive. Returns `None` for
/// unknown names so callers can surface "expected one of …" errors.
pub fn backend_by_name(name: &str) -> Option<&'static CliBackend> {
    BACKENDS.iter().find(|b| b.name.eq_ignore_ascii_case(name))
}

/// Result of a single CLI invocation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CliResult {
    pub backend: String,
    pub binary_path: String,
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
}

/// Spawn the configured CLI for `backend`, feed it `prompt`, capture
/// stdout. Extra `passthrough_args` get appended after `default_flags`
/// (lets the caller override e.g. `--output-format json`).
///
/// Errors:
///   - Unknown backend name.
///   - Binary not on PATH (clear message about installing the CLI).
///   - Timeout exceeded (configurable via `timeout`; default 10 min).
pub async fn execute_cli(
    backend: &str,
    prompt: &str,
    passthrough_args: &[String],
    timeout: Option<Duration>,
) -> Result<CliResult> {
    execute_cli_in_dir(backend, prompt, passthrough_args, None, timeout).await
}

/// Like [`execute_cli`], but runs the vendor CLI with its working directory
/// set to `cwd` (when `Some`). The spawned process's cwd is set *and*, for
/// vendors with a dedicated working-dir flag (`CwdMode::Flag`), that flag is
/// emitted so the CLI's tool sandbox is rooted there. `None` keeps the
/// caller's current directory — preserving the historical behaviour of
/// [`execute_cli`].
pub async fn execute_cli_in_dir(
    backend: &str,
    prompt: &str,
    passthrough_args: &[String],
    cwd: Option<&Path>,
    timeout: Option<Duration>,
) -> Result<CliResult> {
    let cfg = backend_by_name(backend).ok_or_else(|| {
        anyhow!(
            "unknown backend '{backend}'; expected one of: {}",
            BACKENDS
                .iter()
                .map(|b| b.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    // Verify the binary is on PATH before bothering to spawn.
    let bin_path = which_on_path(cfg.binary).ok_or_else(|| {
        anyhow!(
            "backend '{}' requires `{}` on PATH; install with `npm i -g {}` (claude/codex/gemini) \
             or follow the vendor's installer. Layer-2 backends won't be available on this member \
             until the CLI is present — Layer 1 (direct API) still works via `--model claude-…`.",
            cfg.name,
            cfg.binary,
            match cfg.name {
                "claude" => "@anthropic-ai/claude-code",
                "codex" => "@openai/codex",
                "gemini" => "@google/gemini-cli",
                "kimi" => "kimi-cli (Moonshot)",
                _ => "<vendor>",
            }
        )
    })?;

    let mut cmd = tokio::process::Command::new(bin_path.as_str());
    // Tell the CLI which directory to operate in (sets the process cwd and,
    // where the vendor has one, the dedicated working-dir flag).
    //
    // The cwd flag is emitted *before* `default_flags` on purpose: some
    // vendor flags are variadic (e.g. claude's `--add-dir <dirs...>`), so a
    // trailing `--add-dir <dir>` would greedily swallow the prompt that we
    // append last. Placing it first means the next flag (`-p`/`exec`/
    // `--print`) terminates the variadic list and the prompt survives.
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        if let CwdMode::Flag(flag) = cfg.cwd_mode {
            cmd.arg(flag);
            cmd.arg(dir.as_os_str());
        }
    }
    cmd.args(cfg.default_flags);
    cmd.args(passthrough_args);
    cmd.arg(prompt);
    cmd.kill_on_drop(true);

    debug!(
        backend = cfg.name,
        binary = %bin_path,
        flags = ?cfg.default_flags,
        passthrough = ?passthrough_args,
        cwd = ?cwd,
        "spawning vendor CLI"
    );

    let start = std::time::Instant::now();
    let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(anyhow!("spawn `{}` failed: {e}", bin_path));
        }
        Err(_) => {
            return Err(anyhow!(
                "backend '{}' exceeded {}s timeout — wedged CLI; consider raising timeout or killing the cred file refresh loop",
                cfg.name,
                timeout.as_secs()
            ));
        }
    };
    let duration_ms = start.elapsed().as_millis();

    let exit_code = out.status.code().map(|c| c as i64).unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    info!(
        backend = cfg.name,
        exit = exit_code,
        duration_ms,
        stdout_chars = stdout.chars().count(),
        "CLI invocation completed"
    );

    Ok(CliResult {
        backend: cfg.name.to_string(),
        binary_path: bin_path.clone(),
        exit_code,
        stdout,
        stderr,
        duration_ms,
    })
}

/// Resolve a binary name through `$PATH`. Returns the absolute path if
/// found, `None` otherwise. Mirrors the helper in
/// `crates/ff-pulse/src/software_collector.rs::which` so we don't pull
/// in the third-party `which` crate just for this one call.
///
/// `pub` so the `ff cli` front-door can probe which vendor CLIs are
/// installed before dispatching (and list them in the not-installed error).
pub fn which_on_path(bin: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backend-name lookup is case-insensitive and resolves to the canonical
    /// lowercase `name` the conductor's `/backend` handler stores.
    #[test]
    fn backend_by_name_is_case_insensitive_and_canonical() {
        for variant in ["claude", "CLAUDE", "Claude"] {
            let b = backend_by_name(variant)
                .unwrap_or_else(|| panic!("{variant:?} should resolve to a backend"));
            assert_eq!(b.name, "claude", "canonical name for {variant:?}");
        }
        // Spot-check the other shipped backends resolve too.
        assert_eq!(backend_by_name("codex").map(|b| b.name), Some("codex"));
        assert_eq!(backend_by_name("KIMI").map(|b| b.name), Some("kimi"));
    }

    /// Unknown names are rejected (`None`) so callers can surface an
    /// "expected one of …" error.
    #[test]
    fn backend_by_name_rejects_unknown() {
        assert!(backend_by_name("bogus").is_none());
        assert!(backend_by_name("").is_none());
    }

    /// `local` is intentionally NOT in `BACKENDS` — it routes through the
    /// existing agent loop, not this CLI module. The conductor accepts "local"
    /// via a separate `eq_ignore_ascii_case("local")` branch in its inline
    /// `/backend` handler (crates/ff-terminal/src/main.rs), so this pure helper
    /// returns `None` for it. Pinned here so the split-of-responsibility stays
    /// intentional.
    #[test]
    fn local_is_not_a_cli_backend() {
        assert!(backend_by_name("local").is_none());
        assert!(backend_by_name("LOCAL").is_none());
        assert!(!BACKENDS.iter().any(|b| b.name == "local"));
    }
}
