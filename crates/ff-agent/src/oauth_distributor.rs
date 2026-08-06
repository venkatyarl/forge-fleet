//! OAuth credential harvest + distribute for the multi-LLM CLI integration.
//!
//! Each vendor CLI (Claude Code, OpenAI Codex, Google Gemini CLI, …) writes
//! its OAuth/session token to a local credential file when the user runs
//! `<cli> login`. ff doesn't reimplement OAuth — instead it:
//!
//! 1. **Imports** (on the leader): reads the local file for one provider,
//!    extracts the access token, stores it in `fleet_secrets` keyed by the
//!    provider's `secret_key` (e.g. `anthropic.oauth_token`).
//! 2. **Distributes**: stores the credential document in the encrypted fleet
//!    secret store and enqueues target-bound reference-only tasks. Credential
//!    bytes never enter ordinary `fleet_tasks` payloads or output.
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
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

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
}

const OAUTH_TASK_CLASS: &str = "oauth";
const OAUTH_RECONCILE_LOCK: i64 = 0x4f41_5554_4851_5545;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct QueueReconcileReport {
    pub pending_found: usize,
    pub would_cancel: usize,
    pub would_enqueue: usize,
    pub preserved_active: usize,
    pub preserved_terminal: usize,
    pub applied: bool,
}

#[derive(Debug, Clone)]
struct PendingOauthRow {
    id: uuid::Uuid,
    task_class: Option<String>,
    dedup_key: Option<String>,
}

fn plan_pending_rows(
    rows: &[PendingOauthRow],
    desired_keys: &std::collections::HashSet<&str>,
) -> (std::collections::HashSet<String>, Vec<uuid::Uuid>) {
    let mut keep = std::collections::HashSet::new();
    let mut cancel = Vec::new();
    for row in rows {
        let valid = row.task_class.as_deref() == Some(OAUTH_TASK_CLASS)
            && row
                .dedup_key
                .as_deref()
                .is_some_and(|key| desired_keys.contains(key))
            && row
                .dedup_key
                .as_ref()
                .is_some_and(|key| keep.insert(key.clone()));
        if !valid {
            cancel.push(row.id);
        }
    }
    (keep, cancel)
}

fn credential_token<'a>(provider: &OauthProvider, json: &'a Value) -> Option<&'a str> {
    provider.token_fields.iter().find_map(|field| {
        json.get(field)
            .or_else(|| json.get("tokens").and_then(|v| v.get(field)))
            .or_else(|| json.get("claudeAiOauth").and_then(|v| v.get(field)))
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
    })
}

fn credential_expiry(json: &Value) -> Option<i64> {
    ["expires_at", "expiresAt", "expiry", "expires"]
        .into_iter()
        .find_map(|field| {
            json.get(field)
                .or_else(|| json.get("tokens").and_then(|v| v.get(field)))
                .or_else(|| json.get("claudeAiOauth").and_then(|v| v.get(field)))
        })
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
                .map(|value| {
                    if value > 10_000_000_000 {
                        value / 1000
                    } else {
                        value
                    }
                })
                .or_else(|| {
                    value.as_str().and_then(|v| {
                        chrono::DateTime::parse_from_rfc3339(v)
                            .ok()
                            .map(|v| v.timestamp())
                    })
                })
        })
}

fn validate_credential_document(provider: &OauthProvider, bytes: &[u8], now: i64) -> Result<Value> {
    if bytes.is_empty() {
        anyhow::bail!("{} credential document is empty", provider.name);
    }
    let json: Value = serde_json::from_slice(bytes)
        .with_context(|| format!("parse {} credential document", provider.name))?;
    credential_token(provider, &json).ok_or_else(|| {
        anyhow!(
            "{} credential document has no non-empty access token",
            provider.name
        )
    })?;
    if credential_expiry(&json).is_some_and(|expiry| expiry <= now) {
        anyhow::bail!("{} credential document is expired", provider.name);
    }
    Ok(json)
}

fn validate_provider_authorization(
    provider: &OauthProvider,
    claude_setup: Option<&str>,
) -> Result<()> {
    if provider.name == "claude" && claude_setup.is_none_or(|value| value.trim().is_empty()) {
        anyhow::bail!(
            "claude.setup_token is absent; refusing to distribute a non-durable Claude session"
        );
    }
    Ok(())
}

pub async fn validate_provider_credential(
    pool: &PgPool,
    provider: &OauthProvider,
) -> Result<Vec<u8>> {
    let bytes = read_leader_cred_bytes(provider).await?;
    validate_credential_document(provider, &bytes, chrono::Utc::now().timestamp())?;
    if provider.name == "claude" {
        let setup = ff_db::pg_get_secret(pool, "claude.setup_token").await?;
        validate_provider_authorization(provider, setup.as_deref())?;
    }
    Ok(bytes)
}

pub async fn validate_provider_for_distribution(
    pool: &PgPool,
    provider: &OauthProvider,
) -> Result<Vec<u8>> {
    let bytes = validate_provider_credential(pool, provider).await?;
    // Claude's subscription CLI probe can rotate/churn the operator-owned
    // OAuth session. The durable setup-token check above is the explicit
    // authorization boundary for Claude distribution, so never invoke the
    // CLI merely to validate a fanout.
    if provider.name == "claude" {
        return Ok(bytes);
    }
    let output =
        crate::cli_executor::execute_cli(provider.name, "ping", &[], Some(Duration::from_secs(30)))
            .await
            .with_context(|| format!("probe {} credential", provider.name))?;
    if output.exit_code != 0 || output.stdout.trim().is_empty() {
        anyhow::bail!(
            "{} credential probe was unauthorized or unusable (exit {})",
            provider.name,
            output.exit_code
        );
    }
    Ok(bytes)
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

/// Reconcile one provider to one pending task per target. Task rows contain
/// only a secret-store reference; the target resolves it immediately before
/// installing the credential.
pub async fn distribute_token(pool: &PgPool, provider: &OauthProvider) -> Result<usize> {
    let bytes = validate_provider_for_distribution(pool, provider).await?;
    let credentials = std::str::from_utf8(&bytes)
        .with_context(|| format!("{} credential document is not UTF-8", provider.name))?;
    ff_db::pg_set_secret(
        pool,
        &format!("{}.credentials", provider.secret_key),
        credentials,
        Some("OAuth credential document resolved only by target workers"),
        Some("ff oauth distribute"),
    )
    .await?;
    let report = reconcile_queue(pool, &[provider], &[provider], true).await?;
    info!(
        provider = provider.name,
        enqueued = report.would_enqueue,
        "OAuth queue reconciled"
    );
    Ok(report.would_enqueue)
}

pub async fn install_secret_ref(pool: &PgPool, provider: &OauthProvider) -> Result<()> {
    let secret_ref = format!("{}.credentials", provider.secret_key);
    let task_id: uuid::Uuid = std::env::var("FF_FLEET_TASK_ID")
        .context("install-ref is restricted to a running fleet task")?
        .parse()
        .context("invalid fleet task identity")?;
    let node = std::env::var("FF_NODE").context("install-ref is missing worker identity")?;
    let authorized: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM fleet_tasks t
            JOIN computers c ON c.id=t.claimed_by_computer_id
            WHERE t.id=$1 AND t.status='running' AND t.task_class='oauth'
              AND t.claimed_by_computer_id=t.preferred_computer_id
              AND t.payload->>'secret_ref'=$2 AND c.name=$3
        )",
    )
    .bind(task_id)
    .bind(&secret_ref)
    .bind(&node)
    .fetch_one(pool)
    .await?;
    if !authorized {
        anyhow::bail!("install-ref is not authorized for this worker task");
    }
    let credentials = ff_db::pg_get_secret(pool, &secret_ref)
        .await?
        .ok_or_else(|| anyhow!("credential reference is unavailable"))?;
    validate_credential_document(
        provider,
        credentials.as_bytes(),
        chrono::Utc::now().timestamp(),
    )?;
    let path = expand_home(provider.cred_path)
        .ok_or_else(|| anyhow!("{} has no credential path", provider.name))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension(format!("ff-oauth-{}", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, credentials.as_bytes()).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(&temporary, &path).await?;
    info!(
        provider = provider.name,
        "installed OAuth credential from secret reference"
    );
    Ok(())
}

pub async fn reconcile_queue(
    pool: &PgPool,
    scope_providers: &[&OauthProvider],
    enqueue_providers: &[&OauthProvider],
    apply: bool,
) -> Result<QueueReconcileReport> {
    let leader_id = ff_db::pg_get_current_leader(pool)
        .await?
        .map(|leader| leader.computer_id);
    let targets = sqlx::query(
        "SELECT id, name FROM computers WHERE status IN ('online','ok','pending','maintenance') ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let desired: Vec<(uuid::Uuid, String, String, String)> = enqueue_providers
        .iter()
        .flat_map(|provider| {
            targets.iter().filter_map(move |row| {
                let id: uuid::Uuid = row.get("id");
                (Some(id) != leader_id).then(|| {
                    let name: String = row.get("name");
                    let key = format!("oauth:{}:{id}", provider.name);
                    (id, name, provider.name.to_string(), key)
                })
            })
        })
        .collect();

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(OAUTH_RECONCILE_LOCK)
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query(
        "SELECT id, status, task_class, preferred_computer_id, payload, summary
           FROM fleet_tasks
          WHERE task_class = 'oauth'
             OR summary LIKE 'oauth-distribute/%'
             OR summary LIKE 'oauth-repush/%'
          ORDER BY created_at, id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut report = QueueReconcileReport {
        applied: apply,
        ..Default::default()
    };
    let desired_keys: std::collections::HashSet<&str> =
        desired.iter().map(|(_, _, _, key)| key.as_str()).collect();
    let mut pending = Vec::new();
    let mut active_keys = std::collections::HashSet::new();
    for row in rows {
        let summary: String = row.get("summary");
        let payload: Value = row.get("payload");
        let in_scope = scope_providers.iter().any(|provider| {
            summary.starts_with(&format!("oauth-distribute/{}", provider.name))
                || summary.starts_with(&format!("oauth-repush/{}", provider.name))
                || payload
                    .get("dedup_key")
                    .and_then(Value::as_str)
                    .is_some_and(|key| key.starts_with(&format!("oauth:{}:", provider.name)))
        });
        if !in_scope {
            continue;
        }
        let status: String = row.get("status");
        if status == "pending" {
            report.pending_found += 1;
            pending.push(PendingOauthRow {
                id: row.get("id"),
                task_class: row.get("task_class"),
                dedup_key: payload
                    .get("dedup_key")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        } else if matches!(status.as_str(), "running" | "claimed" | "paused") {
            report.preserved_active += 1;
            if row.get::<Option<String>, _>("task_class").as_deref() == Some(OAUTH_TASK_CLASS)
                && let Some(key) = payload.get("dedup_key").and_then(Value::as_str)
                && desired_keys.contains(key)
            {
                active_keys.insert(key.to_string());
            }
        } else {
            report.preserved_terminal += 1;
        }
    }
    let (keep, cancel) = plan_pending_rows(&pending, &desired_keys);
    report.would_cancel = cancel.len();
    let missing: Vec<_> = desired
        .iter()
        .filter(|(_, _, _, key)| !keep.contains(key) && !active_keys.contains(key))
        .collect();
    report.would_enqueue = missing.len();

    if apply {
        if !cancel.is_empty() {
            sqlx::query(
                "UPDATE fleet_tasks SET status='cancelled', completed_at=NOW(), error='superseded by oauth queue reconciliation'
                  WHERE id = ANY($1) AND status='pending'",
            )
            .bind(&cancel)
            .execute(&mut *tx)
            .await?;
        }
        for (target_id, target_name, provider, key) in missing {
            let secret_ref = format!(
                "{}.credentials",
                provider_by_name(provider).unwrap().secret_key
            );
            let payload = serde_json::json!({
                "command": format!("ff oauth install-ref {provider}"),
                "secret_ref": secret_ref,
                "dedup_key": key,
            });
            sqlx::query(
                "INSERT INTO fleet_tasks
                    (task_type, summary, payload, priority, preferred_computer_id, task_class)
                 VALUES ('shell', $1, $2, 70, $3, 'oauth')",
            )
            .bind(format!("oauth-distribute/{provider}: target {target_name}"))
            .bind(payload)
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(report)
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
        out.push(ProviderStatus {
            name: p.name.to_string(),
            cred_file_present: cred_present,
            cred_file_mtime_secs_ago: mtime_ago,
            token_in_secrets: token.is_some(),
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
    let probe = probe_one(pool, provider).await;
    if matches!(probe.status.as_str(), "unauthorized" | "forbidden") {
        anyhow::bail!(
            "{} credential was rejected as {}; refusing to enqueue distribution",
            provider.name,
            probe.status
        );
    }
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
        // A Claude follower must not trigger a repush without the operator's
        // durable setup-token authorization, and probing Claude itself can
        // churn that operator-owned OAuth session.
        if provider.name == "claude" {
            let setup = ff_db::pg_get_secret(pool, "claude.setup_token")
                .await
                .ok()
                .flatten();
            if validate_provider_authorization(provider, setup.as_deref()).is_err() {
                continue;
            }
            continue;
        }
        let probe = probe_one(pool, provider).await;
        if probe.status == "ok" {
            continue;
        }
        let key = format!("oauth-repush:{}:{worker_name}", provider.name);
        let payload = serde_json::json!({
            "command": format!("ff oauth refresh {}", provider.name),
            "dedup_key": key,
        });
        let result = sqlx::query(
            "WITH lock AS MATERIALIZED (
                SELECT pg_advisory_xact_lock(hashtext($4)::bigint)
             )
             INSERT INTO fleet_tasks
                (task_type, summary, payload, priority, preferred_computer_id, task_class)
             SELECT 'shell', $1, $2, 90, $3, 'oauth' FROM lock
              WHERE NOT EXISTS (
                SELECT 1 FROM fleet_tasks
                 WHERE task_class='oauth'
                   AND payload->>'dedup_key'=$4
                   AND status IN ('pending','claimed','running')
              )
                AND NOT EXISTS (
                SELECT 1 FROM fleet_tasks
                 WHERE task_class='oauth'
                   AND payload->>'dedup_key'=$4
                   AND created_at > NOW() - INTERVAL '15 minutes'
              )",
        )
        .bind(format!(
            "oauth-repush/{} requested-by {worker_name}",
            provider.name
        ))
        .bind(payload)
        .bind(leader.computer_id)
        .bind(&key)
        .execute(pool)
        .await;
        if let Err(error) = result {
            warn!(provider = provider.name, %error, "failed to request OAuth re-push from leader");
        } else if result.is_ok_and(|result| result.rows_affected() == 1) {
            warn!(provider = provider.name, status = %probe.status,
                "startup OAuth validation failed; requested leader re-push");
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
    fn valid_codex_document_is_accepted_without_exposing_token() {
        let provider = provider_by_name("codex").unwrap();
        let document = br#"{"tokens":{"access_token":"secret-codex","expires_at":4102444800}}"#;
        let parsed = validate_credential_document(provider, document, 1_800_000_000).unwrap();
        assert_eq!(credential_token(provider, &parsed), Some("secret-codex"));
    }

    #[test]
    fn stale_kimi_and_empty_credentials_are_rejected() {
        let kimi = provider_by_name("kimi").unwrap();
        let stale = br#"{"access_token":"stale-kimi","expires_at":1700000000}"#;
        assert!(validate_credential_document(kimi, stale, 1_800_000_000).is_err());
        assert!(validate_credential_document(kimi, b"", 1_800_000_000).is_err());
    }

    #[test]
    fn claude_requires_explicit_durable_setup_token() {
        let claude = provider_by_name("claude").unwrap();
        assert!(validate_provider_authorization(claude, None).is_err());
        assert!(validate_provider_authorization(claude, Some("  ")).is_err());
        assert!(validate_provider_authorization(claude, Some("setup-token")).is_ok());
    }

    #[test]
    fn flood_reconciliation_keeps_one_and_cancels_legacy_and_duplicates() {
        let key = "oauth:codex:00000000-0000-0000-0000-000000000001";
        let desired = std::collections::HashSet::from([key]);
        let mut rows = Vec::with_capacity(48_457);
        for index in 0..48_457u128 {
            rows.push(PendingOauthRow {
                id: uuid::Uuid::from_u128(index + 1),
                task_class: (index != 0).then(|| OAUTH_TASK_CLASS.to_string()),
                dedup_key: (index != 0).then(|| key.to_string()),
            });
        }
        let (keep, cancel) = plan_pending_rows(&rows, &desired);
        assert_eq!(keep.len(), 1);
        assert_eq!(cancel.len(), 48_456);
    }

    #[test]
    fn concurrent_refresh_shape_is_single_flight_and_legacy_class_is_superseded() {
        let key = "oauth:codex:target";
        let desired = std::collections::HashSet::from([key]);
        let rows = vec![
            PendingOauthRow {
                id: uuid::Uuid::from_u128(1),
                task_class: None,
                dedup_key: None,
            },
            PendingOauthRow {
                id: uuid::Uuid::from_u128(2),
                task_class: Some(OAUTH_TASK_CLASS.into()),
                dedup_key: Some(key.into()),
            },
            PendingOauthRow {
                id: uuid::Uuid::from_u128(3),
                task_class: Some(OAUTH_TASK_CLASS.into()),
                dedup_key: Some(key.into()),
            },
        ];
        let (keep, cancel) = plan_pending_rows(&rows, &desired);
        assert_eq!(keep, std::collections::HashSet::from([key.to_string()]));
        assert_eq!(cancel.len(), 2);
    }
}
