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
use tracing::{info, warn};

use crate::task_runner::pg_enqueue_shell_task;

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
        cred_path: "~/.moonshot/auth.json",
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

/// Read the leader's credential file for one provider, extract the
/// access token, write to `fleet_secrets[<provider>.oauth_token]`.
///
/// Returns `Err` if the cred file is missing or the JSON has no token
/// field — callers surface those as actionable messages ("run `<cli>
/// login` first").
pub async fn import_token(pool: &PgPool, provider: &OauthProvider) -> Result<()> {
    let path = expand_home(provider.cred_path)
        .ok_or_else(|| anyhow!("provider {} has no cred_path configured — set the token manually with `ff secrets set {}`", provider.name, provider.secret_key))?;

    let bytes = tokio::fs::read(&path).await.with_context(|| {
        format!(
            "read cred file {} for provider {} — run `{} login` first",
            path.display(),
            provider.name,
            provider.name
        )
    })?;

    let json: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse cred file {} as JSON", path.display()))?;

    // Try each known token field in order. Some CLIs nest the token
    // under e.g. `tokens.access_token`; we don't currently descend, but
    // the most common shapes are flat top-level fields.
    let token = provider
        .token_fields
        .iter()
        .find_map(|field| json.get(field).and_then(Value::as_str))
        .ok_or_else(|| {
            anyhow!(
                "no token field found in {} (tried {:?}); the cred file shape may have changed",
                path.display(),
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

    info!(
        provider = provider.name,
        "imported OAuth token to fleet_secrets"
    );
    Ok(())
}

/// Push the full credential file to every fleet member's matching path.
///
/// Uses base64 of the file contents and `pg_enqueue_shell_task` to fan
/// out via the existing wave dispatcher. Each per-target task writes the
/// decoded payload to the target's `<cred_path>` (with `mode 0600`) so
/// the local CLI sees the same login the leader did. Members without
/// the directory get it created (`mkdir -p`) before write.
pub async fn distribute_token(pool: &PgPool, provider: &OauthProvider) -> Result<usize> {
    let path = expand_home(provider.cred_path).ok_or_else(|| {
        anyhow!(
            "provider {} has no cred_path — distribute is N/A",
            provider.name
        )
    })?;

    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("read leader cred file {}", path.display()))?;
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
        "SELECT id, name, ssh_user, primary_ip
           FROM computers
          WHERE status IN ('ok', 'pending', 'maintenance')",
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
        // The remote payload: write the cred file with a heredoc (no
        // shell expansion of `$` inside the b64 blob), chmod 0600.
        let cmd = format!(
            "set -e\n\
             echo \"== distributing {provider} cred file to {target} ==\"\n\
             ssh -T -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
                 {ssh_user}@{primary_ip} bash -l <<'FF_OAUTH_EOF'\n\
             mkdir -p \"$(dirname {cred_path})\"\n\
             umask 077\n\
             printf '%s' '{b64}' | base64 -d > {cred_path}\n\
             chmod 600 {cred_path}\n\
             echo distributed: $(stat -c %y {cred_path} 2>/dev/null || stat -f %Sm {cred_path})\n\
             FF_OAUTH_EOF\n",
            provider = provider.name,
            target = name,
            ssh_user = ssh_user,
            primary_ip = primary_ip,
            cred_path = provider.cred_path,
            b64 = b64,
        );

        pg_enqueue_shell_task(
            pool,
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
        .with_context(|| format!("enqueue distribute task for {}", name))?;
        enqueued += 1;
    }

    info!(
        provider = provider.name,
        enqueued, "OAuth distribute tasks enqueued"
    );
    Ok(enqueued)
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
