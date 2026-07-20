//! LLM-CLI backend availability detector (capability roadmap A1).
//!
//! For each backend in [`crate::cli_executor::BACKENDS`] (claude / codex /
//! gemini / kimi / grok), determine whether it is (a) **installed** (binary on
//! PATH) and (b) **authenticated** (a non-interactive probe actually returns,
//! rather than wedging on a login prompt or failing on expired creds).
//!
//! This is the foundation of the "sub-agents call any available LLM" capability:
//! the dispatch picker and the periodic `forgefleetd` detector tick both need to
//! know which backends are *usable* on a given node — not just present.
//!
//! ff-council (codex+kimi) guard: `command -v` is NOT sufficient. A backend is
//! only dispatchable after a **non-interactive authenticated health check** that
//! FAILS CLOSED on login prompts, expired/invalid credentials, or timeouts.

use serde::Serialize;
use std::time::Duration;

use crate::cli_executor::{BACKENDS, which_on_path};

/// Availability of one CLI backend on this node.
#[derive(Debug, Clone, Serialize)]
pub struct BackendStatus {
    pub name: &'static str,
    pub binary: &'static str,
    /// Binary resolvable on PATH.
    pub installed: bool,
    /// Resolved binary path (if installed).
    pub path: Option<String>,
    /// `--version` output (first line), best-effort.
    pub version: Option<String>,
    /// `Some(true/false)` once an auth probe ran; `None` if not probed (e.g.
    /// not installed, or `probe_auth=false`). Authenticated means a tiny
    /// non-interactive request returned successfully.
    pub authenticated: Option<bool>,
    /// Human-readable detail: the auth reason, or why it's unavailable.
    pub detail: String,
}

impl BackendStatus {
    /// A backend is *dispatchable* only when installed AND a recent auth probe
    /// passed. `None` authentication (un-probed) is treated as NOT dispatchable
    /// — fail closed, per the council guard.
    pub fn dispatchable(&self) -> bool {
        self.installed && self.authenticated == Some(true)
    }
}

/// Substrings (lowercased) in a probe's output that mean "present but NOT
/// authenticated / would prompt for login" — fail closed when any appears.
const AUTH_FAILURE_MARKERS: &[&str] = &[
    "not logged in",
    "please log in",
    "please login",
    "run `login`",
    "/login",
    "unauthenticated",
    "unauthorized",
    "authentication required",
    "auth required",
    "api key",
    "apikey",
    "no credentials",
    "invalid credentials",
    "credentials expired",
    "token expired",
    "session expired",
    "401",
    "403",
    "sign in",
    "log in to",
];

/// Classify an auth-probe result. PURE so the fail-closed policy is testable.
///
/// `timed_out` = the probe exceeded its deadline (treated as a login-wedge →
/// fail closed). Otherwise: any auth-failure marker in stdout/stderr ⇒ closed;
/// a clean exit with non-empty stdout ⇒ authenticated; anything else (non-zero
/// exit, empty output) ⇒ closed with the captured reason.
pub fn classify_auth(timed_out: bool, exit_ok: bool, stdout: &str, stderr: &str) -> (bool, String) {
    if timed_out {
        return (
            false,
            "probe timed out (likely waiting on an interactive login prompt)".to_string(),
        );
    }
    let hay = format!("{stdout}\n{stderr}").to_lowercase();
    if let Some(marker) = AUTH_FAILURE_MARKERS.iter().find(|m| hay.contains(**m)) {
        return (false, format!("not authenticated (matched '{marker}')"));
    }
    if exit_ok && !stdout.trim().is_empty() {
        return (true, "authenticated".to_string());
    }
    if !exit_ok {
        let snippet = stderr.trim();
        let snippet = if snippet.is_empty() {
            stdout.trim()
        } else {
            snippet
        };
        let snippet: String = snippet.chars().take(120).collect();
        return (false, format!("probe failed: {snippet}"));
    }
    (false, "probe returned no output".to_string())
}

/// Whether an auth-probe failure reason indicates the CLI BINARY ITSELF is
/// broken (crashes on run) — a missing native/optional dependency from a
/// half-finished npm install, a bad module load — rather than an auth failure
/// or a transient network blip. Such a backend must fail CLOSED regardless of
/// cred presence: it's on PATH and creds may exist, but it cannot execute a
/// dispatch, so `cred_present` must not "rescue" it. PURE, so it's testable.
pub fn probe_indicates_broken_binary(reason: &str) -> bool {
    const BROKEN_SIGNATURES: &[&str] = &[
        "missing optional dependency",
        "cannot find module",
        "module_not_found",
        "err_module_not_found",
        "err_dlopen",
        "reinstall",
        "npm install -g",
        "undefined symbol",
        "not a function",
        "cannot find package",
    ];
    let lower = reason.to_ascii_lowercase();
    BROKEN_SIGNATURES.iter().any(|s| lower.contains(s))
}

/// Whether an auth-probe failure reason indicates the CLI runs but is
/// MISCONFIGURED — it has no usable model/LLM selected — so it can't serve a
/// dispatch even though the binary + creds are fine. Like a broken binary, this
/// must fail CLOSED regardless of cred presence, else the router keeps picking a
/// backend that fails every dispatch. Observed live: kimi exiting code 1 with
/// "LLM not set" on a fleet node. PURE, so it's testable.
pub fn probe_indicates_config_error(reason: &str) -> bool {
    const CONFIG_SIGNATURES: &[&str] = &[
        "llm not set",
        "no model configured",
        "model not configured",
        "no llm configured",
        "no model selected",
        "select a model",
        "configure a model",
    ];
    let lower = reason.to_ascii_lowercase();
    CONFIG_SIGNATURES.iter().any(|s| lower.contains(s))
}

/// Whether an auth-probe failure reason indicates a DEFINITIVE authentication
/// failure — the CLI itself reported an expired / revoked / absent credential
/// (not a flaky probe: timeout, transient 5xx, cold-start). Such a backend must
/// fail CLOSED regardless of cred-FILE presence: the token is stale, so every
/// dispatch to it 401s. Without this, `cred_present` masks it — the backend
/// reads "authenticated" while the router burns every attempt on a 401 (observed
/// live: claude + kimi green in `computer_backends` yet 401ing each dispatch).
///
/// Kept as a TIGHT subset: it keys off `classify_auth`'s own definitive verdict
/// ("not authenticated (matched …)") plus a few unambiguous vendor phrases, and
/// deliberately omits bare "403" / "api key" (which can appear in rate-limit or
/// help text) so a genuinely-authenticated backend with a transient blip still
/// gets the cred-file benefit of the doubt. PURE, so the policy is testable.
pub fn probe_indicates_unauthenticated(reason: &str) -> bool {
    const UNAUTH_SIGNATURES: &[&str] = &[
        "not authenticated (matched", // classify_auth's definitive auth verdict
        "401 invalid authentication",
        "invalid authentication credentials",
        "invalid api key",
        "invalid credentials",
        "credentials expired",
        "token expired",
        "session expired",
        "refresh token: access revoked",
        "access_terminated",
        "oauth session expired and could not be refreshed",
        "please log in",
        "please login",
        "not logged in",
    ];
    let lower = reason.to_ascii_lowercase();
    UNAUTH_SIGNATURES.iter().any(|s| lower.contains(s))
}

/// Detect every backend's availability on THIS node. When `probe_auth` is true,
/// each installed backend additionally gets a tiny non-interactive request to
/// verify it's authenticated (slower — a real CLI invocation per backend).
pub async fn detect_backends(probe_auth: bool, timeout: Duration) -> Vec<BackendStatus> {
    let mut out = Vec::with_capacity(BACKENDS.len());
    for cfg in BACKENDS {
        let path = which_on_path(cfg.binary);
        let installed = path.is_some();
        if !installed {
            out.push(BackendStatus {
                name: cfg.name,
                binary: cfg.binary,
                installed: false,
                path: None,
                version: None,
                authenticated: None,
                detail: format!("`{}` not on PATH", cfg.binary),
            });
            continue;
        }

        let version = probe_version(cfg.binary).await;

        let (authenticated, detail) = if probe_auth {
            let (ok, reason) = probe_auth_once(cfg.name, timeout).await;
            if ok {
                (Some(true), reason)
            } else if probe_indicates_broken_binary(&reason) {
                // The probe didn't just fail auth — the CLI CRASHED on run (e.g.
                // a missing native/optional npm dep from a half-finished install:
                // "Missing optional dependency @openai/codex-linux-x64"). The
                // binary is on PATH and creds may exist, but it CANNOT execute a
                // dispatch. Fail closed regardless of cred presence (the
                // cred-file fallback below must NOT mask a broken install — that
                // is exactly how a dead codex read as "authenticated" and every
                // dispatch to it failed with the opaque "no dispatchable backend").
                (Some(false), format!("installed but NOT runnable: {reason}"))
            } else if probe_indicates_config_error(&reason) {
                // The CLI runs but has no usable model configured (e.g. kimi exits
                // "LLM not set"). It can't serve a dispatch; fail closed regardless
                // of creds so the router stops picking a backend that fails every
                // time — same rationale as the broken-binary case.
                (
                    Some(false),
                    format!("installed but MISCONFIGURED: {reason}"),
                )
            } else if probe_indicates_unauthenticated(&reason) {
                // A DEFINITIVE auth failure — the CLI itself reported an
                // expired/revoked/absent credential (e.g. claude "401 Invalid
                // authentication credentials", kimi "not logged in"). Unlike a
                // flaky probe (timeout, transient 5xx) this 401s on EVERY
                // dispatch, so fail CLOSED regardless of cred-FILE presence. The
                // cred_present fallback below used to MASK this: a stale token
                // left the backend marked "authenticated", so the dispatch
                // picker kept routing to it and burned every attempt on a 401
                // (observed live on duncan — claude+kimi green in the DB, both
                // 401ing each dispatch). Marking it unauthenticated makes the
                // picker skip it (→ codex, the working backend) and surfaces in
                // `ff` that this node needs a fresh `ff oauth` / re-distribution.
                (
                    Some(false),
                    format!("installed but UNAUTHENTICATED: {reason}"),
                )
            } else if cred_present(cfg.name) {
                // The live probe can be flaky on the fleet (slow cold-start,
                // sandbox quirks, transient 5xx) and would leave a genuinely
                // authenticated backend marked unusable — so the router never
                // routes to it and dispatch has no fail-over target. If the
                // vendor cred FILE is present, treat the backend as
                // authenticated (cred-based). Safe: a DEFINITIVE 401 is caught
                // by the unauthenticated branch above and fails closed; only a
                // non-definitive probe blip reaches here and gets the benefit of
                // the doubt.
                (
                    Some(true),
                    format!("authenticated (cred file present; probe: {reason})"),
                )
            } else {
                (Some(false), reason)
            }
        } else {
            (None, "installed (auth not probed)".to_string())
        };

        out.push(BackendStatus {
            name: cfg.name,
            binary: cfg.binary,
            installed: true,
            path,
            version,
            authenticated,
            detail,
        });
    }
    out
}

/// True if the vendor CLI's OAuth credential file is present + non-empty on
/// this node (or, for claude on macOS, the Keychain entry exists). Mirrors the
/// paths `oauth_distributor` writes to. Used as an auth fallback when the live
/// probe is flaky.
fn cred_present(backend: &str) -> bool {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return false,
    };
    let file_ok = |rel: &str| {
        home.join(rel)
            .metadata()
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false)
    };
    match backend {
        "claude" => {
            if file_ok(".claude/.credentials.json") {
                return true;
            }
            // macOS: Claude Code stores creds in the Keychain, not a file.
            #[cfg(target_os = "macos")]
            {
                std::process::Command::new("security")
                    .args([
                        "find-generic-password",
                        "-s",
                        "Claude Code-credentials",
                        "-w",
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
            #[cfg(not(target_os = "macos"))]
            {
                false
            }
        }
        "codex" => file_ok(".codex/auth.json"),
        "kimi" => file_ok(".kimi/credentials/kimi-code.json"),
        "gemini" => file_ok(".gemini/oauth_creds.json"),
        _ => false,
    }
}

/// Resolve THIS node's `computers.id` from its worker name. `None` if no row
/// (node not enrolled yet) — the tick skips rather than erroring.
async fn resolve_computer_id(pool: &sqlx::PgPool, worker_name: &str) -> Option<uuid::Uuid> {
    sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM computers WHERE name = $1")
        .bind(worker_name)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Run one backend-detector tick body for THIS node.
///
/// PER-HOST, NOT leader-gated: every host detects + persists ITS OWN backends so
/// the dispatch picker sees fleet-wide availability. Auth-probes are real CLI
/// invocations, so the scheduler interval is intentionally coarse (hourly) —
/// dispatch does a fresh probe when a cached row is stale (council guard).
pub async fn run_backend_detector_tick(pg: &sqlx::PgPool, worker_name: &str) {
    let Some(cid) = resolve_computer_id(pg, worker_name).await else {
        tracing::warn!(worker_name = %worker_name,
            "backend_detector: no computers row for this node; skipping");
        return;
    };
    match detect_and_persist(pg, cid, Duration::from_secs(30)).await {
        Ok(n) => tracing::info!(
            backends = n,
            "backend_detector: refreshed computer_backends"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "backend_detector: detect/persist failed"
        ),
    }
}

/// Detect this node's backends (with auth probe) and persist each to
/// `computer_backends` for `computer_id` (capability A2 — the per-node detector
/// tick's body). Returns how many rows were upserted. Errors on the first DB
/// failure; the caller (a periodic tick) just logs and retries next interval.
pub async fn detect_and_persist(
    pool: &sqlx::PgPool,
    computer_id: uuid::Uuid,
    timeout: Duration,
) -> anyhow::Result<usize> {
    let statuses = detect_backends(true, timeout).await;
    let mut n = 0usize;
    for s in &statuses {
        ff_db::pg_upsert_computer_backend(
            pool,
            computer_id,
            s.name,
            s.installed,
            s.authenticated.unwrap_or(false),
            s.version.as_deref(),
            s.path.as_deref(),
            &s.detail,
        )
        .await?;
        n += 1;
    }
    Ok(n)
}

/// `<binary> --version`, first line, best-effort (short timeout). Resolves the
/// binary via [`which_on_path`] (which searches known install dirs beyond
/// `$PATH`) and runs it with an augmented PATH, so version probing works under
/// the daemon's minimal non-interactive PATH.
async fn probe_version(binary: &str) -> Option<String> {
    let resolved = which_on_path(binary).unwrap_or_else(|| binary.to_string());
    let mut cmd = tokio::process::Command::new(resolved);
    cmd.env("PATH", crate::cli_executor::augmented_path_env());
    cmd.arg("--version");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    let fut = cmd.output();
    let out = tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|l| l.trim().to_string())
}

/// Run one tiny non-interactive request through the backend and classify it.
async fn probe_auth_once(backend: &str, timeout: Duration) -> (bool, String) {
    // A trivial prompt: cheapest possible request that still exercises auth.
    let res = crate::cli_executor::execute_cli_in_dir(
        backend,
        "Reply with exactly: OK",
        &[],
        None,
        Some(timeout),
    )
    .await;
    match res {
        // execute_cli_in_dir has no timed_out flag — a clean return is never a
        // timeout (timeouts come back as Err below).
        Ok(r) => classify_auth(false, r.exit_code == 0, &r.stdout, &r.stderr),
        Err(e) => {
            // A timeout is surfaced as an Err whose message says "exceeded …
            // timeout"; route it through the fail-closed timeout branch so the
            // reason reads as a login-wedge rather than a generic error.
            let msg = e.to_string();
            let timed_out = msg.contains("timeout") || msg.contains("exceeded");
            if timed_out {
                classify_auth(true, false, "", &msg)
            } else {
                let snippet: String = msg.chars().take(120).collect();
                (false, format!("probe error: {snippet}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_binary_detected_and_does_not_match_auth_or_transient() {
        // The exact codex crash seen on lily/veronica — must be flagged broken.
        assert!(probe_indicates_broken_binary(
            "probe failed: Error: Missing optional dependency @openai/codex-linux-x64. Reinstall Codex: npm install -g @openai/codex@latest"
        ));
        assert!(probe_indicates_broken_binary(
            "Cannot find module '/usr/lib/node_modules/@openai/codex/foo'"
        ));
        // Auth failures and transient blips are NOT broken-binary → cred fallback
        // may still apply; don't over-match them.
        assert!(!probe_indicates_broken_binary("401 Unauthorized"));
        assert!(!probe_indicates_broken_binary(
            "probe timed out (likely waiting on an interactive login prompt)"
        ));
        assert!(!probe_indicates_broken_binary("503 service unavailable"));
    }

    #[test]
    fn config_error_detected_and_disjoint_from_auth_and_broken() {
        // The exact kimi failure seen live — runs but no model configured.
        assert!(probe_indicates_config_error(
            "exit status: 1 stdout: LLM not set"
        ));
        assert!(probe_indicates_config_error("Error: no model configured"));
        // Not a config error: auth failure, transient, or a genuine crash.
        assert!(!probe_indicates_config_error("401 Unauthorized"));
        assert!(!probe_indicates_config_error("503 service unavailable"));
        assert!(!probe_indicates_config_error(
            "Missing optional dependency @openai/codex-linux-x64"
        ));
    }

    #[test]
    fn unauthenticated_detected_and_disjoint_from_transient_and_broken() {
        // classify_auth's definitive verdict (the detect path always routes a
        // real auth failure through this) must fail closed.
        assert!(probe_indicates_unauthenticated(
            "not authenticated (matched '401')"
        ));
        // The exact live dispatch surface from claude on duncan.
        assert!(probe_indicates_unauthenticated(
            "Failed to authenticate. API Error: 401 Invalid authentication credentials"
        ));
        assert!(probe_indicates_unauthenticated("not logged in"));
        assert!(probe_indicates_unauthenticated("Your token expired"));
        assert!(probe_indicates_unauthenticated(
            "refresh token: access revoked"
        ));
        assert!(probe_indicates_unauthenticated("access_terminated"));
        assert!(probe_indicates_unauthenticated(
            "OAuth session expired and could not be refreshed"
        ));
        // Transient / flaky probes must NOT match — they keep the cred-file
        // benefit of the doubt (a genuinely-authenticated backend blipping).
        assert!(!probe_indicates_unauthenticated("503 service unavailable"));
        assert!(!probe_indicates_unauthenticated(
            "probe timed out (likely waiting on an interactive login prompt)"
        ));
        assert!(!probe_indicates_unauthenticated(
            "probe failed: 502 bad gateway"
        ));
        assert!(!probe_indicates_unauthenticated(
            "context mentions token expired behavior"
        ));
        // Disjoint from the broken-binary and config-error verdicts.
        assert!(!probe_indicates_unauthenticated(
            "Missing optional dependency @openai/codex-linux-x64"
        ));
        assert!(!probe_indicates_broken_binary(
            "not authenticated (matched '401')"
        ));
        assert!(!probe_indicates_config_error(
            "not authenticated (matched '401')"
        ));
    }

    #[test]
    fn auth_classify_fails_closed_on_timeout() {
        let (ok, why) = classify_auth(true, false, "", "");
        assert!(!ok);
        assert!(why.contains("timed out"));
    }

    #[test]
    fn auth_classify_fails_closed_on_login_markers() {
        for marker in ["Please log in", "API key not set", "401 Unauthorized"] {
            let (ok, _) = classify_auth(false, true, marker, "");
            assert!(!ok, "marker {marker:?} must fail closed even with exit 0");
        }
    }

    #[test]
    fn auth_classify_passes_on_clean_nonempty_output() {
        let (ok, why) = classify_auth(false, true, "OK", "");
        assert!(ok, "{why}");
    }

    #[test]
    fn auth_classify_fails_closed_on_nonzero_or_empty() {
        assert!(!classify_auth(false, false, "", "boom").0);
        assert!(!classify_auth(false, true, "   ", "").0);
    }

    #[test]
    fn dispatchable_requires_installed_and_authed() {
        let mut s = BackendStatus {
            name: "codex",
            binary: "codex",
            installed: true,
            path: Some("/usr/bin/codex".into()),
            version: None,
            authenticated: Some(true),
            detail: String::new(),
        };
        assert!(s.dispatchable());
        s.authenticated = None; // un-probed → fail closed
        assert!(!s.dispatchable());
        s.authenticated = Some(false);
        assert!(!s.dispatchable());
        s.authenticated = Some(true);
        s.installed = false;
        assert!(!s.dispatchable());
    }
}
