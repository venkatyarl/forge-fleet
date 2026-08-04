//! OAuth credential harvest + distribute for the multi-LLM CLI integration.
//!
//! Each vendor CLI (Claude Code, OpenAI Codex, Google Gemini CLI, …) writes
//! its OAuth/session token to a local credential file when the user runs
//! `<cli> login`. ff doesn't reimplement OAuth — instead it:
//!
//! 1. **Imports** (on the leader): reads the local file for one provider,
//!    extracts the access token, stores it in `fleet_secrets` keyed by the
//!    provider's `secret_key` (e.g. `anthropic.oauth_token`).
//! 2. **Distributes**: pushes the entire credential file to every other
//!    fleet member's matching path via the existing `fleet_tasks` shell
//!    dispatcher (`pg_enqueue_shell_task` + base64 of the file payload).
//! 3. **Status**: reports per-provider whether the token is present,
//!    decoded expiry, and last refresh time.
//! 4. **RefreshWatch**: long-lived loop that polls the leader's cred files
//!    every `REFRESH_POLL_SECS` and re-imports + redistributes whenever
//!    the file's mtime changes (new token from a vendor refresh).
//!
//! Layer 1 (`cloud_llm.rs::try_route_to_cloud`) reads
//! `fleet_secrets[<provider>.oauth_token]` for the `oauth_subscription`
//! `auth_kind` and uses it as the `Authorization: Bearer …` value.
//!
//! See `~/.claude/plans/cosmic-splashing-chipmunk.md` for the full
//! roadmap context.

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use sqlx::PgPool;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::task_runner::{EnqueueOnceOutcome, pg_enqueue_shell_task_once};

/// One row in the OAuth provider catalog. Drives the import + distribute
/// + status logic — provider-agnostic from a single source of truth.
#[derive(Debug, Clone, Copy)]
pub struct OauthProvider {
    /// Short name used on the CLI: `claude`, `codex`, `gemini`, `kimi`, `grok`.
    pub name: &'static str,
    /// Path on the leader where the vendor CLI stores its credentials.
    /// `~/` is expanded to `$HOME/`. Empty string means the provider has
    /// no on-disk cred file (Grok today — token must be set manually via
    /// `ff secrets set`).
    pub cred_path: &'static str,
    /// `fleet_secrets` key where the access token gets stored.
    pub secret_key: &'static str,
    /// Field name(s) inside the JSON cred file that hold the access
    /// token. Tried in order; first hit wins.
    pub token_fields: &'static [&'static str],
}

/// Catalog of providers we know how to harvest credentials for.
///
/// Cred-file paths are best-guess as of 2026-04-27 — research items 4, 5,
/// 6 in the plan should verify and update these. The import logic
/// gracefully reports "no cred file at <path>" rather than panicking
/// when a path is wrong.
pub const OAUTH_PROVIDERS: &[OauthProvider] = &[
    OauthProvider {
        name: "claude",
        cred_path: "~/.claude/.credentials.json",
        secret_key: "anthropic.oauth_token",
        token_fields: &["accessToken", "access_token"],
    },
    OauthProvider {
        name: "codex",
        cred_path: "~/.codex/auth.json",
        secret_key: "openai.oauth_token",
        token_fields: &["access_token", "accessToken", "token"],
    },
    OauthProvider {
        name: "gemini",
        cred_path: "~/.gemini/oauth_creds.json",
        secret_key: "google.oauth_token",
        token_fields: &["access_token", "accessToken"],
    },
    OauthProvider {
        name: "kimi",
        // Kimi Code stores its OAuth creds at ~/.kimi/credentials/kimi-code.json
        // (flat {access_token, refresh_token, expires_at, ...}), NOT the
        // Moonshot-CLI ~/.moonshot/auth.json path.
        cred_path: "~/.kimi/credentials/kimi-code.json",
        secret_key: "moonshot.oauth_token",
        token_fields: &["access_token", "accessToken", "token"],
    },
    OauthProvider {
        name: "grok",
        cred_path: "",
        secret_key: "xai.oauth_token",
        token_fields: &[],
    },
];

/// Refresh-watch poll interval. Vendor CLIs typically refresh tokens
/// every 30-60 min; 30s polling is overkill but cheap (one stat() per
/// provider per cycle) and ensures peers see new tokens fast.
pub const REFRESH_POLL_SECS: u64 = 30;

/// Providers whose CLIs own renewable OAuth sessions. Invoking the CLI before
/// reading its credential file lets its native refresh-token flow run; ff then
/// copies the resulting complete credential document to peers.
const AUTO_REFRESH_PROVIDERS: &[&str] = &["claude", "codex", "kimi"];

/// Emergency kill switch for every autonomous OAuth task producer.
///
/// Missing rows preserve the historical enabled behavior. A TTL-expired
/// temporary disable restores to enabled. Any database/read error fails closed
/// so an unavailable gate authority cannot create another queue flood.
pub const OAUTH_DISTRIBUTION_ENABLED_KEY: &str = "oauth_distribution_enabled";
const OAUTH_DISTRIBUTION_DEFAULT: bool = true;
const OAUTH_DISTRIBUTION_RESTORE_ON_EXPIRY: bool = true;

const OAUTH_BACKLOG_CANCEL_SQL: &str = "UPDATE fleet_tasks
        SET status = 'cancelled',
            completed_at = COALESCE(completed_at, NOW()),
            progress_message = 'cancelled by ff oauth cancel-backlog'
      WHERE task_type = 'shell'
        AND status IN ('pending', 'dispatchable')
        AND (
            summary LIKE 'oauth-repush/%'
            OR summary LIKE 'oauth-distribute/%'
        )";
const OAUTH_BACKLOG_COUNT_SQL: &str = "SELECT COUNT(*)::bigint
       FROM fleet_tasks
      WHERE task_type = 'shell'
        AND status IN ('pending', 'dispatchable')
        AND (
            summary LIKE 'oauth-repush/%'
            OR summary LIKE 'oauth-distribute/%'
        )";

fn resolve_oauth_distribution_gate<E>(result: std::result::Result<bool, E>) -> bool
where
    E: std::fmt::Display,
{
    match result {
        Ok(enabled) => enabled,
        Err(error) => {
            warn!(
                key = OAUTH_DISTRIBUTION_ENABLED_KEY,
                %error,
                "OAuth distribution gate read failed; refusing new tasks"
            );
            false
        }
    }
}

pub async fn oauth_distribution_enabled(pool: &PgPool) -> bool {
    resolve_oauth_distribution_gate(
        ff_db::pg_read_safety_gate(
            pool,
            OAUTH_DISTRIBUTION_ENABLED_KEY,
            OAUTH_DISTRIBUTION_DEFAULT,
            OAUTH_DISTRIBUTION_RESTORE_ON_EXPIRY,
        )
        .await,
    )
}

fn oauth_distribute_enqueue_key(provider: &str, target_id: uuid::Uuid) -> String {
    format!("oauth-distribute:{provider}:{target_id}")
}

fn oauth_repush_enqueue_key(provider: &str, leader_id: uuid::Uuid) -> String {
    format!("oauth-repush:{provider}:{leader_id}")
}

/// Read the password value of a macOS Keychain generic-password entry
/// via `security find-generic-password -s <service> -a $USER -w`. Used
/// by `import_token` when the vendor CLI stores creds in Keychain
/// instead of (or in addition to) a flat file. Returns the raw bytes;
/// caller parses as JSON.
#[cfg(target_os = "macos")]
async fn keychain_read(service_name: &str) -> Result<Vec<u8>> {
    let user = std::env::var("USER").context("USER env var not set")?;
    let out = tokio::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            service_name,
            "-a",
            user.as_str(),
            "-w",
        ])
        .output()
        .await
        .context("spawn security")?;
    if !out.status.success() {
        anyhow::bail!(
            "keychain entry `{service_name}` not found (security exit {:?})",
            out.status.code()
        );
    }
    let mut bytes = out.stdout;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(bytes)
}

#[cfg(not(target_os = "macos"))]
async fn keychain_read(_service_name: &str) -> Result<Vec<u8>> {
    anyhow::bail!("Keychain not available on this OS")
}

/// Look up a provider by name. Returns `None` for unknown names.
pub fn provider_by_name(name: &str) -> Option<&'static OauthProvider> {
    OAUTH_PROVIDERS
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}

/// Expand `~/` prefix to `$HOME/`. Pure path manipulation, no I/O.
fn expand_home(path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = dirs::home_dir()?;
        Some(home.join(rest))
    } else {
        Some(PathBuf::from(path))
    }
}

/// Per-provider snapshot returned by `status`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderStatus {
    pub name: String,
    pub cred_file_present: bool,
    pub cred_file_mtime_secs_ago: Option<u64>,
    pub token_in_secrets: bool,
    pub token_preview: Option<String>,
}

/// Read the leader's raw credential bytes for one provider.
///
/// Source-of-truth varies by OS / vendor:
///  • Linux: every vendor CLI stores creds in a file at `cred_path`.
///  • macOS: Claude Code stores creds in the macOS Keychain (service name
///    "Claude Code-credentials") instead of `~/.claude/.credentials.json`.
///    Other vendor CLIs on macOS still use a file.
///
/// Keychain-first, file-fallback for `claude` on macOS; file-only otherwise.
/// Shared by `import_token` (extract token → fleet_secrets) and
/// `distribute_token` (push the cred to followers) so both honor the same
/// source — otherwise `distribute` reads a file that doesn't exist on a
/// macOS leader and claude silently fails to fan out.
async fn read_leader_cred_bytes(provider: &OauthProvider) -> Result<Vec<u8>> {
    let path = expand_home(provider.cred_path).ok_or_else(|| {
        anyhow!(
            "provider {} has no cred_path configured — set the token manually with `ff secrets set {}`",
            provider.name,
            provider.secret_key
        )
    })?;

    if cfg!(target_os = "macos")
        && provider.name == "claude"
        && let Ok(b) = keychain_read("Claude Code-credentials").await
        && !b.is_empty()
    {
        return Ok(b);
    }

    tokio::fs::read(&path).await.with_context(|| {
        let kc = if cfg!(target_os = "macos") && provider.name == "claude" {
            " (also tried macOS Keychain `Claude Code-credentials`)"
        } else {
            ""
        };
        format!(
            "read cred file {} for provider {} — run `{} login` first{kc}",
            path.display(),
            provider.name,
            provider.name
        )
    })
}

/// Read the leader's credential file for one provider, extract the
/// access token, write to `fleet_secrets[<provider>.oauth_token]`.
///
/// Returns `Err` if the cred file is missing or the JSON has no token
/// field — callers surface those as actionable messages ("run `<cli>
/// login` first").
pub async fn import_token(pool: &PgPool, provider: &OauthProvider) -> Result<()> {
    let path = expand_home(provider.cred_path)
        .ok_or_else(|| anyhow!("provider {} has no cred_path configured — set the token manually with `ff secrets set {}`", provider.name, provider.secret_key))?;

    let bytes = read_leader_cred_bytes(provider).await?;

    let json: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse cred for provider {} as JSON", provider.name))?;

    // Try each known token field. Walk three layouts:
    //   • flat top-level (most CLIs)
    //   • `tokens.<field>` (OpenAI codex CLI, ~/.codex/auth.json)
    //   • `claudeAiOauth.<field>` (Claude Code on macOS, Keychain blob)
    let token = provider
        .token_fields
        .iter()
        .find_map(|field| json.get(field).and_then(Value::as_str))
        .or_else(|| {
            provider.token_fields.iter().find_map(|field| {
                json.get("tokens")
                    .and_then(|t| t.get(field))
                    .and_then(Value::as_str)
            })
        })
        .or_else(|| {
            provider.token_fields.iter().find_map(|field| {
                json.get("claudeAiOauth")
                    .and_then(|t| t.get(field))
                    .and_then(Value::as_str)
            })
        })
        .ok_or_else(|| {
            anyhow!(
                "no token field found for provider {} (tried {:?} flat, under `tokens.*`, and under `claudeAiOauth.*`); cred shape may have changed",
                provider.name,
                provider.token_fields
            )
        })?;

    ff_db::pg_set_secret(
        pool,
        provider.secret_key,
        token,
        Some(&format!(
            "OAuth subscription token for {} (imported from {})",
            provider.name,
            path.display()
        )),
        Some("ff oauth import"),
    )
    .await
    .context("write token to fleet_secrets")?;

    // Enrollment needs the complete vendor-owned document (refresh token,
    // expiry, account metadata, etc.), not a guessed token-only JSON shape.
    // Keep it beside the extracted bearer token so bootstrap can pull it via
    // the enrollment-token allowlist without an ad-hoc file copy.
    let credentials_key = format!("{}.credentials", provider.secret_key);
    let credentials = std::str::from_utf8(&bytes)
        .with_context(|| format!("credential document for {} is not UTF-8", provider.name))?;
    ff_db::pg_set_secret(
        pool,
        &credentials_key,
        credentials,
        Some(&format!(
            "Complete OAuth credential document for {} onboarding",
            provider.name
        )),
        Some("ff oauth import"),
    )
    .await
    .context("write credential document to fleet_secrets")?;

    info!(
        provider = provider.name,
        "imported OAuth token to fleet_secrets"
    );
    Ok(())
}

/// Push the full credential file to every fleet member's matching path.
///
/// Uses base64 of the file contents and `pg_enqueue_shell_task_once` to fan
/// out via the existing wave dispatcher. Each per-target task writes the
/// decoded payload to the target's `<cred_path>` (with `mode 0600`) so
/// the local CLI sees the same login the leader did. Members without
/// the directory get it created (`mkdir -p`) before write.
pub async fn distribute_token(pool: &PgPool, provider: &OauthProvider) -> Result<usize> {
    if !oauth_distribution_enabled(pool).await {
        warn!(
            provider = provider.name,
            key = OAUTH_DISTRIBUTION_ENABLED_KEY,
            "OAuth distribution disabled; no fleet tasks enqueued"
        );
        return Ok(0);
    }

    // Keychain-first (macOS claude) / file source — same resolver as
    // `import_token`, so a macOS leader can fan out its Keychain-held claude
    // creds instead of failing on a nonexistent `~/.claude/.credentials.json`.
    let bytes = read_leader_cred_bytes(provider).await?;
    let b64 = BASE64.encode(&bytes);

    // Target list = every fleet member EXCEPT the leader (the leader's
    // local copy is already authoritative). Members are looked up by
    // primary_ip + ssh_user from the `computers` table.
    let leader_id = ff_db::pg_get_current_leader(pool)
        .await
        .ok()
        .flatten()
        .map(|l| l.computer_id);

    let rows = sqlx::query(
        // 'online' is the live status the heartbeat materializer writes;
        // ('ok','pending','maintenance') are legacy/transitional vocab kept
        // for compat. Omitting 'online' made distribute resolve ZERO targets
        // on a fleet whose members are all 'online' (silent enqueued=0).
        "SELECT id, name, ssh_user, primary_ip
           FROM computers
          WHERE status IN ('online', 'ok', 'pending', 'maintenance')",
    )
    .fetch_all(pool)
    .await
    .context("list computers")?;

    let mut enqueued = 0usize;
    let leader_uuid = leader_id;
    for row in rows {
        use sqlx::Row;
        let id: uuid::Uuid = row.get("id");
        if Some(id) == leader_uuid {
            continue;
        }
        let name: String = row.get("name");
        let ssh_user: String = row.get("ssh_user");
        let primary_ip: String = row.get("primary_ip");
        // Bind the DB-sourced target IP + the (const) cred path to shell vars
        // ONCE, then reference them QUOTED. This branch gates a local secret
        // WRITE on an identity check, so `primary_ip` must not be able to
        // misparse as grep options/pattern (hence `-- "$IP"`), and the path
        // must survive spaces/metachars. `~` is pre-expanded to `$HOME` here so
        // the quoted var still resolves (a quoted `~` would stay literal).
        let cred_path_sh = provider
            .cred_path
            .strip_prefix("~/")
            .map(|rest| format!("$HOME/{rest}"))
            .unwrap_or_else(|| provider.cred_path.to_string());
        // Escape single quotes so the `IP='…'` assignment is injection-proof
        // even if the DB value ever contained one (POSIX: close-quote, escaped
        // literal quote, reopen-quote). primary_ip is validated IP data today,
        // but this write path handles a secret — belt and suspenders.
        // Every DB-sourced value that reaches the shell is bound to a var via a
        // single-quote-escaped assignment (POSIX close/escaped-quote/reopen) and
        // referenced QUOTED — so no metacharacter in primary_ip / ssh_user /
        // name can inject into this secret-write command. Const/base64 values
        // (provider, cred_path, b64) are not attacker-influenced.
        let sh_squote = |s: &str| s.replace('\'', "'\\''");
        let primary_ip_sh = sh_squote(&primary_ip);
        let ssh_user_sh = sh_squote(&ssh_user);
        let target_sh = sh_squote(&name);
        // The remote payload: write the cred file with a heredoc (no
        // shell expansion of `$` inside the b64 blob), chmod 0600.
        let cmd = format!(
            "set -e\n\
             IP='{primary_ip_sh}'\n\
             SSH_USER='{ssh_user_sh}'\n\
             TARGET='{target_sh}'\n\
             CRED_PATH=\"{cred_path_sh}\"\n\
             echo \"== distributing {provider} cred file to $TARGET ==\"\n\
             __ff_local_ips() {{ (hostname -I 2>/dev/null; \
                 ifconfig 2>/dev/null | awk '/inet /{{print $2}}') | tr ' ' '\\n'; }}\n\
             if __ff_local_ips | grep -Fxq -- \"$IP\"; then\n\
             echo '(on target — local write, no SSH)'\n\
             mkdir -p \"$(dirname \"$CRED_PATH\")\"\n\
             umask 077\n\
             printf '%s' '{b64}' | base64 -d > \"$CRED_PATH\"\n\
             chmod 600 \"$CRED_PATH\"\n\
             echo distributed: $(stat -c %y \"$CRED_PATH\" 2>/dev/null || stat -f %Sm \"$CRED_PATH\")\n\
             else\n\
             ssh -T {ssh_bypass} -o StrictHostKeyChecking=accept-new \
                 \"$SSH_USER@$IP\" bash -l <<'FF_OAUTH_EOF'\n\
             mkdir -p \"$(dirname {cred_path})\"\n\
             umask 077\n\
             printf '%s' '{b64}' | base64 -d > {cred_path}\n\
             chmod 600 {cred_path}\n\
             echo distributed: $(stat -c %y {cred_path} 2>/dev/null || stat -f %Sm {cred_path})\n\
             FF_OAUTH_EOF\n\
             fi\n",
            provider = provider.name,
            primary_ip_sh = primary_ip_sh,
            ssh_user_sh = ssh_user_sh,
            target_sh = target_sh,
            cred_path = provider.cred_path,
            cred_path_sh = cred_path_sh,
            b64 = b64,
            ssh_bypass = crate::ssh_opts::SSH_AGENT_BYPASS,
        );

        let enqueue_once_key = oauth_distribute_enqueue_key(provider.name, id);
        let outcome = pg_enqueue_shell_task_once(
            pool,
            &enqueue_once_key,
            &format!(
                "oauth-distribute/{}: {} → {}",
                provider.name, provider.name, name
            ),
            &cmd,
            &[],
            Some(&name),
            None,
            70,
            None,
        )
        .await
        .with_context(|| format!("enqueue distribute task for {name}"))?;
        if outcome.was_enqueued() {
            enqueued += 1;
        } else {
            debug!(
                provider = provider.name,
                target = %name,
                task_id = %outcome.task_id(),
                "OAuth distribute task already active; duplicate suppressed"
            );
        }
    }

    info!(
        provider = provider.name,
        enqueued, "OAuth distribute tasks enqueued"
    );
    Ok(enqueued)
}

/// Result of the narrowly-scoped OAuth queue cleanup verb.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct OauthBacklogCancellation {
    /// Number of pending/dispatchable OAuth tasks matched by the operation.
    pub eligible: u64,
    /// Number actually moved to `cancelled` (`0` for a dry run).
    pub cancelled: u64,
    pub applied: bool,
}

/// Preview or cancel only the unstarted OAuth repush/distribute backlog.
///
/// Both paths use one transaction. Apply mode uses a single guarded `UPDATE`,
/// so a row that races to `running` before its lock is acquired is re-checked
/// and left untouched. Running, completed, failed, and already-cancelled rows
/// are never eligible.
pub async fn cancel_oauth_task_backlog(
    pool: &PgPool,
    apply: bool,
) -> Result<OauthBacklogCancellation> {
    let mut tx = pool
        .begin()
        .await
        .context("begin OAuth backlog transaction")?;
    if apply {
        let result = sqlx::query(OAUTH_BACKLOG_CANCEL_SQL)
            .execute(&mut *tx)
            .await
            .context("cancel pending OAuth task backlog")?;
        let cancelled = result.rows_affected();
        tx.commit()
            .await
            .context("commit OAuth backlog cancellation")?;
        Ok(OauthBacklogCancellation {
            eligible: cancelled,
            cancelled,
            applied: true,
        })
    } else {
        let eligible: i64 = sqlx::query_scalar(OAUTH_BACKLOG_COUNT_SQL)
            .fetch_one(&mut *tx)
            .await
            .context("count pending OAuth task backlog")?;
        tx.commit().await.context("finish OAuth backlog dry run")?;
        Ok(OauthBacklogCancellation {
            eligible: u64::try_from(eligible).unwrap_or(0),
            cancelled: 0,
            applied: false,
        })
    }
}

/// Per-provider snapshot of the leader's local state + fleet_secrets entry.
pub async fn status(pool: &PgPool) -> Result<Vec<ProviderStatus>> {
    let mut out = Vec::with_capacity(OAUTH_PROVIDERS.len());
    for p in OAUTH_PROVIDERS {
        let (cred_present, mtime_ago) = match expand_home(p.cred_path) {
            Some(path) => match tokio::fs::metadata(&path).await {
                Ok(meta) => {
                    let ago = meta
                        .modified()
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .map(|d| d.as_secs());
                    (true, ago)
                }
                Err(_) => (false, None),
            },
            None => (false, None),
        };

        let token = ff_db::pg_get_secret(pool, p.secret_key)
            .await
            .ok()
            .flatten();
        let preview = token.as_deref().map(|t| {
            let head: String = t.chars().take(8).collect();
            format!("{head}…({} chars)", t.chars().count())
        });

        out.push(ProviderStatus {
            name: p.name.to_string(),
            cred_file_present: cred_present,
            cred_file_mtime_secs_ago: mtime_ago,
            token_in_secrets: token.is_some(),
            token_preview: preview,
        });
    }
    Ok(out)
}

/// Outcome of one OAuth probe.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeResult {
    pub provider: String,
    /// `ok` (200/2xx), `unauthorized` (401), `forbidden` (403),
    /// `no_token` (no fleet_secrets entry), `network_error`,
    /// `http_<code>` for other status codes.
    pub status: String,
    pub http_status: Option<u16>,
    pub message: Option<String>,
}

/// Probe every configured provider in [`OAUTH_PROVIDERS`]. Skips entries
/// that have no `cred_path` configured (e.g. grok today, where the token
/// is set manually rather than harvested) — they have nothing to probe.
pub async fn probe_all(pool: &PgPool) -> Vec<ProbeResult> {
    let mut out = Vec::with_capacity(OAUTH_PROVIDERS.len());
    for p in OAUTH_PROVIDERS {
        if p.cred_path.is_empty() {
            // Provider has no on-disk cred file (manual-set token).
            // Probe still runs because we just need a token, not a file.
        }
        out.push(probe_one(pool, p).await);
    }
    out
}

/// Trigger the provider CLI's native refresh flow, then import and distribute
/// the complete refreshed credential. Import still runs after a failed probe:
/// some CLIs rotate credentials before returning a non-zero diagnostic.
pub async fn refresh_and_distribute(pool: &PgPool, provider: &OauthProvider) -> Result<usize> {
    if !oauth_distribution_enabled(pool).await {
        return Ok(0);
    }
    let probe = probe_one(pool, provider).await;
    if probe.status != "ok" {
        warn!(provider = provider.name, status = %probe.status, detail = ?probe.message,
            "OAuth native refresh probe did not return cleanly; importing freshest credential anyway");
    }
    import_token(pool, provider).await?;
    distribute_token(pool, provider).await
}

/// Validate this node's distributed credentials once at daemon startup. A
/// stale follower asks the current leader for an immediate provider refresh;
/// the leader-side task is naturally serialized by the fleet task runner.
pub async fn validate_startup_and_request_repush(pool: &PgPool, worker_name: &str) {
    if crate::leader_cache::is_current_leader() {
        return;
    }
    if !oauth_distribution_enabled(pool).await {
        return;
    }
    let leader = match ff_db::pg_get_current_leader(pool).await {
        Ok(Some(leader)) => leader,
        _ => return,
    };
    let leader_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM computers WHERE id = $1")
            .bind(leader.computer_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some(leader_name) = leader_name else {
        return;
    };
    if leader_name == worker_name {
        return;
    }

    for name in AUTO_REFRESH_PROVIDERS {
        let Some(provider) = provider_by_name(name) else {
            continue;
        };
        let probe = probe_one(pool, provider).await;
        if probe.status == "ok" {
            continue;
        }
        let title = format!("oauth-repush/{} requested-by {worker_name}", provider.name);
        let command = format!("ff oauth refresh {}", provider.name);
        let enqueue_once_key = oauth_repush_enqueue_key(provider.name, leader.computer_id);
        let outcome = pg_enqueue_shell_task_once(
            pool,
            &enqueue_once_key,
            &title,
            &command,
            &[],
            Some(&leader_name),
            None,
            90,
            None,
        )
        .await;
        match outcome {
            Err(error) => {
                warn!(provider = provider.name, %error, "failed to request OAuth re-push from leader");
            }
            Ok(EnqueueOnceOutcome::Enqueued(task_id)) => {
                warn!(provider = provider.name, status = %probe.status, %task_id,
                    "startup OAuth validation failed; requested leader re-push");
            }
            Ok(EnqueueOnceOutcome::AlreadyActive(task_id)) => {
                debug!(provider = provider.name, status = %probe.status, %task_id,
                    "startup OAuth validation failed; leader re-push already active");
            }
        }
    }
}

/// Leader-gated: spawned unconditionally on every node, but each tick checks
/// the process-local leader cache and skips unless this node is the current
/// leader (only the leader probes the shared OAuth credentials).
pub fn spawn_oauth_probe_tick(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }
                    if !oauth_distribution_enabled(&pg).await {
                        continue;
                    }

                    for name in AUTO_REFRESH_PROVIDERS {
                        let Some(provider) = provider_by_name(name) else { continue };
                        match refresh_and_distribute(&pg, provider).await {
                            Ok(enqueued) => info!(provider = provider.name, enqueued,
                                "periodic OAuth refresh and distribution complete"),
                            Err(error) => warn!(provider = provider.name, %error,
                                "periodic OAuth refresh and distribution failed"),
                        }
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

/// Probe one OAuth provider's token by hitting its `/v1/models`-style
/// endpoint. Returns shape suitable for CLI rendering or alert dispatch.
pub async fn probe_one(pool: &PgPool, provider: &OauthProvider) -> ProbeResult {
    let token = match ff_db::pg_get_secret(pool, provider.secret_key).await {
        Ok(Some(t)) if !t.is_empty() => t,
        _ => {
            return ProbeResult {
                provider: provider.name.to_string(),
                status: "no_token".to_string(),
                http_status: None,
                message: Some(format!(
                    "fleet_secrets[{}] is unset; run `ff oauth import {}` first",
                    provider.secret_key, provider.name
                )),
            };
        }
    };

    // Vendor subscription tokens are NOT API bearer tokens — probe by
    // spawning the vendor CLI with a tiny prompt. Reuse cli_executor so probes
    // and fleet dispatch share the same hardened headless flags and detached
    // stdin behavior. See feedback in 2026-05-03 session: api.openai.com 403
    // on a valid Plus token.
    let _ = token;
    if crate::cli_executor::backend_by_name(provider.name).is_none() {
        return ProbeResult {
            provider: provider.name.to_string(),
            status: "no_token".to_string(),
            http_status: None,
            message: Some(format!(
                "unknown provider `{}`; no probe path",
                provider.name
            )),
        };
    }

    match crate::cli_executor::execute_cli(
        provider.name,
        "ping",
        &[],
        Some(Duration::from_secs(30)),
    )
    .await
    {
        Ok(out) => {
            if out.exit_code == 0 && !out.stdout.is_empty() {
                ProbeResult {
                    provider: provider.name.to_string(),
                    status: "ok".to_string(),
                    http_status: None,
                    message: None,
                }
            } else {
                ProbeResult {
                    provider: provider.name.to_string(),
                    status: if out.exit_code == 1 {
                        "unauthorized".to_string()
                    } else {
                        "cli_error".to_string()
                    },
                    http_status: Some(out.exit_code as u16),
                    message: Some(out.stderr.chars().take(200).collect()),
                }
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let (status, message) = if msg.contains("requires `") {
                ("cli_missing".to_string(), msg)
            } else if msg.contains("exceeded 30s timeout") {
                (
                    "timeout".to_string(),
                    format!("{} did not respond within 30s", provider.name),
                )
            } else {
                ("cli_error".to_string(), msg)
            };
            ProbeResult {
                provider: provider.name.to_string(),
                status,
                http_status: None,
                message: Some(message),
            }
        }
    }
}

/// Long-lived foreground loop. Polls every leader cred file every
/// `REFRESH_POLL_SECS`; on mtime change, re-imports + redistributes.
/// Exits when `shutdown` flips to true.
pub fn spawn_refresh_watch(pool: PgPool, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Track last-seen mtime per provider so we only fire on change.
        let mut last_mtime: std::collections::HashMap<&str, SystemTime> =
            std::collections::HashMap::new();

        loop {
            for p in OAUTH_PROVIDERS {
                let Some(path) = expand_home(p.cred_path) else {
                    continue;
                };
                let Ok(meta) = tokio::fs::metadata(&path).await else {
                    continue;
                };
                let Ok(mtime) = meta.modified() else {
                    continue;
                };
                let prev = last_mtime.insert(p.name, mtime);
                let changed = match prev {
                    Some(prev_t) => prev_t != mtime,
                    // First sighting — don't fire (the import was either
                    // already done or will be done explicitly via
                    // `ff oauth import`).
                    None => false,
                };
                if changed {
                    info!(provider = p.name, "cred file changed — re-importing");
                    if let Err(e) = import_token(&pool, p).await {
                        warn!(provider = p.name, error = %e, "auto-import failed");
                        continue;
                    }
                    if let Err(e) = distribute_token(&pool, p).await {
                        warn!(provider = p.name, error = %e, "auto-distribute failed");
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(REFRESH_POLL_SECS)) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_gate_defaults_and_ttl_restore_enabled_but_errors_fail_closed() {
        assert!(OAUTH_DISTRIBUTION_DEFAULT);
        assert!(OAUTH_DISTRIBUTION_RESTORE_ON_EXPIRY);
        assert!(resolve_oauth_distribution_gate(Ok::<bool, anyhow::Error>(
            true
        )));
        assert!(!resolve_oauth_distribution_gate(Ok::<bool, anyhow::Error>(
            false
        )));
        assert!(!resolve_oauth_distribution_gate(Err(anyhow::anyhow!(
            "gate database unavailable"
        ))));
    }

    #[test]
    fn enqueue_once_keys_scope_repush_and_distribution_independently() {
        let leader = uuid::Uuid::nil();
        let target = uuid::Uuid::from_u128(1);
        assert_eq!(
            oauth_repush_enqueue_key("codex", leader),
            format!("oauth-repush:codex:{leader}")
        );
        assert_eq!(
            oauth_distribute_enqueue_key("codex", target),
            format!("oauth-distribute:codex:{target}")
        );
        assert_ne!(
            oauth_distribute_enqueue_key("codex", target),
            oauth_distribute_enqueue_key("kimi", target)
        );
    }

    #[test]
    fn backlog_cleanup_sql_is_narrow_and_never_names_running_or_terminal_statuses() {
        for sql in [OAUTH_BACKLOG_COUNT_SQL, OAUTH_BACKLOG_CANCEL_SQL] {
            assert!(sql.contains("task_type = 'shell'"));
            assert!(sql.contains("status IN ('pending', 'dispatchable')"));
            assert!(sql.contains("summary LIKE 'oauth-repush/%'"));
            assert!(sql.contains("summary LIKE 'oauth-distribute/%'"));
            assert!(!sql.contains("status IN ('running'"));
            assert!(!sql.contains("status IN ('completed'"));
        }
    }

    #[test]
    fn both_autonomous_producers_check_the_gate_before_enqueuing() {
        let source = include_str!("oauth_distributor.rs");
        let startup = source
            .split("pub async fn validate_startup_and_request_repush")
            .nth(1)
            .expect("startup producer")
            .split("pub fn spawn_oauth_probe_tick")
            .next()
            .expect("startup body");
        assert!(startup.contains("oauth_distribution_enabled(pool).await"));
        assert!(startup.contains("pg_enqueue_shell_task_once"));

        let periodic = source
            .split("pub fn spawn_oauth_probe_tick")
            .nth(1)
            .expect("periodic producer")
            .split("pub async fn probe_one")
            .next()
            .expect("periodic body");
        assert!(periodic.contains("oauth_distribution_enabled(&pg).await"));
        assert!(periodic.contains("refresh_and_distribute(&pg, provider).await"));
    }
}
