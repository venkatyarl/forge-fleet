//! Descriptor-safe immutable runtime-bundle registration.
//!
//! A bundle is one canonical JSON manifest plus its executable and shared
//! library files. Every one of those files is registered as an ordinary V291
//! artifact with a shared version, upstream llama.cpp commit, target triple,
//! computer, and holder. No directory digest is submitted to PostgreSQL.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use ff_core::model_integrity::{
    ModelArtifactKind, ModelIntegrityError, ModelIntegrityLimits, constant_time_sha256_eq,
    model_integrity_worker_allowed, parse_sha256_hex, verify_model_path,
};
use ff_db::{
    DbError, PgPool, ReleaseArtifactAssertion, ReleaseArtifactBatchAssertion,
    ReleaseArtifactBatchRegistration, pg_get_release_artifact, pg_register_release_artifact_batch,
};
use ff_runtime::process_manager::{
    LLAMA_SERVER_RUNTIME_POLICY_PATH, LlamaServerRuntimePolicy, PinnedRuntimeArtifact,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::artifact_registry::local_release_build_root;
use crate::fleet_info::LocalComputerIdentity;

pub const RUNTIME_BUNDLE_SCHEMA_VERSION: u32 = 2;
pub const RUNTIME_MANIFEST_ARTIFACT_NAME: &str = "llama-runtime-manifest";
pub const CPU_ROLLBACK_MANIFEST_ARTIFACT_NAME: &str = "llama-runtime-cpu-rollback-manifest";
pub const RUNTIME_MANIFEST_FILE_NAME: &str = "runtime-manifest.json";
pub const RUNTIME_INSTALL_ROOT: &str = "/opt/forgefleet/llama-runtime";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleManifest {
    pub schema_version: u32,
    pub bundle_version: String,
    pub llama_cpp_commit: String,
    pub target_triple: String,
    pub platform: RuntimeBundlePlatform,
    pub runtime: RuntimeBundlePolicy,
    pub components: Vec<RuntimeBundleComponent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundlePlatform {
    pub target_os: String,
    pub target_arch: String,
    pub os_id: String,
    pub os_version_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeBundlePolicy {
    Rocm {
        rocm_version: String,
        gpu_arch: String,
        cmake_flags: Vec<String>,
        loader_dependencies: Vec<RuntimeLoaderDependency>,
        cpu_rollback_bundle: RuntimeBundleIdentity,
    },
    CpuRollback {
        cmake_flags: Vec<String>,
        loader_dependencies: Vec<RuntimeLoaderDependency>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleIdentity {
    pub bundle_version: String,
    pub llama_cpp_commit: String,
    pub target_triple: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeComponentRole {
    Binary,
    HipLibrary,
    SharedLibrary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleComponent {
    pub artifact_name: String,
    pub role: RuntimeComponentRole,
    pub relative_path: String,
    pub install_path: PathBuf,
    pub sha256: String,
    pub size_bytes: i64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLoaderDependency {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: i64,
    pub mode: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeArtifactError {
    #[error("runtime-bundle verification is forbidden on Vinny")]
    VinnyExcluded,
    #[error("runtime manifest path must name a file below the release root")]
    InvalidManifestPath,
    #[error("runtime manifest is larger than {MAX_MANIFEST_BYTES} bytes")]
    ManifestTooLarge,
    #[error("runtime manifest is not canonical JSON")]
    NonCanonicalManifest,
    #[error("runtime manifest is invalid: {0}")]
    InvalidManifest(String),
    #[error(
        "runtime bundle directory differs from its manifest: expected {expected:?}, found {found:?}"
    )]
    DirectorySetMismatch {
        expected: BTreeSet<String>,
        found: BTreeSet<String>,
    },
    #[error("runtime component {path} is not a single regular file")]
    NotRegularFile { path: PathBuf },
    #[error("runtime path {path} is not owned exclusively enough for custody evidence")]
    UnsafeOwnership { path: PathBuf },
    #[error("runtime component mode disagrees with its manifest: {path}")]
    ModeMismatch { path: PathBuf },
    #[error("runtime component size disagrees with its manifest: {path}")]
    SizeMismatch { path: PathBuf },
    #[error("runtime component digest disagrees with its manifest: {path}")]
    DigestMismatch { path: PathBuf },
    #[error("runtime bundle mutated between verification passes")]
    Mutated,
    #[error("referenced CPU rollback manifest is not registered: {0:?}")]
    RollbackBundleMissing(RuntimeBundleIdentity),
    #[error("referenced CPU rollback manifest digest does not match V291")]
    RollbackDigestMismatch,
    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Integrity(#[from] ModelIntegrityError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Database(#[from] DbError),
}

/// Verify the fixed local release root and atomically register its exact file
/// set. No caller-supplied holder or release root reaches production storage.
pub async fn register_local_runtime_bundle(
    pool: &PgPool,
    identity: &LocalComputerIdentity,
    manifest_relative_path: &Path,
) -> Result<ReleaseArtifactBatchRegistration, RuntimeArtifactError> {
    let release_root = local_release_build_root().map_err(|error| {
        RuntimeArtifactError::InvalidManifest(format!("release root is unavailable: {error}"))
    })?;
    let verified = verify_runtime_bundle_at(identity, &release_root, manifest_relative_path)?;
    if let Some(reference) = cpu_rollback_reference(&verified.manifest) {
        verify_registered_cpu_rollback(pool, reference).await?;
    }
    Ok(pg_register_release_artifact_batch(pool, &verified.assertion).await?)
}

#[derive(Debug, Clone)]
struct VerifiedRuntimeBundle {
    manifest: RuntimeBundleManifest,
    assertion: ReleaseArtifactBatchAssertion,
}

fn verify_runtime_bundle_at(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    manifest_relative_path: &Path,
) -> Result<VerifiedRuntimeBundle, RuntimeArtifactError> {
    if !model_integrity_worker_allowed(&identity.name) {
        return Err(RuntimeArtifactError::VinnyExcluded);
    }
    let first = verify_runtime_bundle_once(identity, release_root, manifest_relative_path)?;
    let second = verify_runtime_bundle_once(identity, release_root, manifest_relative_path)?;
    if first.manifest != second.manifest
        || !batch_assertions_match(&first.assertion, &second.assertion)
    {
        return Err(RuntimeArtifactError::Mutated);
    }
    Ok(second)
}

fn verify_runtime_bundle_once(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    manifest_relative_path: &Path,
) -> Result<VerifiedRuntimeBundle, RuntimeArtifactError> {
    let (bundle_relative_path, manifest_name) = split_manifest_path(manifest_relative_path)?;
    let bundle = open_bundle_directory(release_root, &bundle_relative_path)?;
    let manifest_bytes = read_regular_file_at(
        bundle.as_raw_fd(),
        &manifest_name,
        MAX_MANIFEST_BYTES,
        0o644,
        manifest_relative_path,
    )?;
    let manifest: RuntimeBundleManifest = serde_json::from_slice(&manifest_bytes)?;
    if serde_json::to_vec(&manifest)? != manifest_bytes {
        return Err(RuntimeArtifactError::NonCanonicalManifest);
    }
    let manifest_sha256 = hex_sha256(&manifest_bytes);
    validate_manifest(&manifest, &manifest_name, &manifest_sha256)?;
    let manifest_artifact_name = manifest_artifact_name(&manifest.runtime);

    let mut expected_names = BTreeSet::from([manifest_name.clone()]);
    for component in &manifest.components {
        expected_names.insert(component.relative_path.clone());
    }
    let found_names = list_directory_names(&bundle, &bundle_relative_path)?;
    if found_names != expected_names {
        return Err(RuntimeArtifactError::DirectorySetMismatch {
            expected: expected_names,
            found: found_names,
        });
    }

    let manifest_digest = verify_one_file(
        identity,
        release_root,
        manifest_relative_path,
        i64::try_from(manifest_bytes.len()).map_err(|_| RuntimeArtifactError::ManifestTooLarge)?,
        &manifest_sha256,
        0o644,
    )?;
    let mut artifacts = Vec::with_capacity(manifest.components.len() + 1);
    artifacts.push(ReleaseArtifactAssertion {
        artifact_name: manifest_artifact_name.to_string(),
        artifact_version: manifest.bundle_version.clone(),
        source_commit: manifest.llama_cpp_commit.clone(),
        target_triple: manifest.target_triple.clone(),
        sha256: manifest_digest.0,
        size_bytes: manifest_digest.1,
        computer_id: identity.id,
        holder_name: identity.name.clone(),
        relative_path: utf8_path(manifest_relative_path)?,
    });

    for component in &manifest.components {
        let relative_path = bundle_relative_path.join(&component.relative_path);
        let digest = verify_one_file(
            identity,
            release_root,
            &relative_path,
            component.size_bytes,
            &component.sha256,
            component.mode,
        )?;
        artifacts.push(ReleaseArtifactAssertion {
            artifact_name: component.artifact_name.clone(),
            artifact_version: manifest.bundle_version.clone(),
            source_commit: manifest.llama_cpp_commit.clone(),
            target_triple: manifest.target_triple.clone(),
            sha256: digest.0,
            size_bytes: digest.1,
            computer_id: identity.id,
            holder_name: identity.name.clone(),
            relative_path: utf8_path(&relative_path)?,
        });
    }

    for dependency in loader_dependencies(&manifest.runtime) {
        verify_external_dependency(identity, dependency)?;
    }

    Ok(VerifiedRuntimeBundle {
        manifest,
        assertion: ReleaseArtifactBatchAssertion {
            manifest_artifact_name: manifest_artifact_name.to_string(),
            artifacts,
        },
    })
}

async fn verify_registered_cpu_rollback(
    pool: &PgPool,
    reference: &RuntimeBundleIdentity,
) -> Result<(), RuntimeArtifactError> {
    let row = pg_get_release_artifact(
        pool,
        CPU_ROLLBACK_MANIFEST_ARTIFACT_NAME,
        &reference.bundle_version,
        &reference.llama_cpp_commit,
        &reference.target_triple,
    )
    .await?
    .ok_or_else(|| RuntimeArtifactError::RollbackBundleMissing(reference.clone()))?;
    if !constant_time_sha256_eq(&row.sha256, &reference.manifest_sha256) {
        return Err(RuntimeArtifactError::RollbackDigestMismatch);
    }
    Ok(())
}

/// Deterministically derive the strict runtime policy installed at
/// [`LLAMA_SERVER_RUNTIME_POLICY_PATH`]. This function performs all pure
/// manifest validation; filesystem verification and V291 registration remain
/// separate mandatory gates.
pub fn derive_llama_server_runtime_policy(
    manifest: &RuntimeBundleManifest,
) -> Result<LlamaServerRuntimePolicy, RuntimeArtifactError> {
    let encoded = serde_json::to_vec(manifest)?;
    validate_manifest(manifest, RUNTIME_MANIFEST_FILE_NAME, &hex_sha256(&encoded))?;

    let binary_path = manifest
        .components
        .iter()
        .find(|component| component.role == RuntimeComponentRole::Binary)
        .expect("validated manifest has exactly one binary")
        .install_path
        .clone();
    let bundle_artifacts = manifest
        .components
        .iter()
        .map(|component| PinnedRuntimeArtifact {
            path: component.install_path.clone(),
            sha256: component.sha256.clone(),
        })
        .collect();
    let pinned_dependencies = |dependencies: &[RuntimeLoaderDependency]| {
        dependencies
            .iter()
            .map(|dependency| PinnedRuntimeArtifact {
                path: dependency.path.clone(),
                sha256: dependency.sha256.clone(),
            })
            .collect()
    };

    match &manifest.runtime {
        RuntimeBundlePolicy::Rocm {
            rocm_version,
            gpu_arch,
            loader_dependencies,
            ..
        } => {
            let hip_library_path = manifest
                .components
                .iter()
                .find(|component| component.role == RuntimeComponentRole::HipLibrary)
                .expect("validated ROCm manifest has exactly one HIP library")
                .install_path
                .clone();
            Ok(LlamaServerRuntimePolicy::Rocm {
                binary_path,
                hip_library_path,
                bundle_artifacts,
                loader_dependencies: pinned_dependencies(loader_dependencies),
                target_os: manifest.platform.target_os.clone(),
                target_arch: manifest.platform.target_arch.clone(),
                os_id: manifest.platform.os_id.clone(),
                os_version_id: manifest.platform.os_version_id.clone(),
                rocm_version: rocm_version.clone(),
                gpu_arch: gpu_arch.clone(),
            })
        }
        RuntimeBundlePolicy::CpuRollback {
            loader_dependencies,
            ..
        } => Ok(LlamaServerRuntimePolicy::CpuRollback {
            binary_path,
            bundle_artifacts,
            loader_dependencies: pinned_dependencies(loader_dependencies),
            target_os: manifest.platform.target_os.clone(),
            target_arch: manifest.platform.target_arch.clone(),
            os_id: manifest.platform.os_id.clone(),
            os_version_id: manifest.platform.os_version_id.clone(),
        }),
    }
}

/// Canonical JSON bytes for the fixed root-authoritative runtime-policy path.
/// This function never writes `/etc`; activation code owns that privileged
/// operation and must use these exact bytes.
pub fn canonical_llama_server_runtime_policy_json(
    manifest: &RuntimeBundleManifest,
) -> Result<Vec<u8>, RuntimeArtifactError> {
    debug_assert_eq!(
        LLAMA_SERVER_RUNTIME_POLICY_PATH,
        "/etc/forgefleet/llama-server-runtime.json"
    );
    Ok(serde_json::to_vec(&derive_llama_server_runtime_policy(
        manifest,
    )?)?)
}

fn validate_manifest(
    manifest: &RuntimeBundleManifest,
    manifest_name: &str,
    manifest_sha256: &str,
) -> Result<(), RuntimeArtifactError> {
    if manifest.schema_version != RUNTIME_BUNDLE_SCHEMA_VERSION {
        return Err(RuntimeArtifactError::InvalidManifest(format!(
            "unsupported schema version {}",
            manifest.schema_version
        )));
    }
    if manifest_name != RUNTIME_MANIFEST_FILE_NAME {
        return Err(RuntimeArtifactError::InvalidManifest(format!(
            "manifest filename must be {RUNTIME_MANIFEST_FILE_NAME}"
        )));
    }
    if !canonical_token(&manifest.bundle_version)
        || !is_lower_hex(&manifest.llama_cpp_commit, 40)
        || !is_canonical_target_triple(&manifest.target_triple)
    {
        return Err(RuntimeArtifactError::InvalidManifest(
            "bundle version, llama.cpp commit, or target triple is non-canonical".to_string(),
        ));
    }
    for (field, value) in [
        ("platform.target_os", manifest.platform.target_os.as_str()),
        (
            "platform.target_arch",
            manifest.platform.target_arch.as_str(),
        ),
        ("platform.os_id", manifest.platform.os_id.as_str()),
        (
            "platform.os_version_id",
            manifest.platform.os_version_id.as_str(),
        ),
    ] {
        if !canonical_token(value) {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "{field} must be a canonical token"
            )));
        }
    }
    let target_components: Vec<_> = manifest.target_triple.split('-').collect();
    if target_components.first().copied() != Some(manifest.platform.target_arch.as_str())
        || !target_components.contains(&manifest.platform.target_os.as_str())
    {
        return Err(RuntimeArtifactError::InvalidManifest(
            "platform target_os/target_arch disagree with target_triple".to_string(),
        ));
    }
    if manifest.components.is_empty() {
        return Err(RuntimeArtifactError::InvalidManifest(
            "components must not be empty".to_string(),
        ));
    }
    let names: Vec<_> = manifest
        .components
        .iter()
        .map(|component| component.artifact_name.as_str())
        .collect();
    if !strictly_sorted_unique(&names) {
        return Err(RuntimeArtifactError::InvalidManifest(
            "components must be sorted by unique artifact_name".to_string(),
        ));
    }
    let mut paths = BTreeSet::new();
    let mut install_paths = BTreeSet::new();
    let mut role_counts = BTreeMap::new();
    let install_directory = Path::new(RUNTIME_INSTALL_ROOT).join(&manifest.bundle_version);
    for component in &manifest.components {
        if component.artifact_name == RUNTIME_MANIFEST_ARTIFACT_NAME
            || component.artifact_name == CPU_ROLLBACK_MANIFEST_ARTIFACT_NAME
        {
            return Err(RuntimeArtifactError::InvalidManifest(
                "component must not reuse the manifest artifact name".to_string(),
            ));
        }
        if component.relative_path == manifest_name
            || !one_normal_component(Path::new(&component.relative_path))
            || !paths.insert(component.relative_path.clone())
        {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "component path must be a unique direct sibling: {}",
                component.relative_path
            )));
        }
        if !canonical_absolute_path(&component.install_path)
            || component.install_path.parent() != Some(install_directory.as_path())
            || component.install_path.file_name().and_then(OsStr::to_str)
                != Some(component.relative_path.as_str())
            || !install_paths.insert(component.install_path.clone())
        {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "component {} must have one unique fixed destination under {}",
                component.artifact_name,
                install_directory.display()
            )));
        }
        if parse_sha256_hex(&component.sha256).is_none()
            || component
                .sha256
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
            || component.size_bytes <= 0
        {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "component {} has invalid digest or size",
                component.artifact_name
            )));
        }
        let required_mode = match component.role {
            RuntimeComponentRole::Binary => 0o755,
            RuntimeComponentRole::HipLibrary | RuntimeComponentRole::SharedLibrary => 0o644,
        };
        if component.mode != required_mode {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "component {} must pin exact mode {required_mode:#o}",
                component.artifact_name,
            )));
        }
        if component.role != RuntimeComponentRole::Binary
            && !component.relative_path.contains(".so")
        {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "library component {} must have a shared-library filename",
                component.artifact_name
            )));
        }
        *role_counts.entry(component.role).or_insert(0_usize) += 1;
    }
    if role_counts.get(&RuntimeComponentRole::Binary) != Some(&1) {
        return Err(RuntimeArtifactError::InvalidManifest(
            "bundle must contain exactly one binary role".to_string(),
        ));
    }

    match &manifest.runtime {
        RuntimeBundlePolicy::Rocm {
            rocm_version,
            gpu_arch,
            cmake_flags,
            loader_dependencies,
            cpu_rollback_bundle,
        } => {
            if !canonical_token(rocm_version) || gpu_arch != "gfx1151" {
                return Err(RuntimeArtifactError::InvalidManifest(
                    "ROCm runtime must pin a non-empty version and exactly gpu_arch gfx1151"
                        .to_string(),
                ));
            }
            if role_counts.get(&RuntimeComponentRole::HipLibrary) != Some(&1) {
                return Err(RuntimeArtifactError::InvalidManifest(
                    "ROCm bundle must contain exactly one hip_library role".to_string(),
                ));
            }
            validate_cmake_flags(cmake_flags, true)?;
            validate_loader_dependencies(loader_dependencies, &install_paths)?;
            validate_rollback_identity(manifest, cpu_rollback_bundle, manifest_sha256)?;
        }
        RuntimeBundlePolicy::CpuRollback {
            cmake_flags,
            loader_dependencies,
        } => {
            if role_counts
                .get(&RuntimeComponentRole::HipLibrary)
                .copied()
                .unwrap_or(0)
                != 0
            {
                return Err(RuntimeArtifactError::InvalidManifest(
                    "CPU rollback bundle must not contain a hip_library role".to_string(),
                ));
            }
            validate_cmake_flags(cmake_flags, false)?;
            validate_loader_dependencies(loader_dependencies, &install_paths)?;
        }
    }
    Ok(())
}

fn validate_cmake_flags(flags: &[String], rocm: bool) -> Result<(), RuntimeArtifactError> {
    if flags.is_empty() {
        return Err(RuntimeArtifactError::InvalidManifest(
            "cmake_flags must not be empty".to_string(),
        ));
    }
    let mut keys = BTreeSet::new();
    for flag in flags {
        let Some((key, value)) = flag
            .strip_prefix("-D")
            .and_then(|flag| flag.split_once('='))
        else {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "CMake flag must use exact -DKEY=VALUE form: {flag}"
            )));
        };
        if key.is_empty()
            || value.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
            || !keys.insert(key)
        {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "CMake flag is empty, non-canonical, or duplicates a key: {flag}"
            )));
        }
    }
    let required: &[(&str, &str)] = if rocm {
        &[
            ("CMAKE_BUILD_TYPE", "Release"),
            ("GGML_NATIVE", "OFF"),
            ("GGML_HIP", "ON"),
            ("AMDGPU_TARGETS", "gfx1151"),
        ]
    } else {
        &[
            ("CMAKE_BUILD_TYPE", "Release"),
            ("GGML_NATIVE", "OFF"),
            ("GGML_HIP", "OFF"),
        ]
    };
    for (key, value) in required {
        let expected = format!("-D{key}={value}");
        if !flags.contains(&expected) {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "cmake_flags is missing required exact flag {expected}"
            )));
        }
    }
    if !rocm && keys.contains("AMDGPU_TARGETS") {
        return Err(RuntimeArtifactError::InvalidManifest(
            "CPU rollback cmake_flags must not contain AMDGPU_TARGETS".to_string(),
        ));
    }
    Ok(())
}

fn validate_loader_dependencies(
    dependencies: &[RuntimeLoaderDependency],
    install_paths: &BTreeSet<PathBuf>,
) -> Result<(), RuntimeArtifactError> {
    if dependencies.is_empty() {
        return Err(RuntimeArtifactError::InvalidManifest(
            "loader_dependencies must not be empty".to_string(),
        ));
    }
    let paths: Vec<_> = dependencies
        .iter()
        .map(|dependency| &dependency.path)
        .collect();
    if !strictly_sorted_unique(&paths) {
        return Err(RuntimeArtifactError::InvalidManifest(
            "loader_dependencies must be sorted by unique canonical path".to_string(),
        ));
    }
    for dependency in dependencies {
        if !canonical_absolute_path(&dependency.path)
            || install_paths.contains(&dependency.path)
            || parse_sha256_hex(&dependency.sha256).is_none()
            || dependency
                .sha256
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
            || dependency.size_bytes <= 0
            || dependency.mode > 0o777
            || dependency.mode & 0o022 != 0
        {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "external loader dependency is non-canonical or incomplete: {}",
                dependency.path.display()
            )));
        }
    }
    Ok(())
}

fn validate_rollback_identity(
    manifest: &RuntimeBundleManifest,
    reference: &RuntimeBundleIdentity,
    manifest_sha256: &str,
) -> Result<(), RuntimeArtifactError> {
    if !canonical_token(&reference.bundle_version)
        || !is_lower_hex(&reference.llama_cpp_commit, 40)
        || !is_canonical_target_triple(&reference.target_triple)
        || !is_lower_hex(&reference.manifest_sha256, 64)
        || reference.target_triple != manifest.target_triple
        || (reference.bundle_version == manifest.bundle_version
            && reference.llama_cpp_commit == manifest.llama_cpp_commit)
        || constant_time_sha256_eq(&reference.manifest_sha256, manifest_sha256)
    {
        return Err(RuntimeArtifactError::InvalidManifest(
            "CPU rollback reference must be a distinct exact V291 manifest identity for the same target"
                .to_string(),
        ));
    }
    Ok(())
}

fn loader_dependencies(policy: &RuntimeBundlePolicy) -> &[RuntimeLoaderDependency] {
    match policy {
        RuntimeBundlePolicy::Rocm {
            loader_dependencies,
            ..
        }
        | RuntimeBundlePolicy::CpuRollback {
            loader_dependencies,
            ..
        } => loader_dependencies,
    }
}

fn cpu_rollback_reference(manifest: &RuntimeBundleManifest) -> Option<&RuntimeBundleIdentity> {
    match &manifest.runtime {
        RuntimeBundlePolicy::Rocm {
            cpu_rollback_bundle,
            ..
        } => Some(cpu_rollback_bundle),
        RuntimeBundlePolicy::CpuRollback { .. } => None,
    }
}

fn manifest_artifact_name(policy: &RuntimeBundlePolicy) -> &'static str {
    match policy {
        RuntimeBundlePolicy::Rocm { .. } => RUNTIME_MANIFEST_ARTIFACT_NAME,
        RuntimeBundlePolicy::CpuRollback { .. } => CPU_ROLLBACK_MANIFEST_ARTIFACT_NAME,
    }
}

fn canonical_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn canonical_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._+-".contains(&byte)
        })
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_target_triple(value: &str) -> bool {
    let components: Vec<_> = value.split('-').collect();
    value.len() <= 128
        && (3..=5).contains(&components.len())
        && components.iter().all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'
                        || byte == b'.'
                })
        })
}

fn verify_one_file(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    relative_path: &Path,
    expected_size: i64,
    expected_sha256: &str,
    expected_mode: u32,
) -> Result<(String, i64), RuntimeArtifactError> {
    let expected_bytes =
        u64::try_from(expected_size).map_err(|_| RuntimeArtifactError::SizeMismatch {
            path: relative_path.to_path_buf(),
        })?;
    let digest = verify_model_path(
        &identity.name,
        release_root,
        relative_path,
        ModelIntegrityLimits {
            max_files: 1,
            max_bytes: expected_bytes,
            max_depth: 16,
            timeout: VERIFY_TIMEOUT,
        },
    )?;
    if digest.kind != ModelArtifactKind::File
        || digest.files != 1
        || digest.entries != 1
        || digest.algorithm != "sha256"
    {
        return Err(RuntimeArtifactError::NotRegularFile {
            path: relative_path.to_path_buf(),
        });
    }
    if digest.bytes != expected_bytes {
        return Err(RuntimeArtifactError::SizeMismatch {
            path: relative_path.to_path_buf(),
        });
    }
    if !constant_time_sha256_eq(&digest.sha256, expected_sha256) {
        return Err(RuntimeArtifactError::DigestMismatch {
            path: relative_path.to_path_buf(),
        });
    }

    let metadata = descriptor_metadata(release_root, relative_path)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(RuntimeArtifactError::NotRegularFile {
            path: relative_path.to_path_buf(),
        });
    }
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(RuntimeArtifactError::UnsafeOwnership {
            path: relative_path.to_path_buf(),
        });
    }
    if metadata.mode() & 0o777 != expected_mode {
        return Err(RuntimeArtifactError::ModeMismatch {
            path: relative_path.to_path_buf(),
        });
    }
    Ok((digest.sha256, expected_size))
}

fn verify_external_dependency(
    identity: &LocalComputerIdentity,
    dependency: &RuntimeLoaderDependency,
) -> Result<(), RuntimeArtifactError> {
    use std::os::unix::fs::OpenOptionsExt;

    let canonical =
        std::fs::canonicalize(&dependency.path).map_err(|error| io(&dependency.path, error))?;
    if canonical != dependency.path {
        return Err(RuntimeArtifactError::InvalidManifest(format!(
            "loader dependency path is not canonical: {}",
            dependency.path.display()
        )));
    }
    let parent = dependency.path.parent().ok_or_else(|| {
        RuntimeArtifactError::InvalidManifest("loader dependency has no parent".to_string())
    })?;
    let file_name = dependency.path.file_name().ok_or_else(|| {
        RuntimeArtifactError::InvalidManifest("loader dependency has no filename".to_string())
    })?;
    let expected_bytes =
        u64::try_from(dependency.size_bytes).map_err(|_| RuntimeArtifactError::SizeMismatch {
            path: dependency.path.clone(),
        })?;
    let digest = verify_model_path(
        &identity.name,
        parent,
        Path::new(file_name),
        ModelIntegrityLimits {
            max_files: 1,
            max_bytes: expected_bytes,
            max_depth: 1,
            timeout: VERIFY_TIMEOUT,
        },
    )?;
    if digest.kind != ModelArtifactKind::File || digest.files != 1 || digest.entries != 1 {
        return Err(RuntimeArtifactError::NotRegularFile {
            path: dependency.path.clone(),
        });
    }
    if digest.bytes != expected_bytes {
        return Err(RuntimeArtifactError::SizeMismatch {
            path: dependency.path.clone(),
        });
    }
    if !constant_time_sha256_eq(&digest.sha256, &dependency.sha256) {
        return Err(RuntimeArtifactError::DigestMismatch {
            path: dependency.path.clone(),
        });
    }

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&dependency.path)
        .map_err(|error| io(&dependency.path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| io(&dependency.path, error))?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(RuntimeArtifactError::NotRegularFile {
            path: dependency.path.clone(),
        });
    }
    if metadata.uid() != 0 || metadata.mode() & 0o777 != dependency.mode {
        return Err(RuntimeArtifactError::UnsafeOwnership {
            path: dependency.path.clone(),
        });
    }
    Ok(())
}

fn split_manifest_path(path: &Path) -> Result<(PathBuf, String), RuntimeArtifactError> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(RuntimeArtifactError::InvalidManifestPath);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(RuntimeArtifactError::InvalidManifestPath)?;
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or(RuntimeArtifactError::InvalidManifestPath)?
        .to_string();
    Ok((parent.to_path_buf(), name))
}

fn one_normal_component(path: &Path) -> bool {
    !path.is_absolute()
        && path.components().count() == 1
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn strictly_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|window| window[0] < window[1])
}

fn utf8_path(path: &Path) -> Result<String, RuntimeArtifactError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or(RuntimeArtifactError::InvalidManifestPath)
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn batch_assertions_match(
    left: &ReleaseArtifactBatchAssertion,
    right: &ReleaseArtifactBatchAssertion,
) -> bool {
    if left.manifest_artifact_name != right.manifest_artifact_name
        || left.artifacts.len() != right.artifacts.len()
    {
        return false;
    }
    left.artifacts
        .iter()
        .zip(&right.artifacts)
        .all(|(left, right)| {
            left.artifact_name == right.artifact_name
                && left.artifact_version == right.artifact_version
                && left.source_commit == right.source_commit
                && left.target_triple == right.target_triple
                && constant_time_sha256_eq(&left.sha256, &right.sha256)
                && left.size_bytes == right.size_bytes
                && left.computer_id == right.computer_id
                && left.holder_name == right.holder_name
                && left.relative_path == right.relative_path
        })
}

fn io(path: &Path, source: std::io::Error) -> RuntimeArtifactError {
    RuntimeArtifactError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn c_name(name: &OsStr, path: &Path) -> Result<CString, RuntimeArtifactError> {
    CString::new(name.as_bytes()).map_err(|_| {
        io(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"),
        )
    })
}

fn openat_owned(
    directory: RawFd,
    name: &OsStr,
    flags: libc::c_int,
    display: &Path,
) -> Result<OwnedFd, RuntimeArtifactError> {
    let name = c_name(name, display)?;
    let fd = unsafe { libc::openat(directory, name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io(display, std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_bundle_directory(
    release_root: &Path,
    bundle_relative_path: &Path,
) -> Result<OwnedFd, RuntimeArtifactError> {
    if !release_root.is_absolute()
        || release_root.components().any(|part| {
            matches!(
                part,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
        || bundle_relative_path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(RuntimeArtifactError::InvalidManifestPath);
    }
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY;
    let root_name = CString::new("/").expect("literal has no NUL");
    let root_fd = unsafe { libc::open(root_name.as_ptr(), flags) };
    if root_fd < 0 {
        return Err(io(Path::new("/"), std::io::Error::last_os_error()));
    }
    let mut current = unsafe { OwnedFd::from_raw_fd(root_fd) };
    let mut display = PathBuf::from("/");
    for component in release_root.components() {
        if let Component::Normal(name) = component {
            display.push(name);
            current = openat_owned(current.as_raw_fd(), name, flags, &display)?;
        }
    }
    let root_metadata = owned_fd_metadata(&current, release_root)?;
    validate_directory_control(&root_metadata, release_root)?;
    let root_device = root_metadata.dev();
    for component in bundle_relative_path.components() {
        let Component::Normal(name) = component else {
            return Err(RuntimeArtifactError::InvalidManifestPath);
        };
        display.push(name);
        current = openat_owned(current.as_raw_fd(), name, flags, &display)?;
        let metadata = owned_fd_metadata(&current, &display)?;
        validate_directory_control(&metadata, &display)?;
        if metadata.dev() != root_device {
            return Err(io(
                &display,
                std::io::Error::new(std::io::ErrorKind::InvalidData, "cross-device path"),
            ));
        }
    }
    Ok(current)
}

fn owned_fd_metadata(
    descriptor: &OwnedFd,
    display: &Path,
) -> Result<std::fs::Metadata, RuntimeArtifactError> {
    File::from(descriptor.try_clone().map_err(|error| io(display, error))?)
        .metadata()
        .map_err(|error| io(display, error))
}

fn validate_directory_control(
    metadata: &std::fs::Metadata,
    display: &Path,
) -> Result<(), RuntimeArtifactError> {
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(RuntimeArtifactError::UnsafeOwnership {
            path: display.to_path_buf(),
        });
    }
    Ok(())
}

fn read_regular_file_at(
    directory: RawFd,
    name: &str,
    max_bytes: u64,
    expected_mode: u32,
    display: &Path,
) -> Result<Vec<u8>, RuntimeArtifactError> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = openat_owned(directory, OsStr::new(name), flags, display)?;
    let mut file = File::from(fd);
    let before = file.metadata().map_err(|error| io(display, error))?;
    if !before.is_file() || before.nlink() != 1 {
        return Err(RuntimeArtifactError::NotRegularFile {
            path: display.to_path_buf(),
        });
    }
    if before.uid() != unsafe { libc::geteuid() } || before.mode() & 0o022 != 0 {
        return Err(RuntimeArtifactError::UnsafeOwnership {
            path: display.to_path_buf(),
        });
    }
    if before.mode() & 0o777 != expected_mode {
        return Err(RuntimeArtifactError::ModeMismatch {
            path: display.to_path_buf(),
        });
    }
    if before.len() > max_bytes {
        return Err(RuntimeArtifactError::ManifestTooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
    (&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io(display, error))?;
    if bytes.len() as u64 > max_bytes {
        return Err(RuntimeArtifactError::ManifestTooLarge);
    }
    let after = file.metadata().map_err(|error| io(display, error))?;
    if file_snapshot(&before) != file_snapshot(&after) || after.len() != bytes.len() as u64 {
        return Err(RuntimeArtifactError::Mutated);
    }
    Ok(bytes)
}

fn file_snapshot(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn descriptor_metadata(
    release_root: &Path,
    relative_path: &Path,
) -> Result<std::fs::Metadata, RuntimeArtifactError> {
    let (parent, name) = split_manifest_path(relative_path)?;
    let directory = open_bundle_directory(release_root, &parent)?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = openat_owned(
        directory.as_raw_fd(),
        OsStr::new(&name),
        flags,
        relative_path,
    )?;
    File::from(fd)
        .metadata()
        .map_err(|error| io(relative_path, error))
}

fn list_directory_names(
    directory: &OwnedFd,
    display: &Path,
) -> Result<BTreeSet<String>, RuntimeArtifactError> {
    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let entries = std::fs::read_dir(&proc_path).map_err(|error| io(display, error))?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| io(display, error))?;
        let name = entry.file_name().into_string().map_err(|_| {
            RuntimeArtifactError::InvalidManifest("non-UTF-8 bundle entry".to_string())
        })?;
        names.insert(name);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use uuid::Uuid;

    fn identity(name: &str) -> LocalComputerIdentity {
        LocalComputerIdentity {
            id: Uuid::new_v4(),
            name: name.to_string(),
        }
    }

    fn component(
        version: &str,
        name: &str,
        path: &str,
        bytes: &[u8],
        role: RuntimeComponentRole,
    ) -> RuntimeBundleComponent {
        RuntimeBundleComponent {
            artifact_name: name.to_string(),
            role,
            relative_path: path.to_string(),
            install_path: Path::new(RUNTIME_INSTALL_ROOT).join(version).join(path),
            sha256: hex_sha256(bytes),
            size_bytes: i64::try_from(bytes.len()).unwrap(),
            mode: if role == RuntimeComponentRole::Binary {
                0o755
            } else {
                0o644
            },
        }
    }

    fn loader_dependency() -> RuntimeLoaderDependency {
        let path = std::fs::canonicalize("/usr/bin/env").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        RuntimeLoaderDependency {
            path,
            sha256: hex_sha256(&bytes),
            size_bytes: i64::try_from(metadata.len()).unwrap(),
            mode: metadata.mode() & 0o777,
        }
    }

    fn manifest() -> RuntimeBundleManifest {
        let version = "2026.8.5_logan_1";
        RuntimeBundleManifest {
            schema_version: RUNTIME_BUNDLE_SCHEMA_VERSION,
            bundle_version: version.to_string(),
            llama_cpp_commit: "6dc4086b7217cb8c2ccc1945b1e1f3213b9b1941".to_string(),
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            platform: RuntimeBundlePlatform {
                target_os: "linux".to_string(),
                target_arch: "x86_64".to_string(),
                os_id: "ubuntu".to_string(),
                os_version_id: "26.04".to_string(),
            },
            runtime: RuntimeBundlePolicy::Rocm {
                rocm_version: "7.1.52801-9999".to_string(),
                gpu_arch: "gfx1151".to_string(),
                cmake_flags: vec![
                    "-DCMAKE_BUILD_TYPE=Release".to_string(),
                    "-DGGML_NATIVE=OFF".to_string(),
                    "-DGGML_HIP=ON".to_string(),
                    "-DAMDGPU_TARGETS=gfx1151".to_string(),
                ],
                loader_dependencies: vec![loader_dependency()],
                cpu_rollback_bundle: RuntimeBundleIdentity {
                    bundle_version: "2026.8.5_logan_cpu_1".to_string(),
                    llama_cpp_commit: "7dc4086b7217cb8c2ccc1945b1e1f3213b9b1941".to_string(),
                    target_triple: "x86_64-unknown-linux-gnu".to_string(),
                    manifest_sha256: "2".repeat(64),
                },
            },
            components: vec![
                component(
                    version,
                    "libggml-hip",
                    "libggml-hip.so",
                    b"hip",
                    RuntimeComponentRole::HipLibrary,
                ),
                component(
                    version,
                    "libllama",
                    "libllama.so",
                    b"library",
                    RuntimeComponentRole::SharedLibrary,
                ),
                component(
                    version,
                    "llama-server",
                    "llama-server",
                    b"server",
                    RuntimeComponentRole::Binary,
                ),
            ],
        }
    }

    fn cpu_manifest() -> RuntimeBundleManifest {
        let version = "2026.8.5_logan_cpu_1";
        RuntimeBundleManifest {
            schema_version: RUNTIME_BUNDLE_SCHEMA_VERSION,
            bundle_version: version.to_string(),
            llama_cpp_commit: "7dc4086b7217cb8c2ccc1945b1e1f3213b9b1941".to_string(),
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            platform: RuntimeBundlePlatform {
                target_os: "linux".to_string(),
                target_arch: "x86_64".to_string(),
                os_id: "ubuntu".to_string(),
                os_version_id: "26.04".to_string(),
            },
            runtime: RuntimeBundlePolicy::CpuRollback {
                cmake_flags: vec![
                    "-DCMAKE_BUILD_TYPE=Release".to_string(),
                    "-DGGML_NATIVE=OFF".to_string(),
                    "-DGGML_HIP=OFF".to_string(),
                ],
                loader_dependencies: vec![loader_dependency()],
            },
            components: vec![
                component(
                    version,
                    "libllama",
                    "libllama.so",
                    b"library",
                    RuntimeComponentRole::SharedLibrary,
                ),
                component(
                    version,
                    "llama-server",
                    "llama-server",
                    b"server",
                    RuntimeComponentRole::Binary,
                ),
            ],
        }
    }

    fn write_bundle(root: &Path, manifest: &RuntimeBundleManifest) -> PathBuf {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let directory = root.join("logan-runtime");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        for component in &manifest.components {
            let bytes: &[u8] = match component.relative_path.as_str() {
                "libggml-hip.so" => b"hip",
                "libllama.so" => b"library",
                "llama-server" => b"server",
                _ => panic!("test component bytes missing"),
            };
            let path = directory.join(&component.relative_path);
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(component.mode))
                .unwrap();
        }
        let relative = PathBuf::from("logan-runtime/runtime-manifest.json");
        std::fs::write(root.join(&relative), serde_json::to_vec(manifest).unwrap()).unwrap();
        std::fs::set_permissions(root.join(&relative), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        relative
    }

    fn validate_test_manifest(
        manifest: &RuntimeBundleManifest,
    ) -> Result<(), RuntimeArtifactError> {
        let encoded = serde_json::to_vec(manifest).unwrap();
        validate_manifest(manifest, RUNTIME_MANIFEST_FILE_NAME, &hex_sha256(&encoded))
    }

    #[test]
    fn canonical_bundle_emits_only_exact_file_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let relative = write_bundle(root.path(), &manifest());
        let verified =
            verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).unwrap();
        let batch = &verified.assertion;

        assert_eq!(batch.manifest_artifact_name, RUNTIME_MANIFEST_ARTIFACT_NAME);
        assert_eq!(batch.artifacts.len(), 4);
        assert_eq!(
            batch.artifacts[0].artifact_name,
            RUNTIME_MANIFEST_ARTIFACT_NAME
        );
        assert!(
            batch
                .artifacts
                .iter()
                .all(|artifact| artifact.size_bytes > 0)
        );
        assert!(
            batch
                .artifacts
                .iter()
                .all(|artifact| artifact.relative_path != "logan-runtime")
        );

        let policy = derive_llama_server_runtime_policy(&verified.manifest).unwrap();
        match policy {
            LlamaServerRuntimePolicy::Rocm {
                binary_path,
                hip_library_path,
                bundle_artifacts,
                loader_dependencies,
                rocm_version,
                gpu_arch,
                ..
            } => {
                assert_eq!(
                    binary_path,
                    Path::new(RUNTIME_INSTALL_ROOT)
                        .join("2026.8.5_logan_1")
                        .join("llama-server")
                );
                assert_eq!(hip_library_path.file_name().unwrap(), "libggml-hip.so");
                assert_eq!(bundle_artifacts.len(), 3);
                assert_eq!(loader_dependencies.len(), 1);
                assert_eq!(rocm_version, "7.1.52801-9999");
                assert_eq!(gpu_arch, "gfx1151");
            }
            _ => panic!("expected ROCm policy"),
        }
        let policy_json = canonical_llama_server_runtime_policy_json(&verified.manifest).unwrap();
        assert!(!policy_json.ends_with(b"\n"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&policy_json).unwrap()["backend"],
            "rocm"
        );
    }

    #[test]
    fn cpu_bundle_maps_one_to_one_to_explicit_rollback_policy() {
        let manifest = cpu_manifest();
        let policy = derive_llama_server_runtime_policy(&manifest).unwrap();
        match policy {
            LlamaServerRuntimePolicy::CpuRollback {
                binary_path,
                bundle_artifacts,
                loader_dependencies,
                target_os,
                target_arch,
                os_id,
                os_version_id,
            } => {
                assert_eq!(binary_path.file_name().unwrap(), "llama-server");
                assert_eq!(bundle_artifacts.len(), 2);
                assert_eq!(loader_dependencies.len(), 1);
                assert_eq!(target_os, "linux");
                assert_eq!(target_arch, "x86_64");
                assert_eq!(os_id, "ubuntu");
                assert_eq!(os_version_id, "26.04");
            }
            _ => panic!("expected CPU rollback policy"),
        }

        let root = tempfile::tempdir().unwrap();
        let relative = write_bundle(root.path(), &manifest);
        let verified =
            verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).unwrap();
        assert_eq!(verified.assertion.artifacts.len(), 3);
        assert_eq!(
            verified.assertion.manifest_artifact_name,
            CPU_ROLLBACK_MANIFEST_ARTIFACT_NAME
        );
        assert_eq!(
            verified.assertion.artifacts[0].artifact_name,
            CPU_ROLLBACK_MANIFEST_ARTIFACT_NAME
        );
        assert!(cpu_rollback_reference(&verified.manifest).is_none());
    }

    #[test]
    fn rejects_partial_duplicate_or_drifted_policy_fields() {
        let valid = manifest();

        let mut invalid = valid.clone();
        invalid
            .components
            .retain(|component| component.role != RuntimeComponentRole::HipLibrary);
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.components[1].role = RuntimeComponentRole::HipLibrary;
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.components[1].install_path = invalid.components[0].install_path.clone();
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.components[0].mode = 0o600;
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.platform.target_arch = "aarch64".to_string();
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        let RuntimeBundlePolicy::Rocm { gpu_arch, .. } = &mut invalid.runtime else {
            unreachable!()
        };
        *gpu_arch = "gfx1150".to_string();
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        let RuntimeBundlePolicy::Rocm { rocm_version, .. } = &mut invalid.runtime else {
            unreachable!()
        };
        *rocm_version = "7.1\nforged".to_string();
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        let RuntimeBundlePolicy::Rocm { cmake_flags, .. } = &mut invalid.runtime else {
            unreachable!()
        };
        cmake_flags.push("-DGGML_HIP=OFF".to_string());
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid.clone();
        let RuntimeBundlePolicy::Rocm {
            loader_dependencies,
            ..
        } = &mut invalid.runtime
        else {
            unreachable!()
        };
        loader_dependencies.push(loader_dependencies[0].clone());
        assert!(validate_test_manifest(&invalid).is_err());

        let mut invalid = valid;
        let RuntimeBundlePolicy::Rocm {
            cpu_rollback_bundle,
            ..
        } = &mut invalid.runtime
        else {
            unreachable!()
        };
        cpu_rollback_bundle.target_triple = "aarch64-unknown-linux-gnu".to_string();
        assert!(validate_test_manifest(&invalid).is_err());
    }

    #[test]
    fn external_loader_digest_is_verified_but_not_registered_as_bundle_custody() {
        let root = tempfile::tempdir().unwrap();
        let mut drifted = manifest();
        let RuntimeBundlePolicy::Rocm {
            loader_dependencies,
            ..
        } = &mut drifted.runtime
        else {
            unreachable!()
        };
        let dependency_path = loader_dependencies[0].path.clone();
        loader_dependencies[0].sha256 = "0".repeat(64);
        let relative = write_bundle(root.path(), &drifted);
        assert!(verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).is_err());

        let valid = manifest();
        let root = tempfile::tempdir().unwrap();
        let relative = write_bundle(root.path(), &valid);
        let verified =
            verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).unwrap();
        assert!(
            verified
                .assertion
                .artifacts
                .iter()
                .all(|artifact| artifact.relative_path != dependency_path.to_string_lossy())
        );
    }

    #[test]
    fn rejects_noncanonical_unknown_and_unsorted_manifests() {
        let root = tempfile::tempdir().unwrap();
        let mut unsorted = manifest();
        unsorted.components.reverse();
        let relative = write_bundle(root.path(), &unsorted);
        assert!(verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).is_err());

        let canonical = serde_json::to_vec(&manifest()).unwrap();
        std::fs::write(
            root.path().join(&relative),
            [canonical.as_slice(), b"\n"].concat(),
        )
        .unwrap();
        assert!(matches!(
            verify_runtime_bundle_at(&identity("logan"), root.path(), &relative),
            Err(RuntimeArtifactError::NonCanonicalManifest)
        ));

        let mut value = serde_json::to_value(manifest()).unwrap();
        value["unknown"] = serde_json::json!(true);
        std::fs::write(
            root.path().join(&relative),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).is_err());
    }

    #[test]
    fn rejects_missing_extra_symlink_digest_size_and_mode_drift() {
        let root = tempfile::tempdir().unwrap();
        let relative = write_bundle(root.path(), &manifest());
        let directory = root.path().join("logan-runtime");

        std::fs::write(directory.join("extra"), b"extra").unwrap();
        assert!(verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).is_err());
        std::fs::remove_file(directory.join("extra")).unwrap();

        std::fs::remove_file(directory.join("libllama.so")).unwrap();
        symlink("llama-server", directory.join("libllama.so")).unwrap();
        assert!(verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).is_err());
        std::fs::remove_file(directory.join("libllama.so")).unwrap();
        std::fs::write(directory.join("libllama.so"), b"changed").unwrap();
        assert!(verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).is_err());

        std::fs::write(directory.join("libllama.so"), b"library").unwrap();
        std::fs::set_permissions(
            directory.join("libllama.so"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).is_err());
    }

    #[test]
    fn rejects_vinny_and_manifest_self_listing() {
        let root = tempfile::tempdir().unwrap();
        let relative = write_bundle(root.path(), &manifest());
        assert!(matches!(
            verify_runtime_bundle_at(&identity("vinny"), root.path(), &relative),
            Err(RuntimeArtifactError::VinnyExcluded)
        ));

        let mut invalid = manifest();
        invalid.components[0].artifact_name = RUNTIME_MANIFEST_ARTIFACT_NAME.to_string();
        let second_root = tempfile::tempdir().unwrap();
        let relative = write_bundle(second_root.path(), &invalid);
        assert!(
            verify_runtime_bundle_at(&identity("logan"), second_root.path(), &relative).is_err()
        );
    }
}
