//! OAuth credential harvest + distribute for the multi-LLM CLI integration.
//!
//! Each vendor CLI (Claude Code, OpenAI Codex, Google Gemini CLI, …) writes
//! its OAuth/session token to a local credential file when the user runs
//! `<cli> login`. ff doesn't reimplement OAuth — instead it:
//!
//! 1. **Imports** (on the leader): reads the local file for one provider,
//!    extracts the access token, stores it in `fleet_secrets` keyed by the
//!    provider's `secret_key` (e.g. `anthropic.oauth_token`).
//! 2. **Distributes**: queues a typed, non-secret reference for every target.
//!    The target resolves the complete credential document from
//!    `fleet_secrets` just in time and installs it without a shell.
//! 3. **Status**: reports per-provider whether the token is present,
//!    decoded expiry, and last refresh time.
//! 4. **RefreshWatch**: long-lived loop that polls the leader's cred files
//!    every `REFRESH_POLL_SECS` and re-imports whenever the file's mtime
//!    changes. Distribution always requires an explicit target list.
//!
//! Layer 1 (`cloud_llm.rs::try_route_to_cloud`) reads
//! `fleet_secrets[<provider>.oauth_token]` for the `oauth_subscription`
//! `auth_kind` and uses it as the `Authorization: Bearer …` value.
//!
//! See `~/.claude/plans/cosmic-splashing-chipmunk.md` for the full
//! roadmap context.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::future::try_join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Component, PathBuf};
use std::time::{Duration, SystemTime};
#[cfg(unix)]
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
        unix::fs::MetadataExt,
    },
};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::task_runner::{
    EnqueueOnceOutcome, pg_enqueue_oauth_credential_install_once_tx, pg_enqueue_shell_task_once,
};

const OAUTH_CREDENTIAL_INSTALL_OPERATION: &str = "install_oauth_credentials";
const OAUTH_CREDENTIAL_INSTALL_VERSION: u8 = 1;
const MAX_CREDENTIAL_DOCUMENT_BYTES: usize = 1024 * 1024;
const OAUTH_LEADER_FRESHNESS_SECS: i64 = 45;
const OAUTH_TARGET_FRESHNESS_SECS: i64 = 180;
const OAUTH_TARGET_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const OAUTH_CREDENTIAL_SOURCE_TIMEOUT: Duration = Duration::from_secs(10);
const OAUTH_AUTHORITY_XACT_LOCK_KEY: i64 = 0x4646_4f41_5554_4801;

#[derive(Debug, Clone)]
struct OauthLeaderAuthority {
    computer_id: uuid::Uuid,
    member_name: String,
    epoch: i64,
}

#[derive(Debug, Clone)]
struct OauthTarget {
    computer_id: uuid::Uuid,
    name: String,
    primary_ip: IpAddr,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct GatewayPortAuthority {
    port: u16,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OauthCredentialInstallPayload {
    operation: String,
    version: u8,
    provider: String,
    secret_ref: String,
    secret_version: String,
    target_computer_id: uuid::Uuid,
}

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

#[cfg(test)]
const LEGACY_OAUTH_PAYLOAD_PREDICATE: &str = "task_type = 'shell'
        AND summary LIKE 'oauth-distribute/%'
        AND jsonb_typeof(payload) = 'object'
        AND jsonb_typeof(payload->'command') = 'string'
        AND payload->>'command' LIKE '%FF_OAUTH_EOF%'
        AND payload->>'command' LIKE '%base64 -d%'";

const OAUTH_BACKLOG_COUNT_SQL: &str = "SELECT
        COUNT(*) FILTER (WHERE task_type = 'shell'
            AND summary LIKE 'oauth-distribute/%'
            AND jsonb_typeof(payload) = 'object'
            AND jsonb_typeof(payload->'command') = 'string'
            AND payload->>'command' LIKE '%FF_OAUTH_EOF%'
            AND payload->>'command' LIKE '%base64 -d%')::bigint AS legacy_matched,
        COUNT(*) FILTER (WHERE task_type = 'shell'
            AND summary LIKE 'oauth-distribute/%'
            AND jsonb_typeof(payload) = 'object'
            AND jsonb_typeof(payload->'command') = 'string'
            AND payload->>'command' LIKE '%FF_OAUTH_EOF%'
            AND payload->>'command' LIKE '%base64 -d%'
            AND status = 'running')::bigint AS running_blocked,
        COUNT(*) FILTER (WHERE task_type = 'shell'
            AND status IN ('pending', 'dispatchable')
            AND (summary LIKE 'oauth-repush/%' OR (
                summary LIKE 'oauth-distribute/%'
                AND jsonb_typeof(payload) = 'object'
                AND jsonb_typeof(payload->'command') = 'string'
                AND payload->>'command' LIKE '%FF_OAUTH_EOF%'
                AND payload->>'command' LIKE '%base64 -d%')))::bigint AS cancel_eligible
      FROM fleet_tasks";

const LEGACY_OAUTH_SCRUB_SQL: &str = "WITH legacy AS MATERIALIZED (
        SELECT id, status
          FROM fleet_tasks
         WHERE task_type = 'shell'
           AND summary LIKE 'oauth-distribute/%'
           AND jsonb_typeof(payload) = 'object'
           AND jsonb_typeof(payload->'command') = 'string'
           AND payload->>'command' LIKE '%FF_OAUTH_EOF%'
           AND payload->>'command' LIKE '%base64 -d%'
         FOR UPDATE
    ), scrubbed AS (
        UPDATE fleet_tasks task
           SET payload = jsonb_build_object(
                   'operation', 'legacy_oauth_payload_redacted',
                   'version', 1,
                   'redacted', true),
               status = CASE
                   WHEN legacy.status IN ('pending', 'dispatchable') THEN 'cancelled'
                   ELSE task.status
               END,
               completed_at = CASE
                   WHEN legacy.status IN ('pending', 'dispatchable')
                       THEN COALESCE(task.completed_at, NOW())
                   ELSE task.completed_at
               END,
               progress_message = CASE
                   WHEN legacy.status IN ('pending', 'dispatchable')
                       THEN 'cancelled and credential payload redacted by ff oauth cancel-backlog'
                   ELSE task.progress_message
               END
          FROM legacy
         WHERE task.id = legacy.id
           AND NOT EXISTS (SELECT 1 FROM legacy WHERE status = 'running')
        RETURNING legacy.status AS previous_status
    )
    SELECT
        (SELECT COUNT(*) FROM legacy)::bigint AS legacy_matched,
        (SELECT COUNT(*) FROM legacy WHERE status = 'running')::bigint AS running_blocked,
        (SELECT COUNT(*) FROM scrubbed)::bigint AS scrubbed,
        (SELECT COUNT(*) FROM scrubbed
          WHERE previous_status IN ('pending', 'dispatchable'))::bigint AS cancelled";

const OAUTH_REPUSH_CANCEL_SQL: &str = "UPDATE fleet_tasks
        SET status = 'cancelled',
            completed_at = COALESCE(completed_at, NOW()),
            progress_message = 'cancelled by ff oauth cancel-backlog',
            payload = jsonb_build_object(
                'operation', 'oauth_repush_cancelled',
                'version', 1,
                'redacted', true)
      WHERE task_type = 'shell'
        AND status IN ('pending', 'dispatchable')
        AND summary LIKE 'oauth-repush/%'";

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

fn require_locked_oauth_distribution_gate(
    row: Option<(String, Option<DateTime<Utc>>, DateTime<Utc>)>,
) -> Result<()> {
    let (value, expires_at, database_now) = row.ok_or_else(|| {
        anyhow!("OAuth distribution gate authority is missing; refusing batch commit")
    })?;
    let enabled = matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on" | "enabled"
    );
    let restored = !enabled
        && expires_at.is_some_and(|expires_at| expires_at < database_now)
        && OAUTH_DISTRIBUTION_RESTORE_ON_EXPIRY;
    if !enabled && !restored {
        anyhow::bail!("OAuth distribution was disabled during target preflight");
    }
    Ok(())
}

async fn lock_oauth_distribution_gate_for_commit(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let row: Option<(String, Option<DateTime<Utc>>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT value, expires_at, clock_timestamp()
           FROM fleet_secrets
          WHERE key = 'oauth_distribution_enabled'
          FOR SHARE",
    )
    .fetch_optional(&mut **tx)
    .await
    .context("lock OAuth distribution gate for commit")?;
    require_locked_oauth_distribution_gate(row)
}

fn canonical_oauth_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name == name.trim()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn reject_recovery_identity(name: &str) -> Result<()> {
    if name.eq_ignore_ascii_case("vinny") || name.eq_ignore_ascii_case("taylor") {
        anyhow::bail!("OAuth credential operations are forbidden for recovery identity `{name}`");
    }
    Ok(())
}

async fn lock_local_oauth_authority(
    tx: &mut Transaction<'_, Postgres>,
    local_identity: &crate::fleet_info::LocalComputerIdentity,
) -> Result<OauthLeaderAuthority> {
    reject_recovery_identity(&local_identity.name)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(OAUTH_AUTHORITY_XACT_LOCK_KEY)
        .execute(&mut **tx)
        .await
        .context("lock OAuth authority")?;

    let row = sqlx::query(
        "SELECT l.computer_id, l.member_name, l.epoch
           FROM fleet_leader_state l
           JOIN computers c
             ON c.id = l.computer_id AND c.name = l.member_name
           JOIN fleet_workers w
             ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip
          WHERE l.singleton_key = 'current'
            AND l.heartbeat_at > clock_timestamp() - make_interval(secs => $1)
            AND (l.relinquishing_until IS NULL
                 OR l.relinquishing_until <= clock_timestamp())
            AND l.computer_id = $2
            AND l.member_name = $3
            AND c.enrolled_at IS NOT NULL
            AND c.status = 'online'
            AND w.status = 'online'
        ",
    )
    .bind(OAUTH_LEADER_FRESHNESS_SECS)
    .bind(local_identity.id)
    .bind(&local_identity.name)
    .fetch_optional(&mut **tx)
    .await
    .context("resolve locked OAuth leader authority")?
    .ok_or_else(|| {
        anyhow!("this host is not the fresh, non-relinquishing, canonical OAuth leader")
    })?;

    let member_name: String = row.try_get("member_name")?;
    if !canonical_oauth_node_name(&member_name) {
        anyhow::bail!("elected OAuth leader name is not canonical");
    }
    reject_recovery_identity(&member_name)?;
    Ok(OauthLeaderAuthority {
        computer_id: row.try_get("computer_id")?,
        member_name,
        epoch: row.try_get("epoch")?,
    })
}

async fn revalidate_oauth_authority_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    authority: &OauthLeaderAuthority,
) -> Result<()> {
    let valid: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM fleet_leader_state l
               JOIN computers c
                 ON c.id = l.computer_id AND c.name = l.member_name
               JOIN fleet_workers w
                 ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip
              WHERE l.singleton_key = 'current'
                AND l.computer_id = $1
                AND l.member_name = $2
                AND l.epoch = $3
                AND l.heartbeat_at > clock_timestamp() - make_interval(secs => $4)
                AND (l.relinquishing_until IS NULL
                     OR l.relinquishing_until <= clock_timestamp())
                AND c.enrolled_at IS NOT NULL
                AND c.status = 'online'
                AND w.status = 'online'
         )",
    )
    .bind(authority.computer_id)
    .bind(&authority.member_name)
    .bind(authority.epoch)
    .bind(OAUTH_LEADER_FRESHNESS_SECS)
    .fetch_one(&mut **tx)
    .await
    .context("revalidate locked OAuth leader authority")?;
    if !valid {
        anyhow::bail!("OAuth leader authority changed before commit");
    }
    Ok(())
}

async fn lock_oauth_authority_for_commit(
    tx: &mut Transaction<'_, Postgres>,
    authority: &OauthLeaderAuthority,
) -> Result<()> {
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT l.computer_id
           FROM fleet_leader_state l
           JOIN computers c
             ON c.id = l.computer_id AND c.name = l.member_name
           JOIN fleet_workers w
             ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip
          WHERE l.singleton_key = 'current'
            AND l.computer_id = $1
            AND l.member_name = $2
            AND l.epoch = $3
            AND l.heartbeat_at > clock_timestamp() - make_interval(secs => $4)
            AND (l.relinquishing_until IS NULL
                 OR l.relinquishing_until <= clock_timestamp())
            AND c.enrolled_at IS NOT NULL
            AND c.status = 'online'
            AND w.status = 'online'
          FOR SHARE OF l, c, w",
    )
    .bind(authority.computer_id)
    .bind(&authority.member_name)
    .bind(authority.epoch)
    .bind(OAUTH_LEADER_FRESHNESS_SECS)
    .fetch_optional(&mut **tx)
    .await
    .context("lock OAuth leader epoch for commit")?;
    if row.is_none() {
        anyhow::bail!("OAuth leader authority changed before commit");
    }
    Ok(())
}

async fn begin_local_oauth_authority(
    pool: &PgPool,
) -> Result<(Transaction<'_, Postgres>, OauthLeaderAuthority)> {
    let local_identity = crate::fleet_info::resolve_this_computer_identity_strict(pool)
        .await
        .map_err(anyhow::Error::msg)?;
    let mut tx = pool
        .begin()
        .await
        .context("begin OAuth authority transaction")?;
    let authority = lock_local_oauth_authority(&mut tx, &local_identity).await?;
    Ok((tx, authority))
}

fn normalize_requested_targets(requested: &[String], leader_name: &str) -> Result<Vec<String>> {
    if requested.is_empty() {
        anyhow::bail!("at least one explicit --target is required");
    }
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(requested.len());
    for target in requested {
        if !canonical_oauth_node_name(target) {
            anyhow::bail!("OAuth target `{target}` is not a canonical computer name");
        }
        if target.eq_ignore_ascii_case("all") {
            anyhow::bail!(
                "OAuth target `all` is forbidden; name every intended computer explicitly"
            );
        }
        reject_recovery_identity(target)?;
        if target.eq_ignore_ascii_case(leader_name) {
            anyhow::bail!("OAuth target `{target}` is the current leader");
        }
        let folded = target.to_ascii_lowercase();
        if !seen.insert(folded.clone()) {
            anyhow::bail!("duplicate OAuth target `{target}`");
        }
        normalized.push(folded);
    }
    // Stable ordering prevents concurrent batches with the same targets in a
    // different CLI order from acquiring per-target advisory locks inversely.
    normalized.sort_unstable();
    Ok(normalized)
}

fn oauth_distribute_enqueue_key(
    provider: &str,
    target_id: uuid::Uuid,
    secret_version: &str,
) -> String {
    format!("oauth-distribute:{provider}:{target_id}:{secret_version}")
}

fn oauth_repush_enqueue_key(
    provider: &str,
    leader_id: uuid::Uuid,
    leader_epoch: i64,
    requester_id: uuid::Uuid,
) -> String {
    format!("oauth-repush:{provider}:{leader_id}:{leader_epoch}:{requester_id}")
}

/// Read the password value of a macOS Keychain generic-password entry
/// via `security find-generic-password -s <service> -a $USER -w`. Used
/// by `import_token` when the vendor CLI stores creds in Keychain
/// instead of (or in addition to) a flat file. Returns the raw bytes;
/// caller parses as JSON.
#[cfg(target_os = "macos")]
async fn keychain_read(service_name: &str) -> Result<Vec<u8>> {
    let user = std::env::var("USER").context("USER env var not set")?;
    let mut command = tokio::process::Command::new("security");
    command.kill_on_drop(true).args([
        "find-generic-password",
        "-s",
        service_name,
        "-a",
        user.as_str(),
        "-w",
    ]);
    let out = tokio::time::timeout(OAUTH_CREDENTIAL_SOURCE_TIMEOUT, command.output())
        .await
        .context("macOS Keychain credential read timed out")?
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
#[cfg(unix)]
fn read_private_credential_file_under(
    home: &std::path::Path,
    relative: &std::path::Path,
) -> Result<Vec<u8>> {
    let canonical_home = home
        .canonicalize()
        .context("resolve local home for OAuth credential read")?;
    let mut components: Vec<_> = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_os_string()),
            _ => Err(anyhow!("OAuth credential path escaped the local home")),
        })
        .collect::<Result<Vec<_>>>()?;
    let file_name = components
        .pop()
        .ok_or_else(|| anyhow!("OAuth credential path has no file name"))?;

    let mut directory =
        File::open(&canonical_home).context("open local home for OAuth credential read")?;
    verify_private_directory(&directory)?;
    for component in components {
        let component = cstring_component(&component)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("open OAuth credential directory without following links");
        }
        directory = unsafe { File::from_raw_fd(fd) };
        verify_private_directory(&directory)?;
    }

    let file_name = cstring_component(&file_name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open OAuth credential document without following links");
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .context("inspect OAuth credential document")?;
    let expected_uid = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.uid() != expected_uid
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() == 0
        || metadata.len() > MAX_CREDENTIAL_DOCUMENT_BYTES as u64
    {
        anyhow::bail!(
            "OAuth credential document is not a private, owner-only, single-link regular file"
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take((MAX_CREDENTIAL_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("read bounded OAuth credential document")?;
    if bytes.is_empty() || bytes.len() > MAX_CREDENTIAL_DOCUMENT_BYTES {
        anyhow::bail!("OAuth credential document has an invalid size");
    }
    let after = file
        .metadata()
        .context("reinspect OAuth credential document")?;
    if metadata.dev() != after.dev()
        || metadata.ino() != after.ino()
        || metadata.len() != after.len()
        || bytes.len() as u64 != after.len()
        || metadata.mtime() != after.mtime()
        || metadata.mtime_nsec() != after.mtime_nsec()
        || metadata.ctime() != after.ctime()
        || metadata.ctime_nsec() != after.ctime_nsec()
    {
        anyhow::bail!("OAuth credential document changed while it was being read");
    }
    Ok(bytes)
}

#[cfg(unix)]
fn read_private_credential_file(provider: &OauthProvider) -> Result<Vec<u8>> {
    let home = dirs::home_dir().context("resolve local home for OAuth credential read")?;
    let relative = credential_relative_path(provider)?;
    read_private_credential_file_under(&home, &relative)
}

#[cfg(not(unix))]
fn read_private_credential_file(provider: &OauthProvider) -> Result<Vec<u8>> {
    let path = expand_home(provider.cred_path)
        .ok_or_else(|| anyhow!("provider {} has no credential path", provider.name))?;
    let metadata = std::fs::symlink_metadata(&path)
        .context("inspect OAuth credential document without following links")?;
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_CREDENTIAL_DOCUMENT_BYTES as u64
    {
        anyhow::bail!("OAuth credential document is not a bounded regular file");
    }
    std::fs::read(path).context("read OAuth credential document")
}

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

    let provider_copy = *provider;
    tokio::time::timeout(
        OAUTH_CREDENTIAL_SOURCE_TIMEOUT,
        tokio::task::spawn_blocking(move || read_private_credential_file(&provider_copy)),
    )
    .await
    .context("private OAuth credential read timed out")?
    .context("join private OAuth credential read")?
    .with_context(|| {
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

fn credentials_secret_ref(provider: &OauthProvider) -> String {
    format!("{}.credentials", provider.secret_key)
}

fn credential_access_token<'a>(document: &'a Value, provider: &OauthProvider) -> Option<&'a str> {
    provider
        .token_fields
        .iter()
        .find_map(|field| document.get(field).and_then(Value::as_str))
        .or_else(|| {
            provider.token_fields.iter().find_map(|field| {
                document
                    .get("tokens")
                    .and_then(|tokens| tokens.get(field))
                    .and_then(Value::as_str)
            })
        })
        .or_else(|| {
            provider.token_fields.iter().find_map(|field| {
                document
                    .get("claudeAiOauth")
                    .and_then(|oauth| oauth.get(field))
                    .and_then(Value::as_str)
            })
        })
        .filter(|token| !token.trim().is_empty())
}

fn validate_credential_document(document: &str, provider: &OauthProvider) -> Result<()> {
    if document.is_empty() || document.len() > MAX_CREDENTIAL_DOCUMENT_BYTES {
        anyhow::bail!(
            "credential document for provider {} is empty or exceeds the safe size limit",
            provider.name
        );
    }
    let parsed: Value = serde_json::from_str(document).map_err(|_| {
        anyhow!(
            "credential document for provider {} is malformed JSON",
            provider.name
        )
    })?;
    if !parsed.is_object() || credential_access_token(&parsed, provider).is_none() {
        anyhow::bail!(
            "credential document for provider {} lacks a non-empty canonical token field",
            provider.name
        );
    }
    Ok(())
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
    let (mut tx, authority) = begin_local_oauth_authority(pool).await?;
    // The credential source is intentionally not opened until the durable
    // leader row, epoch, roster projection, and OAuth advisory fence are held.
    let bytes = read_leader_cred_bytes(provider).await?;
    let json: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse cred for provider {} as JSON", provider.name))?;

    // Try each known token field. Walk three layouts:
    //   • flat top-level (most CLIs)
    //   • `tokens.<field>` (OpenAI codex CLI, ~/.codex/auth.json)
    //   • `claudeAiOauth.<field>` (Claude Code on macOS, Keychain blob)
    let token = credential_access_token(&json, provider)
        .ok_or_else(|| {
            anyhow!(
                "no non-empty canonical token field found for provider {}; credential shape may have changed",
                provider.name
            )
        })?;

    // Enrollment needs the complete vendor-owned document (refresh token,
    // expiry, account metadata, etc.), not a guessed token-only JSON shape.
    // Keep it beside the extracted bearer token so bootstrap can pull it via
    // the enrollment-token allowlist without an ad-hoc file copy.
    let credentials_key = credentials_secret_ref(provider);
    let credentials = std::str::from_utf8(&bytes)
        .with_context(|| format!("credential document for {} is not UTF-8", provider.name))?;
    validate_credential_document(credentials, provider)?;
    lock_oauth_authority_for_commit(&mut tx, &authority).await?;

    // One timestamp and one transaction make the bearer token and complete
    // vendor document a single versioned authority state.
    let updated_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .context("timestamp OAuth authority update")?;
    let token_description = format!(
        "OAuth subscription token for {} (imported from {})",
        provider.name,
        path.display()
    );
    let credentials_description = format!(
        "Complete OAuth credential document for {} onboarding",
        provider.name
    );
    for (key, value, description) in [
        (provider.secret_key, token, token_description.as_str()),
        (
            credentials_key.as_str(),
            credentials,
            credentials_description.as_str(),
        ),
    ] {
        sqlx::query(
            "INSERT INTO fleet_secrets
                 (key, value, description, expires_at, previous_value, updated_at, updated_by)
             VALUES ($1, $2, $3, NULL, NULL, $4, 'ff oauth import')
             ON CONFLICT (key) DO UPDATE SET
                 value = EXCLUDED.value,
                 description = EXCLUDED.description,
                 expires_at = NULL,
                 previous_value = NULL,
                 updated_at = EXCLUDED.updated_at,
                 updated_by = EXCLUDED.updated_by",
        )
        .bind(key)
        .bind(value)
        .bind(description)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .context("write atomic OAuth authority state")?;
    }
    tx.commit().await.context("commit OAuth authority state")?;

    info!(
        provider = provider.name,
        leader = %authority.member_name,
        epoch = authority.epoch,
        "imported OAuth token to fleet_secrets"
    );
    Ok(())
}

async fn resolve_locked_oauth_targets(
    tx: &mut Transaction<'_, Postgres>,
    normalized_names: &[String],
) -> Result<Vec<OauthTarget>> {
    let rows = sqlx::query(
        "SELECT c.id, c.name, c.primary_ip
           FROM computers c
           JOIN fleet_workers w
             ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip
          WHERE lower(c.name) = ANY($1::text[])
            AND c.enrolled_at IS NOT NULL
            AND c.status = 'online'
            AND w.status = 'online'
            AND c.last_seen_at > clock_timestamp() - make_interval(secs => $2)
            AND w.updated_at > clock_timestamp() - make_interval(secs => $2)
          ORDER BY lower(c.name), c.id
          FOR SHARE OF c, w",
    )
    .bind(normalized_names)
    .bind(OAUTH_TARGET_FRESHNESS_SECS)
    .fetch_all(&mut **tx)
    .await
    .context("resolve locked OAuth target roster")?;

    let requested: HashSet<&str> = normalized_names.iter().map(String::as_str).collect();
    let mut by_name: HashMap<String, OauthTarget> = HashMap::new();
    for row in rows {
        let name: String = row.try_get("name")?;
        let folded = name.to_ascii_lowercase();
        if !canonical_oauth_node_name(&name) || !requested.contains(folded.as_str()) {
            anyhow::bail!("OAuth target roster returned a non-canonical identity");
        }
        reject_recovery_identity(&name)?;
        let primary_ip_raw: String = row
            .try_get("primary_ip")
            .context("OAuth target lacks an authoritative IP")?;
        let primary_ip: IpAddr = primary_ip_raw
            .parse()
            .context("OAuth target has an invalid authoritative IP")?;
        let target = OauthTarget {
            computer_id: row.try_get("id")?,
            name,
            primary_ip,
        };
        if by_name.insert(folded, target).is_some() {
            anyhow::bail!("OAuth target roster is ambiguous under case folding");
        }
    }
    if by_name.len() != normalized_names.len() {
        let missing = normalized_names
            .iter()
            .filter(|name| !by_name.contains_key(name.as_str()))
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "OAuth target(s) are missing, unenrolled, offline, stale, or inconsistent across computers/fleet_workers: {missing}"
        );
    }
    Ok(normalized_names
        .iter()
        .filter_map(|name| by_name.remove(name))
        .collect())
}

fn parse_gateway_port_authority(
    row: Option<(String, DateTime<Utc>)>,
) -> Result<GatewayPortAuthority> {
    let (raw, updated_at) = row.ok_or_else(|| {
        anyhow!("fleet_secrets is missing required gateway port authority `port.gateway`")
    })?;
    let port = raw
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| anyhow!("fleet_secrets `port.gateway` is not a valid non-zero TCP port"))?;
    Ok(GatewayPortAuthority { port, updated_at })
}

async fn resolve_locked_gateway_port(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<GatewayPortAuthority> {
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT value, updated_at
           FROM fleet_secrets
          WHERE key = 'port.gateway'
            AND disabled_reason IS NULL
            AND (expires_at IS NULL OR expires_at > NOW())
          FOR SHARE",
    )
    .fetch_optional(&mut **tx)
    .await
    .context("resolve canonical gateway port authority")?;
    parse_gateway_port_authority(row)
}

async fn probe_oauth_target_health(targets: &[OauthTarget], gateway_port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(OAUTH_TARGET_HEALTH_TIMEOUT)
        .build()
        .context("build OAuth target health client")?;
    try_join_all(targets.iter().cloned().map(|target| {
        let client = client.clone();
        async move {
            let host = match target.primary_ip {
                IpAddr::V4(ip) => ip.to_string(),
                IpAddr::V6(ip) => format!("[{ip}]"),
            };
            let url = format!("http://{host}:{gateway_port}/health");
            let response = client
                .get(url)
                .send()
                .await
                .with_context(|| format!("OAuth target {} health probe failed", target.name))?;
            if !response.status().is_success() {
                anyhow::bail!(
                    "OAuth target {} health probe returned HTTP {}",
                    target.name,
                    response.status()
                );
            }
            let body: Value = response
                .json()
                .await
                .with_context(|| format!("OAuth target {} health JSON is invalid", target.name))?;
            if !valid_oauth_target_health_document(&body) {
                anyhow::bail!(
                    "OAuth target {} did not identify as a healthy ForgeFleet agent",
                    target.name
                );
            }
            Ok::<(), anyhow::Error>(())
        }
    }))
    .await?;
    Ok(())
}

fn valid_oauth_target_health_document(body: &Value) -> bool {
    body.get("status").and_then(Value::as_str) == Some("ok")
        && body.get("service").and_then(Value::as_str) == Some("ff-gateway")
}

/// Enqueue a non-secret credential reference for explicit, healthy members.
///
/// The queue never receives the token, credential document, an encoded form of
/// either, or a shell command. The target resolves the exact referenced secret
/// version just in time in [`run_oauth_credential_install`].
pub async fn distribute_token(
    pool: &PgPool,
    provider: &OauthProvider,
    requested_targets: &[String],
) -> Result<usize> {
    if !oauth_distribution_enabled(pool).await {
        warn!(
            provider = provider.name,
            key = OAUTH_DISTRIBUTION_ENABLED_KEY,
            "OAuth distribution disabled; no fleet tasks enqueued"
        );
        return Ok(0);
    }

    let (mut read_tx, read_authority) = begin_local_oauth_authority(pool).await?;
    let normalized_targets =
        normalize_requested_targets(requested_targets, &read_authority.member_name)?;
    let secret_ref = credentials_secret_ref(provider);
    let secret_authority: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT value, updated_at
           FROM fleet_secrets
          WHERE key = $1
            AND disabled_reason IS NULL
            AND (expires_at IS NULL OR expires_at > NOW())
          FOR SHARE",
    )
    .bind(&secret_ref)
    .fetch_optional(&mut *read_tx)
    .await
    .context("resolve OAuth credential authority")?;
    let Some((credential_document, updated_at)) = secret_authority else {
        anyhow::bail!(
            "current credential authority for provider {} is missing, disabled, or expired; run `ff oauth import {}` first",
            provider.name,
            provider.name
        );
    };
    validate_credential_document(&credential_document, provider)?;
    drop(credential_document);
    let secret_version = updated_at.to_rfc3339_opts(SecondsFormat::Micros, true);
    let gateway_authority = resolve_locked_gateway_port(&mut read_tx).await?;
    let targets = resolve_locked_oauth_targets(&mut read_tx, &normalized_targets).await?;
    revalidate_oauth_authority_snapshot(&mut read_tx, &read_authority).await?;
    read_tx.commit().await.context("commit OAuth preflight")?;
    probe_oauth_target_health(&targets, gateway_authority.port).await?;

    // Re-acquire the global authority fence after the network probes. The
    // leader epoch, exact credential version, and every target roster row are
    // locked in the same transaction as the complete batch enqueue.
    let (mut write_tx, write_authority) = begin_local_oauth_authority(pool).await?;
    if write_authority.computer_id != read_authority.computer_id
        || write_authority.epoch != read_authority.epoch
    {
        anyhow::bail!("OAuth leader changed during target health preflight");
    }
    let version_still_current: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT updated_at FROM fleet_secrets
          WHERE key = $1
            AND updated_at = $2
            AND disabled_reason IS NULL
            AND (expires_at IS NULL OR expires_at > NOW())
          FOR SHARE",
    )
    .bind(&secret_ref)
    .bind(updated_at)
    .fetch_one(&mut *write_tx)
    .await
    .context("lock current OAuth credential version")?;
    if version_still_current.is_none() {
        anyhow::bail!("OAuth credential authority rotated during target health preflight");
    }
    let write_gateway_authority = resolve_locked_gateway_port(&mut write_tx).await?;
    if write_gateway_authority != gateway_authority {
        anyhow::bail!("canonical gateway port authority changed during target health preflight");
    }
    let locked_targets = resolve_locked_oauth_targets(&mut write_tx, &normalized_targets).await?;
    if locked_targets
        .iter()
        .map(|target| (&target.name, target.computer_id, target.primary_ip))
        .collect::<Vec<_>>()
        != targets
            .iter()
            .map(|target| (&target.name, target.computer_id, target.primary_ip))
            .collect::<Vec<_>>()
    {
        anyhow::bail!("OAuth target authority changed during health preflight");
    }
    lock_oauth_authority_for_commit(&mut write_tx, &write_authority).await?;
    lock_oauth_distribution_gate_for_commit(&mut write_tx).await?;

    let mut enqueued = 0usize;
    let mut publish_after_commit = Vec::new();
    for target in locked_targets {
        let id = target.computer_id;
        let name = target.name;
        let payload = serde_json::to_value(OauthCredentialInstallPayload {
            operation: OAUTH_CREDENTIAL_INSTALL_OPERATION.to_string(),
            version: OAUTH_CREDENTIAL_INSTALL_VERSION,
            provider: provider.name.to_string(),
            secret_ref: secret_ref.clone(),
            secret_version: secret_version.clone(),
            target_computer_id: id,
        })
        .context("serialize typed OAuth credential install payload")?;
        let enqueue_once_key = oauth_distribute_enqueue_key(provider.name, id, &secret_version);
        let outcome = pg_enqueue_oauth_credential_install_once_tx(
            &mut write_tx,
            &enqueue_once_key,
            &format!(
                "oauth-distribute/{}: {} → {}",
                provider.name, provider.name, name
            ),
            &payload,
            id,
            70,
        )
        .await
        .with_context(|| format!("enqueue distribute task for {name}"))?;
        if outcome.was_enqueued() {
            enqueued += 1;
            publish_after_commit.push(outcome.task_id());
        } else {
            debug!(
                provider = provider.name,
                target = %name,
                task_id = %outcome.task_id(),
                "OAuth distribute task already active; duplicate suppressed"
            );
        }
    }
    write_tx
        .commit()
        .await
        .context("commit atomic OAuth distribution batch")?;
    for task_id in publish_after_commit {
        crate::nats_jetstream::publish_task_inserted(task_id).await;
    }

    info!(
        provider = provider.name,
        leader = %write_authority.member_name,
        epoch = write_authority.epoch,
        enqueued, "OAuth distribute tasks enqueued"
    );
    Ok(enqueued)
}

fn validate_install_payload(
    payload: &Value,
    my_computer_id: uuid::Uuid,
) -> Result<(
    OauthCredentialInstallPayload,
    &'static OauthProvider,
    DateTime<Utc>,
)> {
    let parsed: OauthCredentialInstallPayload = serde_json::from_value(payload.clone())
        .map_err(|_| anyhow!("OAuth credential install payload has an invalid shape"))?;
    if parsed.operation != OAUTH_CREDENTIAL_INSTALL_OPERATION
        || parsed.version != OAUTH_CREDENTIAL_INSTALL_VERSION
    {
        anyhow::bail!("OAuth credential install operation or version is unsupported");
    }
    if parsed.target_computer_id != my_computer_id {
        anyhow::bail!("OAuth credential install target does not match this computer");
    }
    let provider = provider_by_name(&parsed.provider)
        .filter(|provider| provider.name == parsed.provider)
        .ok_or_else(|| anyhow!("OAuth credential install provider is not canonical"))?;
    if provider.cred_path.is_empty() || parsed.secret_ref != credentials_secret_ref(provider) {
        anyhow::bail!("OAuth credential install secret reference is not canonical");
    }
    let secret_version = DateTime::parse_from_rfc3339(&parsed.secret_version)
        .map_err(|_| anyhow!("OAuth credential install secret version is invalid"))?
        .with_timezone(&Utc);
    if secret_version.to_rfc3339_opts(SecondsFormat::Micros, true) != parsed.secret_version {
        anyhow::bail!("OAuth credential install secret version is not canonical");
    }
    Ok((parsed, provider, secret_version))
}

fn credential_relative_path(provider: &OauthProvider) -> Result<PathBuf> {
    let relative = provider.cred_path.strip_prefix("~/").ok_or_else(|| {
        anyhow!(
            "provider {} credential path is not home-relative",
            provider.name
        )
    })?;
    let relative = PathBuf::from(relative);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("provider {} credential path is unsafe", provider.name);
    }
    Ok(relative)
}

#[cfg(unix)]
fn cstring_component(component: &std::ffi::OsStr) -> Result<CString> {
    CString::new(component.as_bytes())
        .map_err(|_| anyhow!("OAuth credential path contains an invalid NUL byte"))
}

#[cfg(unix)]
fn verify_private_directory(directory: &File) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("inspect OAuth credential directory");
    }
    let stat = unsafe { stat.assume_init() };
    let effective_uid = unsafe { libc::geteuid() };
    if stat.st_uid != effective_uid || stat.st_mode & 0o022 != 0 {
        anyhow::bail!(
            "OAuth credential directory is not privately owned (owner={}, expected={}, mode={:o})",
            stat.st_uid,
            effective_uid,
            stat.st_mode & 0o777
        );
    }
    Ok(())
}

/// Install through descriptor-relative, no-follow operations. Each directory
/// component is opened with `O_NOFOLLOW`; the final document is a new 0600 inode
/// in the destination directory and is renamed into place only after fsync.
#[cfg(unix)]
fn install_credential_document_under(
    home: &std::path::Path,
    relative_path: &std::path::Path,
    document: &str,
) -> Result<()> {
    let canonical_home = home
        .canonicalize()
        .context("resolve local home for OAuth credential install")?;
    let mut components: Vec<_> = relative_path
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_os_string()),
            _ => Err(anyhow!("OAuth credential path escaped the local home")),
        })
        .collect::<Result<Vec<_>>>()?;
    let file_name = components
        .pop()
        .ok_or_else(|| anyhow!("OAuth credential path has no file name"))?;

    let mut directory =
        File::open(&canonical_home).context("open local home for OAuth credential install")?;
    verify_private_directory(&directory)?;
    for component in components {
        let component = cstring_component(&component)?;
        let created = unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o700) };
        if created != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(error).context("create OAuth credential directory");
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("open OAuth credential directory without following links");
        }
        directory = unsafe { File::from_raw_fd(fd) };
        verify_private_directory(&directory)?;
    }

    let file_name = cstring_component(&file_name)?;
    let mut existing = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let existing_result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            existing.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if existing_result == 0 {
        let existing = unsafe { existing.assume_init() };
        if existing.st_mode & libc::S_IFMT != libc::S_IFREG {
            anyhow::bail!("OAuth credential destination is not a regular file");
        }
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error).context("inspect OAuth credential destination");
        }
    }

    let temporary_name = CString::new(format!(".ff-oauth-{}.tmp", uuid::Uuid::new_v4()))
        .expect("UUID temporary name contains no NUL");
    let temporary_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if temporary_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("create private OAuth credential temporary file");
    }
    let mut temporary = unsafe { File::from_raw_fd(temporary_fd) };
    let install_result = (|| -> std::io::Result<()> {
        temporary.write_all(document.as_bytes())?;
        temporary.sync_all()?;
        if unsafe { libc::fchmod(temporary.as_raw_fd(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary_name.as_ptr(),
                directory.as_raw_fd(),
                file_name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::fsync(directory.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();
    if install_result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary_name.as_ptr(), 0);
        }
    }
    install_result.context("atomically install OAuth credential document")?;
    Ok(())
}

#[cfg(not(unix))]
fn install_credential_document_under(
    _home: &std::path::Path,
    _relative_path: &std::path::Path,
    _document: &str,
) -> Result<()> {
    anyhow::bail!("secure OAuth credential install is unsupported on this operating system")
}

async fn install_credential_document(provider: &OauthProvider, document: String) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("local home directory is unavailable"))?;
    let relative_path = credential_relative_path(provider)?;
    tokio::task::spawn_blocking(move || {
        install_credential_document_under(&home, &relative_path, &document)
    })
    .await
    .map_err(|_| anyhow!("OAuth credential install worker stopped unexpectedly"))??;
    Ok(())
}

async fn resolve_install_document(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task_id: uuid::Uuid,
    my_computer_id: uuid::Uuid,
    secret_ref: &str,
    secret_version: DateTime<Utc>,
) -> Result<String> {
    let target_fence: Option<bool> = sqlx::query_scalar(
        "SELECT TRUE
           FROM fleet_tasks
          WHERE id = $1
            AND task_type = 'oauth_credential_install'
            AND status = 'running'
            AND claimed_by_computer_id = $2
            AND preferred_computer_id = $2
          FOR SHARE",
    )
    .bind(task_id)
    .bind(my_computer_id)
    .fetch_optional(&mut **tx)
    .await
    .context("verify OAuth credential task target fence")?;
    if target_fence.is_none() {
        anyhow::bail!("OAuth credential install task target fence is not authoritative");
    }
    sqlx::query_scalar(
        "SELECT value
           FROM fleet_secrets
          WHERE key = $1
            AND updated_at = $2
            AND disabled_reason IS NULL
            AND (expires_at IS NULL OR expires_at > NOW())
          FOR SHARE",
    )
    .bind(secret_ref)
    .bind(secret_version)
    .fetch_optional(&mut **tx)
    .await
    .context("resolve exact OAuth credential secret reference")?
    .ok_or_else(|| anyhow!("OAuth credential authority is unavailable or rotated"))
}

/// Execute the dedicated exact-target credential operation. The row lock keeps
/// the referenced secret version stable until the atomically replaced file is
/// durable. Errors and the returned result contain identifiers only.
pub(crate) async fn run_oauth_credential_install(
    pool: &PgPool,
    task_id: uuid::Uuid,
    my_computer_id: uuid::Uuid,
    payload: &Value,
) -> Result<Value> {
    let (payload, provider, secret_version) = validate_install_payload(payload, my_computer_id)?;
    let mut tx = pool
        .begin()
        .await
        .context("begin OAuth credential install transaction")?;
    let document = resolve_install_document(
        &mut tx,
        task_id,
        my_computer_id,
        &payload.secret_ref,
        secret_version,
    )
    .await?;
    validate_credential_document(&document, provider)?;
    install_credential_document(provider, document).await?;
    tx.commit()
        .await
        .context("commit OAuth credential install authority read")?;
    info!(
        provider = provider.name,
        target_computer_id = %my_computer_id,
        "OAuth credential document installed from canonical reference"
    );
    Ok(serde_json::json!({
        "exit": 0,
        "operation": OAUTH_CREDENTIAL_INSTALL_OPERATION,
        "provider": provider.name,
        "secret_ref": payload.secret_ref,
        "target_computer_id": my_computer_id,
        "installed": true
    }))
}

/// Result of the narrowly-scoped OAuth queue cleanup verb.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct OauthBacklogCancellation {
    /// Historical and unstarted legacy distribute rows whose payload has the
    /// exact old credential-bearing shell fingerprint.
    pub legacy_matched: u64,
    /// Number whose legacy payload was replaced (`0` for a dry run).
    pub scrubbed: u64,
    /// Pending/dispatchable legacy-distribute plus repush rows eligible for
    /// cancellation.
    pub cancel_eligible: u64,
    /// Number actually moved to `cancelled` (`0` for a dry run).
    pub cancelled: u64,
    /// Matching rows currently running. Apply fails without changing any row
    /// when this is non-zero.
    pub running_blocked: u64,
    pub applied: bool,
}

/// Preview or repair the retained legacy OAuth task payloads and backlog.
///
/// Apply locks every exact-fingerprint legacy row. If any is running, one SQL
/// guard updates nothing and the transaction rolls back. Otherwise all legacy
/// payloads are replaced with a constant marker; terminal statuses and audit
/// columns remain unchanged, while pending/dispatchable rows are cancelled in
/// that same update. Short, non-secret repush commands are cancelled and
/// replaced separately inside the same transaction. Re-running is idempotent
/// because redacted rows no longer match the legacy fingerprint.
pub async fn cancel_oauth_task_backlog(
    pool: &PgPool,
    apply: bool,
) -> Result<OauthBacklogCancellation> {
    let mut tx = pool
        .begin()
        .await
        .context("begin OAuth backlog transaction")?;
    if apply {
        let (legacy_matched, running_blocked, scrubbed, legacy_cancelled): (i64, i64, i64, i64) =
            sqlx::query_as(LEGACY_OAUTH_SCRUB_SQL)
                .fetch_one(&mut *tx)
                .await
                .context("lock and scrub legacy OAuth task payloads")?;
        if running_blocked > 0 {
            tx.rollback()
                .await
                .context("roll back blocked OAuth payload scrub")?;
            anyhow::bail!(
                "refusing OAuth payload scrub while {running_blocked} matching task(s) are running"
            );
        }
        let repush_cancelled = sqlx::query(OAUTH_REPUSH_CANCEL_SQL)
            .execute(&mut *tx)
            .await
            .context("cancel pending OAuth repush backlog")?
            .rows_affected();
        tx.commit()
            .await
            .context("commit OAuth payload scrub and backlog cancellation")?;
        let cancelled = u64::try_from(legacy_cancelled).unwrap_or(0) + repush_cancelled;
        Ok(OauthBacklogCancellation {
            legacy_matched: u64::try_from(legacy_matched).unwrap_or(0),
            scrubbed: u64::try_from(scrubbed).unwrap_or(0),
            cancel_eligible: cancelled,
            cancelled,
            running_blocked: 0,
            applied: true,
        })
    } else {
        let (legacy_matched, running_blocked, cancel_eligible): (i64, i64, i64) =
            sqlx::query_as(OAUTH_BACKLOG_COUNT_SQL)
                .fetch_one(&mut *tx)
                .await
                .context("count retained OAuth payloads and pending backlog")?;
        tx.commit().await.context("finish OAuth backlog dry run")?;
        Ok(OauthBacklogCancellation {
            legacy_matched: u64::try_from(legacy_matched).unwrap_or(0),
            scrubbed: 0,
            cancel_eligible: u64::try_from(cancel_eligible).unwrap_or(0),
            cancelled: 0,
            running_blocked: u64::try_from(running_blocked).unwrap_or(0),
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

        let token_in_secrets: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM fleet_secrets WHERE key = $1 AND value <> ''
            )",
        )
        .bind(p.secret_key)
        .fetch_one(pool)
        .await
        .with_context(|| format!("check OAuth secret presence for provider {}", p.name))?;
        out.push(ProviderStatus {
            name: p.name.to_string(),
            cred_file_present: cred_present,
            cred_file_mtime_secs_ago: mtime_ago,
            token_in_secrets,
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
async fn refresh_and_import(pool: &PgPool, provider: &OauthProvider) -> Result<()> {
    if !oauth_distribution_enabled(pool).await {
        return Ok(());
    }
    let probe = probe_one(pool, provider).await;
    if probe.status != "ok" {
        warn!(provider = provider.name, status = %probe.status, detail = ?probe.message,
            "OAuth native refresh probe did not return cleanly; importing freshest credential anyway");
    }
    import_token(pool, provider).await
}

pub async fn refresh_and_distribute(
    pool: &PgPool,
    provider: &OauthProvider,
    requested_targets: &[String],
) -> Result<usize> {
    refresh_and_import(pool, provider).await?;
    distribute_token(pool, provider, requested_targets).await
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
    if !canonical_oauth_node_name(worker_name) || reject_recovery_identity(worker_name).is_err() {
        warn!(worker = %worker_name, "refusing OAuth re-push request for non-canonical or recovery identity");
        return;
    }
    let local_identity = match crate::fleet_info::resolve_this_computer_identity_strict(pool).await
    {
        Ok(identity) if identity.name == worker_name => identity,
        Ok(identity) => {
            warn!(worker = %worker_name, canonical_worker = %identity.name,
                "refusing OAuth re-push for caller-supplied non-local identity");
            return;
        }
        Err(error) => {
            warn!(worker = %worker_name, %error,
                "refusing OAuth re-push without strict local computer identity");
            return;
        }
    };
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
    if !canonical_oauth_node_name(&leader_name) || reject_recovery_identity(&leader_name).is_err() {
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
        let command = format!(
            "ff oauth refresh {} --target {}",
            provider.name, worker_name
        );
        let enqueue_once_key = oauth_repush_enqueue_key(
            provider.name,
            leader.computer_id,
            leader.epoch,
            local_identity.id,
        );
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
                        match refresh_and_import(&pg, provider).await {
                            Ok(()) => info!(provider = provider.name,
                                "periodic OAuth refresh and leader import complete"),
                            Err(error) => warn!(provider = provider.name, %error,
                                "periodic OAuth refresh and leader import failed"),
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
/// `REFRESH_POLL_SECS`; on mtime change, re-imports on the leader. Distribution
/// always requires a separate command with explicit targets.
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
    fn commit_gate_fails_closed_when_missing_or_disabled_during_preflight() {
        let now = Utc::now();
        assert!(require_locked_oauth_distribution_gate(None).is_err());
        for value in ["false", "0", "no", "off", "disabled", "garbage"] {
            assert!(
                require_locked_oauth_distribution_gate(Some((
                    value.into(),
                    Some(now + chrono::Duration::minutes(5)),
                    now,
                )))
                .is_err(),
                "commit gate accepted disabled value {value}"
            );
        }
        assert!(require_locked_oauth_distribution_gate(Some(("true".into(), None, now))).is_ok());
        assert!(
            require_locked_oauth_distribution_gate(Some((
                "false".into(),
                Some(now - chrono::Duration::seconds(1)),
                now,
            )))
            .is_ok(),
            "expired kill-switch must preserve the existing TTL restore semantics"
        );
    }

    #[test]
    fn enqueue_once_keys_scope_repush_and_distribution_independently() {
        let leader = uuid::Uuid::nil();
        let target = uuid::Uuid::from_u128(1);
        let requester = uuid::Uuid::from_u128(2);
        let version = "2026-08-05T10:00:00.000000Z";
        assert_eq!(
            oauth_repush_enqueue_key("codex", leader, 7, requester),
            format!("oauth-repush:codex:{leader}:7:{requester}")
        );
        assert_ne!(
            oauth_repush_enqueue_key("codex", leader, 7, requester),
            oauth_repush_enqueue_key("codex", leader, 7, uuid::Uuid::from_u128(3))
        );
        assert_eq!(
            oauth_distribute_enqueue_key("codex", target, version),
            format!("oauth-distribute:codex:{target}:{version}")
        );
        assert_ne!(
            oauth_distribute_enqueue_key("codex", target, version),
            oauth_distribute_enqueue_key("kimi", target, version)
        );
        assert_ne!(
            oauth_distribute_enqueue_key("codex", target, version),
            oauth_distribute_enqueue_key("codex", target, "2026-08-05T10:00:01.000000Z")
        );
    }

    #[test]
    fn explicit_targets_are_canonical_sorted_and_fail_closed() {
        assert_eq!(
            normalize_requested_targets(&["Sia".into(), "adele".into()], "Beyonce").unwrap(),
            vec!["adele", "sia"]
        );
        for rejected in [
            Vec::<String>::new(),
            vec!["all".into()],
            vec!["Vinny".into()],
            vec!["taylor".into()],
            vec!["Beyonce".into()],
            vec!["sia".into(), "SIA".into()],
            vec!["sia;reboot".into()],
            vec![" sia".into()],
        ] {
            assert!(
                normalize_requested_targets(&rejected, "Beyonce").is_err(),
                "accepted unsafe targets: {rejected:?}"
            );
        }
    }

    #[test]
    fn target_health_requires_exact_forgefleet_agent_semantics() {
        assert!(valid_oauth_target_health_document(&serde_json::json!({
            "status": "ok",
            "service": "ff-gateway",
            "version": "2026.4.7",
            "build_sha": "0123456789",
            "uptime_epoch": 1785931200
        })));
        for body in [
            serde_json::json!({"status": "ok"}),
            serde_json::json!({"status": "ok", "service": "proxy"}),
            serde_json::json!({"status": "failed", "service": "ff-gateway"}),
            serde_json::json!("ok"),
        ] {
            assert!(!valid_oauth_target_health_document(&body));
        }
    }

    #[test]
    fn gateway_port_authority_rejects_missing_invalid_and_conflicting_versions() {
        let stamp = Utc::now();
        assert!(parse_gateway_port_authority(None).is_err());
        for raw in ["", "0", "65536", "51002x", "-1"] {
            assert!(parse_gateway_port_authority(Some((raw.into(), stamp))).is_err());
        }
        let first = parse_gateway_port_authority(Some(("51002".into(), stamp))).unwrap();
        assert_eq!(first.port, 51_002);
        let changed_value = parse_gateway_port_authority(Some(("51003".into(), stamp))).unwrap();
        let changed_version = parse_gateway_port_authority(Some((
            "51002".into(),
            stamp + chrono::Duration::microseconds(1),
        )))
        .unwrap();
        assert_ne!(first, changed_value);
        assert_ne!(first, changed_version);
    }

    #[test]
    fn backlog_cleanup_fingerprint_and_atomic_guard_are_exact() {
        for marker in [
            "task_type = 'shell'",
            "summary LIKE 'oauth-distribute/%'",
            "jsonb_typeof(payload) = 'object'",
            "jsonb_typeof(payload->'command') = 'string'",
            "FF_OAUTH_EOF",
            "base64 -d",
        ] {
            assert!(LEGACY_OAUTH_PAYLOAD_PREDICATE.contains(marker));
            assert!(OAUTH_BACKLOG_COUNT_SQL.contains(marker));
            assert!(LEGACY_OAUTH_SCRUB_SQL.contains(marker));
        }
        assert!(LEGACY_OAUTH_SCRUB_SQL.contains("FOR UPDATE"));
        assert!(
            LEGACY_OAUTH_SCRUB_SQL
                .contains("NOT EXISTS (SELECT 1 FROM legacy WHERE status = 'running')")
        );
        assert!(LEGACY_OAUTH_SCRUB_SQL.contains("legacy_oauth_payload_redacted"));
        assert!(LEGACY_OAUTH_SCRUB_SQL.contains("RETURNING legacy.status AS previous_status"));
        assert!(OAUTH_REPUSH_CANCEL_SQL.contains("summary LIKE 'oauth-repush/%'"));
        assert!(!OAUTH_REPUSH_CANCEL_SQL.contains("payload->>'command'"));
    }

    #[test]
    fn typed_payload_contains_references_only_and_validates_exact_target_and_provider() {
        let target = uuid::Uuid::from_u128(42);
        let payload = serde_json::to_value(OauthCredentialInstallPayload {
            operation: OAUTH_CREDENTIAL_INSTALL_OPERATION.to_string(),
            version: OAUTH_CREDENTIAL_INSTALL_VERSION,
            provider: "codex".to_string(),
            secret_ref: "openai.oauth_token.credentials".to_string(),
            secret_version: "2026-08-05T10:00:00.000000Z".to_string(),
            target_computer_id: target,
        })
        .unwrap();
        let serialized = payload.to_string();
        for forbidden in ["access_token", "refresh_token", "base64", "command"] {
            assert!(
                !serialized.contains(forbidden),
                "payload leaked {forbidden}"
            );
        }
        validate_install_payload(&payload, target).expect("canonical payload");

        let mut wrong_target = payload.clone();
        wrong_target["target_computer_id"] = Value::String(uuid::Uuid::nil().to_string());
        assert!(validate_install_payload(&wrong_target, target).is_err());

        let mut mixed_case = payload.clone();
        mixed_case["provider"] = Value::String("Codex".to_string());
        assert!(validate_install_payload(&mixed_case, target).is_err());

        let mut wrong_ref = payload.clone();
        wrong_ref["secret_ref"] = Value::String("other.credentials".to_string());
        assert!(validate_install_payload(&wrong_ref, target).is_err());

        let mut unknown = payload;
        unknown["credential"] = Value::String("must-not-be-accepted".to_string());
        assert!(validate_install_payload(&unknown, target).is_err());
    }

    #[test]
    fn credential_validation_errors_never_echo_document_content() {
        let provider = provider_by_name("codex").unwrap();
        let marker = "credential-marker-that-must-not-escape";
        for document in [
            marker.to_string(),
            format!(r#"{{"refresh_token":"{marker}"}}"#),
            format!(r#"{{"access_token":"" ,"note":"{marker}"}}"#),
        ] {
            let error = validate_credential_document(&document, provider)
                .expect_err("malformed or tokenless document must fail")
                .to_string();
            assert!(!error.contains(marker));
        }
    }

    #[cfg(unix)]
    #[test]
    fn atomic_installer_writes_exact_document_mode_0600_and_rejects_symlinks() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let relative = std::path::Path::new(".codex/auth.json");
        let document = r#"{"access_token":"opaque-test-value","refresh_token":"refresh"}"#;
        install_credential_document_under(home.path(), relative, document).unwrap();
        let installed = home.path().join(relative);
        assert_eq!(std::fs::read_to_string(&installed).unwrap(), document);
        let metadata = std::fs::metadata(&installed).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.nlink(), 1);
        assert!(
            std::fs::read_dir(installed.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp"))
        );

        let linked_home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), linked_home.path().join(".codex")).unwrap();
        let error = install_credential_document_under(linked_home.path(), relative, document)
            .expect_err("symlinked credential directory must fail")
            .to_string();
        assert!(!error.contains("opaque-test-value"));
        assert!(!outside.path().join("auth.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_reader_rejects_links_and_non_private_documents() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = home.path().join(".codex");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let document = directory.join("auth.json");
        let marker = "opaque-private-reader-marker";
        std::fs::write(&document, format!(r#"{{"access_token":"{marker}"}}"#)).unwrap();
        std::fs::set_permissions(&document, std::fs::Permissions::from_mode(0o600)).unwrap();
        let bytes = read_private_credential_file_under(
            home.path(),
            std::path::Path::new(".codex/auth.json"),
        )
        .unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains(marker));

        let linked = directory.join("linked.json");
        symlink(&document, &linked).unwrap();
        let link_error = read_private_credential_file_under(
            home.path(),
            std::path::Path::new(".codex/linked.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(!link_error.contains(marker));

        std::fs::set_permissions(&document, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mode_error = read_private_credential_file_under(
            home.path(),
            std::path::Path::new(".codex/auth.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(!mode_error.contains(marker));
        std::fs::set_permissions(&document, std::fs::Permissions::from_mode(0o600)).unwrap();

        let hard_link_path = directory.join("second-link.json");
        std::fs::hard_link(&document, hard_link_path).unwrap();
        let link_count_error = read_private_credential_file_under(
            home.path(),
            std::path::Path::new(".codex/auth.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(!link_count_error.contains(marker));
    }

    #[test]
    fn distributor_source_has_no_credential_encoding_or_shell_enqueue() {
        let source = include_str!("oauth_distributor.rs");
        let distribute = source
            .split("pub async fn distribute_token")
            .nth(1)
            .unwrap()
            .split("fn validate_install_payload")
            .next()
            .unwrap();
        assert!(!distribute.contains("BASE64"));
        assert!(!distribute.contains("base64 -d"));
        assert!(!distribute.contains("pg_enqueue_shell_task_once"));
        assert!(distribute.contains("pg_enqueue_oauth_credential_install_once"));
    }

    #[tokio::test]
    async fn postgres_scrub_is_terminal_safe_idempotent_and_running_atomic() {
        let Some(database_url) = std::env::var("FF_OAUTH_TEST_DATABASE_URL").ok() else {
            eprintln!("FF_OAUTH_TEST_DATABASE_URL unset; skipping disposable PostgreSQL proof");
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .expect("connect disposable PostgreSQL");
        sqlx::query("DROP TABLE IF EXISTS fleet_tasks")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE fleet_tasks (
                id uuid PRIMARY KEY,
                task_type text NOT NULL,
                summary text NOT NULL,
                payload jsonb NOT NULL,
                status text NOT NULL,
                completed_at timestamptz,
                progress_message text,
                dedup_signature text,
                preferred_computer_id uuid,
                claimed_by_computer_id uuid,
                created_at timestamptz NOT NULL DEFAULT NOW()
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let legacy_command =
            "set -e\nprintf '%s' 'credential-marker' | base64 -d > target\nFF_OAUTH_EOF";
        let statuses = ["pending", "cancelled", "completed", "failed"];
        let mut ids = Vec::new();
        for status in statuses {
            let id = uuid::Uuid::new_v4();
            ids.push((id, status));
            sqlx::query(
                "INSERT INTO fleet_tasks
                    (id, task_type, summary, payload, status, progress_message, dedup_signature)
                 VALUES ($1, 'shell', $2, jsonb_build_object('command', $3), $4,
                         'original progress', $5)",
            )
            .bind(id)
            .bind(format!("oauth-distribute/codex: {status}"))
            .bind(legacy_command)
            .bind(status)
            .bind(format!("audit-{status}"))
            .execute(&pool)
            .await
            .unwrap();
        }
        let repush_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO fleet_tasks (id, task_type, summary, payload, status)
             VALUES ($1, 'shell', 'oauth-repush/codex',
                     jsonb_build_object('command', 'ff oauth refresh codex'), 'pending')",
        )
        .bind(repush_id)
        .execute(&pool)
        .await
        .unwrap();
        let near_miss_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO fleet_tasks (id, task_type, summary, payload, status)
             VALUES ($1, 'shell', 'oauth-distribute/codex: near-miss',
                     jsonb_build_object('command', 'base64 -d but no sentinel'), 'completed')",
        )
        .bind(near_miss_id)
        .execute(&pool)
        .await
        .unwrap();

        let before: Vec<(uuid::Uuid, chrono::DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT id, created_at, dedup_signature FROM fleet_tasks
              WHERE id = ANY($1) ORDER BY id",
        )
        .bind(ids.iter().map(|(id, _)| *id).collect::<Vec<_>>())
        .fetch_all(&pool)
        .await
        .unwrap();
        let dry = cancel_oauth_task_backlog(&pool, false).await.unwrap();
        assert_eq!(dry.legacy_matched, 4);
        assert_eq!(dry.cancel_eligible, 2);
        assert_eq!(dry.running_blocked, 0);
        assert_eq!(dry.scrubbed, 0);
        assert_eq!(dry.cancelled, 0);

        let applied = cancel_oauth_task_backlog(&pool, true).await.unwrap();
        assert_eq!(applied.legacy_matched, 4);
        assert_eq!(applied.scrubbed, 4);
        assert_eq!(applied.cancelled, 2);
        for (id, original_status) in &ids {
            let (status, payload, progress): (String, Value, Option<String>) = sqlx::query_as(
                "SELECT status, payload, progress_message FROM fleet_tasks WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            let expected_status = if *original_status == "pending" {
                "cancelled"
            } else {
                original_status
            };
            assert_eq!(status, expected_status);
            assert_eq!(payload["operation"], "legacy_oauth_payload_redacted");
            assert!(!payload.to_string().contains("credential-marker"));
            if *original_status != "pending" {
                assert_eq!(progress.as_deref(), Some("original progress"));
            }
        }
        let after: Vec<(uuid::Uuid, chrono::DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT id, created_at, dedup_signature FROM fleet_tasks
              WHERE id = ANY($1) ORDER BY id",
        )
        .bind(ids.iter().map(|(id, _)| *id).collect::<Vec<_>>())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(before, after, "audit identity changed during scrub");
        let repush: (String, Value) =
            sqlx::query_as("SELECT status, payload FROM fleet_tasks WHERE id = $1")
                .bind(repush_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(repush.0, "cancelled");
        assert_eq!(repush.1["operation"], "oauth_repush_cancelled");
        let near_miss: Value = sqlx::query_scalar("SELECT payload FROM fleet_tasks WHERE id = $1")
            .bind(near_miss_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(near_miss.get("command").is_some());
        let repeated = cancel_oauth_task_backlog(&pool, true).await.unwrap();
        assert_eq!(repeated.legacy_matched, 0);
        assert_eq!(repeated.scrubbed, 0);
        assert_eq!(repeated.cancelled, 0);

        let running_id = uuid::Uuid::new_v4();
        let pending_id = uuid::Uuid::new_v4();
        for id in [running_id, pending_id] {
            sqlx::query(
                "INSERT INTO fleet_tasks (id, task_type, summary, payload, status)
                 VALUES ($1, 'shell', 'oauth-distribute/codex: race',
                         jsonb_build_object('command', $2), 'pending')",
            )
            .bind(id)
            .bind(legacy_command)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut claim = pool.begin().await.unwrap();
        sqlx::query("UPDATE fleet_tasks SET status = 'running' WHERE id = $1")
            .bind(running_id)
            .execute(&mut *claim)
            .await
            .unwrap();
        let scrub_pool = pool.clone();
        let scrub = tokio::spawn(async move { cancel_oauth_task_backlog(&scrub_pool, true).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        claim.commit().await.unwrap();
        let error = scrub
            .await
            .unwrap()
            .expect_err("running fingerprint must block the whole scrub")
            .to_string();
        assert!(error.contains("matching task(s) are running"));
        let race_rows: Vec<(uuid::Uuid, String, Value)> = sqlx::query_as(
            "SELECT id, status, payload FROM fleet_tasks
              WHERE id = ANY($1) ORDER BY id",
        )
        .bind(vec![running_id, pending_id])
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(race_rows.len(), 2);
        assert!(race_rows.iter().any(|row| row.1 == "running"));
        assert!(race_rows.iter().any(|row| row.1 == "pending"));
        assert!(
            race_rows
                .iter()
                .all(|row| row.2["command"].as_str() == Some(legacy_command))
        );

        sqlx::query("DROP TABLE IF EXISTS fleet_secrets")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE fleet_secrets (
                key text PRIMARY KEY,
                value text NOT NULL,
                updated_at timestamptz NOT NULL,
                expires_at timestamptz,
                disabled_reason text
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        let typed_task_id = uuid::Uuid::new_v4();
        let target_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO fleet_tasks (
                id, task_type, summary, payload, status,
                preferred_computer_id, claimed_by_computer_id
             ) VALUES ($1, 'oauth_credential_install', 'typed oauth test', '{}'::jsonb,
                       'running', $2, $2)",
        )
        .bind(typed_task_id)
        .bind(target_id)
        .execute(&pool)
        .await
        .unwrap();
        let secret_version = DateTime::parse_from_rfc3339("2026-08-05T10:00:00.000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let test_document = r#"{"access_token":"jit-test-secret"}"#;
        sqlx::query(
            "INSERT INTO fleet_secrets (key, value, updated_at)
             VALUES ('openai.oauth_token.credentials', $1, $2)",
        )
        .bind(test_document)
        .bind(secret_version)
        .execute(&pool)
        .await
        .unwrap();

        let mut exact = pool.begin().await.unwrap();
        let resolved = resolve_install_document(
            &mut exact,
            typed_task_id,
            target_id,
            "openai.oauth_token.credentials",
            secret_version,
        )
        .await
        .unwrap();
        assert_eq!(resolved, test_document);
        exact.rollback().await.unwrap();

        let mut wrong_target = pool.begin().await.unwrap();
        let target_error = resolve_install_document(
            &mut wrong_target,
            typed_task_id,
            uuid::Uuid::new_v4(),
            "openai.oauth_token.credentials",
            secret_version,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(!target_error.contains("jit-test-secret"));
        wrong_target.rollback().await.unwrap();

        let mut rotated = pool.begin().await.unwrap();
        let stale_error = resolve_install_document(
            &mut rotated,
            typed_task_id,
            target_id,
            "openai.oauth_token.credentials",
            secret_version + chrono::Duration::microseconds(1),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(stale_error.contains("unavailable or rotated"));
        assert!(!stale_error.contains("jit-test-secret"));
        rotated.rollback().await.unwrap();

        sqlx::query("DROP TABLE fleet_secrets")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP TABLE fleet_tasks")
            .execute(&pool)
            .await
            .unwrap();
    }

    #[test]
    fn autonomous_producers_never_broadly_distribute() {
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
        assert!(startup.contains("--target {}"));
        assert!(startup.contains("resolve_this_computer_identity_strict(pool).await"));
        assert!(startup.contains("local_identity.id"));
        assert!(startup.contains("leader.epoch"));

        let periodic = source
            .split("pub fn spawn_oauth_probe_tick")
            .nth(1)
            .expect("periodic producer")
            .split("pub async fn probe_one")
            .next()
            .expect("periodic body");
        assert!(periodic.contains("oauth_distribution_enabled(&pg).await"));
        assert!(periodic.contains("refresh_and_import(&pg, provider).await"));
        assert!(!periodic.contains("distribute_token("));

        let watcher = source
            .split("pub fn spawn_refresh_watch")
            .nth(1)
            .expect("refresh watcher")
            .split("#[cfg(test)]")
            .next()
            .expect("refresh watcher body");
        assert!(watcher.contains("import_token(&pool, p).await"));
        assert!(!watcher.contains("distribute_token("));
    }

    #[test]
    fn import_and_distribution_hold_authority_and_atomic_batch_boundaries() {
        let source = include_str!("oauth_distributor.rs");
        let import = source
            .split("pub async fn import_token")
            .nth(1)
            .unwrap()
            .split("async fn resolve_locked_oauth_targets")
            .next()
            .unwrap();
        let leader = import
            .find("begin_local_oauth_authority(pool).await")
            .unwrap();
        let read = import
            .find("read_leader_cred_bytes(provider).await")
            .unwrap();
        let first_revalidate = import
            .find("lock_oauth_authority_for_commit(&mut tx, &authority).await")
            .unwrap();
        let write = import.find("INSERT INTO fleet_secrets").unwrap();
        assert!(leader < read && read < first_revalidate && first_revalidate < write);
        assert!(import.contains("tx.commit().await"));

        let distribute = source
            .split("pub async fn distribute_token")
            .nth(1)
            .unwrap()
            .split("fn validate_install_payload")
            .next()
            .unwrap();
        assert!(distribute.contains("normalize_requested_targets"));
        assert!(distribute.contains("probe_oauth_target_health"));
        assert!(source.contains(".no_proxy()"));
        assert!(source.contains("ff-gateway"));
        assert!(distribute.contains("resolve_locked_gateway_port"));
        assert!(!distribute.contains("50002"));
        assert!(!distribute.contains("51002"));
        assert!(distribute.contains("pg_enqueue_oauth_credential_install_once_tx"));
        let gate_lock = distribute
            .find("lock_oauth_distribution_gate_for_commit")
            .unwrap();
        let enqueue = distribute
            .find("pg_enqueue_oauth_credential_install_once_tx")
            .unwrap();
        assert!(gate_lock < enqueue);
        assert!(distribute.contains("commit atomic OAuth distribution batch"));
        assert!(distribute.contains("publish_task_inserted"));
        assert!(!distribute.contains("status IN ('online', 'ok', 'pending', 'maintenance')"));

        let initial_authority = source
            .split("async fn lock_local_oauth_authority")
            .nth(1)
            .unwrap()
            .split("async fn revalidate_oauth_authority_snapshot")
            .next()
            .unwrap();
        assert!(initial_authority.contains("pg_advisory_xact_lock"));
        assert!(!initial_authority.contains("FOR UPDATE"));
        assert!(!initial_authority.contains("FOR SHARE"));
        let commit_authority = source
            .split("async fn lock_oauth_authority_for_commit")
            .nth(1)
            .unwrap()
            .split("async fn begin_local_oauth_authority")
            .next()
            .unwrap();
        assert!(commit_authority.contains("FOR SHARE OF l, c, w"));
        assert!(source.contains("OAUTH_CREDENTIAL_SOURCE_TIMEOUT"));
        assert!(source.contains("kill_on_drop(true)"));
    }
}
