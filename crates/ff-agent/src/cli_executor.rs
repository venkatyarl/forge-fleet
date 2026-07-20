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
    /// Canonical local HTTP-bridge port (must match the `cli_backends` DB seed
    /// in schema.rs). EXPLICIT — never derived from array position. The bridge
    /// (`ff-gateway::cli_bridge`) used to assign `51100 + array_index`, which
    /// silently cross-wired kimi/gemini when the array order diverged from the
    /// DB seed (deep review 2026-06-17, conflict #8). Binding the port to the
    /// backend makes array order irrelevant.
    pub port: u16,
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
        port: 51100,
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
        port: 51101,
        // Codex `exec` is the headless equivalent. `--skip-git-repo-check`
        // lets it run outside a git repo (matches `ff cli` running anywhere).
        //
        // `--sandbox danger-full-access` DISABLES codex's OS-level filesystem
        // sandbox. Codex's `workspace-write` (and `read-only`) sandboxes shell
        // out to **bubblewrap**, which needs to create a user namespace + uid
        // map. On the Linux fleet followers that fails with
        // `bwrap: setting up uid map: Permission denied` (unprivileged userns is
        // restricted on these hosts), so EVERY codex file-write silently failed
        // ("Failed to write file …") and a real multi-file dispatch retried/
        // reasoned until it hit the 1800s timeout — the "codex hangs 30 min,
        // 0 PRs" symptom (dogfooded 2026-06-30). The bwrap sandbox is redundant
        // here anyway: `ff cli` already runs codex inside a DEDICATED per-task
        // git worktree under `~/.forgefleet/sub-agents/`, so isolation comes from
        // the worktree, not bwrap. `danger-full-access` is the minimal change
        // that lets codex actually write (approval policy is unchanged — `exec`
        // is `approval: never` regardless). Verified on adele: file created,
        // exit 0, seconds not minutes.
        default_flags: &[
            "exec",
            "--skip-git-repo-check",
            "--sandbox",
            "danger-full-access",
        ],
        // `-C/--cd <dir>` sets the agent's working root.
        cwd_mode: CwdMode::Flag("-C"),
        prompt_is_positional: true,
    },
    CliBackend {
        name: "gemini",
        binary: "gemini",
        port: 51103,
        // Gemini CLI's print-mode flag.
        default_flags: &["-p"],
        cwd_mode: CwdMode::ProcessOnly,
        prompt_is_positional: true,
    },
    CliBackend {
        name: "kimi",
        binary: "kimi",
        port: 51102,
        // Kimi CLI (Moonshot) headless form. `--quiet` is the documented alias
        // for `--print --output-format text --final-message-only`: it runs the
        // full non-interactive agent (auto-DISMISSES AskUserQuestion + auto-
        // APPROVES tool calls for the invocation — same headless behavior `--afk`
        // gave) AND prints only the final assistant message (cleanest to capture).
        // `-p`/`--prompt` carries the prompt. We deliberately DROPPED `--afk`:
        // it's a newer flag absent on many installed kimi builds, which rejected
        // it with "No such option: --afk" and broke every kimi dispatch on those
        // hosts (operator-reported 2026-06-20). `--print`/`--quiet` are the
        // fundamental headless flags present across kimi versions, so this is the
        // version-robust form. `--yolo` was redundant with --print's auto-approve.
        default_flags: &["--quiet", "-p"],
        // `-w/--work-dir <dir>` sets the agent's working directory.
        cwd_mode: CwdMode::Flag("-w"),
        prompt_is_positional: true,
    },
    CliBackend {
        name: "grok",
        // No official xAI CLI today — the row is here for symmetry and
        // returns a clear "not installed" error until a `grok` binary ships.
        binary: "grok",
        port: 51104,
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

    let budget_pool = budget_pool().await;
    if let Some(pg) = &budget_pool
        && crate::cloud_budget::provider_is_exhausted(pg, cfg.name).await
    {
        return Err(anyhow!(
            "backend '{}' skipped: known cloud quota window is exhausted",
            cfg.name
        ));
    }

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
    // Give the child a PATH that includes the known install dirs, so the CLI and
    // any node/git subprocess it forks resolve even under a minimal
    // non-interactive PATH (same reason we resolve `bin_path` absolutely above).
    cmd.env("PATH", augmented_path_env());
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
    // Detach stdin. The prompt is passed as an ARG (never stdin), so a vendor CLI
    // has no reason to read stdin — but if it inherits the parent's stdin it can
    // BLOCK on a read (or contend for it when several CLIs run in parallel under
    // one `ff` process, e.g. `ff council`). That presents as a "wedge" that only
    // ends at the timeout. Null stdin → any read returns EOF and the CLI proceeds.
    // (Four other ff-agent spawn sites already do this; this one was the outlier.)
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Put the child in its own process group so a timeout can kill the WHOLE tree
    // (the vendor CLI plus any model-call / auth subprocesses it forks). Killing
    // just the direct child via kill_on_drop leaves grandchildren orphaned.
    #[cfg(unix)]
    cmd.process_group(0);

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
    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("spawn `{bin_path}` failed: {e}"))?;
    // Capture the pid before the wait future takes ownership — we need it to kill
    // the process group if the call times out.
    let child_pid = child.id();
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(anyhow!("wait `{bin_path}` failed: {e}"));
        }
        Err(_) => {
            // Timed out. kill_on_drop reaps the direct child when the future
            // drops; additionally SIGKILL the whole process group so any
            // subprocess the CLI forked dies too (no orphans holding the GPU/cred
            // file). pgid == child pid because we spawned it as a group leader.
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                // Safety: killpg is async-signal-safe and we only pass a pid we own.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = child_pid; // used only on unix
            return Err(anyhow!(
                "backend '{}' exceeded {}s timeout — killed the CLI process group. \
                 The prompt may be too large for the model, the endpoint may be down, \
                 or the run legitimately needs more time (raise --timeout).",
                cfg.name,
                timeout.as_secs()
            ));
        }
    };
    let duration_ms = start.elapsed().as_millis();

    let mut exit_code = out.status.code().map(|c| c as i64).unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // Some exhausted CLIs (observed with Kimi) return exit 0 and no output.
    // Promote that to a provider failure so callers never classify it as a
    // legitimate no-diff result and retry healthy work against the same lane.
    if exit_code == 0 && stdout.trim().is_empty() {
        exit_code = 75;
        if stderr.trim().is_empty() {
            stderr = "provider returned success with empty output; probable quota exhaustion"
                .to_string();
        }
    }

    if let Some(pg) = &budget_pool {
        if exit_code == 0 {
            crate::cloud_budget::record_success(pg, cfg.name).await;
        } else {
            let combined = format!("{stdout}\n{stderr}");
            // Empty-output failures have no vendor reset hint. Treat them as a
            // one-hour window, matching Kimi's observed rolling reset.
            let signal =
                crate::cloud_budget::parse_quota_signal(exit_code, &combined, chrono::Utc::now())
                    .or_else(|| {
                        (exit_code == 75).then(|| crate::cloud_budget::QuotaSignal {
                            exhausted_until: chrono::Utc::now() + chrono::Duration::hours(1),
                            source: "empty_success",
                        })
                    });
            if let Some(signal) = signal {
                crate::cloud_budget::record_failure(pg, cfg.name, &signal).await;
            }
        }
    }

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

async fn budget_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("FORGEFLEET_POSTGRES_URL")
        .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        .ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(1))
        .connect(&url)
        .await
        .ok()
}

/// Resolve a binary name through `$PATH`. Returns the absolute path if
/// found, `None` otherwise. Mirrors the helper in
/// `crates/ff-pulse/src/software_collector.rs::which` so we don't pull
/// in the third-party `which` crate just for this one call.
///
/// `pub` so the `ff cli` front-door can probe which vendor CLIs are
/// installed before dispatching (and list them in the not-installed error).
pub fn which_on_path(bin: &str) -> Option<String> {
    // 1. Honor `$PATH` (an operator override or a login shell wins).
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    // 2. Fall back to the well-known install dirs a NON-INTERACTIVE shell
    //    (SSH / systemd / launchd / sub-agent) drops from `$PATH`. Without this,
    //    `which codex` returns nothing under a login-less shell even though
    //    /opt/homebrew/bin/codex exists — the "`codex` not on PATH" false
    //    negative that made ff blind to installed vendor CLIs.
    for dir in known_bin_dirs() {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Directories where vendor CLIs commonly install but that a non-interactive
/// shell does NOT put on `$PATH`. macOS Homebrew → /opt/homebrew/bin; uv/pipx
/// tools (e.g. `kimi-cli`) → ~/.local/bin; `cargo install` → ~/.cargo/bin; npm
/// user-global → ~/.npm-global/bin. Searched AFTER `$PATH` so an explicit PATH
/// entry still wins. Order is precedence order.
pub fn known_bin_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".npm-global/bin"));
    }
    for p in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
    ] {
        dirs.push(std::path::PathBuf::from(p));
    }
    dirs
}

/// `$PATH` with [`known_bin_dirs`] prepended (deduped, order-preserving). Set on
/// spawned vendor-CLI processes so the CLI itself — and any node/git subprocess
/// IT forks — can resolve tools even when ff was launched under a minimal
/// non-interactive PATH. Falls back to the raw `$PATH` if joining fails.
pub fn augmented_path_env() -> std::ffi::OsString {
    let mut parts: Vec<std::path::PathBuf> = known_bin_dirs();
    if let Some(path_var) = std::env::var_os("PATH") {
        parts.extend(std::env::split_paths(&path_var));
    }
    let mut seen = std::collections::HashSet::new();
    parts.retain(|p| seen.insert(p.clone()));
    std::env::join_paths(parts).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
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
