//! Fail-closed activation of immutable V291 ForgeFleet release artifacts.
//!
//! This is deliberately a local authority boundary.  Callers select only a
//! version and an exact full source commit; artifact names, target platform,
//! custody holders, source paths, install paths, service labels, and rollback
//! locations are derived here.  There is no force, PATH lookup, source-build,
//! or caller-selected remote fallback.

use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ff_core::model_integrity::constant_time_sha256_eq;
use ff_db::{
    PgPool, ReleaseArtifactCustodyRow, ReleaseArtifactRow, pg_get_node, pg_get_release_artifact,
    pg_get_secret, pg_list_release_artifact_custody,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::artifact_registry::{
    LocalReleaseArtifactSpec, authority_home_dir, local_release_build_root,
    register_local_release_artifact,
};
use crate::fleet_info::{LocalComputerIdentity, resolve_this_computer_identity_strict};

const ARTIFACT_NAMES: [&str; 2] = ["ff", "forgefleetd"];
const MCP_UNIT: &str = "forgefleet-mcp.service";
const DAEMON_UNIT: &str = "forgefleetd.service";
const MCP_LABEL: &str = "com.forgefleet.forgefleet-mcp";
const DAEMON_LABEL: &str = "com.forgefleet.forgefleetd";
#[derive(Debug, Clone)]
pub struct LocalReleaseActivationRequest {
    pub source_commit: String,
    /// Optional coordinator-selected transaction identity. Fleet rollout uses
    /// the sealed V295 transaction UUID so a lost SSH response can be adopted
    /// from the same private local receipt after a crash. Interactive local
    /// activation leaves this unset and receives a fresh UUID.
    pub transaction_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct LocalReleaseRollbackRequest {
    pub transaction_id: Uuid,
}

#[derive(Debug, Clone)]
struct CanonicalReleaseIdentity {
    artifact_version: String,
    source_commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ServicePorts {
    mcp: u16,
    gateway: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivatedArtifactReceipt {
    pub artifact_name: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub destinations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PriorReleaseIdentity {
    FullSha { sha: String },
    LegacyReported { short_sha: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseActivationReceipt {
    pub transaction_id: Uuid,
    pub artifact_version: String,
    pub source_commit: String,
    pub prior_release_identity: PriorReleaseIdentity,
    pub target_triple: String,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub activated_at: DateTime<Utc>,
    pub mcp_service: String,
    pub daemon_service: String,
    pub artifacts: Vec<ActivatedArtifactReceipt>,
    pub receipt_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RestoredArtifactReceipt {
    pub artifact_name: String,
    pub restored_sha256: String,
    pub restored_size_bytes: i64,
    pub replaced_sha256: String,
    pub replaced_size_bytes: i64,
    pub destinations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRollbackReceipt {
    pub transaction_id: Uuid,
    pub replaced_source_commit: String,
    pub restored_release_identity: PriorReleaseIdentity,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub rolled_back_at: DateTime<Utc>,
    pub artifacts: Vec<RestoredArtifactReceipt>,
    pub receipt_path: String,
}

/// Read-only evidence that a committed activation retains the exact local
/// predecessor bytes required by explicit rollback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRollbackProof {
    pub transaction_id: Uuid,
    pub source_commit: String,
    pub prior_release_identity: PriorReleaseIdentity,
    pub computer_id: Uuid,
    pub computer_name: String,
    pub manifest_sha256: String,
    pub activation_receipt_sha256: String,
    pub verified_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReleaseActivationError {
    #[error("release activation refused: {0}")]
    Refused(String),
    #[error("release activation I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("release activation database operation failed: {0}")]
    Database(#[from] ff_db::DbError),
    #[error("release artifact custody registration failed: {0}")]
    Registry(#[from] crate::artifact_registry::ArtifactRegistryError),
    #[error("release activation JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("release activation blocking worker failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

type Result<T> = std::result::Result<T, ReleaseActivationError>;

#[derive(Debug, Clone)]
struct ResolvedArtifact {
    row: ReleaseArtifactRow,
    custody: ReleaseArtifactCustodyRow,
    origin_computer_id: Uuid,
    origin_holder: String,
}

#[derive(Debug, Clone)]
struct LocalCustodyResolution {
    custody: ReleaseArtifactCustodyRow,
    origin_computer_id: Uuid,
    origin_holder: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ServicePlatform {
    Linux,
    Macos,
}

#[derive(Debug)]
struct OpenArtifact {
    file: File,
    initial: libc::stat,
}

#[derive(Debug)]
struct InstallEntry {
    artifact_name: String,
    expected_sha256: String,
    expected_size: u64,
    previous_sha256: String,
    previous_size: u64,
    dir: OwnedFd,
    dir_path: PathBuf,
    destination: CString,
    stage: CString,
    backup: CString,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackManifest {
    transaction_id: Uuid,
    artifact_version: String,
    source_commit: String,
    prior_release_identity: PriorReleaseIdentity,
    target_triple: String,
    computer_id: Uuid,
    computer_name: String,
    platform: ServicePlatform,
    created_at: DateTime<Utc>,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    artifact_name: String,
    destination: String,
    stage: String,
    backup: String,
    sha256: String,
    size_bytes: u64,
    previous_sha256: String,
    previous_size_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEvent {
    at: DateTime<Utc>,
    state: String,
    detail: String,
}

#[derive(Debug)]
struct ActiveTransaction {
    id: Uuid,
    version: String,
    source_commit: String,
    prior_release_identity: PriorReleaseIdentity,
    target_triple: String,
    identity: LocalComputerIdentity,
    platform: ServicePlatform,
    ports: ServicePorts,
    home: PathBuf,
    activation_dir: PathBuf,
    journal_path: PathBuf,
    entries: Vec<InstallEntry>,
    _operation_lock: File,
}

#[derive(Debug)]
struct PreparedExplicitRollback {
    receipt: ReleaseRollbackReceipt,
    journal_path: PathBuf,
    receipt_path: PathBuf,
    receipt_already_committed: bool,
    _operation_lock: File,
}

#[derive(Debug)]
struct InspectedCommittedActivation {
    receipt: ReleaseActivationReceipt,
    proof: ReleaseRollbackProof,
    _operation_lock: File,
}

/// Activate the exact `ff` + `forgefleetd` V291 pair for this host.
///
/// Missing local custody is acquired only from the single canonical matching
/// custody holder in PostgreSQL and is registered locally before any install
/// path or service is touched.
pub async fn activate_local_release_pair(
    pool: &PgPool,
    request: &LocalReleaseActivationRequest,
) -> Result<ReleaseActivationReceipt> {
    validate_full_source_commit(&request.source_commit)?;
    let platform_identity = current_platform_identity(&request.source_commit)?;
    let target_triple = platform_identity.target_triple.clone();
    let platform = platform_identity.service_platform;
    let canonical = CanonicalReleaseIdentity {
        artifact_version: platform_identity.artifact_version,
        source_commit: request.source_commit.clone(),
    };
    let identity = resolve_this_computer_identity_strict(pool)
        .await
        .map_err(ReleaseActivationError::Refused)?;
    if identity.name.eq_ignore_ascii_case("vinny") {
        return Err(ReleaseActivationError::Refused(
            "release activation is forbidden on Vinny".into(),
        ));
    }
    let home = authority_home_dir().ok_or_else(|| {
        ReleaseActivationError::Refused("effective-user home is unavailable".into())
    })?;
    let ports = resolve_service_ports(pool, &home).await?;
    if let Some(transaction_id) = request.transaction_id {
        let identity_for_worker = identity.clone();
        let home_for_worker = home.clone();
        let existing = tokio::task::spawn_blocking(move || {
            inspect_committed_activation(
                transaction_id,
                identity_for_worker,
                platform,
                home_for_worker,
                &SystemCommandRunner,
                true,
            )
        })
        .await??;
        if let Some(existing) = existing {
            if existing.receipt.source_commit != request.source_commit {
                return Err(ReleaseActivationError::Refused(
                    "coordinator transaction receipt belongs to another source commit".into(),
                ));
            }
            verify_running_release(&request.source_commit, ports).await?;
            return Ok(existing.receipt);
        }
    }
    let prior_release_identity = probe_running_release_identity(ports).await?;
    if prior_release_identity
        == (PriorReleaseIdentity::FullSha {
            sha: request.source_commit.clone(),
        })
    {
        return Err(ReleaseActivationError::Refused(
            "the requested exact release is already running".into(),
        ));
    }

    let mut pair = Vec::with_capacity(2);
    for artifact_name in ARTIFACT_NAMES {
        let row = pg_get_release_artifact(
            pool,
            artifact_name,
            &canonical.artifact_version,
            &canonical.source_commit,
            &target_triple,
        )
        .await?
        .ok_or_else(|| {
            ReleaseActivationError::Refused(format!(
                "no immutable V291 artifact for {artifact_name} version={} source={} target={target_triple}",
                canonical.artifact_version, canonical.source_commit
            ))
        })?;
        validate_registry_row(&row, artifact_name, &canonical, &target_triple)?;
        let custody = ensure_local_custody(pool, &identity, &row).await?;
        pair.push(ResolvedArtifact {
            row,
            custody: custody.custody,
            origin_computer_id: custody.origin_computer_id,
            origin_holder: custody.origin_holder,
        });
    }
    validate_pair(&pair, &identity, &canonical, &target_triple)?;

    let request_owned = canonical;
    let identity_for_worker = identity.clone();
    let target_for_worker = target_triple.clone();
    let previous_for_worker = prior_release_identity.clone();
    let requested_transaction_id = request.transaction_id;
    let transaction = tokio::task::spawn_blocking(move || match requested_transaction_id {
        Some(transaction_id) => prepare_swap_and_restart_with_id(
            transaction_id,
            pair,
            identity_for_worker,
            request_owned,
            target_for_worker,
            platform,
            ports,
            home,
            previous_for_worker,
            &SystemCommandRunner,
            None,
        ),
        None => prepare_swap_and_restart(
            pair,
            identity_for_worker,
            request_owned,
            target_for_worker,
            platform,
            ports,
            home,
            previous_for_worker,
            &SystemCommandRunner,
            None,
        ),
    })
    .await??;

    if let Err(primary) =
        verify_running_release(&transaction.source_commit, transaction.ports).await
    {
        let detail = primary.to_string();
        let rollback = tokio::task::spawn_blocking(move || {
            rollback_transaction(transaction, &SystemCommandRunner, &detail)
        })
        .await?;
        return match rollback {
            Ok(()) => Err(primary),
            Err(rollback_error) => Err(ReleaseActivationError::Refused(format!(
                "post-activation verification failed ({primary}); mandatory rollback also failed ({rollback_error})"
            ))),
        };
    }

    tokio::task::spawn_blocking(move || commit_transaction(transaction, &SystemCommandRunner))
        .await?
}

/// Prove, without changing services or installed bytes, that a committed
/// activation retains a complete exact predecessor pair for explicit rollback.
pub async fn prove_local_release_rollback(
    pool: &PgPool,
    request: &LocalReleaseRollbackRequest,
) -> Result<ReleaseRollbackProof> {
    let identity = resolve_this_computer_identity_strict(pool)
        .await
        .map_err(ReleaseActivationError::Refused)?;
    if identity.name.eq_ignore_ascii_case("vinny") {
        return Err(ReleaseActivationError::Refused(
            "release rollback proof is forbidden on Vinny".into(),
        ));
    }
    let home = authority_home_dir().ok_or_else(|| {
        ReleaseActivationError::Refused("effective-user home is unavailable".into())
    })?;
    let ports = resolve_service_ports(pool, &home).await?;
    let platform = current_service_platform()?;
    let transaction_id = request.transaction_id;
    let inspected = tokio::task::spawn_blocking(move || {
        inspect_committed_activation(
            transaction_id,
            identity,
            platform,
            home,
            &SystemCommandRunner,
            false,
        )
    })
    .await??
    .ok_or_else(|| {
        ReleaseActivationError::Refused(
            "rollback proof requires a committed activation receipt".into(),
        )
    })?;
    verify_running_release(&inspected.receipt.source_commit, ports).await?;
    Ok(inspected.proof)
}

/// Restore the exact predecessor pair retained by a committed activation.
///
/// The transaction UUID is the only caller-selected identity. Every pathname,
/// digest, service, platform, prior release identity, and local computer identity
/// is re-derived from private durable activation authority. There is no force
/// mode and Vinny is permanently out of scope.
pub async fn rollback_local_release_transaction(
    pool: &PgPool,
    request: &LocalReleaseRollbackRequest,
) -> Result<ReleaseRollbackReceipt> {
    let identity = resolve_this_computer_identity_strict(pool)
        .await
        .map_err(ReleaseActivationError::Refused)?;
    if identity.name.eq_ignore_ascii_case("vinny") {
        return Err(ReleaseActivationError::Refused(
            "release rollback is forbidden on Vinny".into(),
        ));
    }
    let home = authority_home_dir().ok_or_else(|| {
        ReleaseActivationError::Refused("effective-user home is unavailable".into())
    })?;
    let ports = resolve_service_ports(pool, &home).await?;
    let platform = current_service_platform()?;
    let transaction_id = request.transaction_id;
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_explicit_rollback(
            transaction_id,
            identity,
            platform,
            ports,
            home,
            &SystemCommandRunner,
            None,
        )
    })
    .await??;

    if let Err(error) =
        verify_running_release_identity(&prepared.receipt.restored_release_identity, ports).await
    {
        let _ = append_journal(
            &prepared.journal_path,
            "rollback_verification_failed",
            &error.to_string(),
        );
        return Err(error);
    }

    tokio::task::spawn_blocking(move || commit_explicit_rollback(prepared, &SystemCommandRunner))
        .await?
}

async fn resolve_service_ports(pool: &PgPool, home: &Path) -> Result<ServicePorts> {
    let config_path = home.join(".forgefleet").join("fleet.toml");
    let mut mcp_sources: Vec<(String, u16)> = Vec::new();
    let mut gateway_sources: Vec<(String, u16)> = Vec::new();
    if config_path.exists() {
        let mut file = open_private_config(&config_path)?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        let value: toml::Value = toml::from_str(&raw).map_err(|error| {
            ReleaseActivationError::Refused(format!("cannot parse fleet.toml: {error}"))
        })?;
        collect_config_ports(&value, &mut mcp_sources, &mut gateway_sources)?;
    }
    if let Some(value) = pg_get_secret(pool, "port.mcp").await? {
        mcp_sources.push((
            "fleet_secrets.port.mcp".into(),
            parse_port(&value, "fleet_secrets.port.mcp")?,
        ));
    }
    if let Some(value) = pg_get_secret(pool, "port.gateway").await? {
        gateway_sources.push((
            "fleet_secrets.port.gateway".into(),
            parse_port(&value, "fleet_secrets.port.gateway")?,
        ));
    }
    Ok(ServicePorts {
        mcp: reconcile_port_sources("MCP", &mcp_sources)?,
        gateway: reconcile_port_sources("gateway", &gateway_sources)?,
    })
}

fn collect_config_ports(
    value: &toml::Value,
    mcp_sources: &mut Vec<(String, u16)>,
    gateway_sources: &mut Vec<(String, u16)>,
) -> Result<()> {
    let table = value
        .as_table()
        .ok_or_else(|| ReleaseActivationError::Refused("fleet.toml root is not a table".into()))?;
    if let Some(ports) = table.get("ports") {
        let ports = ports.as_table().ok_or_else(|| {
            ReleaseActivationError::Refused("fleet.toml [ports] is not a table".into())
        })?;
        if let Some(port) = ports.get("forgefleet") {
            mcp_sources.push((
                "fleet.toml ports.forgefleet".into(),
                parse_toml_port(port, "ports.forgefleet")?,
            ));
        }
    }
    if let Some(mcp) = table.get("mcp") {
        let mcp = mcp.as_table().ok_or_else(|| {
            ReleaseActivationError::Refused("fleet.toml [mcp] is not a table".into())
        })?;
        if let Some(forgefleet) = mcp.get("forgefleet") {
            let forgefleet = forgefleet.as_table().ok_or_else(|| {
                ReleaseActivationError::Refused("fleet.toml [mcp.forgefleet] is not a table".into())
            })?;
            if let Some(port) = forgefleet.get("port") {
                mcp_sources.push((
                    "fleet.toml mcp.forgefleet.port".into(),
                    parse_toml_port(port, "mcp.forgefleet.port")?,
                ));
            }
            for key in ["endpoint", "url"] {
                if let Some(endpoint) = forgefleet.get(key) {
                    let endpoint = endpoint.as_str().ok_or_else(|| {
                        ReleaseActivationError::Refused(format!(
                            "mcp.forgefleet.{key} is not a string"
                        ))
                    })?;
                    let normalized = if endpoint.contains("://") {
                        endpoint.to_string()
                    } else {
                        format!("http://{endpoint}")
                    };
                    let parsed = reqwest::Url::parse(&normalized).map_err(|error| {
                        ReleaseActivationError::Refused(format!(
                            "invalid mcp.forgefleet.{key}: {error}"
                        ))
                    })?;
                    if !matches!(parsed.scheme(), "http" | "https")
                        || parsed.host_str().is_none()
                        || !matches!(parsed.path(), "" | "/" | "/mcp")
                        || parsed.query().is_some()
                        || parsed.fragment().is_some()
                    {
                        return Err(ReleaseActivationError::Refused(format!(
                            "invalid mcp.forgefleet.{key} authority"
                        )));
                    }
                    let port = parsed.port_or_known_default().ok_or_else(|| {
                        ReleaseActivationError::Refused(format!("mcp.forgefleet.{key} has no port"))
                    })?;
                    mcp_sources.push((format!("fleet.toml mcp.forgefleet.{key}"), port));
                }
            }
        }
    }
    for section in ["general", "fleet"] {
        if let Some(settings) = table.get(section) {
            let settings = settings.as_table().ok_or_else(|| {
                ReleaseActivationError::Refused(format!("fleet.toml [{section}] is not a table"))
            })?;
            if let Some(api_port) = settings.get("api_port") {
                let base = parse_toml_port(api_port, &format!("{section}.api_port"))?;
                let gateway = base.checked_add(2).ok_or_else(|| {
                    ReleaseActivationError::Refused(format!(
                        "{section}.api_port cannot derive a gateway port"
                    ))
                })?;
                gateway_sources.push((format!("fleet.toml {section}.api_port+2"), gateway));
            }
        }
    }
    Ok(())
}

fn open_private_config(path: &Path) -> Result<File> {
    let path_c = cstring(path.as_os_str())?;
    let raw = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let stat = fstat_fd(fd.as_raw_fd())?;
    validate_regular_stat(&stat, unsafe { libc::geteuid() }, true)?;
    if stat.st_mode & 0o022 != 0 || stat.st_size < 0 || stat.st_size > 4 * 1024 * 1024 {
        return Err(ReleaseActivationError::Refused(
            "fleet.toml is writable by another identity or exceeds 4 MiB".into(),
        ));
    }
    Ok(File::from(fd))
}

fn parse_toml_port(value: &toml::Value, source: &str) -> Result<u16> {
    let raw = match value {
        toml::Value::Integer(number) => number.to_string(),
        toml::Value::String(text) => text.clone(),
        _ => {
            return Err(ReleaseActivationError::Refused(format!(
                "{source} is not an integer port"
            )));
        }
    };
    parse_port(&raw, source)
}

fn parse_port(raw: &str, source: &str) -> Result<u16> {
    let trimmed = raw.trim();
    let port: u16 = trimmed.parse().map_err(|_| {
        ReleaseActivationError::Refused(format!("{source} is not a valid TCP port"))
    })?;
    if port == 0 || trimmed != port.to_string() {
        return Err(ReleaseActivationError::Refused(format!(
            "{source} is not a canonical TCP port"
        )));
    }
    Ok(port)
}

fn reconcile_port_sources(kind: &str, sources: &[(String, u16)]) -> Result<u16> {
    let first = sources.first().ok_or_else(|| {
        ReleaseActivationError::Refused(format!("{kind} port authority is missing"))
    })?;
    if sources.iter().any(|(_, port)| *port != first.1) {
        return Err(ReleaseActivationError::Refused(format!(
            "{kind} port authorities conflict: {}",
            sources
                .iter()
                .map(|(name, port)| format!("{name}={port}"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(first.1)
}

fn validate_full_source_commit(source_commit: &str) -> Result<()> {
    if source_commit.len() != 40
        || !source_commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ReleaseActivationError::Refused(
            "source commit must be exactly 40 lowercase hexadecimal characters".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalPlatformIdentity {
    target_triple: String,
    artifact_version: String,
    service_platform: ServicePlatform,
}

fn current_platform_identity(source_commit: &str) -> Result<LocalPlatformIdentity> {
    let os_release = trusted_os_release()?;
    derive_platform_identity(
        source_commit,
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else {
            ""
        },
        &os_release,
    )
}

fn current_service_platform() -> Result<ServicePlatform> {
    Ok(current_platform_identity(&"0".repeat(40))?.service_platform)
}

fn derive_platform_identity(
    source_commit: &str,
    os: &str,
    arch: &str,
    env: &str,
    os_release: &str,
) -> Result<LocalPlatformIdentity> {
    validate_full_source_commit(source_commit)?;
    let (target_triple, qualifier, service_platform) = match (os, arch, env, os_release) {
        ("linux", "x86_64", "gnu", "24") => (
            "x86_64-unknown-linux-gnu",
            "ubuntu24-x86_64",
            ServicePlatform::Linux,
        ),
        ("linux", "aarch64", "gnu", "24") => (
            "aarch64-unknown-linux-gnu",
            "ubuntu24-aarch64",
            ServicePlatform::Linux,
        ),
        ("linux", "x86_64", "gnu", "26") => (
            "x86_64-unknown-linux-gnu",
            "ubuntu26-x86_64",
            ServicePlatform::Linux,
        ),
        ("macos", "aarch64", _, "26") => (
            "aarch64-apple-darwin",
            "macos26-arm64",
            ServicePlatform::Macos,
        ),
        _ => Err(ReleaseActivationError::Refused(format!(
            "unsupported release activation platform os={os} arch={arch} env={env} release={os_release}"
        )))?,
    };
    Ok(LocalPlatformIdentity {
        target_triple: target_triple.into(),
        artifact_version: format!("recovery.{source_commit}.{qualifier}"),
        service_platform,
    })
}

fn trusted_os_release() -> Result<String> {
    if cfg!(target_os = "linux") {
        // `/etc/os-release` is normally a symlink on Ubuntu. Read its fixed,
        // root-owned canonical target without following a caller-controlled
        // link at activation time.
        let mut file = open_absolute_regular(Path::new("/usr/lib/os-release"), 0)?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        return parse_ubuntu_release(&raw);
    }
    if cfg!(target_os = "macos") {
        let output = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()?;
        if !output.status.success() {
            return Err(ReleaseActivationError::Refused("sw_vers failed".into()));
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        return Ok(raw.trim().split('.').next().unwrap_or("").to_string());
    }
    Err(ReleaseActivationError::Refused(
        "unsupported operating system authority".into(),
    ))
}

fn parse_ubuntu_release(raw: &str) -> Result<String> {
    let field = |name: &str| {
        raw.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map(|value| value.trim_matches('"'))
    };
    if field("ID") != Some("ubuntu") {
        return Err(ReleaseActivationError::Refused(
            "canonical os-release is not Ubuntu".into(),
        ));
    }
    let version = field("VERSION_ID").ok_or_else(|| {
        ReleaseActivationError::Refused("canonical os-release lacks VERSION_ID".into())
    })?;
    let major = version.split('.').next().unwrap_or(version);
    if !matches!(major, "24" | "26") {
        return Err(ReleaseActivationError::Refused(format!(
            "unsupported Ubuntu VERSION_ID {version}"
        )));
    }
    Ok(major.to_string())
}

fn validate_registry_row(
    row: &ReleaseArtifactRow,
    expected_name: &str,
    request: &CanonicalReleaseIdentity,
    target: &str,
) -> Result<()> {
    if !ARTIFACT_NAMES.contains(&row.artifact_name.as_str())
        || row.artifact_name != expected_name
        || row.artifact_version != request.artifact_version
        || row.source_commit != request.source_commit
        || row.target_triple != target
        || row.size_bytes <= 0
        || row.sha256.len() != 64
        || !row
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ReleaseActivationError::Refused(format!(
            "immutable registry row for {expected_name} does not match the requested canonical identity"
        )));
    }
    Ok(())
}

fn validate_pair(
    pair: &[ResolvedArtifact],
    identity: &LocalComputerIdentity,
    request: &CanonicalReleaseIdentity,
    target: &str,
) -> Result<()> {
    if identity.name.eq_ignore_ascii_case("vinny") {
        return Err(ReleaseActivationError::Refused(
            "release activation is forbidden on Vinny".into(),
        ));
    }
    if pair.len() != 2 {
        return Err(ReleaseActivationError::Refused(
            "release activation requires exactly two artifacts".into(),
        ));
    }
    if pair[0].origin_computer_id != pair[1].origin_computer_id
        || pair[0].origin_holder != pair[1].origin_holder
    {
        return Err(ReleaseActivationError::Refused(
            "ff and forgefleetd do not share one canonical origin custodian".into(),
        ));
    }
    for (artifact, expected_name) in pair.iter().zip(ARTIFACT_NAMES) {
        validate_registry_row(&artifact.row, expected_name, request, target)?;
        if artifact.custody.artifact_id != artifact.row.id
            || artifact.custody.computer_id != identity.id
            || artifact.custody.holder_name_at_registration != identity.name
        {
            return Err(ReleaseActivationError::Refused(format!(
                "{expected_name} is not in exact local canonical custody"
            )));
        }
        validate_safe_relative_path(Path::new(&artifact.custody.relative_path))?;
    }
    Ok(())
}

async fn ensure_local_custody(
    pool: &PgPool,
    identity: &LocalComputerIdentity,
    row: &ReleaseArtifactRow,
) -> Result<LocalCustodyResolution> {
    let custody = pg_list_release_artifact_custody(pool, row.id).await?;
    let origin = select_canonical_origin(&custody, &row.artifact_name)?;
    if origin
        .holder_name_at_registration
        .eq_ignore_ascii_case("vinny")
    {
        return Err(ReleaseActivationError::Refused(
            "Vinny may not supply release custody".into(),
        ));
    }
    let local: Vec<_> = custody
        .iter()
        .filter(|entry| entry.computer_id == identity.id)
        .cloned()
        .collect();
    if local.len() == 1 {
        let local = local.into_iter().next().expect("one local custody row");
        if local.holder_name_at_registration != identity.name {
            return Err(ReleaseActivationError::Refused(format!(
                "stale local custody holder for {}",
                row.artifact_name
            )));
        }
        return Ok(LocalCustodyResolution {
            custody: local,
            origin_computer_id: origin.computer_id,
            origin_holder: origin.holder_name_at_registration,
        });
    }
    if !local.is_empty() {
        return Err(ReleaseActivationError::Refused(format!(
            "ambiguous local custody for {}",
            row.artifact_name
        )));
    }

    if origin.computer_id == identity.id {
        return Err(ReleaseActivationError::Refused(
            "canonical origin claims this computer but local custody is missing".into(),
        ));
    }
    let peer = pg_get_node(pool, &origin.holder_name_at_registration)
        .await?
        .ok_or_else(|| {
            ReleaseActivationError::Refused(format!(
                "custodian {} has no canonical fleet SSH authority",
                origin.holder_name_at_registration
            ))
        })?;
    if peer.name != origin.holder_name_at_registration
        || peer.name.eq_ignore_ascii_case("vinny")
        || !canonical_live_status(&peer.status)
        || !peer
            .computer_status
            .as_deref()
            .is_some_and(canonical_live_status)
    {
        return Err(ReleaseActivationError::Refused(format!(
            "custodian {} is not an exact active canonical peer",
            origin.holder_name_at_registration
        )));
    }
    validate_ssh_authority(&peer.ssh_user, &peer.ip)?;
    validate_safe_remote_relative_path(&origin.relative_path)?;

    let row_owned = row.clone();
    let remote_path = origin.relative_path.clone();
    let ssh_user = peer.ssh_user.clone();
    let ip = peer.ip.clone();
    let local_relative = acquired_relative_path(row)?;
    let local_relative_for_worker = local_relative.clone();
    tokio::task::spawn_blocking(move || {
        acquire_remote_artifact(
            &ssh_user,
            &ip,
            &remote_path,
            &local_relative_for_worker,
            &row_owned,
        )
    })
    .await??;

    let spec = LocalReleaseArtifactSpec {
        artifact_name: row.artifact_name.clone(),
        artifact_version: row.artifact_version.clone(),
        source_commit: row.source_commit.clone(),
        target_triple: row.target_triple.clone(),
        expected_sha256: row.sha256.clone(),
        expected_size_bytes: row.size_bytes,
        relative_path: local_relative,
    };
    let registered = register_local_release_artifact(pool, identity, &spec).await?;
    if registered.artifact != *row
        || registered.custody.computer_id != identity.id
        || registered.custody.holder_name_at_registration != identity.name
    {
        return Err(ReleaseActivationError::Refused(
            "local custody registration did not return the exact immutable artifact".into(),
        ));
    }
    Ok(LocalCustodyResolution {
        custody: registered.custody,
        origin_computer_id: origin.computer_id,
        origin_holder: origin.holder_name_at_registration,
    })
}

fn canonical_live_status(status: &str) -> bool {
    matches!(status, "active" | "online")
}

/// The first immutable verifier is the origin. Later recipient custody rows do
/// not make rollout ambiguous; only a tie at the earliest timestamp does.
fn select_canonical_origin(
    custody: &[ReleaseArtifactCustodyRow],
    artifact_name: &str,
) -> Result<ReleaseArtifactCustodyRow> {
    let earliest = custody
        .iter()
        .map(|entry| entry.first_verified_at)
        .min()
        .ok_or_else(|| {
            ReleaseActivationError::Refused(format!(
                "{artifact_name} has no canonical custody origin"
            ))
        })?;
    let origins: Vec<_> = custody
        .iter()
        .filter(|entry| entry.first_verified_at == earliest)
        .cloned()
        .collect();
    if origins.len() != 1 {
        return Err(ReleaseActivationError::Refused(format!(
            "{artifact_name} has an ambiguous earliest custody origin"
        )));
    }
    Ok(origins.into_iter().next().expect("one earliest origin"))
}

fn validate_ssh_authority(user: &str, ip: &str) -> Result<()> {
    if user.is_empty()
        || user.len() > 64
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ReleaseActivationError::Refused(
            "canonical SSH user is unsafe".into(),
        ));
    }
    let parsed: IpAddr = ip.parse().map_err(|_| {
        ReleaseActivationError::Refused("canonical SSH address is not an IP literal".into())
    })?;
    if !matches!(parsed, IpAddr::V4(_))
        || parsed.is_loopback()
        || parsed.is_unspecified()
        || parsed.is_multicast()
    {
        return Err(ReleaseActivationError::Refused(
            "canonical SSH address is not a usable fleet IPv4 address".into(),
        ));
    }
    Ok(())
}

fn validate_safe_remote_relative_path(path: &str) -> Result<()> {
    validate_safe_relative_path(Path::new(path))?;
    if !path.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+')
    }) {
        return Err(ReleaseActivationError::Refused(
            "remote custody path is not safe for the fixed SSH transfer protocol".into(),
        ));
    }
    Ok(())
}

fn acquired_relative_path(row: &ReleaseArtifactRow) -> Result<PathBuf> {
    for component in [
        row.target_triple.as_str(),
        row.artifact_version.as_str(),
        row.source_commit.as_str(),
        row.artifact_name.as_str(),
    ] {
        if component.is_empty()
            || !component.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+')
            })
        {
            return Err(ReleaseActivationError::Refused(
                "artifact identity cannot form a canonical local custody path".into(),
            ));
        }
    }
    Ok(PathBuf::from("acquired")
        .join(&row.target_triple)
        .join(&row.artifact_version)
        .join(&row.source_commit)
        .join(&row.artifact_name))
}

fn acquire_remote_artifact(
    ssh_user: &str,
    ip: &str,
    remote_relative_path: &str,
    local_relative_path: &Path,
    row: &ReleaseArtifactRow,
) -> Result<()> {
    let root = local_release_build_root().map_err(|error| {
        ReleaseActivationError::Refused(format!("release-build root unavailable: {error}"))
    })?;
    let parent = local_relative_path
        .parent()
        .ok_or_else(|| ReleaseActivationError::Refused("acquired artifact has no parent".into()))?;
    let parent_path = root.join(parent);
    ensure_owned_directory_tree(&root, parent)?;
    let transaction_id = Uuid::new_v4();
    let temp_name = format!(".{}.acquire-{transaction_id}.tmp", row.artifact_name);
    let final_name = local_relative_path
        .file_name()
        .ok_or_else(|| ReleaseActivationError::Refused("missing artifact filename".into()))?;
    let parent_fd = open_owned_dir(&parent_path)?;
    // A prior attempt may have promoted the exact bytes and then lost its DB
    // transaction. Adopt only a descriptor-verified immutable match; any
    // drift remains a hard refusal.
    if adopt_existing_acquired_artifact(parent_fd.as_raw_fd(), final_name, row)? {
        return Ok(());
    }
    ensure_absent_at(parent_fd.as_raw_fd(), OsStr::new(&temp_name))?;
    let temp_fd = open_new_file_at(parent_fd.as_raw_fd(), OsStr::new(&temp_name), 0o600)?;
    let temp_file = File::from(temp_fd);
    let transfer_result = (|| -> Result<()> {
        let stdout_file = temp_file.try_clone()?;
        let remote_path = format!("~/.forgefleet/release-builds/{remote_relative_path}");
        let destination = format!("{ssh_user}@{ip}");
        let output = Command::new("ssh")
            .args([
                "-oBatchMode=yes",
                "-oStrictHostKeyChecking=yes",
                "-oConnectTimeout=15",
                "-oConnectionAttempts=1",
                "-oServerAliveInterval=10",
                "-oServerAliveCountMax=3",
                "--",
                &destination,
                "cat",
                "--",
                &remote_path,
            ])
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            return Err(ReleaseActivationError::Refused(format!(
                "canonical custody transfer failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        finalize_acquired_temp(
            parent_fd.as_raw_fd(),
            OsStr::new(&temp_name),
            final_name,
            &temp_file,
            row,
        )
    })();
    cleanup_failed_acquisition(
        parent_fd.as_raw_fd(),
        OsStr::new(&temp_name),
        transfer_result,
    )
}

fn adopt_existing_acquired_artifact(
    parent: RawFd,
    final_name: &OsStr,
    row: &ReleaseArtifactRow,
) -> Result<bool> {
    if !exists_at(parent, final_name)? {
        return Ok(false);
    }
    let fd = open_read_file_at(parent, final_name)?;
    let mut file = File::from(fd);
    verify_open_file(&mut file, row.size_bytes as u64, &row.sha256)?;
    Ok(true)
}

fn finalize_acquired_temp(
    parent: RawFd,
    temp_name: &OsStr,
    final_name: &OsStr,
    temp_file: &File,
    row: &ReleaseArtifactRow,
) -> Result<()> {
    temp_file.sync_all()?;
    let mut verified = unsafe { File::from_raw_fd(dup_fd(temp_file.as_raw_fd())?) };
    verify_open_file(&mut verified, row.size_bytes as u64, &row.sha256)?;
    fsync_fd(parent)?;
    ensure_absent_at(parent, final_name)?;
    rename_noreplace(parent, temp_name, parent, final_name)?;
    fsync_fd(parent)
}

fn cleanup_failed_acquisition(parent: RawFd, temp_name: &OsStr, result: Result<()>) -> Result<()> {
    if result.is_err() {
        unlink_at(parent, temp_name);
        let _ = fsync_fd(parent);
    }
    result
}

fn ensure_owned_directory_tree(root: &Path, relative: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    let mut current = root.to_path_buf();
    validate_owned_directory(&current)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(ReleaseActivationError::Refused(
                "directory tree contains a non-normal component".into(),
            ));
        };
        current.push(name);
        match fs::create_dir(&current) {
            Ok(()) => fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        validate_owned_directory(&current)?;
    }
    Ok(())
}

fn validate_owned_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(ReleaseActivationError::Refused(format!(
            "authority directory is not a private effective-user-owned directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_safe_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(ReleaseActivationError::Refused(
            "custody path must be a non-empty normal relative path".into(),
        ));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(ReleaseActivationError::Refused(
                "custody path contains a non-normal component".into(),
            ));
        }
    }
    Ok(())
}

trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandResult>;
}

#[derive(Debug, Clone)]
struct CommandResult {
    success: bool,
    stdout: String,
    stderr: String,
}

struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandResult> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn prepare_swap_and_restart(
    pair: Vec<ResolvedArtifact>,
    identity: LocalComputerIdentity,
    request: CanonicalReleaseIdentity,
    target_triple: String,
    platform: ServicePlatform,
    ports: ServicePorts,
    home: PathBuf,
    prior_release_identity: PriorReleaseIdentity,
    runner: &dyn CommandRunner,
    fail_after_installs: Option<usize>,
) -> Result<ActiveTransaction> {
    prepare_swap_and_restart_with_id(
        Uuid::new_v4(),
        pair,
        identity,
        request,
        target_triple,
        platform,
        ports,
        home,
        prior_release_identity,
        runner,
        fail_after_installs,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_swap_and_restart_with_id(
    transaction_id: Uuid,
    pair: Vec<ResolvedArtifact>,
    identity: LocalComputerIdentity,
    request: CanonicalReleaseIdentity,
    target_triple: String,
    platform: ServicePlatform,
    ports: ServicePorts,
    home: PathBuf,
    prior_release_identity: PriorReleaseIdentity,
    runner: &dyn CommandRunner,
    fail_after_installs: Option<usize>,
) -> Result<ActiveTransaction> {
    let activation_dir = home.join(".forgefleet").join("release-activations");
    ensure_activation_directory(&activation_dir)?;
    let operation_lock = acquire_operation_lock(&activation_dir)?;
    recover_unfinished_transactions(&activation_dir, platform, ports, &home, runner)?;
    ensure_no_unfinished_transaction(&activation_dir)?;
    preflight_service_topology(platform, ports, &home, runner)?;
    let installed_prior = probe_installed_pair_identity(&home, runner)?;
    if installed_prior != prior_release_identity {
        return Err(ReleaseActivationError::Refused(
            "installed ff/forgefleetd provenance disagrees with live daemon health".into(),
        ));
    }

    let release_root = local_release_build_root().map_err(|error| {
        ReleaseActivationError::Refused(format!("release-build root unavailable: {error}"))
    })?;
    let ff_artifact = pair
        .iter()
        .find(|artifact| artifact.row.artifact_name == "ff")
        .ok_or_else(|| ReleaseActivationError::Refused("missing ff artifact".into()))?;
    let daemon_artifact = pair
        .iter()
        .find(|artifact| artifact.row.artifact_name == "forgefleetd")
        .ok_or_else(|| ReleaseActivationError::Refused("missing forgefleetd artifact".into()))?;
    let mut ff_source =
        open_artifact_beneath(&release_root, Path::new(&ff_artifact.custody.relative_path))?;
    let mut daemon_source = open_artifact_beneath(
        &release_root,
        Path::new(&daemon_artifact.custody.relative_path),
    )?;

    let local_bin = home.join(".local").join("bin");
    let cargo_bin = home.join(".cargo").join("bin");
    let local_dir = open_owned_dir(&local_bin)?;
    let local_ff_old = hash_existing_destination(local_dir.as_raw_fd(), OsStr::new("ff"))?;
    let daemon_old = hash_existing_destination(local_dir.as_raw_fd(), OsStr::new("forgefleetd"))?;

    let cargo_mirror_exists = file_exists_at_path(&cargo_bin.join("ff"))?;
    let cargo_destination = if cargo_mirror_exists {
        let dir = open_owned_dir(&cargo_bin)?;
        let cargo_old = hash_existing_destination(dir.as_raw_fd(), OsStr::new("ff"))?;
        if local_ff_old != cargo_old {
            return Err(ReleaseActivationError::Refused(
                "pre-existing ~/.cargo/bin/ff diverges from ~/.local/bin/ff".into(),
            ));
        }
        Some((dir, cargo_old))
    } else {
        None
    };

    let mut entries = Vec::with_capacity(if cargo_destination.is_some() { 3 } else { 2 });
    entries.push(stage_install_entry(
        transaction_id,
        "ff",
        &ff_artifact.row,
        &mut ff_source,
        local_dir,
        local_bin.clone(),
        "ff",
        &local_ff_old,
    )?);
    entries.push(stage_install_entry(
        transaction_id,
        "forgefleetd",
        &daemon_artifact.row,
        &mut daemon_source,
        open_owned_dir(&local_bin)?,
        local_bin.clone(),
        "forgefleetd",
        &daemon_old,
    )?);
    if let Some((cargo_dir, cargo_old)) = cargo_destination {
        entries.push(stage_install_entry(
            transaction_id,
            "ff",
            &ff_artifact.row,
            &mut ff_source,
            cargo_dir,
            cargo_bin,
            "ff",
            &cargo_old,
        )?);
    }

    for entry in &entries {
        if platform == ServicePlatform::Macos {
            verify_codesign(entry, true, runner)?;
        }
    }
    smoke_staged_pair(&entries, &request.source_commit, runner)?;

    let journal_path = activation_dir.join(format!("{transaction_id}.journal.jsonl"));
    let manifest = RollbackManifest {
        transaction_id,
        artifact_version: request.artifact_version.clone(),
        source_commit: request.source_commit.clone(),
        prior_release_identity: prior_release_identity.clone(),
        target_triple: target_triple.clone(),
        computer_id: identity.id,
        computer_name: identity.name.clone(),
        platform,
        created_at: Utc::now(),
        entries: entries
            .iter()
            .map(|entry| ManifestEntry {
                artifact_name: entry.artifact_name.clone(),
                destination: entry
                    .dir_path
                    .join(os_from_cstr(&entry.destination))
                    .display()
                    .to_string(),
                stage: entry
                    .dir_path
                    .join(os_from_cstr(&entry.stage))
                    .display()
                    .to_string(),
                backup: entry
                    .dir_path
                    .join(os_from_cstr(&entry.backup))
                    .display()
                    .to_string(),
                sha256: entry.expected_sha256.clone(),
                size_bytes: entry.expected_size,
                previous_sha256: entry.previous_sha256.clone(),
                previous_size_bytes: entry.previous_size,
            })
            .collect(),
    };
    write_manifest(&activation_dir, &manifest)?;
    create_journal(
        &journal_path,
        "prepared",
        "artifacts staged and rollback manifest durable",
    )?;

    let mut transaction = ActiveTransaction {
        id: transaction_id,
        version: request.artifact_version,
        source_commit: request.source_commit,
        prior_release_identity,
        target_triple,
        identity,
        platform,
        ports,
        home,
        activation_dir,
        journal_path,
        entries,
        _operation_lock: operation_lock,
    };

    if let Err(primary) = stop_services(transaction.platform, &transaction.home, runner) {
        return Err(handle_stop_failure(&transaction, runner, primary));
    }
    append_journal(
        &transaction.journal_path,
        "services_stopped",
        "exact MCP and daemon services quiesced",
    )?;

    if let Err(primary) = swap_entries(
        &transaction.entries,
        &transaction.journal_path,
        fail_after_installs,
    ) {
        return rollback_after_failure(&mut transaction, runner, primary);
    }
    if let Err(primary) = start_services(transaction.platform, &transaction.home, runner) {
        return rollback_after_failure(&mut transaction, runner, primary);
    }
    append_journal(
        &transaction.journal_path,
        "services_started",
        "exact MCP and daemon services restarted",
    )?;

    if let Err(primary) =
        verify_installed_entries(&transaction.entries, transaction.platform, runner).and_then(
            |()| smoke_installed_pair(&transaction.entries, &transaction.source_commit, runner),
        )
    {
        return rollback_after_failure(&mut transaction, runner, primary);
    }
    Ok(transaction)
}

fn rollback_after_failure(
    transaction: &mut ActiveTransaction,
    runner: &dyn CommandRunner,
    primary: ReleaseActivationError,
) -> Result<ActiveTransaction> {
    match rollback_transaction_inner(transaction, runner, &primary.to_string()) {
        Ok(()) => Err(primary),
        Err(rollback) => Err(ReleaseActivationError::Refused(format!(
            "activation failed ({primary}); mandatory pair rollback also failed ({rollback})"
        ))),
    }
}

fn handle_stop_failure(
    transaction: &ActiveTransaction,
    runner: &dyn CommandRunner,
    primary: ReleaseActivationError,
) -> ReleaseActivationError {
    let stop_journal = append_journal(
        &transaction.journal_path,
        "stop_failed",
        &primary.to_string(),
    );
    let restart = start_services(transaction.platform, &transaction.home, runner);
    cleanup_stages(&transaction.entries);
    if let Err(restart_error) = restart {
        let detail = format!(
            "partial stop failed ({primary}); mandatory service restoration failed ({restart_error})"
        );
        let _ = append_journal(&transaction.journal_path, "rollback_failed", &detail);
        return ReleaseActivationError::Refused(detail);
    }
    if let Err(journal_error) = stop_journal.and_then(|()| {
        append_journal(
            &transaction.journal_path,
            "rolled_back",
            "no swap occurred; original services restored",
        )
    }) {
        return ReleaseActivationError::Refused(format!(
            "partial stop failed ({primary}); services were restored but durable rollback state failed ({journal_error})"
        ));
    }
    primary
}

fn rollback_transaction(
    mut transaction: ActiveTransaction,
    runner: &dyn CommandRunner,
    reason: &str,
) -> Result<()> {
    rollback_transaction_inner(&mut transaction, runner, reason)
}

fn rollback_transaction_inner(
    transaction: &mut ActiveTransaction,
    runner: &dyn CommandRunner,
    reason: &str,
) -> Result<()> {
    // Never rewrite an executable while either managed service may still be
    // running it. A failed quiesce deliberately leaves the durable manifest,
    // backups, and stages in place for a later recovery attempt.
    if let Err(error) = stop_services_best_effort(transaction.platform, &transaction.home, runner) {
        let detail =
            format!("rollback refused because exact services could not be quiesced: {error}");
        let _ = append_journal(&transaction.journal_path, "rollback_failed", &detail);
        return Err(ReleaseActivationError::Refused(detail));
    }
    let mut errors = Vec::new();
    for entry in transaction.entries.iter().rev() {
        let dir_fd = entry.dir.as_raw_fd();
        let backup = os_from_cstr(&entry.backup);
        let destination = os_from_cstr(&entry.destination);
        if exists_at(dir_fd, backup)? {
            if exists_at(dir_fd, destination)?
                && let Err(error) = unlink_at_checked(dir_fd, destination)
            {
                errors.push(format!(
                    "remove candidate {}: {error}",
                    entry.dir_path.display()
                ));
                continue;
            }
            if let Err(error) = rename_noreplace(dir_fd, backup, dir_fd, destination) {
                errors.push(format!("restore {}: {error}", entry.dir_path.display()));
            }
        }
        if exists_at(dir_fd, os_from_cstr(&entry.stage))?
            && let Err(error) = unlink_at_checked(dir_fd, os_from_cstr(&entry.stage))
        {
            errors.push(format!("remove stage: {error}"));
        }
        if let Err(error) = fsync_fd(dir_fd) {
            errors.push(format!("fsync rollback directory: {error}"));
        }
    }
    if errors.is_empty()
        && let Err(error) = start_services(transaction.platform, &transaction.home, runner)
    {
        errors.push(format!("restart restored services: {error}"));
    }
    let detail = if errors.is_empty() {
        format!("mandatory pair rollback complete after: {reason}")
    } else {
        format!("rollback incomplete after {reason}: {}", errors.join("; "))
    };
    let state = if errors.is_empty() {
        "rolled_back"
    } else {
        "rollback_failed"
    };
    if let Err(error) = append_journal(&transaction.journal_path, state, &detail) {
        errors.push(format!("persist {state} journal state: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ReleaseActivationError::Refused(format!(
            "rollback incomplete after {reason}: {}",
            errors.join("; ")
        )))
    }
}

fn commit_transaction(
    mut transaction: ActiveTransaction,
    runner: &dyn CommandRunner,
) -> Result<ReleaseActivationReceipt> {
    let pending_path = transaction
        .activation_dir
        .join(format!(".{}.receipt.pending", transaction.id));
    let receipt_path = transaction
        .activation_dir
        .join(format!("{}.receipt.json", transaction.id));
    let artifacts = build_artifact_receipts(&transaction.entries);
    let receipt = ReleaseActivationReceipt {
        transaction_id: transaction.id,
        artifact_version: transaction.version.clone(),
        source_commit: transaction.source_commit.clone(),
        prior_release_identity: transaction.prior_release_identity.clone(),
        target_triple: transaction.target_triple.clone(),
        computer_id: transaction.identity.id,
        computer_name: transaction.identity.name.clone(),
        activated_at: Utc::now(),
        mcp_service: match transaction.platform {
            ServicePlatform::Linux => MCP_UNIT,
            ServicePlatform::Macos => MCP_LABEL,
        }
        .into(),
        daemon_service: match transaction.platform {
            ServicePlatform::Linux => DAEMON_UNIT,
            ServicePlatform::Macos => DAEMON_LABEL,
        }
        .into(),
        artifacts,
        receipt_path: receipt_path.display().to_string(),
    };
    if let Err(primary) = write_pending_receipt(&pending_path, &receipt)
        .and_then(|()| {
            rename_path_noreplace(&pending_path, &receipt_path)?;
            fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o444))?;
            sync_parent(&receipt_path)
        })
        .and_then(|()| {
            append_journal(
                &transaction.journal_path,
                "committed",
                "activation receipt durable",
            )
        })
    {
        let _ = fs::remove_file(&pending_path);
        let _ = fs::remove_file(&receipt_path);
        return match rollback_transaction_inner(&mut transaction, runner, &primary.to_string()) {
            Ok(()) => Err(primary),
            Err(rollback) => Err(ReleaseActivationError::Refused(format!(
                "receipt commit failed ({primary}); rollback also failed ({rollback})"
            ))),
        };
    }
    Ok(receipt)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollbackPathState {
    CandidateInstalled,
    CandidateParked,
    PreviousRestored,
}

fn inspect_committed_activation(
    transaction_id: Uuid,
    identity: LocalComputerIdentity,
    platform: ServicePlatform,
    home: PathBuf,
    runner: &dyn CommandRunner,
    archive_terminal_rollback: bool,
) -> Result<Option<InspectedCommittedActivation>> {
    if identity.name.eq_ignore_ascii_case("vinny") {
        return Err(ReleaseActivationError::Refused(
            "release activation inspection is forbidden on Vinny".into(),
        ));
    }
    let activation_dir = home.join(".forgefleet").join("release-activations");
    ensure_activation_directory(&activation_dir)?;
    let operation_lock = acquire_operation_lock(&activation_dir)?;
    let manifest_path = activation_dir.join(format!("{transaction_id}.manifest.json"));
    let journal_path = activation_dir.join(format!("{transaction_id}.journal.jsonl"));
    let receipt_path = activation_dir.join(format!("{transaction_id}.receipt.json"));
    if archive_terminal_rollback
        && !receipt_path.exists()
        && archive_terminal_rollback_for_retry(
            &activation_dir,
            transaction_id,
            &identity,
            platform,
            &home,
            runner,
        )?
    {
        return Ok(None);
    }
    let present = [
        manifest_path.exists(),
        journal_path.exists(),
        receipt_path.exists(),
    ];
    if present.iter().all(|value| !value) {
        return Ok(None);
    }
    if present.iter().any(|value| !value) {
        return Err(ReleaseActivationError::Refused(
            "coordinator activation authority is partial or crash-incomplete".into(),
        ));
    }

    let manifest_raw =
        read_private_authority_text(&manifest_path, 4 * 1024 * 1024, "rollback manifest")?;
    let receipt_raw =
        read_private_authority_text(&receipt_path, 4 * 1024 * 1024, "activation receipt")?;
    let manifest: RollbackManifest = serde_json::from_str(&manifest_raw)?;
    let receipt: ReleaseActivationReceipt = serde_json::from_str(&receipt_raw)?;
    validate_explicit_rollback_authority(
        transaction_id,
        &identity,
        platform,
        &home,
        &manifest,
        &receipt,
        &receipt_path,
    )?;
    validate_latest_activation_receipt(&activation_dir, transaction_id, receipt.activated_at)?;
    ensure_no_other_unfinished_transaction(&activation_dir, transaction_id)?;
    if last_journal_state(&journal_path)?.as_deref() != Some("committed") {
        return Err(ReleaseActivationError::Refused(
            "coordinator activation is not durably committed or already entered rollback".into(),
        ));
    }
    let entries = entries_from_manifest(&manifest, &home)?;
    validate_activation_receipt_entries(&receipt, &entries)?;
    for entry in &entries {
        if classify_rollback_paths(entry)? != RollbackPathState::CandidateInstalled {
            return Err(ReleaseActivationError::Refused(
                "committed activation no longer retains the exact candidate/predecessor pair"
                    .into(),
            ));
        }
        if platform == ServicePlatform::Macos {
            verify_codesign_path(
                &entry.dir_path.join(os_from_cstr(&entry.backup)),
                &entry.artifact_name,
                runner,
            )?;
        }
    }

    let proof = ReleaseRollbackProof {
        transaction_id,
        source_commit: receipt.source_commit.clone(),
        prior_release_identity: receipt.prior_release_identity.clone(),
        computer_id: identity.id,
        computer_name: identity.name,
        manifest_sha256: format!("{:x}", Sha256::digest(manifest_raw.as_bytes())),
        activation_receipt_sha256: format!("{:x}", Sha256::digest(receipt_raw.as_bytes())),
        verified_at: Utc::now(),
    };
    Ok(Some(InspectedCommittedActivation {
        receipt,
        proof,
        _operation_lock: operation_lock,
    }))
}

fn terminal_rollback_archive_dir(
    activation_dir: &Path,
    transaction_id: Uuid,
    manifest_raw: &str,
) -> PathBuf {
    let manifest_sha256 = format!("{:x}", Sha256::digest(manifest_raw.as_bytes()));
    activation_dir
        .join("retired")
        .join(format!("{transaction_id}-rolled-back-{manifest_sha256}"))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            sync_parent(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    validate_owned_directory(path)
}

fn incomplete_terminal_archive(retired: &Path, transaction_id: Uuid) -> Result<Option<PathBuf>> {
    if !retired.exists() {
        return Ok(None);
    }
    validate_owned_directory(retired)?;
    let prefix = format!("{transaction_id}-rolled-back-");
    let mut incomplete = Vec::new();
    for entry in fs::read_dir(retired)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(ReleaseActivationError::Refused(
                "retired release authority contains a non-UTF8 entry".into(),
            ));
        };
        let Some(digest) = name.strip_prefix(&prefix) else {
            continue;
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ReleaseActivationError::Refused(
                "retired release authority has a malformed terminal archive name".into(),
            ));
        }
        let path = entry.path();
        validate_owned_directory(&path)?;
        let manifest = path.join(format!("{transaction_id}.manifest.json"));
        let journal = path.join(format!("{transaction_id}.journal.jsonl"));
        if file_exists_at_path(&manifest)? && !file_exists_at_path(&journal)? {
            incomplete.push(path);
        }
    }
    if incomplete.len() > 1 {
        return Err(ReleaseActivationError::Refused(
            "multiple incomplete terminal rollback archives are ambiguous".into(),
        ));
    }
    Ok(incomplete.pop())
}

fn archive_terminal_rollback_for_retry(
    activation_dir: &Path,
    transaction_id: Uuid,
    identity: &LocalComputerIdentity,
    platform: ServicePlatform,
    home: &Path,
    runner: &dyn CommandRunner,
) -> Result<bool> {
    if identity.name.eq_ignore_ascii_case("vinny") {
        return Err(ReleaseActivationError::Refused(
            "terminal release retry is forbidden on Vinny".into(),
        ));
    }
    let manifest_live = activation_dir.join(format!("{transaction_id}.manifest.json"));
    let journal_live = activation_dir.join(format!("{transaction_id}.journal.jsonl"));
    let retired = activation_dir.join("retired");
    let live_manifest_exists = file_exists_at_path(&manifest_live)?;
    let live_journal_exists = file_exists_at_path(&journal_live)?;
    if live_manifest_exists && live_journal_exists {
        if last_journal_state(&journal_live)?.as_deref() != Some("rolled_back") {
            return Ok(false);
        }
    } else if live_manifest_exists || !live_journal_exists {
        return Ok(false);
    }

    let (journal_path, manifest_raw, archive) = if live_manifest_exists {
        let manifest_raw =
            read_private_authority_text(&manifest_live, 4 * 1024 * 1024, "rollback manifest")?;
        let archive = terminal_rollback_archive_dir(activation_dir, transaction_id, &manifest_raw);
        (journal_live.clone(), manifest_raw, archive)
    } else {
        let archive = incomplete_terminal_archive(&retired, transaction_id)?.ok_or_else(|| {
            ReleaseActivationError::Refused(
                "terminal rollback manifest is missing from live and retired authority".into(),
            )
        })?;
        let archived_manifest = archive.join(format!("{transaction_id}.manifest.json"));
        let manifest_raw = read_private_authority_text(
            &archived_manifest,
            4 * 1024 * 1024,
            "retired rollback manifest",
        )?;
        (journal_live.clone(), manifest_raw, archive)
    };
    if last_journal_state(&journal_path)?.as_deref() != Some("rolled_back") {
        return Ok(false);
    }
    ensure_private_directory(&retired)?;
    ensure_private_directory(&archive)?;
    let manifest_archived = archive.join(format!("{transaction_id}.manifest.json"));
    let journal_archived = archive.join(format!("{transaction_id}.journal.jsonl"));

    let manifest: RollbackManifest = serde_json::from_str(&manifest_raw)?;
    if manifest.transaction_id != transaction_id
        || manifest.computer_id != identity.id
        || manifest.computer_name != identity.name
        || manifest.platform != platform
        || last_journal_state(&journal_path)?.as_deref() != Some("rolled_back")
    {
        return Err(ReleaseActivationError::Refused(
            "terminal rollback authority does not prove this exact local transaction".into(),
        ));
    }
    let entries = entries_from_manifest(&manifest, home)?;
    verify_restored_entries(&entries, platform, runner)?;
    for entry in &entries {
        if optional_path_identity(entry.dir.as_raw_fd(), os_from_cstr(&entry.stage))?.is_some()
            || optional_path_identity(entry.dir.as_raw_fd(), os_from_cstr(&entry.backup))?.is_some()
        {
            return Err(ReleaseActivationError::Refused(
                "terminal rollback still retains a stage or rollback pathname".into(),
            ));
        }
    }
    if probe_installed_pair_identity(home, runner)? != manifest.prior_release_identity {
        return Err(ReleaseActivationError::Refused(
            "terminal rollback predecessor authority differs from the installed pair".into(),
        ));
    }

    for (live, archived) in [
        (&manifest_live, &manifest_archived),
        (&journal_live, &journal_archived),
    ] {
        match (file_exists_at_path(live)?, file_exists_at_path(archived)?) {
            (true, false) => {
                rename_path_noreplace(live, archived)?;
                sync_parent(live)?;
            }
            (false, true) => {}
            _ => {
                return Err(ReleaseActivationError::Refused(
                    "terminal rollback archive changed during reconciliation".into(),
                ));
            }
        }
    }
    read_private_authority_text(&manifest_archived, 4 * 1024 * 1024, "retired manifest")?;
    if last_journal_state(&journal_archived)?.as_deref() != Some("rolled_back") {
        return Err(ReleaseActivationError::Refused(
            "retired terminal journal lost its rolled_back state".into(),
        ));
    }
    sync_parent(&manifest_archived)?;
    sync_parent(&archive)?;
    Ok(true)
}

fn prepare_explicit_rollback(
    transaction_id: Uuid,
    identity: LocalComputerIdentity,
    platform: ServicePlatform,
    ports: ServicePorts,
    home: PathBuf,
    runner: &dyn CommandRunner,
    fail_after_transitions: Option<usize>,
) -> Result<PreparedExplicitRollback> {
    if identity.name.eq_ignore_ascii_case("vinny") {
        return Err(ReleaseActivationError::Refused(
            "release rollback is forbidden on Vinny".into(),
        ));
    }
    let activation_dir = home.join(".forgefleet").join("release-activations");
    ensure_activation_directory(&activation_dir)?;
    let operation_lock = acquire_operation_lock(&activation_dir)?;
    let manifest_path = activation_dir.join(format!("{transaction_id}.manifest.json"));
    let journal_path = activation_dir.join(format!("{transaction_id}.journal.jsonl"));
    let activation_receipt_path = activation_dir.join(format!("{transaction_id}.receipt.json"));
    let rollback_receipt_path = activation_dir.join(format!("{transaction_id}.rollback.json"));
    let pending_receipt_path = activation_dir.join(format!(".{transaction_id}.rollback.pending"));

    let manifest: RollbackManifest = read_private_json(&manifest_path, "rollback manifest")?;
    let activation_receipt: ReleaseActivationReceipt =
        read_private_json(&activation_receipt_path, "activation receipt")?;
    validate_explicit_rollback_authority(
        transaction_id,
        &identity,
        platform,
        &home,
        &manifest,
        &activation_receipt,
        &activation_receipt_path,
    )?;
    validate_latest_activation_receipt(
        &activation_dir,
        transaction_id,
        activation_receipt.activated_at,
    )?;
    let entries = entries_from_manifest(&manifest, &home)?;
    validate_activation_receipt_entries(&activation_receipt, &entries)?;

    let state = last_journal_state(&journal_path)?
        .ok_or_else(|| ReleaseActivationError::Refused("activation journal is empty".into()))?;
    let allowed = matches!(
        state.as_str(),
        "committed"
            | "rollback_started"
            | "rollback_stop_failed"
            | "rollback_candidate_parked"
            | "rollback_previous_restored"
            | "rollback_start_failed"
            | "rollback_services_started"
            | "rollback_verification_failed"
            | "rollback_verified"
            | "rollback_committed"
    );
    if !allowed {
        return Err(ReleaseActivationError::Refused(format!(
            "transaction is not a committed activation or resumable explicit rollback: {state}"
        )));
    }
    ensure_no_other_unfinished_transaction(&activation_dir, transaction_id)?;

    let mut path_states = Vec::with_capacity(entries.len());
    for entry in &entries {
        path_states.push(classify_rollback_paths(entry)?);
    }
    if state == "committed"
        && path_states
            .iter()
            .any(|state| *state != RollbackPathState::CandidateInstalled)
    {
        return Err(ReleaseActivationError::Refused(
            "committed activation pathnames do not all contain the exact candidate and predecessor"
                .into(),
        ));
    }

    if state == "rollback_committed" {
        require_all_previous_restored(&path_states)?;
        verify_restored_entries(&entries, platform, runner)?;
        if probe_installed_pair_identity(&home, runner)? != manifest.prior_release_identity {
            return Err(ReleaseActivationError::Refused(
                "committed rollback no longer reproduces the retained prior release report".into(),
            ));
        }
        let receipt: ReleaseRollbackReceipt =
            read_private_json(&rollback_receipt_path, "rollback receipt")?;
        validate_rollback_receipt(
            &receipt,
            &manifest,
            &identity,
            &entries,
            &rollback_receipt_path,
        )?;
        return Ok(PreparedExplicitRollback {
            receipt,
            journal_path,
            receipt_path: rollback_receipt_path,
            receipt_already_committed: true,
            _operation_lock: operation_lock,
        });
    }

    if state == "committed" {
        preflight_service_topology(platform, ports, &home, runner)?;
    }
    if platform == ServicePlatform::Macos {
        for entry in &entries {
            if classify_rollback_paths(entry)? != RollbackPathState::PreviousRestored {
                verify_codesign_path(
                    &entry.dir_path.join(os_from_cstr(&entry.backup)),
                    &entry.artifact_name,
                    runner,
                )?;
            }
        }
    }
    if state == "committed" {
        append_journal(
            &journal_path,
            "rollback_started",
            "explicit exact-predecessor rollback accepted",
        )?;
    }
    if let Err(error) = stop_services_best_effort(platform, &home, runner) {
        let detail = format!("explicit rollback could not quiesce exact services: {error}");
        let _ = append_journal(&journal_path, "rollback_stop_failed", &detail);
        return Err(ReleaseActivationError::Refused(detail));
    }

    let mut transitions = 0_usize;
    for (index, entry) in entries.iter().enumerate() {
        let dir = entry.dir.as_raw_fd();
        let mut entry_state = classify_rollback_paths(entry)?;
        if entry_state == RollbackPathState::CandidateInstalled {
            rename_noreplace(
                dir,
                os_from_cstr(&entry.destination),
                dir,
                os_from_cstr(&entry.stage),
            )?;
            fsync_fd(dir)?;
            append_journal(
                &journal_path,
                "rollback_candidate_parked",
                &format!("index={index} artifact={}", entry.artifact_name),
            )?;
            transitions += 1;
            if fail_after_transitions == Some(transitions) {
                return Err(ReleaseActivationError::Refused(format!(
                    "injected explicit rollback crash after transition {transitions}"
                )));
            }
            entry_state = classify_rollback_paths(entry)?;
        }
        if entry_state == RollbackPathState::CandidateParked {
            rename_noreplace(
                dir,
                os_from_cstr(&entry.backup),
                dir,
                os_from_cstr(&entry.destination),
            )?;
            fsync_fd(dir)?;
            append_journal(
                &journal_path,
                "rollback_previous_restored",
                &format!("index={index} artifact={}", entry.artifact_name),
            )?;
            transitions += 1;
            if fail_after_transitions == Some(transitions) {
                return Err(ReleaseActivationError::Refused(format!(
                    "injected explicit rollback crash after transition {transitions}"
                )));
            }
        }
    }

    let final_states: Vec<_> = entries
        .iter()
        .map(classify_rollback_paths)
        .collect::<Result<_>>()?;
    require_all_previous_restored(&final_states)?;
    verify_restored_entries(&entries, platform, runner)?;
    let restored_identity = probe_installed_pair_identity(&home, runner)?;
    if restored_identity != manifest.prior_release_identity {
        return Err(ReleaseActivationError::Refused(
            "restored exact bytes do not reproduce the retained prior release report".into(),
        ));
    }
    if let Err(error) = start_services(platform, &home, runner) {
        let detail = format!("predecessor restored but exact services failed to start: {error}");
        let _ = append_journal(&journal_path, "rollback_start_failed", &detail);
        return Err(ReleaseActivationError::Refused(detail));
    }
    append_journal(
        &journal_path,
        "rollback_services_started",
        "restored predecessor pair installed and exact services restarted",
    )?;

    let receipt = load_or_build_rollback_receipt(
        &manifest,
        &identity,
        &entries,
        &rollback_receipt_path,
        &pending_receipt_path,
    )?;
    Ok(PreparedExplicitRollback {
        receipt,
        journal_path,
        receipt_path: rollback_receipt_path,
        receipt_already_committed: false,
        _operation_lock: operation_lock,
    })
}

fn commit_explicit_rollback(
    prepared: PreparedExplicitRollback,
    _runner: &dyn CommandRunner,
) -> Result<ReleaseRollbackReceipt> {
    if prepared.receipt_already_committed {
        return Ok(prepared.receipt);
    }
    append_journal(
        &prepared.journal_path,
        "rollback_verified",
        "restored exact release identity, MCP semantics, and daemon health verified",
    )?;
    let pending_path = prepared
        .receipt_path
        .parent()
        .expect("receipt parent")
        .join(format!(
            ".{}.rollback.pending",
            prepared.receipt.transaction_id
        ));
    if !prepared.receipt_path.exists() {
        if !pending_path.exists() {
            write_new_json(&pending_path, &prepared.receipt, 0o600)?;
        } else {
            let pending: ReleaseRollbackReceipt =
                read_private_json(&pending_path, "pending rollback receipt")?;
            if pending != prepared.receipt {
                return Err(ReleaseActivationError::Refused(
                    "pending rollback receipt differs from durable rollback authority".into(),
                ));
            }
        }
        rename_path_noreplace(&pending_path, &prepared.receipt_path)?;
        fs::set_permissions(&prepared.receipt_path, fs::Permissions::from_mode(0o444))?;
        sync_parent(&prepared.receipt_path)?;
    } else {
        let durable: ReleaseRollbackReceipt =
            read_private_json(&prepared.receipt_path, "rollback receipt")?;
        if durable != prepared.receipt {
            return Err(ReleaseActivationError::Refused(
                "durable rollback receipt differs from prepared rollback authority".into(),
            ));
        }
    }
    append_journal(
        &prepared.journal_path,
        "rollback_committed",
        "stable explicit rollback receipt durable",
    )?;
    Ok(prepared.receipt)
}

fn read_private_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T> {
    serde_json::from_str(&read_private_authority_text(path, 4 * 1024 * 1024, label)?)
        .map_err(Into::into)
}

fn validate_explicit_rollback_authority(
    transaction_id: Uuid,
    identity: &LocalComputerIdentity,
    platform: ServicePlatform,
    home: &Path,
    manifest: &RollbackManifest,
    receipt: &ReleaseActivationReceipt,
    receipt_path: &Path,
) -> Result<()> {
    validate_full_source_commit(&manifest.source_commit)?;
    validate_prior_release_identity(&manifest.prior_release_identity)?;
    if manifest.prior_release_identity
        == (PriorReleaseIdentity::FullSha {
            sha: manifest.source_commit.clone(),
        })
    {
        return Err(ReleaseActivationError::Refused(
            "rollback manifest candidate and predecessor source commits are identical".into(),
        ));
    }
    let derived = current_platform_identity(&manifest.source_commit)?;
    let now = Utc::now();
    if manifest.transaction_id != transaction_id
        || receipt.transaction_id != transaction_id
        || manifest.computer_id != identity.id
        || receipt.computer_id != identity.id
        || manifest.computer_name != identity.name
        || receipt.computer_name != identity.name
        || manifest.platform != platform
        || derived.service_platform != platform
        || manifest.artifact_version != derived.artifact_version
        || manifest.target_triple != derived.target_triple
        || receipt.artifact_version != manifest.artifact_version
        || receipt.source_commit != manifest.source_commit
        || receipt.prior_release_identity != manifest.prior_release_identity
        || receipt.target_triple != manifest.target_triple
        || receipt.receipt_path != receipt_path.display().to_string()
        || receipt.activated_at < manifest.created_at
        || receipt.activated_at > now + chrono::TimeDelta::minutes(5)
    {
        return Err(ReleaseActivationError::Refused(
            "manifest/receipt/local identity authority mismatch".into(),
        ));
    }
    let expected_services = match platform {
        ServicePlatform::Linux => (MCP_UNIT, DAEMON_UNIT),
        ServicePlatform::Macos => (MCP_LABEL, DAEMON_LABEL),
    };
    if receipt.mcp_service != expected_services.0 || receipt.daemon_service != expected_services.1 {
        return Err(ReleaseActivationError::Refused(
            "activation receipt service authority mismatch".into(),
        ));
    }
    let expected_prefix = home.join(".forgefleet/release-activations");
    if receipt_path.parent() != Some(expected_prefix.as_path()) {
        return Err(ReleaseActivationError::Refused(
            "activation receipt is outside the fixed private authority directory".into(),
        ));
    }
    Ok(())
}

fn ensure_no_other_unfinished_transaction(activation_dir: &Path, selected_id: Uuid) -> Result<()> {
    for entry in fs::read_dir(activation_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(ReleaseActivationError::Refused(
                "activation authority directory contains a non-UTF8 entry".into(),
            ));
        };
        let Some(id_text) = name.strip_suffix(".journal.jsonl") else {
            continue;
        };
        let id = Uuid::parse_str(id_text).map_err(|_| {
            ReleaseActivationError::Refused("malformed activation journal filename".into())
        })?;
        if id == selected_id {
            continue;
        }
        if !matches!(
            last_journal_state(&entry.path())?.as_deref(),
            Some("committed" | "rolled_back" | "rollback_committed")
        ) {
            return Err(ReleaseActivationError::Refused(format!(
                "another unfinished release transaction must be recovered first: {id}"
            )));
        }
    }
    Ok(())
}

fn validate_latest_activation_receipt(
    activation_dir: &Path,
    requested_id: Uuid,
    requested_at: DateTime<Utc>,
) -> Result<()> {
    for entry in fs::read_dir(activation_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(ReleaseActivationError::Refused(
                "activation authority directory contains a non-UTF8 entry".into(),
            ));
        };
        let Some(id_text) = name.strip_suffix(".receipt.json") else {
            continue;
        };
        let id = Uuid::parse_str(id_text).map_err(|_| {
            ReleaseActivationError::Refused("malformed activation receipt filename".into())
        })?;
        let receipt: ReleaseActivationReceipt =
            read_private_json(&entry.path(), "activation receipt")?;
        if receipt.transaction_id != id
            || receipt.receipt_path != entry.path().display().to_string()
        {
            return Err(ReleaseActivationError::Refused(
                "activation receipt filename/path identity mismatch".into(),
            ));
        }
        if receipt.activated_at > requested_at
            || (receipt.activated_at == requested_at && id != requested_id)
        {
            return Err(ReleaseActivationError::Refused(format!(
                "transaction {requested_id} is stale; a newer or ambiguous activation receipt exists"
            )));
        }
    }
    Ok(())
}

fn validate_activation_receipt_entries(
    receipt: &ReleaseActivationReceipt,
    entries: &[InstallEntry],
) -> Result<()> {
    if receipt.artifacts != build_artifact_receipts(entries) {
        return Err(ReleaseActivationError::Refused(
            "activation receipt artifact authority differs from rollback manifest".into(),
        ));
    }
    Ok(())
}

fn optional_path_identity(dir: RawFd, name: &OsStr) -> Result<Option<(u64, String)>> {
    if !exists_at(dir, name)? {
        return Ok(None);
    }
    hash_existing_destination(dir, name).map(Some)
}

fn identity_matches(actual: &Option<(u64, String)>, size: u64, sha256: &str) -> bool {
    actual.as_ref().is_some_and(|(actual_size, actual_sha)| {
        *actual_size == size && constant_time_sha256_eq(actual_sha, sha256)
    })
}

fn classify_rollback_paths(entry: &InstallEntry) -> Result<RollbackPathState> {
    let dir = entry.dir.as_raw_fd();
    let destination = optional_path_identity(dir, os_from_cstr(&entry.destination))?;
    let stage = optional_path_identity(dir, os_from_cstr(&entry.stage))?;
    let backup = optional_path_identity(dir, os_from_cstr(&entry.backup))?;
    let destination_candidate =
        identity_matches(&destination, entry.expected_size, &entry.expected_sha256);
    let destination_previous =
        identity_matches(&destination, entry.previous_size, &entry.previous_sha256);
    let stage_candidate = identity_matches(&stage, entry.expected_size, &entry.expected_sha256);
    let backup_previous = identity_matches(&backup, entry.previous_size, &entry.previous_sha256);
    match (
        destination_candidate,
        destination_previous,
        stage.is_none(),
        stage_candidate,
        backup.is_none(),
        backup_previous,
    ) {
        (true, false, true, false, false, true) => Ok(RollbackPathState::CandidateInstalled),
        (false, false, false, true, false, true) if destination.is_none() => {
            Ok(RollbackPathState::CandidateParked)
        }
        (false, true, false, true, true, false) => Ok(RollbackPathState::PreviousRestored),
        _ => Err(ReleaseActivationError::Refused(format!(
            "rollback pathname identity is missing, partial, stale, or tampered for {}",
            entry
                .dir_path
                .join(os_from_cstr(&entry.destination))
                .display()
        ))),
    }
}

fn require_all_previous_restored(states: &[RollbackPathState]) -> Result<()> {
    if states
        .iter()
        .all(|state| *state == RollbackPathState::PreviousRestored)
    {
        Ok(())
    } else {
        Err(ReleaseActivationError::Refused(
            "explicit rollback did not restore the complete exact predecessor pair".into(),
        ))
    }
}

fn verify_restored_entries(
    entries: &[InstallEntry],
    platform: ServicePlatform,
    runner: &dyn CommandRunner,
) -> Result<()> {
    for entry in entries {
        let actual =
            hash_existing_destination(entry.dir.as_raw_fd(), os_from_cstr(&entry.destination))?;
        if actual.0 != entry.previous_size
            || !constant_time_sha256_eq(&actual.1, &entry.previous_sha256)
        {
            return Err(ReleaseActivationError::Refused(format!(
                "restored {} differs from retained predecessor size/SHA",
                entry.artifact_name
            )));
        }
        if platform == ServicePlatform::Macos {
            verify_codesign_path(
                &entry.dir_path.join(os_from_cstr(&entry.destination)),
                &entry.artifact_name,
                runner,
            )?;
        }
    }
    Ok(())
}

fn build_rollback_receipt(
    manifest: &RollbackManifest,
    identity: &LocalComputerIdentity,
    entries: &[InstallEntry],
    receipt_path: &Path,
) -> ReleaseRollbackReceipt {
    let artifacts = ARTIFACT_NAMES
        .iter()
        .filter_map(|name| {
            let matching: Vec<_> = entries
                .iter()
                .filter(|entry| entry.artifact_name == *name)
                .collect();
            matching.first().map(|first| RestoredArtifactReceipt {
                artifact_name: (*name).to_string(),
                restored_sha256: first.previous_sha256.clone(),
                restored_size_bytes: first.previous_size as i64,
                replaced_sha256: first.expected_sha256.clone(),
                replaced_size_bytes: first.expected_size as i64,
                destinations: matching
                    .iter()
                    .map(|entry| {
                        entry
                            .dir_path
                            .join(os_from_cstr(&entry.destination))
                            .display()
                            .to_string()
                    })
                    .collect(),
            })
        })
        .collect();
    ReleaseRollbackReceipt {
        transaction_id: manifest.transaction_id,
        replaced_source_commit: manifest.source_commit.clone(),
        restored_release_identity: manifest.prior_release_identity.clone(),
        computer_id: identity.id,
        computer_name: identity.name.clone(),
        rolled_back_at: Utc::now(),
        artifacts,
        receipt_path: receipt_path.display().to_string(),
    }
}

fn load_or_build_rollback_receipt(
    manifest: &RollbackManifest,
    identity: &LocalComputerIdentity,
    entries: &[InstallEntry],
    receipt_path: &Path,
    pending_path: &Path,
) -> Result<ReleaseRollbackReceipt> {
    for (path, label) in [
        (receipt_path, "rollback receipt"),
        (pending_path, "pending rollback receipt"),
    ] {
        if path.exists() {
            let receipt: ReleaseRollbackReceipt = read_private_json(path, label)?;
            validate_rollback_receipt(&receipt, manifest, identity, entries, receipt_path)?;
            return Ok(receipt);
        }
    }
    Ok(build_rollback_receipt(
        manifest,
        identity,
        entries,
        receipt_path,
    ))
}

fn validate_rollback_receipt(
    receipt: &ReleaseRollbackReceipt,
    manifest: &RollbackManifest,
    identity: &LocalComputerIdentity,
    entries: &[InstallEntry],
    receipt_path: &Path,
) -> Result<()> {
    let expected = build_rollback_receipt(manifest, identity, entries, receipt_path);
    if receipt.transaction_id != expected.transaction_id
        || receipt.replaced_source_commit != expected.replaced_source_commit
        || receipt.restored_release_identity != expected.restored_release_identity
        || receipt.computer_id != expected.computer_id
        || receipt.computer_name != expected.computer_name
        || receipt.artifacts != expected.artifacts
        || receipt.receipt_path != expected.receipt_path
        || receipt.rolled_back_at < manifest.created_at
        || receipt.rolled_back_at > Utc::now() + chrono::TimeDelta::minutes(5)
    {
        return Err(ReleaseActivationError::Refused(
            "rollback receipt differs from exact retained authority".into(),
        ));
    }
    Ok(())
}

fn build_artifact_receipts(entries: &[InstallEntry]) -> Vec<ActivatedArtifactReceipt> {
    ARTIFACT_NAMES
        .iter()
        .filter_map(|name| {
            let matching: Vec<_> = entries
                .iter()
                .filter(|entry| entry.artifact_name == *name)
                .collect();
            matching.first().map(|first| ActivatedArtifactReceipt {
                artifact_name: (*name).to_string(),
                sha256: first.expected_sha256.clone(),
                size_bytes: first.expected_size as i64,
                destinations: matching
                    .iter()
                    .map(|entry| {
                        entry
                            .dir_path
                            .join(os_from_cstr(&entry.destination))
                            .display()
                            .to_string()
                    })
                    .collect(),
            })
        })
        .collect()
}

fn open_artifact_beneath(root: &Path, relative: &Path) -> Result<OpenArtifact> {
    validate_safe_relative_path(relative)?;
    let mut directory = open_owned_dir(root)?;
    let components: Vec<_> = relative.components().collect();
    let (filename, parents) = components
        .split_last()
        .ok_or_else(|| ReleaseActivationError::Refused("custody path is empty".into()))?;
    for component in parents {
        let Component::Normal(name) = component else {
            return Err(ReleaseActivationError::Refused(
                "custody path is not normal".into(),
            ));
        };
        directory = open_owned_dir_at(directory.as_raw_fd(), name)?;
    }
    let Component::Normal(filename) = filename else {
        return Err(ReleaseActivationError::Refused(
            "custody filename is not normal".into(),
        ));
    };
    let fd = open_read_file_at(directory.as_raw_fd(), filename)?;
    let stat = fstat_fd(fd.as_raw_fd())?;
    validate_regular_stat(&stat, unsafe { libc::geteuid() }, true)?;
    Ok(OpenArtifact {
        file: File::from(fd),
        initial: stat,
    })
}

fn open_absolute_regular(path: &Path, expected_uid: libc::uid_t) -> Result<File> {
    let path = cstring(path.as_os_str())?;
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let stat = fstat_fd(fd.as_raw_fd())?;
    validate_regular_stat(&stat, expected_uid, true)?;
    Ok(File::from(fd))
}

fn open_owned_dir(path: &Path) -> Result<OwnedFd> {
    let path = cstring(path.as_os_str())?;
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    validate_directory_stat(&fstat_fd(fd.as_raw_fd())?)?;
    Ok(fd)
}

fn open_owned_dir_at(parent: RawFd, name: &OsStr) -> Result<OwnedFd> {
    let name = cstring(name)?;
    let raw = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    validate_directory_stat(&fstat_fd(fd.as_raw_fd())?)?;
    Ok(fd)
}

fn open_read_file_at(parent: RawFd, name: &OsStr) -> Result<OwnedFd> {
    let name = cstring(name)?;
    let raw = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn open_new_file_at(parent: RawFd, name: &OsStr, mode: libc::mode_t) -> Result<OwnedFd> {
    let name = cstring(name)?;
    let raw = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn fstat_fd(fd: RawFd) -> Result<libc::stat> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat)
}

fn validate_directory_stat(stat: &libc::stat) -> Result<()> {
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_uid != unsafe { libc::geteuid() }
        || stat.st_mode & 0o022 != 0
    {
        return Err(ReleaseActivationError::Refused(
            "path component is not a private effective-user-owned directory".into(),
        ));
    }
    Ok(())
}

fn validate_regular_stat(
    stat: &libc::stat,
    expected_uid: libc::uid_t,
    require_single_link: bool,
) -> Result<()> {
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_uid != expected_uid
        || (require_single_link && stat.st_nlink != 1)
    {
        return Err(ReleaseActivationError::Refused(
            "artifact is not a single-link regular file owned by the required authority".into(),
        ));
    }
    Ok(())
}

fn stage_install_entry(
    transaction_id: Uuid,
    artifact_name: &str,
    row: &ReleaseArtifactRow,
    source: &mut OpenArtifact,
    dir: OwnedFd,
    dir_path: PathBuf,
    destination: &str,
    previous: &(u64, String),
) -> Result<InstallEntry> {
    let stage = format!(".{destination}.release-{transaction_id}.stage");
    let backup = format!(".{destination}.release-{transaction_id}.rollback");
    ensure_absent_at(dir.as_raw_fd(), OsStr::new(&stage))?;
    ensure_absent_at(dir.as_raw_fd(), OsStr::new(&backup))?;
    let stage_fd = open_new_file_at(dir.as_raw_fd(), OsStr::new(&stage), 0o600)?;
    let mut stage_file = File::from(stage_fd);
    source.file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let expected_size = u64::try_from(row.size_bytes)
        .map_err(|_| ReleaseActivationError::Refused("registry artifact size is invalid".into()))?;
    if previous.0 == expected_size && constant_time_sha256_eq(&previous.1, &row.sha256) {
        unlink_at(dir.as_raw_fd(), OsStr::new(&stage));
        return Err(ReleaseActivationError::Refused(format!(
            "{} candidate bytes are identical to the retained predecessor",
            row.artifact_name
        )));
    }
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = source.file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            ReleaseActivationError::Refused("artifact byte count overflow".into())
        })?;
        if total > expected_size {
            unlink_at(dir.as_raw_fd(), OsStr::new(&stage));
            return Err(ReleaseActivationError::Refused(format!(
                "{} exceeds registered size",
                row.artifact_name
            )));
        }
        hasher.update(&buffer[..read]);
        stage_file.write_all(&buffer[..read])?;
    }
    let digest = format!("{:x}", hasher.finalize());
    let after = fstat_fd(source.file.as_raw_fd())?;
    if !same_file_snapshot(&source.initial, &after)
        || total != expected_size
        || !constant_time_sha256_eq(&digest, &row.sha256)
    {
        unlink_at(dir.as_raw_fd(), OsStr::new(&stage));
        return Err(ReleaseActivationError::Refused(format!(
            "{} changed or failed exact registered size/SHA verification while staging",
            row.artifact_name
        )));
    }
    if unsafe { libc::fchmod(stage_file.as_raw_fd(), 0o755) } != 0 {
        unlink_at(dir.as_raw_fd(), OsStr::new(&stage));
        return Err(std::io::Error::last_os_error().into());
    }
    stage_file.sync_all()?;
    fsync_fd(dir.as_raw_fd())?;
    Ok(InstallEntry {
        artifact_name: artifact_name.into(),
        expected_sha256: row.sha256.clone(),
        expected_size,
        previous_sha256: previous.1.clone(),
        previous_size: previous.0,
        dir,
        dir_path,
        destination: CString::new(destination).expect("fixed destination"),
        stage: CString::new(stage).expect("UUID stage name"),
        backup: CString::new(backup).expect("UUID backup name"),
    })
}

fn same_file_snapshot(before: &libc::stat, after: &libc::stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_ctime == after.st_ctime
        && before.st_nlink == after.st_nlink
        && before.st_uid == after.st_uid
        && after.st_nlink == 1
}

fn verify_open_file(file: &mut File, expected_size: u64, expected_sha: &str) -> Result<()> {
    let before = fstat_fd(file.as_raw_fd())?;
    validate_regular_stat(&before, unsafe { libc::geteuid() }, true)?;
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > expected_size {
            return Err(ReleaseActivationError::Refused(
                "acquired artifact exceeds registered size".into(),
            ));
        }
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    let after = fstat_fd(file.as_raw_fd())?;
    if !same_file_snapshot(&before, &after)
        || total != expected_size
        || !constant_time_sha256_eq(&digest, expected_sha)
    {
        return Err(ReleaseActivationError::Refused(
            "acquired artifact failed exact registered size/SHA verification".into(),
        ));
    }
    Ok(())
}

fn hash_existing_destination(dir: RawFd, name: &OsStr) -> Result<(u64, String)> {
    let fd = open_read_file_at(dir, name)?;
    let mut file = File::from(fd);
    let before = fstat_fd(file.as_raw_fd())?;
    validate_regular_stat(&before, unsafe { libc::geteuid() }, true)?;
    if before.st_mode & 0o022 != 0 {
        return Err(ReleaseActivationError::Refused(format!(
            "installed binary is writable by another identity: {}",
            name.to_string_lossy()
        )));
    }
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let after = fstat_fd(file.as_raw_fd())?;
    if !same_file_snapshot(&before, &after) {
        return Err(ReleaseActivationError::Refused(format!(
            "installed binary changed while being inspected: {}",
            name.to_string_lossy()
        )));
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

fn verify_installed_entries(
    entries: &[InstallEntry],
    platform: ServicePlatform,
    runner: &dyn CommandRunner,
) -> Result<()> {
    for entry in entries {
        let (size, digest) =
            hash_existing_destination(entry.dir.as_raw_fd(), os_from_cstr(&entry.destination))?;
        if size != entry.expected_size || !constant_time_sha256_eq(&digest, &entry.expected_sha256)
        {
            return Err(ReleaseActivationError::Refused(format!(
                "installed {} does not match registered size/SHA",
                entry.artifact_name
            )));
        }
        if platform == ServicePlatform::Macos {
            verify_codesign(entry, false, runner)?;
        }
    }
    Ok(())
}

fn preflight_service_topology(
    platform: ServicePlatform,
    ports: ServicePorts,
    home: &Path,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let binary = home.join(".local").join("bin").join("forgefleetd");
    match platform {
        ServicePlatform::Linux => {
            for (unit, role) in [(MCP_UNIT, "mcp"), (DAEMON_UNIT, "start")] {
                require_command(
                    runner,
                    "systemctl",
                    &["--user", "is-enabled", "--quiet", unit],
                    &format!("{unit} is not enabled"),
                )?;
                require_command(
                    runner,
                    "systemctl",
                    &["--user", "is-active", "--quiet", unit],
                    &format!("{unit} is not active"),
                )?;
                let output = require_command(
                    runner,
                    "systemctl",
                    &["--user", "show", "-p", "ExecStart", "--value", unit],
                    &format!("cannot inspect {unit}"),
                )?;
                let expected_binary = binary.display().to_string();
                if !output.stdout.contains(&expected_binary) || !output.stdout.contains(role) {
                    return Err(ReleaseActivationError::Refused(format!(
                        "{unit} does not execute the exact fixed binary and role"
                    )));
                }
                if unit == MCP_UNIT
                    && (!output.stdout.contains("--listen")
                        || !output.stdout.contains(&format!("0.0.0.0:{}", ports.mcp)))
                {
                    return Err(ReleaseActivationError::Refused(format!(
                        "{MCP_UNIT} does not use the reconciled MCP port {}",
                        ports.mcp
                    )));
                }
            }
        }
        ServicePlatform::Macos => {
            let uid = unsafe { libc::geteuid() }.to_string();
            for (label, role) in [(MCP_LABEL, "mcp"), (DAEMON_LABEL, "start")] {
                let plist = home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{label}.plist"));
                validate_owned_regular_path(&plist)?;
                require_command(
                    runner,
                    "/usr/bin/plutil",
                    &["-lint", &plist.display().to_string()],
                    &format!("invalid {label} plist"),
                )?;
                let domain = format!("gui/{uid}/{label}");
                let output = require_command(
                    runner,
                    "/bin/launchctl",
                    &["print", &domain],
                    &format!("{label} is not loaded"),
                )?;
                if !output.stdout.contains(&binary.display().to_string())
                    || !output.stdout.contains(role)
                {
                    return Err(ReleaseActivationError::Refused(format!(
                        "{label} does not execute the exact fixed binary and role"
                    )));
                }
                if label == MCP_LABEL
                    && (!output.stdout.contains("--listen")
                        || !output.stdout.contains(&format!("0.0.0.0:{}", ports.mcp)))
                {
                    return Err(ReleaseActivationError::Refused(format!(
                        "{MCP_LABEL} does not use the reconciled MCP port {}",
                        ports.mcp
                    )));
                }
            }
        }
    }
    Ok(())
}

fn stop_services(platform: ServicePlatform, home: &Path, runner: &dyn CommandRunner) -> Result<()> {
    match platform {
        ServicePlatform::Linux => {
            require_command(
                runner,
                "systemctl",
                &["--user", "stop", MCP_UNIT],
                "failed to stop exact MCP unit",
            )?;
            if let Err(error) = require_command(
                runner,
                "systemctl",
                &["--user", "stop", DAEMON_UNIT],
                "failed to stop exact daemon unit",
            ) {
                let restoration = require_command(
                    runner,
                    "systemctl",
                    &["--user", "start", MCP_UNIT],
                    "failed to restore MCP after partial stop",
                );
                return match restoration {
                    Ok(_) => Err(error),
                    Err(restoration) => Err(ReleaseActivationError::Refused(format!(
                        "failed to stop exact daemon unit ({error}); failed to restore MCP after partial stop ({restoration})"
                    ))),
                };
            }
        }
        ServicePlatform::Macos => {
            let uid = unsafe { libc::geteuid() }.to_string();
            for label in [MCP_LABEL, DAEMON_LABEL] {
                let plist = home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{label}.plist"));
                if let Err(error) = require_command(
                    runner,
                    "/bin/launchctl",
                    &[
                        "bootout",
                        &format!("gui/{uid}"),
                        &plist.display().to_string(),
                    ],
                    &format!("failed to bootout exact {label}"),
                ) {
                    if label == DAEMON_LABEL {
                        let mcp_plist = home
                            .join("Library")
                            .join("LaunchAgents")
                            .join(format!("{MCP_LABEL}.plist"));
                        let restoration = require_command(
                            runner,
                            "/bin/launchctl",
                            &[
                                "bootstrap",
                                &format!("gui/{uid}"),
                                &mcp_plist.display().to_string(),
                            ],
                            "failed to restore MCP after partial bootout",
                        );
                        if let Err(restoration) = restoration {
                            return Err(ReleaseActivationError::Refused(format!(
                                "failed to bootout exact {label} ({error}); failed to restore MCP after partial bootout ({restoration})"
                            )));
                        }
                    }
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

fn stop_services_best_effort(
    platform: ServicePlatform,
    home: &Path,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let mut errors = Vec::new();
    match platform {
        ServicePlatform::Linux => {
            for unit in [MCP_UNIT, DAEMON_UNIT] {
                if let Err(error) = require_command(
                    runner,
                    "systemctl",
                    &["--user", "stop", unit],
                    &format!("failed to stop {unit}"),
                ) {
                    errors.push(error.to_string());
                }
            }
        }
        ServicePlatform::Macos => {
            let uid = unsafe { libc::geteuid() }.to_string();
            for label in [MCP_LABEL, DAEMON_LABEL] {
                let plist = home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{label}.plist"));
                if let Err(error) = require_command(
                    runner,
                    "/bin/launchctl",
                    &[
                        "bootout",
                        &format!("gui/{uid}"),
                        &plist.display().to_string(),
                    ],
                    &format!("failed to bootout {label}"),
                ) {
                    let domain = format!("gui/{uid}/{label}");
                    let probe = runner.run("/bin/launchctl", &["print".into(), domain.clone()]);
                    match probe {
                        Ok(output) if launchctl_print_confirms_absent(&output) => {}
                        Ok(output) => errors.push(format!(
                            "{error}; exact absence probe for {domain} was not authoritative: {}",
                            output.stderr.trim()
                        )),
                        Err(probe_error) => errors.push(format!(
                            "{error}; exact absence probe for {domain} failed: {probe_error}"
                        )),
                    }
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ReleaseActivationError::Refused(errors.join("; ")))
    }
}

fn launchctl_print_confirms_absent(output: &CommandResult) -> bool {
    if output.success {
        return false;
    }
    let detail = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    [
        "could not find service",
        "could not find specified service",
        "service not found",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

fn start_services(
    platform: ServicePlatform,
    home: &Path,
    runner: &dyn CommandRunner,
) -> Result<()> {
    match platform {
        ServicePlatform::Linux => {
            for unit in [DAEMON_UNIT, MCP_UNIT] {
                require_command(
                    runner,
                    "systemctl",
                    &["--user", "start", unit],
                    &format!("failed to start exact {unit}"),
                )?;
                require_command(
                    runner,
                    "systemctl",
                    &["--user", "is-active", "--quiet", unit],
                    &format!("exact {unit} is not active after start"),
                )?;
            }
        }
        ServicePlatform::Macos => {
            let uid = unsafe { libc::geteuid() }.to_string();
            for label in [DAEMON_LABEL, MCP_LABEL] {
                let plist = home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{label}.plist"));
                require_command(
                    runner,
                    "/bin/launchctl",
                    &[
                        "bootstrap",
                        &format!("gui/{uid}"),
                        &plist.display().to_string(),
                    ],
                    &format!("failed to bootstrap exact {label}"),
                )?;
                let domain = format!("gui/{uid}/{label}");
                require_command(
                    runner,
                    "/bin/launchctl",
                    &["kickstart", "-k", &domain],
                    &format!("failed to kickstart exact {label}"),
                )?;
                require_command(
                    runner,
                    "/bin/launchctl",
                    &["print", &domain],
                    &format!("exact {label} is not loaded after start"),
                )?;
            }
        }
    }
    Ok(())
}

fn require_command(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
    context: &str,
) -> Result<CommandResult> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let output = runner.run(program, &args)?;
    if !output.success {
        return Err(ReleaseActivationError::Refused(format!(
            "{context}: {}",
            output.stderr.trim()
        )));
    }
    Ok(output)
}

fn verify_codesign(entry: &InstallEntry, staged: bool, runner: &dyn CommandRunner) -> Result<()> {
    let name = if staged {
        os_from_cstr(&entry.stage)
    } else {
        os_from_cstr(&entry.destination)
    };
    verify_codesign_path(&entry.dir_path.join(name), &entry.artifact_name, runner)
}

fn verify_codesign_path(
    path: &Path,
    artifact_name: &str,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let path = path.display().to_string();
    // Activation must never sign: signing mutates the bytes whose digest V291
    // registered.  Builders sign before hashing; consumers only verify.
    require_command(
        runner,
        "/usr/bin/codesign",
        &["--verify", "--strict", "--verbose=2", &path],
        &format!("codesign verification failed for {artifact_name}"),
    )?;
    Ok(())
}

fn smoke_staged_pair(
    entries: &[InstallEntry],
    source_commit: &str,
    runner: &dyn CommandRunner,
) -> Result<()> {
    for artifact_name in ARTIFACT_NAMES {
        let entry = entries
            .iter()
            .find(|entry| entry.artifact_name == artifact_name)
            .ok_or_else(|| {
                ReleaseActivationError::Refused(format!("missing staged {artifact_name}"))
            })?;
        smoke_path(
            &entry.dir_path.join(os_from_cstr(&entry.stage)),
            source_commit,
            runner,
        )?;
    }
    Ok(())
}

fn smoke_installed_pair(
    entries: &[InstallEntry],
    source_commit: &str,
    runner: &dyn CommandRunner,
) -> Result<()> {
    for artifact_name in ARTIFACT_NAMES {
        let entry = entries
            .iter()
            .find(|entry| entry.artifact_name == artifact_name)
            .ok_or_else(|| {
                ReleaseActivationError::Refused(format!("missing installed {artifact_name}"))
            })?;
        smoke_path(
            &entry.dir_path.join(os_from_cstr(&entry.destination)),
            source_commit,
            runner,
        )?;
    }
    Ok(())
}

fn smoke_path(path: &Path, source_commit: &str, runner: &dyn CommandRunner) -> Result<()> {
    let output = runner.run(&path.display().to_string(), &["--version".into()])?;
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    if !output.success
        || !combined
            .split(|c: char| !c.is_ascii_hexdigit())
            .any(|token| token == source_commit)
    {
        return Err(ReleaseActivationError::Refused(format!(
            "binary smoke test did not prove exact full source commit: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_prior_release_identity(identity: &PriorReleaseIdentity) -> Result<()> {
    match identity {
        PriorReleaseIdentity::FullSha { sha } => validate_full_source_commit(sha),
        PriorReleaseIdentity::LegacyReported { short_sha }
            if (8..=10).contains(&short_sha.len())
                && short_sha
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
        {
            Ok(())
        }
        PriorReleaseIdentity::LegacyReported { .. } => Err(ReleaseActivationError::Refused(
            "legacy release identity must be exactly 8-10 lowercase hexadecimal characters".into(),
        )),
    }
}

fn parse_reported_release_identity(value: &str) -> Result<PriorReleaseIdentity> {
    let mut identities = value
        .split(|character: char| !character.is_ascii_hexdigit())
        .filter_map(|token| {
            let identity = if token.len() == 40 {
                PriorReleaseIdentity::FullSha {
                    sha: token.to_string(),
                }
            } else if (8..=10).contains(&token.len()) {
                PriorReleaseIdentity::LegacyReported {
                    short_sha: token.to_string(),
                }
            } else {
                return None;
            };
            validate_prior_release_identity(&identity).ok()?;
            Some(identity)
        })
        .collect::<std::collections::BTreeSet<_>>();
    if identities.len() != 1 {
        return Err(ReleaseActivationError::Refused(
            "release probe did not report one unambiguous full or legacy build identity".into(),
        ));
    }
    Ok(identities.pop_first().expect("one identity"))
}

fn parse_canonical_reported_identity(value: &str) -> Result<PriorReleaseIdentity> {
    let identity = if value.len() == 40 {
        PriorReleaseIdentity::FullSha {
            sha: value.to_string(),
        }
    } else {
        PriorReleaseIdentity::LegacyReported {
            short_sha: value.to_string(),
        }
    };
    validate_prior_release_identity(&identity)?;
    Ok(identity)
}

fn probe_installed_pair_identity(
    home: &Path,
    runner: &dyn CommandRunner,
) -> Result<PriorReleaseIdentity> {
    let mut reported = Vec::with_capacity(2);
    for name in ARTIFACT_NAMES {
        let path = home.join(".local/bin").join(name);
        let output = runner.run(&path.display().to_string(), &["--version".into()])?;
        if !output.success {
            return Err(ReleaseActivationError::Refused(format!(
                "installed {name} provenance probe failed: {}",
                output.stderr.trim()
            )));
        }
        reported.push(parse_reported_release_identity(&format!(
            "{}\n{}",
            output.stdout, output.stderr
        ))?);
    }
    if reported[0] != reported[1] {
        return Err(ReleaseActivationError::Refused(
            "installed ff and forgefleetd report different prior release identities".into(),
        ));
    }
    Ok(reported.remove(0))
}

fn swap_entries(
    entries: &[InstallEntry],
    journal: &Path,
    fail_after_installs: Option<usize>,
) -> Result<()> {
    for (index, entry) in entries.iter().enumerate() {
        let dir = entry.dir.as_raw_fd();
        rename_noreplace(
            dir,
            os_from_cstr(&entry.destination),
            dir,
            os_from_cstr(&entry.backup),
        )?;
        fsync_fd(dir)?;
        append_journal(
            journal,
            "backup_moved",
            &format!("index={index} artifact={}", entry.artifact_name),
        )?;
        if fail_after_installs == Some(index) {
            return Err(ReleaseActivationError::Refused(format!(
                "injected swap failure before install index {index}"
            )));
        }
        rename_noreplace(
            dir,
            os_from_cstr(&entry.stage),
            dir,
            os_from_cstr(&entry.destination),
        )?;
        fsync_fd(dir)?;
        append_journal(
            journal,
            "candidate_installed",
            &format!("index={index} artifact={}", entry.artifact_name),
        )?;
    }
    append_journal(journal, "pair_swapped", "all fixed pathnames swapped")?;
    Ok(())
}

fn cleanup_stages(entries: &[InstallEntry]) {
    for entry in entries {
        let dir = entry.dir.as_raw_fd();
        if exists_at(dir, os_from_cstr(&entry.stage)).unwrap_or(false) {
            unlink_at(dir, os_from_cstr(&entry.stage));
            let _ = fsync_fd(dir);
        }
    }
}

fn ensure_activation_directory(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        ReleaseActivationError::Refused("activation directory has no parent".into())
    })?;
    fs::create_dir_all(parent)?;
    if !path.exists() {
        fs::create_dir(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    validate_owned_directory(path)
}

fn acquire_operation_lock(activation_dir: &Path) -> Result<File> {
    let path = activation_dir.join(".operation.lock");
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(&path)?;
    let stat = fstat_fd(file.as_raw_fd())?;
    validate_regular_stat(&stat, unsafe { libc::geteuid() }, true)?;
    if stat.st_mode & 0o077 != 0 {
        return Err(ReleaseActivationError::Refused(
            "release operation lock is not private".into(),
        ));
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(ReleaseActivationError::Refused(format!(
            "another release activation or rollback owns the local operation lock: {error}"
        )));
    }
    Ok(file)
}

fn write_manifest(directory: &Path, manifest: &RollbackManifest) -> Result<()> {
    let path = directory.join(format!("{}.manifest.json", manifest.transaction_id));
    write_new_json(&path, manifest, 0o400)
}

fn create_journal(path: &Path, state: &str, detail: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    write_journal_event(&mut file, state, detail)?;
    sync_parent(path)
}

fn append_journal(path: &Path, state: &str, detail: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(ReleaseActivationError::Refused(
            "transaction journal lost its file identity".into(),
        ));
    }
    let mut file = OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let opened = fstat_fd(file.as_raw_fd())?;
    validate_regular_stat(&opened, unsafe { libc::geteuid() }, true)?;
    if opened.st_mode & 0o022 != 0
        || opened.st_dev != metadata.dev() as libc::dev_t
        || opened.st_ino != metadata.ino() as libc::ino_t
    {
        return Err(ReleaseActivationError::Refused(
            "transaction journal changed identity while opening".into(),
        ));
    }
    write_journal_event(&mut file, state, detail)
}

fn write_journal_event(file: &mut File, state: &str, detail: &str) -> Result<()> {
    let event = JournalEvent {
        at: Utc::now(),
        state: state.into(),
        detail: detail.into(),
    };
    serde_json::to_writer(&mut *file, &event)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn write_new_json<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    sync_parent(path)
}

fn write_pending_receipt(path: &Path, receipt: &ReleaseActivationReceipt) -> Result<()> {
    write_new_json(path, receipt, 0o600)
}

fn ensure_no_unfinished_transaction(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Err(ReleaseActivationError::Refused(
                "activation directory contains a non-UTF8 entry".into(),
            ));
        };
        if !file_name.ends_with(".journal.jsonl") {
            continue;
        }
        let state = last_journal_state(&entry.path())?;
        if !matches!(
            state.as_deref(),
            Some("committed" | "rolled_back" | "rollback_committed")
        ) {
            return Err(ReleaseActivationError::Refused(format!(
                "unfinished activation journal requires recovery: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn last_journal_state(path: &Path) -> Result<Option<String>> {
    let raw = read_private_authority_text(path, 4 * 1024 * 1024, "transaction journal")?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<JournalEvent>(line).map(|event| event.state))
        .next_back()
        .transpose()
        .map_err(Into::into)
}

fn recover_unfinished_transactions(
    activation_dir: &Path,
    platform: ServicePlatform,
    _ports: ServicePorts,
    home: &Path,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let mut pending = Vec::new();
    for entry in fs::read_dir(activation_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(ReleaseActivationError::Refused(
                "activation directory contains a non-UTF8 entry".into(),
            ));
        };
        let Some(id) = name.strip_suffix(".manifest.json") else {
            continue;
        };
        let id = Uuid::parse_str(id).map_err(|_| {
            ReleaseActivationError::Refused("malformed rollback manifest filename".into())
        })?;
        let journal = activation_dir.join(format!("{id}.journal.jsonl"));
        if journal.exists() {
            if matches!(
                last_journal_state(&journal)?.as_deref(),
                Some("committed" | "rolled_back" | "rollback_committed")
            ) {
                continue;
            }
            if last_journal_state(&journal)?
                .as_deref()
                .is_some_and(|state| state.starts_with("rollback_"))
            {
                return Err(ReleaseActivationError::Refused(format!(
                    "explicit rollback transaction requires rollback resume: {id}"
                )));
            }
        } else {
            create_journal(
                &journal,
                "manifest_only",
                "crash occurred after durable manifest and before journal creation",
            )?;
        }
        pending.push((id, journal));
    }
    if pending.len() > 1 {
        return Err(ReleaseActivationError::Refused(
            "multiple unfinished activation journals are ambiguous".into(),
        ));
    }
    let Some((id, journal)) = pending.pop() else {
        return ensure_no_unfinished_transaction(activation_dir);
    };
    let manifest_path = activation_dir.join(format!("{id}.manifest.json"));
    let manifest: RollbackManifest = serde_json::from_str(&read_private_authority_text(
        &manifest_path,
        4 * 1024 * 1024,
        "rollback manifest",
    )?)?;
    if manifest.transaction_id != id {
        return Err(ReleaseActivationError::Refused(
            "unfinished manifest transaction identity mismatch".into(),
        ));
    }
    remove_uncommitted_receipts(activation_dir, id)?;
    let mut entries = entries_from_manifest(&manifest, home)?;
    // Preserve every pathname until both services are certainly quiesced. It
    // is safer to leave a recoverable transaction pending than to replace a
    // binary which a still-running managed process may be executing.
    if let Err(error) = stop_services_best_effort(platform, home, runner) {
        let detail =
            format!("unfinished activation recovery could not quiesce exact services: {error}");
        let _ = append_journal(&journal, "recovery_failed", &detail);
        return Err(ReleaseActivationError::Refused(detail));
    }
    let mut errors = Vec::new();
    for entry in entries.iter_mut().rev() {
        let dir = entry.dir.as_raw_fd();
        match exists_at(dir, os_from_cstr(&entry.backup)) {
            Ok(true) => {
                if exists_at(dir, os_from_cstr(&entry.destination)).unwrap_or(false)
                    && let Err(error) = unlink_at_checked(dir, os_from_cstr(&entry.destination))
                {
                    errors.push(error.to_string());
                    continue;
                }
                if let Err(error) = rename_noreplace(
                    dir,
                    os_from_cstr(&entry.backup),
                    dir,
                    os_from_cstr(&entry.destination),
                ) {
                    errors.push(error.to_string());
                }
            }
            Ok(false) => {
                if !exists_at(dir, os_from_cstr(&entry.destination)).unwrap_or(false) {
                    errors.push(format!(
                        "neither original destination nor rollback exists for {}",
                        entry.dir_path.display()
                    ));
                }
            }
            Err(error) => errors.push(error.to_string()),
        }
        if exists_at(dir, os_from_cstr(&entry.stage)).unwrap_or(false) {
            let _ = unlink_at_checked(dir, os_from_cstr(&entry.stage));
        }
        if let Err(error) = fsync_fd(dir) {
            errors.push(error.to_string());
        }
    }
    if errors.is_empty()
        && let Err(error) = start_services(platform, home, runner)
    {
        errors.push(error.to_string());
    }
    if !errors.is_empty() {
        let _ = append_journal(&journal, "recovery_failed", &errors.join("; "));
        return Err(ReleaseActivationError::Refused(format!(
            "unfinished activation recovery failed: {}",
            errors.join("; ")
        )));
    }
    append_journal(
        &journal,
        "rolled_back",
        "crash recovery restored all fixed pathnames and exact services",
    )
}

fn read_private_authority_text(path: &Path, max_bytes: i64, label: &str) -> Result<String> {
    let path_c = cstring(path.as_os_str())?;
    let raw = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let stat = fstat_fd(fd.as_raw_fd())?;
    validate_regular_stat(&stat, unsafe { libc::geteuid() }, true)?;
    if stat.st_mode & 0o022 != 0 || stat.st_size < 0 || stat.st_size > max_bytes {
        return Err(ReleaseActivationError::Refused(format!(
            "{label} is writable by another identity or exceeds its size bound"
        )));
    }
    let mut file = File::from(fd);
    let mut value = String::new();
    file.read_to_string(&mut value)?;
    let after = fstat_fd(file.as_raw_fd())?;
    if !same_file_snapshot(&stat, &after) || value.len() as i64 != stat.st_size {
        return Err(ReleaseActivationError::Refused(format!(
            "{label} changed while being read"
        )));
    }
    Ok(value)
}

fn remove_uncommitted_receipts(activation_dir: &Path, id: Uuid) -> Result<()> {
    for path in [
        activation_dir.join(format!(".{id}.receipt.pending")),
        activation_dir.join(format!("{id}.receipt.json")),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt;
                if !metadata.file_type().is_file()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.nlink() != 1
                {
                    return Err(ReleaseActivationError::Refused(format!(
                        "uncommitted receipt lost its safe file identity: {}",
                        path.display()
                    )));
                }
                fs::remove_file(&path)?;
                sync_parent(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn entries_from_manifest(manifest: &RollbackManifest, home: &Path) -> Result<Vec<InstallEntry>> {
    let local_ff = home.join(".local/bin/ff");
    let local_daemon = home.join(".local/bin/forgefleetd");
    let cargo_ff = home.join(".cargo/bin/ff");
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    for manifest_entry in &manifest.entries {
        validate_sha256(&manifest_entry.sha256, "candidate manifest SHA-256")?;
        validate_sha256(
            &manifest_entry.previous_sha256,
            "predecessor manifest SHA-256",
        )?;
        if manifest_entry.size_bytes == 0
            || manifest_entry.previous_size_bytes == 0
            || manifest_entry.size_bytes > i64::MAX as u64
            || manifest_entry.previous_size_bytes > i64::MAX as u64
            || (manifest_entry.size_bytes == manifest_entry.previous_size_bytes
                && constant_time_sha256_eq(&manifest_entry.sha256, &manifest_entry.previous_sha256))
        {
            return Err(ReleaseActivationError::Refused(
                "rollback manifest contains invalid or indistinguishable candidate/predecessor authority"
                    .into(),
            ));
        }
        let destination = PathBuf::from(&manifest_entry.destination);
        if destination != local_ff && destination != local_daemon && destination != cargo_ff {
            return Err(ReleaseActivationError::Refused(
                "unfinished manifest contains a non-authoritative destination".into(),
            ));
        }
        if !seen.insert(destination.clone()) {
            return Err(ReleaseActivationError::Refused(
                "unfinished manifest repeats a destination".into(),
            ));
        }
        let parent = destination
            .parent()
            .expect("fixed destination parent")
            .to_path_buf();
        let destination_name = destination.file_name().expect("fixed destination name");
        let expected_artifact_name = if destination == local_daemon {
            "forgefleetd"
        } else {
            "ff"
        };
        if manifest_entry.artifact_name != expected_artifact_name {
            return Err(ReleaseActivationError::Refused(
                "rollback manifest artifact name does not match its fixed destination".into(),
            ));
        }
        let stage = PathBuf::from(&manifest_entry.stage);
        let backup = PathBuf::from(&manifest_entry.backup);
        if stage.parent() != Some(parent.as_path())
            || backup.parent() != Some(parent.as_path())
            || !stage
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name == format!(
                        ".{}.release-{}.stage",
                        destination_name.to_string_lossy(),
                        manifest.transaction_id
                    )
                })
            || !backup
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name == format!(
                        ".{}.release-{}.rollback",
                        destination_name.to_string_lossy(),
                        manifest.transaction_id
                    )
                })
        {
            return Err(ReleaseActivationError::Refused(
                "unfinished manifest staging/rollback path is not canonical".into(),
            ));
        }
        entries.push(InstallEntry {
            artifact_name: manifest_entry.artifact_name.clone(),
            expected_sha256: manifest_entry.sha256.clone(),
            expected_size: manifest_entry.size_bytes,
            previous_sha256: manifest_entry.previous_sha256.clone(),
            previous_size: manifest_entry.previous_size_bytes,
            dir: open_owned_dir(&parent)?,
            dir_path: parent,
            destination: cstring(destination_name)?,
            stage: cstring(stage.file_name().expect("validated stage name"))?,
            backup: cstring(backup.file_name().expect("validated backup name"))?,
        });
    }
    let local_names: Vec<_> = entries
        .iter()
        .filter(|entry| entry.dir_path == home.join(".local/bin"))
        .map(|entry| entry.destination.to_string_lossy().to_string())
        .collect();
    if !local_names.contains(&"ff".to_string()) || !local_names.contains(&"forgefleetd".to_string())
    {
        return Err(ReleaseActivationError::Refused(
            "unfinished manifest lacks the exact local ff+forgefleetd pair".into(),
        ));
    }
    if entries.len() != 2 && entries.len() != 3 {
        return Err(ReleaseActivationError::Refused(
            "rollback manifest must contain exactly the local pair and optional cargo ff mirror"
                .into(),
        ));
    }
    let local_ff = entries
        .iter()
        .find(|entry| {
            entry.dir_path == home.join(".local/bin")
                && os_from_cstr(&entry.destination) == OsStr::new("ff")
        })
        .expect("validated local ff");
    if let Some(cargo_ff) = entries
        .iter()
        .find(|entry| entry.dir_path == home.join(".cargo/bin"))
        && (cargo_ff.expected_size != local_ff.expected_size
            || !constant_time_sha256_eq(&cargo_ff.expected_sha256, &local_ff.expected_sha256)
            || cargo_ff.previous_size != local_ff.previous_size
            || !constant_time_sha256_eq(&cargo_ff.previous_sha256, &local_ff.previous_sha256))
    {
        return Err(ReleaseActivationError::Refused(
            "cargo ff mirror authority diverges from the fixed local ff authority".into(),
        ));
    }
    Ok(entries)
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ReleaseActivationError::Refused(format!(
            "{label} must be exactly 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

async fn verify_running_release(source_commit: &str, ports: ServicePorts) -> Result<()> {
    verify_running_release_identity(
        &PriorReleaseIdentity::FullSha {
            sha: source_commit.to_string(),
        },
        ports,
    )
    .await
}

async fn verify_running_release_identity(
    expected: &PriorReleaseIdentity,
    ports: ServicePorts,
) -> Result<()> {
    validate_prior_release_identity(expected)?;
    let reported = probe_running_release_identity(ports).await?;
    if &reported != expected {
        return Err(ReleaseActivationError::Refused(format!(
            "post-restart daemon provenance mismatch: expected {expected:?}, reported {reported:?}"
        )));
    }
    Ok(())
}

async fn probe_running_release_identity(ports: ServicePorts) -> Result<PriorReleaseIdentity> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|error| {
            ReleaseActivationError::Refused(format!("build loopback probe client: {error}"))
        })?;
    let mcp_url = format!("http://127.0.0.1:{}/mcp", ports.mcp);
    let gateway_url = format!("http://127.0.0.1:{}/health", ports.gateway);
    let mut last_error = String::new();
    for _ in 0..10 {
        match verify_mcp_initialize(&client, &mcp_url).await {
            Ok(()) => match daemon_health_source(&client, &gateway_url).await {
                Ok(source_commit) => return Ok(source_commit),
                Err(error) => last_error = error.to_string(),
            },
            Err(error) => last_error = error.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(ReleaseActivationError::Refused(format!(
        "post-restart semantic/provenance proof failed: {last_error}"
    )))
}

async fn verify_mcp_initialize(client: &reqwest::Client, url: &str) -> Result<()> {
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "forgefleet-release-activation", "version": "1"}
            }
        }))
        .send()
        .await
        .map_err(|error| {
            ReleaseActivationError::Refused(format!("MCP initialize transport: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(ReleaseActivationError::Refused(format!(
            "MCP initialize HTTP status {}",
            response.status()
        )));
    }
    let body = response.text().await.map_err(|error| {
        ReleaseActivationError::Refused(format!("MCP initialize body: {error}"))
    })?;
    verify_mcp_initialize_body(&body)
}

fn verify_mcp_initialize_body(body: &str) -> Result<()> {
    let value = if let Ok(value) = serde_json::from_str::<Value>(body) {
        value
    } else {
        let data = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or_else(|| {
                ReleaseActivationError::Refused("MCP initialize response is not JSON/SSE".into())
            })?;
        serde_json::from_str(data)?
    };
    if value
        .pointer("/result/serverInfo/name")
        .and_then(Value::as_str)
        != Some("forgefleet-mcp")
        || value
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .is_none()
        || value.pointer("/result/capabilities/tools").is_none()
        || value.get("error").is_some()
    {
        return Err(ReleaseActivationError::Refused(
            "MCP initialize response lacks exact ForgeFleet server semantics".into(),
        ));
    }
    Ok(())
}

async fn daemon_health_source(client: &reqwest::Client, url: &str) -> Result<PriorReleaseIdentity> {
    let response = client.get(url).send().await.map_err(|error| {
        ReleaseActivationError::Refused(format!("daemon health transport: {error}"))
    })?;
    if !response.status().is_success() {
        return Err(ReleaseActivationError::Refused(format!(
            "daemon health HTTP status {}",
            response.status()
        )));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|error| ReleaseActivationError::Refused(format!("daemon health JSON: {error}")))?;
    daemon_health_source_value(&value)
}

#[cfg(test)]
fn verify_daemon_health_value(value: &Value, source_commit: &str) -> Result<()> {
    let reported = daemon_health_source_value(value)?;
    if reported
        != (PriorReleaseIdentity::FullSha {
            sha: source_commit.to_string(),
        })
    {
        return Err(ReleaseActivationError::Refused(
            "daemon health did not prove exact full source provenance".into(),
        ));
    }
    Ok(())
}

fn daemon_health_source_value(value: &Value) -> Result<PriorReleaseIdentity> {
    let reported = value.get("build_sha").and_then(Value::as_str).unwrap_or("");
    if value.get("status").and_then(Value::as_str) != Some("ok")
        || value.get("service").and_then(Value::as_str) != Some("ff-gateway")
    {
        return Err(ReleaseActivationError::Refused(
            "daemon health did not prove canonical release provenance".into(),
        ));
    }
    parse_canonical_reported_identity(reported)
}

fn validate_owned_regular_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(ReleaseActivationError::Refused(format!(
            "required authority file is not a single-link effective-user regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn file_exists_at_path(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ReleaseActivationError::Refused(format!(
                    "fixed install path is a symlink: {}",
                    path.display()
                )));
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn exists_at(dir: RawFd, name: &OsStr) -> Result<bool> {
    let name = cstring(name)?;
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let result = unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
    if result == 0 {
        if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(ReleaseActivationError::Refused(
                "transaction pathname was replaced by a symlink".into(),
            ));
        }
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(false)
    } else {
        Err(error.into())
    }
}

fn ensure_absent_at(dir: RawFd, name: &OsStr) -> Result<()> {
    if exists_at(dir, name)? {
        return Err(ReleaseActivationError::Refused(format!(
            "exclusive transaction pathname already exists: {}",
            name.to_string_lossy()
        )));
    }
    Ok(())
}

fn unlink_at(dir: RawFd, name: &OsStr) {
    if let Ok(name) = cstring(name) {
        unsafe {
            libc::unlinkat(dir, name.as_ptr(), 0);
        }
    }
}

fn unlink_at_checked(dir: RawFd, name: &OsStr) -> Result<()> {
    let name = cstring(name)?;
    if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_noreplace(old_dir: RawFd, old: &OsStr, new_dir: RawFd, new: &OsStr) -> Result<()> {
    let old = cstring(old)?;
    let new = cstring(new)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            old_dir,
            old.as_ptr(),
            new_dir,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_noreplace(old_dir: RawFd, old: &OsStr, new_dir: RawFd, new: &OsStr) -> Result<()> {
    let old = cstring(old)?;
    let new = cstring(new)?;
    let result = unsafe {
        libc::renameatx_np(
            old_dir,
            old.as_ptr(),
            new_dir,
            new.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace(_old_dir: RawFd, _old: &OsStr, _new_dir: RawFd, _new: &OsStr) -> Result<()> {
    Err(ReleaseActivationError::Refused(
        "exclusive rename is unsupported on this platform".into(),
    ))
}

fn rename_path_noreplace(old: &Path, new: &Path) -> Result<()> {
    let old_parent =
        open_owned_dir(old.parent().ok_or_else(|| {
            ReleaseActivationError::Refused("rename source has no parent".into())
        })?)?;
    let new_parent = open_owned_dir(new.parent().ok_or_else(|| {
        ReleaseActivationError::Refused("rename destination has no parent".into())
    })?)?;
    rename_noreplace(
        old_parent.as_raw_fd(),
        old.file_name().expect("source filename"),
        new_parent.as_raw_fd(),
        new.file_name().expect("destination filename"),
    )?;
    fsync_fd(new_parent.as_raw_fd())
}

fn fsync_fd(fd: RawFd) -> Result<()> {
    if unsafe { libc::fsync(fd) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| ReleaseActivationError::Refused("durable path has no parent".into()))?;
    let dir = open_owned_dir(parent)?;
    fsync_fd(dir.as_raw_fd())
}

fn dup_fd(fd: RawFd) -> Result<RawFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(duplicate)
}

fn cstring(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| ReleaseActivationError::Refused("path contains an interior NUL byte".into()))
}

fn os_from_cstr(value: &CStr) -> &OsStr {
    OsStr::from_bytes(value.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use std::sync::Mutex;

    const SOURCE: &str = "6dc4086b7217cb8c2ccc1945b1e1f3213b9b1941";
    const PRIOR_SOURCE: &str = "5c1b63fb7217cb8c2ccc1945b1e1f3213b9b1941";
    const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn private_dir(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn row(name: &str, version: &str, target: &str, bytes: &[u8]) -> ReleaseArtifactRow {
        ReleaseArtifactRow {
            id: Uuid::new_v4(),
            artifact_name: name.into(),
            artifact_version: version.into(),
            source_commit: SOURCE.into(),
            target_triple: target.into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            size_bytes: bytes.len() as i64,
            created_at: Utc::now(),
        }
    }

    fn custody(
        artifact_id: Uuid,
        computer_id: Uuid,
        holder: &str,
        relative_path: &str,
        first_verified_at: DateTime<Utc>,
    ) -> ReleaseArtifactCustodyRow {
        ReleaseArtifactCustodyRow {
            artifact_id,
            computer_id,
            holder_name_at_registration: holder.into(),
            relative_path: relative_path.into(),
            first_verified_at,
            last_verified_at: first_verified_at,
        }
    }

    #[derive(Debug)]
    struct FakeRunner {
        home: PathBuf,
        source: String,
        prior_source: String,
        daemon_prior_source: String,
        mcp_port: u16,
        fail_contains: Vec<String>,
        absent_labels: Vec<String>,
        bootstrapped_labels: Mutex<std::collections::BTreeSet<String>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn new(home: PathBuf) -> Self {
            Self {
                home,
                source: SOURCE.into(),
                prior_source: SOURCE.into(),
                daemon_prior_source: SOURCE.into(),
                mcp_port: 51111,
                fail_contains: Vec::new(),
                absent_labels: Vec::new(),
                bootstrapped_labels: Mutex::new(std::collections::BTreeSet::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing(home: PathBuf, needle: &str) -> Self {
            let mut runner = Self::new(home);
            runner.fail_contains.push(needle.into());
            runner
        }

        fn failing_many(home: PathBuf, needles: &[&str]) -> Self {
            let mut runner = Self::new(home);
            runner.fail_contains = needles.iter().map(|needle| (*needle).into()).collect();
            runner
        }

        fn with_absent_labels_and_failures(
            home: PathBuf,
            absent_labels: &[&str],
            fail_contains: &[&str],
        ) -> Self {
            let mut runner = Self::failing_many(home, fail_contains);
            runner.absent_labels = absent_labels
                .iter()
                .map(|label| (*label).to_string())
                .collect();
            runner
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn with_prior_source(mut self, source: &str) -> Self {
            self.prior_source = source.into();
            self.daemon_prior_source = source.into();
            self
        }

        fn with_mismatched_prior_sources(mut self, ff: &str, daemon: &str) -> Self {
            self.prior_source = ff.into();
            self.daemon_prior_source = daemon.into();
            self
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<CommandResult> {
            let call = format!("{} {}", program, args.join(" "));
            self.calls.lock().unwrap().push(call.clone());
            if call.contains("launchctl bootstrap") {
                let mut bootstrapped = self.bootstrapped_labels.lock().unwrap();
                for label in &self.absent_labels {
                    if call.contains(label) {
                        bootstrapped.insert(label.clone());
                    }
                }
            }
            if call.contains("launchctl print")
                && self.absent_labels.iter().any(|label| {
                    call.contains(label)
                        && !self.bootstrapped_labels.lock().unwrap().contains(label)
                })
            {
                return Ok(CommandResult {
                    success: false,
                    stdout: String::new(),
                    stderr: "Could not find service in domain".into(),
                });
            }
            if self
                .fail_contains
                .iter()
                .any(|needle| call.contains(needle))
            {
                return Ok(CommandResult {
                    success: false,
                    stdout: String::new(),
                    stderr: "injected command failure".into(),
                });
            }
            let binary = self.home.join(".local/bin/forgefleetd");
            let stdout = if call.contains("ExecStart") && call.contains(MCP_UNIT) {
                format!(
                    "{} mcp --listen 0.0.0.0:{}",
                    binary.display(),
                    self.mcp_port
                )
            } else if call.contains("ExecStart") && call.contains(DAEMON_UNIT) {
                format!("{} start", binary.display())
            } else if call.contains("launchctl print") && call.contains(MCP_LABEL) {
                format!(
                    "program = {}\narguments = mcp --listen 0.0.0.0:{}",
                    binary.display(),
                    self.mcp_port
                )
            } else if call.contains("launchctl print") && call.contains(DAEMON_LABEL) {
                format!("program = {}\narguments = start", binary.display())
            } else if args == ["--version"] {
                let prior = fs::read(program)
                    .map(|bytes| bytes.starts_with(b"old-"))
                    .unwrap_or(false);
                format!(
                    "release pushed {}",
                    if prior {
                        if program.ends_with("forgefleetd") {
                            &self.daemon_prior_source
                        } else {
                            &self.prior_source
                        }
                    } else {
                        &self.source
                    }
                )
            } else {
                String::new()
            };
            Ok(CommandResult {
                success: true,
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn exact_commit_and_platform_qualified_identity_are_fail_closed() {
        assert!(validate_full_source_commit(SOURCE).is_ok());
        assert!(validate_full_source_commit(&SOURCE.to_ascii_uppercase()).is_err());
        assert!(validate_full_source_commit(&SOURCE[..10]).is_err());

        let lily = derive_platform_identity(SOURCE, "linux", "x86_64", "gnu", "24").unwrap();
        let logan = derive_platform_identity(SOURCE, "linux", "x86_64", "gnu", "26").unwrap();
        assert_eq!(lily.target_triple, logan.target_triple);
        assert_eq!(
            lily.artifact_version,
            format!("recovery.{SOURCE}.ubuntu24-x86_64")
        );
        assert_eq!(
            logan.artifact_version,
            format!("recovery.{SOURCE}.ubuntu26-x86_64")
        );
        assert_ne!(lily.artifact_version, logan.artifact_version);
        assert!(derive_platform_identity(SOURCE, "linux", "x86_64", "gnu", "25").is_err());
        assert!(derive_platform_identity(SOURCE, "linux", "x86_64", "musl", "24").is_err());
    }

    #[test]
    fn canonical_peer_status_accepts_live_projections_only() {
        assert!(canonical_live_status("active"));
        assert!(canonical_live_status("online"));
        for status in ["", "ready", "draining", "offline", "failed", "ONLINE"] {
            assert!(
                !canonical_live_status(status),
                "unexpected live status: {status}"
            );
        }
    }

    #[test]
    fn canonical_ubuntu_release_requires_id_and_supported_version() {
        assert_eq!(
            parse_ubuntu_release("ID=ubuntu\nVERSION_ID=\"24.04\"\n").unwrap(),
            "24"
        );
        assert_eq!(
            parse_ubuntu_release("ID=ubuntu\nVERSION_ID=\"26.04\"\n").unwrap(),
            "26"
        );
        assert!(parse_ubuntu_release("ID=debian\nVERSION_ID=\"24.04\"\n").is_err());
        assert!(parse_ubuntu_release("ID=ubuntu\nVERSION_ID=\"25.10\"\n").is_err());
        assert!(
            Path::new("/etc/os-release").is_symlink() || Path::new("/etc/os-release").is_file()
        );
        if cfg!(target_os = "linux") {
            assert!(trusted_os_release().is_ok());
        }
    }

    #[test]
    fn configured_ports_accept_nondefault_and_bare_endpoint_but_reject_conflict() {
        let value: toml::Value = toml::from_str(
            "[general]\napi_port=52000\n[ports]\nforgefleet=51111\n[mcp.forgefleet]\nport=51111\nendpoint='127.0.0.1:51111/mcp'\n",
        )
        .unwrap();
        let mut mcp = Vec::new();
        let mut gateway = Vec::new();
        collect_config_ports(&value, &mut mcp, &mut gateway).unwrap();
        assert_eq!(reconcile_port_sources("MCP", &mcp).unwrap(), 51111);
        assert_eq!(reconcile_port_sources("gateway", &gateway).unwrap(), 52002);

        let conflict: toml::Value =
            toml::from_str("[ports]\nforgefleet=51111\n[mcp.forgefleet]\nport=51112\n").unwrap();
        let mut mcp = Vec::new();
        let mut gateway = Vec::new();
        collect_config_ports(&conflict, &mut mcp, &mut gateway).unwrap();
        assert!(reconcile_port_sources("MCP", &mcp).is_err());
        assert!(parse_port("051111", "test").is_err());
        assert!(parse_port("0", "test").is_err());

        let default_port: toml::Value =
            toml::from_str("[mcp.forgefleet]\nendpoint='http://127.0.0.1/mcp'\n").unwrap();
        let mut mcp = Vec::new();
        let mut gateway = Vec::new();
        collect_config_ports(&default_port, &mut mcp, &mut gateway).unwrap();
        assert_eq!(reconcile_port_sources("MCP", &mcp).unwrap(), 80);
        let malformed: toml::Value =
            toml::from_str("[mcp.forgefleet]\nendpoint='http://127.0.0.1:51111/not-mcp?x=1'\n")
                .unwrap();
        assert!(collect_config_ports(&malformed, &mut Vec::new(), &mut Vec::new()).is_err());
    }

    #[test]
    fn custody_origin_remains_first_verifier_as_recipients_accumulate() {
        let artifact = Uuid::new_v4();
        let origin_id = Uuid::new_v4();
        let at = Utc::now();
        assert!(select_canonical_origin(&[], "ff").is_err());
        let one = vec![custody(artifact, origin_id, "lily", "origin/ff", at)];
        assert_eq!(
            select_canonical_origin(&one, "ff").unwrap().computer_id,
            origin_id
        );
        let many = vec![
            one[0].clone(),
            custody(
                artifact,
                Uuid::new_v4(),
                "sarah",
                "copy/ff",
                at + TimeDelta::seconds(1),
            ),
            custody(
                artifact,
                Uuid::new_v4(),
                "james",
                "copy2/ff",
                at + TimeDelta::seconds(2),
            ),
        ];
        assert_eq!(
            select_canonical_origin(&many, "ff").unwrap().computer_id,
            origin_id
        );
        let tied = vec![
            one[0].clone(),
            custody(artifact, Uuid::new_v4(), "logan", "other/ff", at),
        ];
        assert!(select_canonical_origin(&tied, "ff").is_err());
    }

    #[test]
    fn pair_rejects_split_origin_target_mismatch_and_vinny() {
        let version = format!("recovery.{SOURCE}.ubuntu24-x86_64");
        let target = "x86_64-unknown-linux-gnu";
        let identity = LocalComputerIdentity {
            id: Uuid::new_v4(),
            name: "lily".into(),
        };
        let origin = Uuid::new_v4();
        let at = Utc::now();
        let rows = [
            row("ff", &version, target, b"abc"),
            row("forgefleetd", &version, target, b"def"),
        ];
        let mut pair: Vec<_> = rows
            .iter()
            .map(|row| ResolvedArtifact {
                row: row.clone(),
                custody: custody(
                    row.id,
                    identity.id,
                    "lily",
                    &format!("x/{}", row.artifact_name),
                    at,
                ),
                origin_computer_id: origin,
                origin_holder: "builder".into(),
            })
            .collect();
        let request = CanonicalReleaseIdentity {
            artifact_version: version.clone(),
            source_commit: SOURCE.into(),
        };
        assert!(validate_pair(&pair, &identity, &request, target).is_ok());
        pair[1].origin_computer_id = Uuid::new_v4();
        assert!(validate_pair(&pair, &identity, &request, target).is_err());
        pair[1].origin_computer_id = origin;
        pair[1].row.target_triple = "aarch64-unknown-linux-gnu".into();
        assert!(validate_pair(&pair, &identity, &request, target).is_err());
        let vinny = LocalComputerIdentity {
            id: identity.id,
            name: "Vinny".into(),
        };
        assert!(validate_pair(&pair, &vinny, &request, target).is_err());
    }

    #[test]
    fn descriptor_open_rejects_symlink_and_hardlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("release");
        private_dir(&root);
        fs::write(root.join("real"), b"abc").unwrap();
        symlink("real", root.join("link")).unwrap();
        fs::hard_link(root.join("real"), root.join("hard")).unwrap();
        assert!(open_artifact_beneath(&root, Path::new("link")).is_err());
        assert!(open_artifact_beneath(&root, Path::new("real")).is_err());
        assert!(open_artifact_beneath(&root, Path::new("hard")).is_err());
    }

    #[test]
    fn exclusive_acquisition_fd_supports_held_fd_readback() {
        let temp = tempfile::tempdir().unwrap();
        private_dir(temp.path());
        let dir = open_owned_dir(temp.path()).unwrap();
        let fd = open_new_file_at(dir.as_raw_fd(), OsStr::new("artifact.tmp"), 0o600).unwrap();
        let mut writer = File::from(fd);
        writer.write_all(b"abc").unwrap();
        writer.sync_all().unwrap();
        let mut reader = unsafe { File::from_raw_fd(dup_fd(writer.as_raw_fd()).unwrap()) };
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abc");
        assert!(validate_ssh_authority("lily", "192.168.5.110").is_ok());
        assert!(validate_ssh_authority("lily", "::1").is_err());
    }

    #[test]
    fn promoted_acquisition_is_adopted_on_registration_retry_and_drift_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        private_dir(temp.path());
        let dir = open_owned_dir(temp.path()).unwrap();
        let artifact = row("ff", "v", "x", b"abc");
        assert!(
            !adopt_existing_acquired_artifact(dir.as_raw_fd(), OsStr::new("ff"), &artifact)
                .unwrap()
        );

        let temp_name = OsStr::new(".ff.acquire.tmp");
        let fd = open_new_file_at(dir.as_raw_fd(), temp_name, 0o600).unwrap();
        let mut file = File::from(fd);
        file.write_all(b"abc").unwrap();
        let result = finalize_acquired_temp(
            dir.as_raw_fd(),
            temp_name,
            OsStr::new("ff"),
            &file,
            &artifact,
        );
        cleanup_failed_acquisition(dir.as_raw_fd(), temp_name, result).unwrap();
        assert!(
            adopt_existing_acquired_artifact(dir.as_raw_fd(), OsStr::new("ff"), &artifact).unwrap(),
            "post-promote/pre-register retry must adopt exact immutable bytes"
        );
        assert!(
            adopt_existing_acquired_artifact(dir.as_raw_fd(), OsStr::new("ff"), &artifact).unwrap(),
            "repeated DB-registration retries remain idempotent"
        );

        fs::write(temp.path().join("ff"), b"abd").unwrap();
        assert!(
            adopt_existing_acquired_artifact(dir.as_raw_fd(), OsStr::new("ff"), &artifact).is_err(),
            "a pre-existing acquisition with drift must never be adopted"
        );
    }

    #[test]
    fn failed_acquisition_verification_unlinks_and_syncs_the_temp_path() {
        let temp = tempfile::tempdir().unwrap();
        private_dir(temp.path());
        let dir = open_owned_dir(temp.path()).unwrap();
        let artifact = row("ff", "v", "x", b"abc");
        let temp_name = OsStr::new(".ff.acquire-bad.tmp");
        let fd = open_new_file_at(dir.as_raw_fd(), temp_name, 0o600).unwrap();
        let mut file = File::from(fd);
        file.write_all(b"abd").unwrap();
        let result = finalize_acquired_temp(
            dir.as_raw_fd(),
            temp_name,
            OsStr::new("ff"),
            &file,
            &artifact,
        );
        assert!(cleanup_failed_acquisition(dir.as_raw_fd(), temp_name, result).is_err());
        assert!(!exists_at(dir.as_raw_fd(), temp_name).unwrap());
        assert!(!exists_at(dir.as_raw_fd(), OsStr::new("ff")).unwrap());
    }

    #[test]
    fn held_fd_stage_detects_path_swap_digest_and_size() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("release");
        let destination = temp.path().join("bin");
        private_dir(&root);
        private_dir(&destination);
        fs::write(root.join("ff"), b"abc").unwrap();
        let mut opened = open_artifact_beneath(&root, Path::new("ff")).unwrap();
        fs::rename(root.join("ff"), root.join("moved")).unwrap();
        fs::write(root.join("ff"), b"evil").unwrap();
        let valid = row("ff", "v", "x", b"abc");
        let previous = (3, format!("{:x}", Sha256::digest(b"old")));
        let staged = stage_install_entry(
            Uuid::new_v4(),
            "ff",
            &valid,
            &mut opened,
            open_owned_dir(&destination).unwrap(),
            destination.clone(),
            "ff",
            &previous,
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join(os_from_cstr(&staged.stage))).unwrap(),
            b"abc",
            "path replacement cannot redirect a held custody FD"
        );

        fs::write(root.join("clean"), b"abc").unwrap();
        let mut clean = open_artifact_beneath(&root, Path::new("clean")).unwrap();
        let bad_digest = row("ff", "v", "x", b"def");
        assert!(
            stage_install_entry(
                Uuid::new_v4(),
                "ff",
                &bad_digest,
                &mut clean,
                open_owned_dir(&destination).unwrap(),
                destination.clone(),
                "ff2",
                &previous,
            )
            .is_err()
        );
        let mut clean = open_artifact_beneath(&root, Path::new("clean")).unwrap();
        let mut bad_size = valid.clone();
        bad_size.size_bytes = 2;
        assert!(
            stage_install_entry(
                Uuid::new_v4(),
                "ff",
                &bad_size,
                &mut clean,
                open_owned_dir(&destination).unwrap(),
                destination,
                "ff3",
                &previous,
            )
            .is_err()
        );
    }

    #[test]
    fn service_topology_uses_exact_units_labels_and_never_signs() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        private_dir(&home.join(".local/bin"));
        private_dir(&home.join("Library/LaunchAgents"));
        for label in [MCP_LABEL, DAEMON_LABEL] {
            fs::write(
                home.join("Library/LaunchAgents")
                    .join(format!("{label}.plist")),
                b"plist",
            )
            .unwrap();
        }
        let ports = ServicePorts {
            mcp: 51111,
            gateway: 52002,
        };
        let linux = FakeRunner::new(home.clone());
        preflight_service_topology(ServicePlatform::Linux, ports, &home, &linux).unwrap();
        let mac = FakeRunner::new(home.clone());
        preflight_service_topology(ServicePlatform::Macos, ports, &home, &mac).unwrap();
        assert!(linux.calls().iter().any(|call| call.contains(MCP_UNIT)));
        assert!(linux.calls().iter().any(|call| call.contains(DAEMON_UNIT)));
        assert!(mac.calls().iter().any(|call| call.contains(MCP_LABEL)));
        assert!(mac.calls().iter().any(|call| call.contains(DAEMON_LABEL)));

        let dir = open_owned_dir(&home.join(".local/bin")).unwrap();
        let entry = InstallEntry {
            artifact_name: "ff".into(),
            expected_sha256: ABC_SHA.into(),
            expected_size: 3,
            previous_sha256: ABC_SHA.into(),
            previous_size: 3,
            dir,
            dir_path: home.join(".local/bin"),
            destination: CString::new("ff").unwrap(),
            stage: CString::new(".ff.stage").unwrap(),
            backup: CString::new(".ff.backup").unwrap(),
        };
        verify_codesign(&entry, true, &mac).unwrap();
        let calls = mac.calls();
        assert!(
            calls
                .iter()
                .any(|call| call.contains("codesign --verify --strict --verbose=2"))
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.contains("--force") || call.contains("--sign"))
        );
    }

    #[test]
    fn mac_quiesce_is_idempotent_only_after_exact_absence_proof() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let already_stopped = FakeRunner::with_absent_labels_and_failures(
            home.clone(),
            &[MCP_LABEL, DAEMON_LABEL],
            &["launchctl bootout"],
        );
        stop_services_best_effort(ServicePlatform::Macos, &home, &already_stopped).unwrap();

        let partially_stopped =
            FakeRunner::with_absent_labels_and_failures(home.clone(), &[MCP_LABEL], &[MCP_LABEL]);
        stop_services_best_effort(ServicePlatform::Macos, &home, &partially_stopped).unwrap();

        let still_loaded = FakeRunner::failing(home, "launchctl bootout");
        assert!(
            stop_services_best_effort(ServicePlatform::Macos, temp.path(), &still_loaded).is_err(),
            "a failed bootout plus a successful print means the label is still loaded"
        );
    }

    #[test]
    fn semantic_health_rejects_stale_or_non_forgefleet_responses() {
        let mcp = json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"forgefleet-mcp","version":"1"}}});
        assert!(verify_mcp_initialize_body(&mcp.to_string()).is_ok());
        let wrong = json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"other"}}});
        assert!(verify_mcp_initialize_body(&wrong.to_string()).is_err());
        let health = json!({"status":"ok","service":"ff-gateway","build_sha":SOURCE});
        assert!(verify_daemon_health_value(&health, SOURCE).is_ok());
        assert!(
            verify_daemon_health_value(&health, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .is_err()
        );
        assert!(
            verify_daemon_health_value(
                &json!({"status":"ok","service":"ff-gateway","build_sha":&SOURCE[..10]}),
                SOURCE
            )
            .is_err()
        );
    }

    #[test]
    fn restart_failures_are_not_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::failing(temp.path().to_path_buf(), "start forgefleetd.service");
        assert!(start_services(ServicePlatform::Linux, temp.path(), &runner).is_err());
    }

    #[test]
    fn partial_stop_and_restoration_failure_stays_nonterminal() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let activation_dir = home.join(".forgefleet/release-activations");
        ensure_activation_directory(&activation_dir).unwrap();
        let id = Uuid::new_v4();
        let entries = transaction_entries(&home, id);
        let journal = activation_dir.join(format!("{id}.journal.jsonl"));
        create_journal(&journal, "prepared", "test").unwrap();
        let operation_lock = acquire_operation_lock(&activation_dir).unwrap();
        let transaction = ActiveTransaction {
            id,
            version: "v".into(),
            source_commit: SOURCE.into(),
            prior_release_identity: PriorReleaseIdentity::FullSha { sha: SOURCE.into() },
            target_triple: "x".into(),
            identity: LocalComputerIdentity {
                id: Uuid::new_v4(),
                name: "lily".into(),
            },
            platform: ServicePlatform::Linux,
            ports: ServicePorts {
                mcp: 51111,
                gateway: 52002,
            },
            home: home.clone(),
            activation_dir,
            journal_path: journal.clone(),
            entries,
            _operation_lock: operation_lock,
        };
        let runner = FakeRunner::failing(home.clone(), "start forgefleetd.service");
        let error = handle_stop_failure(
            &transaction,
            &runner,
            ReleaseActivationError::Refused("injected partial stop".into()),
        );
        assert!(
            error
                .to_string()
                .contains("mandatory service restoration failed")
        );
        assert_eq!(
            last_journal_state(&journal).unwrap().as_deref(),
            Some("rollback_failed")
        );
        assert!(
            !transaction.entries.iter().any(|entry| exists_at(
                entry.dir.as_raw_fd(),
                os_from_cstr(&entry.stage)
            )
            .unwrap())
        );

        let stop_runner = FakeRunner::failing_many(
            home,
            &["stop forgefleetd.service", "start forgefleet-mcp.service"],
        );
        let error = stop_services(ServicePlatform::Linux, temp.path(), &stop_runner).unwrap_err();
        assert!(error.to_string().contains("failed to restore MCP"));
    }

    fn transaction_entries(home: &Path, id: Uuid) -> Vec<InstallEntry> {
        let bin = home.join(".local/bin");
        let cargo_bin = home.join(".cargo/bin");
        private_dir(&bin);
        private_dir(&cargo_bin);
        for (dir, name, old, new) in [
            (&bin, "ff", b"old-ff".as_slice(), b"new-ff".as_slice()),
            (
                &bin,
                "forgefleetd",
                b"old-daemon".as_slice(),
                b"new-daemon".as_slice(),
            ),
            (&cargo_bin, "ff", b"old-ff".as_slice(), b"new-ff".as_slice()),
        ] {
            write_executable(&dir.join(name), old);
            write_executable(&dir.join(format!(".{name}.release-{id}.stage")), new);
        }
        [(&bin, "ff"), (&bin, "forgefleetd"), (&cargo_bin, "ff")]
            .into_iter()
            .map(|(dir_path, name)| {
                let destination = hash_existing_destination(
                    open_owned_dir(dir_path).unwrap().as_raw_fd(),
                    OsStr::new(name),
                )
                .unwrap();
                let stage_name = format!(".{name}.release-{id}.stage");
                let candidate = hash_existing_destination(
                    open_owned_dir(dir_path).unwrap().as_raw_fd(),
                    OsStr::new(&stage_name),
                )
                .unwrap();
                InstallEntry {
                    artifact_name: name.into(),
                    expected_sha256: candidate.1,
                    expected_size: candidate.0,
                    previous_sha256: destination.1,
                    previous_size: destination.0,
                    dir: open_owned_dir(dir_path).unwrap(),
                    dir_path: dir_path.clone(),
                    destination: CString::new(name).unwrap(),
                    stage: CString::new(stage_name).unwrap(),
                    backup: CString::new(format!(".{name}.release-{id}.rollback")).unwrap(),
                }
            })
            .collect()
    }

    fn committed_activation_fixture(
        home: &Path,
        prior_release_identity: PriorReleaseIdentity,
    ) -> (Uuid, LocalComputerIdentity, ServicePlatform, ServicePorts) {
        let activation_dir = home.join(".forgefleet/release-activations");
        ensure_activation_directory(&activation_dir).unwrap();
        let id = Uuid::new_v4();
        let identity = LocalComputerIdentity {
            id: Uuid::new_v4(),
            name: "lily".into(),
        };
        let platform_identity = current_platform_identity(SOURCE).unwrap();
        let platform = platform_identity.service_platform;
        let ports = ServicePorts {
            mcp: 51111,
            gateway: 52002,
        };
        let entries = transaction_entries(home, id);
        let created_at = Utc::now();
        let manifest = RollbackManifest {
            transaction_id: id,
            artifact_version: platform_identity.artifact_version.clone(),
            source_commit: SOURCE.into(),
            prior_release_identity: prior_release_identity.clone(),
            target_triple: platform_identity.target_triple.clone(),
            computer_id: identity.id,
            computer_name: identity.name.clone(),
            platform,
            created_at,
            entries: entries
                .iter()
                .map(|entry| ManifestEntry {
                    artifact_name: entry.artifact_name.clone(),
                    destination: entry
                        .dir_path
                        .join(os_from_cstr(&entry.destination))
                        .display()
                        .to_string(),
                    stage: entry
                        .dir_path
                        .join(os_from_cstr(&entry.stage))
                        .display()
                        .to_string(),
                    backup: entry
                        .dir_path
                        .join(os_from_cstr(&entry.backup))
                        .display()
                        .to_string(),
                    sha256: entry.expected_sha256.clone(),
                    size_bytes: entry.expected_size,
                    previous_sha256: entry.previous_sha256.clone(),
                    previous_size_bytes: entry.previous_size,
                })
                .collect(),
        };
        write_manifest(&activation_dir, &manifest).unwrap();
        let journal_path = activation_dir.join(format!("{id}.journal.jsonl"));
        create_journal(&journal_path, "prepared", "fixture").unwrap();
        swap_entries(&entries, &journal_path, None).unwrap();
        append_journal(&journal_path, "services_started", "fixture").unwrap();
        let receipt_path = activation_dir.join(format!("{id}.receipt.json"));
        let services = match platform {
            ServicePlatform::Linux => (MCP_UNIT, DAEMON_UNIT),
            ServicePlatform::Macos => (MCP_LABEL, DAEMON_LABEL),
        };
        let receipt = ReleaseActivationReceipt {
            transaction_id: id,
            artifact_version: manifest.artifact_version.clone(),
            source_commit: manifest.source_commit.clone(),
            prior_release_identity,
            target_triple: manifest.target_triple.clone(),
            computer_id: identity.id,
            computer_name: identity.name.clone(),
            activated_at: created_at + TimeDelta::milliseconds(1),
            mcp_service: services.0.into(),
            daemon_service: services.1.into(),
            artifacts: build_artifact_receipts(&entries),
            receipt_path: receipt_path.display().to_string(),
        };
        write_new_json(&receipt_path, &receipt, 0o444).unwrap();
        append_journal(&journal_path, "committed", "fixture").unwrap();
        (id, identity, platform, ports)
    }

    fn rolled_back_activation_fixture(
        home: &Path,
    ) -> (Uuid, LocalComputerIdentity, ServicePlatform) {
        let prior = PriorReleaseIdentity::LegacyReported {
            short_sha: "5c1b63fb".into(),
        };
        let (id, identity, platform, _ports) = committed_activation_fixture(home, prior.clone());
        let activation_dir = home.join(".forgefleet/release-activations");
        let manifest: RollbackManifest = read_private_json(
            &activation_dir.join(format!("{id}.manifest.json")),
            "rollback manifest",
        )
        .unwrap();
        let entries = entries_from_manifest(&manifest, home).unwrap();
        for entry in &entries {
            unlink_at_checked(entry.dir.as_raw_fd(), os_from_cstr(&entry.destination)).unwrap();
            rename_noreplace(
                entry.dir.as_raw_fd(),
                os_from_cstr(&entry.backup),
                entry.dir.as_raw_fd(),
                os_from_cstr(&entry.destination),
            )
            .unwrap();
            fsync_fd(entry.dir.as_raw_fd()).unwrap();
        }
        fs::remove_file(activation_dir.join(format!("{id}.receipt.json"))).unwrap();
        append_journal(
            &activation_dir.join(format!("{id}.journal.jsonl")),
            "rolled_back",
            "fixture restored exact predecessor",
        )
        .unwrap();
        (id, identity, platform)
    }

    #[test]
    fn terminal_rollback_archive_is_auditable_idempotent_and_retry_safe() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let (id, identity, platform) = rolled_back_activation_fixture(&home);
        let activation_dir = home.join(".forgefleet/release-activations");
        let runner = FakeRunner::new(home.clone()).with_prior_source("5c1b63fb");
        let manifest_raw = read_private_authority_text(
            &activation_dir.join(format!("{id}.manifest.json")),
            4 * 1024 * 1024,
            "rollback manifest",
        )
        .unwrap();
        let manifest: RollbackManifest = serde_json::from_str(&manifest_raw).unwrap();

        archive_terminal_rollback_for_retry(
            &activation_dir,
            id,
            &identity,
            platform,
            &home,
            &runner,
        )
        .unwrap();
        let archive = terminal_rollback_archive_dir(&activation_dir, id, &manifest_raw);
        assert!(!activation_dir.join(format!("{id}.manifest.json")).exists());
        assert!(!activation_dir.join(format!("{id}.journal.jsonl")).exists());
        assert!(archive.join(format!("{id}.manifest.json")).exists());
        assert_eq!(
            last_journal_state(&archive.join(format!("{id}.journal.jsonl")))
                .unwrap()
                .as_deref(),
            Some("rolled_back")
        );

        archive_terminal_rollback_for_retry(
            &activation_dir,
            id,
            &identity,
            platform,
            &home,
            &runner,
        )
        .unwrap();
        write_manifest(&activation_dir, &manifest).unwrap();
        create_journal(
            &activation_dir.join(format!("{id}.journal.jsonl")),
            "prepared",
            "same transaction may retry after durable retirement",
        )
        .unwrap();
        assert!(
            !archive_terminal_rollback_for_retry(
                &activation_dir,
                id,
                &identity,
                platform,
                &home,
                &runner,
            )
            .unwrap(),
            "a nonterminal new attempt must not be mistaken for the retired rollback"
        );
    }

    #[test]
    fn terminal_rollback_archive_completes_a_crash_between_two_moves() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let (id, identity, platform) = rolled_back_activation_fixture(&home);
        let activation_dir = home.join(".forgefleet/release-activations");
        let retired = activation_dir.join("retired");
        ensure_private_directory(&retired).unwrap();
        let manifest_raw = read_private_authority_text(
            &activation_dir.join(format!("{id}.manifest.json")),
            4 * 1024 * 1024,
            "rollback manifest",
        )
        .unwrap();
        let archive = terminal_rollback_archive_dir(&activation_dir, id, &manifest_raw);
        ensure_private_directory(&archive).unwrap();
        rename_path_noreplace(
            &activation_dir.join(format!("{id}.manifest.json")),
            &archive.join(format!("{id}.manifest.json")),
        )
        .unwrap();

        archive_terminal_rollback_for_retry(
            &activation_dir,
            id,
            &identity,
            platform,
            &home,
            &FakeRunner::new(home.clone()).with_prior_source("5c1b63fb"),
        )
        .unwrap();
        assert!(archive.join(format!("{id}.journal.jsonl")).exists());
        assert!(!activation_dir.join(format!("{id}.journal.jsonl")).exists());
    }

    #[test]
    fn legacy_reported_identity_is_strict_unambiguous_and_pair_consistent() {
        assert_eq!(
            parse_reported_release_identity("ff 5c1b63fb").unwrap(),
            PriorReleaseIdentity::LegacyReported {
                short_sha: "5c1b63fb".into()
            }
        );
        assert_eq!(
            parse_reported_release_identity(&format!("ff {SOURCE}")).unwrap(),
            PriorReleaseIdentity::FullSha { sha: SOURCE.into() }
        );
        assert_eq!(
            daemon_health_source_value(
                &json!({"status":"ok","service":"ff-gateway","build_sha":"5c1b63fb"})
            )
            .unwrap(),
            PriorReleaseIdentity::LegacyReported {
                short_sha: "5c1b63fb".into()
            }
        );
        assert!(
            daemon_health_source_value(
                &json!({"status":"ok","service":"ff-gateway","build_sha":"build-5c1b63fb"})
            )
            .is_err()
        );
        for invalid in [
            "ff 5C1B63FB",
            "ff 5c1b63f",
            "ff 5c1b63fb 6dc4086b",
            "ff 5c1b63fb00f",
        ] {
            assert!(
                parse_reported_release_identity(invalid).is_err(),
                "{invalid}"
            );
        }

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let id = Uuid::new_v4();
        let entries = transaction_entries(home, id);
        drop(entries);
        let matching = FakeRunner::new(home.to_path_buf()).with_prior_source("5c1b63fb");
        assert_eq!(
            probe_installed_pair_identity(home, &matching).unwrap(),
            PriorReleaseIdentity::LegacyReported {
                short_sha: "5c1b63fb".into()
            }
        );
        let mismatch = FakeRunner::new(home.to_path_buf())
            .with_mismatched_prior_sources("5c1b63fb", "6dc4086b");
        assert!(probe_installed_pair_identity(home, &mismatch).is_err());
    }

    #[test]
    fn explicit_rollback_restores_legacy_pair_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let prior = PriorReleaseIdentity::LegacyReported {
            short_sha: "5c1b63fb".into(),
        };
        let (id, identity, platform, ports) = committed_activation_fixture(&home, prior.clone());
        let runner = FakeRunner::new(home.clone()).with_prior_source("5c1b63fb");
        let prepared = prepare_explicit_rollback(
            id,
            identity.clone(),
            platform,
            ports,
            home.clone(),
            &runner,
            None,
        )
        .unwrap();
        assert_eq!(prepared.receipt.restored_release_identity, prior);
        assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), b"old-ff");
        assert_eq!(
            fs::read(home.join(".local/bin/forgefleetd")).unwrap(),
            b"old-daemon"
        );
        let committed = commit_explicit_rollback(prepared, &runner).unwrap();
        assert_eq!(
            last_journal_state(
                &home
                    .join(".forgefleet/release-activations")
                    .join(format!("{id}.journal.jsonl"))
            )
            .unwrap()
            .as_deref(),
            Some("rollback_committed")
        );

        let retried =
            prepare_explicit_rollback(id, identity, platform, ports, home, &runner, None).unwrap();
        assert!(retried.receipt_already_committed);
        assert_eq!(retried.receipt, committed);
    }

    #[test]
    fn coordinator_inspection_proves_committed_predecessor_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let prior = PriorReleaseIdentity::LegacyReported {
            short_sha: "5c1b63fb".into(),
        };
        let (id, identity, platform, _ports) = committed_activation_fixture(&home, prior.clone());
        let before_ff = fs::read(home.join(".local/bin/ff")).unwrap();
        let before_daemon = fs::read(home.join(".local/bin/forgefleetd")).unwrap();
        let inspected = inspect_committed_activation(
            id,
            identity.clone(),
            platform,
            home.clone(),
            &FakeRunner::new(home.clone()),
            false,
        )
        .unwrap()
        .expect("committed activation must be adoptable");
        assert_eq!(inspected.receipt.transaction_id, id);
        assert_eq!(inspected.proof.prior_release_identity, prior);
        assert_eq!(inspected.proof.manifest_sha256.len(), 64);
        assert_eq!(inspected.proof.activation_receipt_sha256.len(), 64);
        drop(inspected);
        assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), before_ff);
        assert_eq!(
            fs::read(home.join(".local/bin/forgefleetd")).unwrap(),
            before_daemon
        );

        fs::write(
            home.join(".local/bin")
                .join(format!(".ff.release-{id}.rollback")),
            b"tampered",
        )
        .unwrap();
        assert!(
            inspect_committed_activation(
                id,
                identity,
                platform,
                home.clone(),
                &FakeRunner::new(home),
                false,
            )
            .is_err(),
            "rollback proof must fail on retained predecessor tamper"
        );
    }

    #[test]
    fn explicit_rollback_restores_full_sha_predecessor() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let (id, identity, platform, ports) = committed_activation_fixture(
            &home,
            PriorReleaseIdentity::FullSha {
                sha: PRIOR_SOURCE.into(),
            },
        );
        let runner = FakeRunner::new(home.clone()).with_prior_source(PRIOR_SOURCE);
        let prepared =
            prepare_explicit_rollback(id, identity, platform, ports, home, &runner, None).unwrap();
        assert_eq!(
            prepared.receipt.restored_release_identity,
            PriorReleaseIdentity::FullSha {
                sha: PRIOR_SOURCE.into()
            }
        );
        commit_explicit_rollback(prepared, &runner).unwrap();
    }

    #[test]
    fn explicit_rollback_resumes_after_each_durable_pair_transition() {
        for transition in 1..=6 {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().to_path_buf();
            let (id, identity, platform, ports) = committed_activation_fixture(
                &home,
                PriorReleaseIdentity::LegacyReported {
                    short_sha: "5c1b63fb".into(),
                },
            );
            let runner = FakeRunner::new(home.clone()).with_prior_source("5c1b63fb");
            assert!(
                prepare_explicit_rollback(
                    id,
                    identity.clone(),
                    platform,
                    ports,
                    home.clone(),
                    &runner,
                    Some(transition),
                )
                .is_err()
            );
            let prepared = prepare_explicit_rollback(
                id,
                identity,
                platform,
                ports,
                home.clone(),
                &runner,
                None,
            )
            .unwrap();
            commit_explicit_rollback(prepared, &runner).unwrap();
            assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), b"old-ff");
            assert_eq!(fs::read(home.join(".cargo/bin/ff")).unwrap(), b"old-ff");
        }
    }

    #[test]
    fn explicit_rollback_rejects_tamper_missing_partial_and_stale_authority() {
        use std::os::unix::fs::symlink;

        for mutation in [
            "digest",
            "missing",
            "partial",
            "symlink",
            "hardlink",
            "receipt_symlink",
            "journal_hardlink",
            "manifest_hardlink",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().to_path_buf();
            let (id, identity, platform, ports) = committed_activation_fixture(
                &home,
                PriorReleaseIdentity::FullSha {
                    sha: PRIOR_SOURCE.into(),
                },
            );
            let bin = home.join(".local/bin");
            let destination = bin.join("ff");
            let backup = bin.join(format!(".ff.release-{id}.rollback"));
            let stage = bin.join(format!(".ff.release-{id}.stage"));
            let activation_dir = home.join(".forgefleet/release-activations");
            match mutation {
                "digest" => fs::write(&backup, b"tampered").unwrap(),
                "missing" => fs::remove_file(&destination).unwrap(),
                "partial" => fs::write(&stage, b"unexpected").unwrap(),
                "symlink" => {
                    fs::remove_file(&backup).unwrap();
                    symlink("ff", &backup).unwrap();
                }
                "hardlink" => {
                    fs::remove_file(&backup).unwrap();
                    let other = bin.join("other-old-ff");
                    fs::write(&other, b"old-ff").unwrap();
                    fs::hard_link(&other, &backup).unwrap();
                }
                "receipt_symlink" => {
                    let receipt = activation_dir.join(format!("{id}.receipt.json"));
                    let original = activation_dir.join("saved-receipt");
                    fs::rename(&receipt, &original).unwrap();
                    symlink("saved-receipt", &receipt).unwrap();
                }
                "journal_hardlink" => {
                    let journal = activation_dir.join(format!("{id}.journal.jsonl"));
                    fs::hard_link(&journal, activation_dir.join("journal-hardlink")).unwrap();
                }
                "manifest_hardlink" => {
                    let manifest = activation_dir.join(format!("{id}.manifest.json"));
                    fs::hard_link(&manifest, activation_dir.join("manifest-hardlink")).unwrap();
                }
                _ => unreachable!(),
            }
            let runner = FakeRunner::new(home.clone());
            assert!(
                prepare_explicit_rollback(id, identity, platform, ports, home, &runner, None)
                    .is_err(),
                "mutation {mutation} must fail closed"
            );
            assert!(
                runner.calls().is_empty(),
                "mutation must fail before quiesce"
            );
        }

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let (id, identity, platform, ports) = committed_activation_fixture(
            &home,
            PriorReleaseIdentity::FullSha {
                sha: PRIOR_SOURCE.into(),
            },
        );
        let activation_dir = home.join(".forgefleet/release-activations");
        let receipt_path = activation_dir.join(format!("{id}.receipt.json"));
        let mut newer: ReleaseActivationReceipt =
            read_private_json(&receipt_path, "activation receipt").unwrap();
        newer.transaction_id = Uuid::new_v4();
        newer.activated_at += TimeDelta::seconds(1);
        let newer_path = activation_dir.join(format!("{}.receipt.json", newer.transaction_id));
        newer.receipt_path = newer_path.display().to_string();
        write_new_json(&newer_path, &newer, 0o444).unwrap();
        let runner = FakeRunner::new(home.clone());
        assert!(
            prepare_explicit_rollback(id, identity, platform, ports, home, &runner, None).is_err()
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn explicit_rollback_rejects_manifest_metadata_drift_and_other_unfinished_work() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let (id, identity, platform, ports) = committed_activation_fixture(
            &home,
            PriorReleaseIdentity::FullSha {
                sha: PRIOR_SOURCE.into(),
            },
        );
        let activation_dir = home.join(".forgefleet/release-activations");
        let manifest_path = activation_dir.join(format!("{id}.manifest.json"));
        let manifest: RollbackManifest =
            read_private_json(&manifest_path, "rollback manifest").unwrap();
        let mut wrong_name = serde_json::to_value(&manifest).unwrap();
        wrong_name["entries"][0]["artifact_name"] = json!("forgefleetd");
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&wrong_name).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o400)).unwrap();
        let runner = FakeRunner::new(home.clone());
        assert!(
            prepare_explicit_rollback(
                id,
                identity.clone(),
                platform,
                ports,
                home.clone(),
                &runner,
                None,
            )
            .is_err()
        );
        assert!(runner.calls().is_empty());

        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o400)).unwrap();
        let other = Uuid::new_v4();
        create_journal(
            &activation_dir.join(format!("{other}.journal.jsonl")),
            "prepared",
            "other unfinished fixture",
        )
        .unwrap();
        assert!(
            prepare_explicit_rollback(id, identity, platform, ports, home, &runner, None).is_err()
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn explicit_rollback_forbids_vinny_without_touching_authority() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let (id, mut identity, platform, ports) = committed_activation_fixture(
            &home,
            PriorReleaseIdentity::FullSha {
                sha: PRIOR_SOURCE.into(),
            },
        );
        identity.name = "Vinny".into();
        let runner = FakeRunner::new(home.clone());
        assert!(
            prepare_explicit_rollback(id, identity, platform, ports, home, &runner, None).is_err()
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn release_operation_lock_serializes_activation_and_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let (id, identity, platform, ports) = committed_activation_fixture(
            &home,
            PriorReleaseIdentity::FullSha {
                sha: PRIOR_SOURCE.into(),
            },
        );
        let activation_dir = home.join(".forgefleet/release-activations");
        let lock = acquire_operation_lock(&activation_dir).unwrap();
        let runner = FakeRunner::new(home.clone());
        assert!(
            prepare_explicit_rollback(id, identity, platform, ports, home, &runner, None).is_err()
        );
        assert!(runner.calls().is_empty());
        drop(lock);
    }

    #[test]
    fn partial_swap_performs_mandatory_pair_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let activation_dir = home.join(".forgefleet/release-activations");
        ensure_activation_directory(&activation_dir).unwrap();
        let id = Uuid::new_v4();
        let entries = transaction_entries(&home, id);
        let journal = activation_dir.join(format!("{id}.journal.jsonl"));
        create_journal(&journal, "services_stopped", "test").unwrap();
        assert!(swap_entries(&entries, &journal, Some(1)).is_err());
        let operation_lock = acquire_operation_lock(&activation_dir).unwrap();
        let mut transaction = ActiveTransaction {
            id,
            version: "v".into(),
            source_commit: SOURCE.into(),
            prior_release_identity: PriorReleaseIdentity::FullSha { sha: SOURCE.into() },
            target_triple: "x".into(),
            identity: LocalComputerIdentity {
                id: Uuid::new_v4(),
                name: "lily".into(),
            },
            platform: ServicePlatform::Linux,
            ports: ServicePorts {
                mcp: 51111,
                gateway: 52002,
            },
            home: home.clone(),
            activation_dir,
            journal_path: journal,
            entries,
            _operation_lock: operation_lock,
        };
        rollback_transaction_inner(&mut transaction, &FakeRunner::new(home.clone()), "injected")
            .unwrap();
        assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), b"old-ff");
        assert_eq!(
            fs::read(home.join(".local/bin/forgefleetd")).unwrap(),
            b"old-daemon"
        );
        assert_eq!(fs::read(home.join(".cargo/bin/ff")).unwrap(), b"old-ff");
        assert_eq!(
            last_journal_state(&transaction.journal_path)
                .unwrap()
                .as_deref(),
            Some("rolled_back")
        );
    }

    #[test]
    fn failed_quiesce_never_mutates_pending_swap_or_marks_rollback_complete() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let activation_dir = home.join(".forgefleet/release-activations");
        ensure_activation_directory(&activation_dir).unwrap();
        let id = Uuid::new_v4();
        let entries = transaction_entries(&home, id);
        let journal = activation_dir.join(format!("{id}.journal.jsonl"));
        create_journal(&journal, "services_stopped", "test").unwrap();
        assert!(swap_entries(&entries, &journal, Some(1)).is_err());
        assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), b"new-ff");
        assert!(!home.join(".local/bin/forgefleetd").exists());
        assert_eq!(
            fs::read(
                home.join(".local/bin")
                    .join(format!(".forgefleetd.release-{id}.rollback"))
            )
            .unwrap(),
            b"old-daemon"
        );

        let operation_lock = acquire_operation_lock(&activation_dir).unwrap();
        let mut transaction = ActiveTransaction {
            id,
            version: "v".into(),
            source_commit: SOURCE.into(),
            prior_release_identity: PriorReleaseIdentity::FullSha { sha: SOURCE.into() },
            target_triple: "x".into(),
            identity: LocalComputerIdentity {
                id: Uuid::new_v4(),
                name: "lily".into(),
            },
            platform: ServicePlatform::Linux,
            ports: ServicePorts {
                mcp: 51111,
                gateway: 52002,
            },
            home: home.clone(),
            activation_dir,
            journal_path: journal.clone(),
            entries,
            _operation_lock: operation_lock,
        };
        let runner = FakeRunner::failing(home.clone(), "stop forgefleetd.service");
        assert!(rollback_transaction_inner(&mut transaction, &runner, "test").is_err());
        assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), b"new-ff");
        assert!(!home.join(".local/bin/forgefleetd").exists());
        assert_eq!(
            last_journal_state(&journal).unwrap().as_deref(),
            Some("rollback_failed")
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.contains("start forgefleetd.service"))
        );
    }

    #[test]
    fn recovery_quiesce_failure_performs_no_swap_and_remains_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let activation_dir = home.join(".forgefleet/release-activations");
        ensure_activation_directory(&activation_dir).unwrap();
        let id = Uuid::new_v4();
        let entries = transaction_entries(&home, id);
        let manifest = RollbackManifest {
            transaction_id: id,
            artifact_version: "v".into(),
            source_commit: SOURCE.into(),
            prior_release_identity: PriorReleaseIdentity::FullSha { sha: SOURCE.into() },
            target_triple: "x".into(),
            computer_id: Uuid::new_v4(),
            computer_name: "lily".into(),
            platform: ServicePlatform::Linux,
            created_at: Utc::now(),
            entries: entries
                .iter()
                .map(|entry| ManifestEntry {
                    artifact_name: entry.artifact_name.clone(),
                    destination: entry
                        .dir_path
                        .join(os_from_cstr(&entry.destination))
                        .display()
                        .to_string(),
                    stage: entry
                        .dir_path
                        .join(os_from_cstr(&entry.stage))
                        .display()
                        .to_string(),
                    backup: entry
                        .dir_path
                        .join(os_from_cstr(&entry.backup))
                        .display()
                        .to_string(),
                    sha256: entry.expected_sha256.clone(),
                    size_bytes: entry.expected_size,
                    previous_sha256: entry.previous_sha256.clone(),
                    previous_size_bytes: entry.previous_size,
                })
                .collect(),
        };
        write_manifest(&activation_dir, &manifest).unwrap();
        let journal = activation_dir.join(format!("{id}.journal.jsonl"));
        create_journal(&journal, "services_stopped", "test").unwrap();
        assert!(swap_entries(&entries, &journal, Some(1)).is_err());
        let runner = FakeRunner::failing(home.clone(), "stop forgefleetd.service");
        assert!(
            recover_unfinished_transactions(
                &activation_dir,
                ServicePlatform::Linux,
                ServicePorts {
                    mcp: 51111,
                    gateway: 52002,
                },
                &home,
                &runner,
            )
            .is_err()
        );
        assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), b"new-ff");
        assert!(!home.join(".local/bin/forgefleetd").exists());
        assert_eq!(
            last_journal_state(&journal).unwrap().as_deref(),
            Some("recovery_failed")
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.contains("start forgefleetd.service"))
        );
    }

    #[test]
    fn mac_recovery_accepts_services_already_stopped_after_exact_absence_proof() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let activation_dir = home.join(".forgefleet/release-activations");
        ensure_activation_directory(&activation_dir).unwrap();
        let id = Uuid::new_v4();
        let entries = transaction_entries(&home, id);
        let manifest = RollbackManifest {
            transaction_id: id,
            artifact_version: "v".into(),
            source_commit: SOURCE.into(),
            prior_release_identity: PriorReleaseIdentity::FullSha { sha: SOURCE.into() },
            target_triple: "x".into(),
            computer_id: Uuid::new_v4(),
            computer_name: "ace".into(),
            platform: ServicePlatform::Macos,
            created_at: Utc::now(),
            entries: entries
                .iter()
                .map(|entry| ManifestEntry {
                    artifact_name: entry.artifact_name.clone(),
                    destination: entry
                        .dir_path
                        .join(os_from_cstr(&entry.destination))
                        .display()
                        .to_string(),
                    stage: entry
                        .dir_path
                        .join(os_from_cstr(&entry.stage))
                        .display()
                        .to_string(),
                    backup: entry
                        .dir_path
                        .join(os_from_cstr(&entry.backup))
                        .display()
                        .to_string(),
                    sha256: entry.expected_sha256.clone(),
                    size_bytes: entry.expected_size,
                    previous_sha256: entry.previous_sha256.clone(),
                    previous_size_bytes: entry.previous_size,
                })
                .collect(),
        };
        write_manifest(&activation_dir, &manifest).unwrap();
        let journal = activation_dir.join(format!("{id}.journal.jsonl"));
        create_journal(&journal, "services_stopped", "crash test").unwrap();
        let runner = FakeRunner::with_absent_labels_and_failures(
            home.clone(),
            &[MCP_LABEL, DAEMON_LABEL],
            &["launchctl bootout"],
        );
        recover_unfinished_transactions(
            &activation_dir,
            ServicePlatform::Macos,
            ServicePorts {
                mcp: 51111,
                gateway: 52002,
            },
            &home,
            &runner,
        )
        .unwrap();
        assert_eq!(
            last_journal_state(&journal).unwrap().as_deref(),
            Some("rolled_back")
        );
        assert!(
            runner
                .calls()
                .iter()
                .any(|call| call.contains("launchctl bootstrap"))
        );
    }

    #[test]
    fn unfinished_prepared_stopped_and_one_swap_journals_are_recovered() {
        for (state, one_swap, create_initial_journal) in [
            ("manifest_only", false, false),
            ("prepared", false, true),
            ("services_stopped", false, true),
            ("candidate_installed", true, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().to_path_buf();
            let activation_dir = home.join(".forgefleet/release-activations");
            ensure_activation_directory(&activation_dir).unwrap();
            let id = Uuid::new_v4();
            let entries = transaction_entries(&home, id);
            let manifest = RollbackManifest {
                transaction_id: id,
                artifact_version: "v".into(),
                source_commit: SOURCE.into(),
                prior_release_identity: PriorReleaseIdentity::FullSha { sha: SOURCE.into() },
                target_triple: "x".into(),
                computer_id: Uuid::new_v4(),
                computer_name: "lily".into(),
                platform: ServicePlatform::Linux,
                created_at: Utc::now(),
                entries: entries
                    .iter()
                    .map(|entry| ManifestEntry {
                        artifact_name: entry.artifact_name.clone(),
                        destination: entry
                            .dir_path
                            .join(os_from_cstr(&entry.destination))
                            .display()
                            .to_string(),
                        stage: entry
                            .dir_path
                            .join(os_from_cstr(&entry.stage))
                            .display()
                            .to_string(),
                        backup: entry
                            .dir_path
                            .join(os_from_cstr(&entry.backup))
                            .display()
                            .to_string(),
                        sha256: entry.expected_sha256.clone(),
                        size_bytes: entry.expected_size,
                        previous_sha256: entry.previous_sha256.clone(),
                        previous_size_bytes: entry.previous_size,
                    })
                    .collect(),
            };
            write_manifest(&activation_dir, &manifest).unwrap();
            let journal = activation_dir.join(format!("{id}.journal.jsonl"));
            if create_initial_journal {
                create_journal(&journal, state, "crash test").unwrap();
            }
            if one_swap {
                let first = &entries[0];
                rename_noreplace(
                    first.dir.as_raw_fd(),
                    os_from_cstr(&first.destination),
                    first.dir.as_raw_fd(),
                    os_from_cstr(&first.backup),
                )
                .unwrap();
                rename_noreplace(
                    first.dir.as_raw_fd(),
                    os_from_cstr(&first.stage),
                    first.dir.as_raw_fd(),
                    os_from_cstr(&first.destination),
                )
                .unwrap();
                let false_receipt = activation_dir.join(format!("{id}.receipt.json"));
                fs::write(&false_receipt, b"{\"status\":\"not-committed\"}\n").unwrap();
            }
            let runner = FakeRunner::new(home.clone());
            recover_unfinished_transactions(
                &activation_dir,
                ServicePlatform::Linux,
                ServicePorts {
                    mcp: 51111,
                    gateway: 52002,
                },
                &home,
                &runner,
            )
            .unwrap();
            assert_eq!(fs::read(home.join(".local/bin/ff")).unwrap(), b"old-ff");
            assert_eq!(
                fs::read(home.join(".local/bin/forgefleetd")).unwrap(),
                b"old-daemon"
            );
            assert_eq!(fs::read(home.join(".cargo/bin/ff")).unwrap(), b"old-ff");
            assert_eq!(
                last_journal_state(&journal).unwrap().as_deref(),
                Some("rolled_back")
            );
            assert!(!activation_dir.join(format!("{id}.receipt.json")).exists());
            assert!(
                runner
                    .calls()
                    .iter()
                    .any(|call| call.contains("stop forgefleet-mcp.service"))
            );
            assert!(
                runner
                    .calls()
                    .iter()
                    .any(|call| call.contains("start forgefleetd.service"))
            );
        }
    }
}
