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

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info};

/// Default per-CLI invocation timeout. Mirrors the worker timeout shipped
/// in PR #12 so a wedged CLI is killed before it blocks downstream work.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One row in the supported-backend catalog. Keeps the
/// "what-binary-and-flags-for-each-vendor" mapping in one place.
#[derive(Debug, Clone, Copy)]
pub struct CliBackend {
    /// Public name used by `--backend X`.
    pub name: &'static str,
    /// Binary on PATH. Existence is checked before spawn.
    pub binary: &'static str,
    /// Flags inserted between the binary and the user's prompt. Each
    /// vendor's "non-interactive, print-and-exit" mode lives here.
    pub default_flags: &'static [&'static str],
}

/// Catalog of supported backends. `local` is intentionally absent — that
/// path stays in the existing agent loop, not this module.
pub const BACKENDS: &[CliBackend] = &[
    CliBackend {
        name: "claude",
        binary: "claude",
        // `-p` runs in print-mode (non-interactive); `--output-format
        // text` keeps stdout free of the agent-loop JSON envelope.
        default_flags: &["-p", "--output-format", "text"],
    },
    CliBackend {
        name: "codex",
        binary: "codex",
        // Codex `exec` is the headless equivalent.
        default_flags: &["exec"],
    },
    CliBackend {
        name: "gemini",
        binary: "gemini",
        // Gemini CLI's print-mode flag.
        default_flags: &["-p"],
    },
    CliBackend {
        name: "kimi",
        // No widely-shipped Moonshot CLI as of 2026-04-27; the row is
        // here for symmetry. `kimi` would be the binary if/when it
        // ships. Until then, calls return a clear error.
        binary: "kimi",
        default_flags: &["-p"],
    },
    CliBackend {
        name: "grok",
        // Same caveat as kimi — no official xAI CLI today.
        binary: "grok",
        default_flags: &["-p"],
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
                _ => "<vendor>",
            }
        )
    })?;

    let mut cmd = tokio::process::Command::new(bin_path.as_str());
    cmd.args(cfg.default_flags);
    cmd.args(passthrough_args);
    cmd.arg(prompt);
    cmd.kill_on_drop(true);

    debug!(
        backend = cfg.name,
        binary = %bin_path,
        flags = ?cfg.default_flags,
        passthrough = ?passthrough_args,
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
fn which_on_path(bin: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}
