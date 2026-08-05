//! Descriptor-safe immutable runtime-bundle registration.
//!
//! A bundle is one canonical JSON manifest plus its executable and shared
//! library files. Every one of those files is registered as an ordinary V291
//! artifact with a shared version, upstream llama.cpp commit, target triple,
//! computer, and holder. No directory digest is submitted to PostgreSQL.

use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fs::File;
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
    ReleaseArtifactBatchRegistration, pg_register_release_artifact_batch,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::artifact_registry::local_release_build_root;
use crate::fleet_info::LocalComputerIdentity;

pub const RUNTIME_BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_MANIFEST_ARTIFACT_NAME: &str = "llama-runtime-manifest";
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
    pub components: Vec<RuntimeBundleComponent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundlePlatform {
    pub os: String,
    pub arch: String,
    pub os_id: String,
    pub os_version_id: String,
    pub hip_version: String,
    pub gpu_arches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleComponent {
    pub artifact_name: String,
    pub relative_path: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub executable: bool,
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
    let assertion = verify_runtime_bundle_at(identity, &release_root, manifest_relative_path)?;
    Ok(pg_register_release_artifact_batch(pool, &assertion).await?)
}

fn verify_runtime_bundle_at(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    manifest_relative_path: &Path,
) -> Result<ReleaseArtifactBatchAssertion, RuntimeArtifactError> {
    if !model_integrity_worker_allowed(&identity.name) {
        return Err(RuntimeArtifactError::VinnyExcluded);
    }
    let first = verify_runtime_bundle_once(identity, release_root, manifest_relative_path)?;
    let second = verify_runtime_bundle_once(identity, release_root, manifest_relative_path)?;
    if !batch_assertions_match(&first, &second) {
        return Err(RuntimeArtifactError::Mutated);
    }
    Ok(second)
}

fn verify_runtime_bundle_once(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    manifest_relative_path: &Path,
) -> Result<ReleaseArtifactBatchAssertion, RuntimeArtifactError> {
    let (bundle_relative_path, manifest_name) = split_manifest_path(manifest_relative_path)?;
    let bundle = open_bundle_directory(release_root, &bundle_relative_path)?;
    let manifest_bytes = read_regular_file_at(
        bundle.as_raw_fd(),
        &manifest_name,
        MAX_MANIFEST_BYTES,
        false,
        manifest_relative_path,
    )?;
    let manifest: RuntimeBundleManifest = serde_json::from_slice(&manifest_bytes)?;
    if serde_json::to_vec(&manifest)? != manifest_bytes {
        return Err(RuntimeArtifactError::NonCanonicalManifest);
    }
    validate_manifest(&manifest, &manifest_name)?;

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
        &hex_sha256(&manifest_bytes),
        false,
    )?;
    let mut artifacts = Vec::with_capacity(manifest.components.len() + 1);
    artifacts.push(ReleaseArtifactAssertion {
        artifact_name: RUNTIME_MANIFEST_ARTIFACT_NAME.to_string(),
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
            component.executable,
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

    Ok(ReleaseArtifactBatchAssertion {
        manifest_artifact_name: RUNTIME_MANIFEST_ARTIFACT_NAME.to_string(),
        artifacts,
    })
}

fn validate_manifest(
    manifest: &RuntimeBundleManifest,
    manifest_name: &str,
) -> Result<(), RuntimeArtifactError> {
    if manifest.schema_version != RUNTIME_BUNDLE_SCHEMA_VERSION {
        return Err(RuntimeArtifactError::InvalidManifest(format!(
            "unsupported schema version {}",
            manifest.schema_version
        )));
    }
    for (field, value) in [
        ("bundle_version", manifest.bundle_version.as_str()),
        ("llama_cpp_commit", manifest.llama_cpp_commit.as_str()),
        ("target_triple", manifest.target_triple.as_str()),
        ("platform.os", manifest.platform.os.as_str()),
        ("platform.arch", manifest.platform.arch.as_str()),
        ("platform.os_id", manifest.platform.os_id.as_str()),
        (
            "platform.os_version_id",
            manifest.platform.os_version_id.as_str(),
        ),
        (
            "platform.hip_version",
            manifest.platform.hip_version.as_str(),
        ),
    ] {
        if value.is_empty() {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "{field} must not be empty"
            )));
        }
    }
    if manifest.components.is_empty() {
        return Err(RuntimeArtifactError::InvalidManifest(
            "components must not be empty".to_string(),
        ));
    }
    if manifest.platform.gpu_arches.is_empty()
        || !strictly_sorted_unique(&manifest.platform.gpu_arches)
        || manifest.platform.gpu_arches.iter().any(String::is_empty)
    {
        return Err(RuntimeArtifactError::InvalidManifest(
            "gpu_arches must be non-empty, sorted, and unique".to_string(),
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
    let mut has_executable = false;
    let mut has_shared_library = false;
    for component in &manifest.components {
        if component.artifact_name == RUNTIME_MANIFEST_ARTIFACT_NAME {
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
        if component.executable {
            has_executable = true;
        } else if component.relative_path.contains(".so") {
            has_shared_library = true;
        } else {
            return Err(RuntimeArtifactError::InvalidManifest(format!(
                "non-executable component {} is not a shared library",
                component.artifact_name
            )));
        }
    }
    if !has_executable || !has_shared_library {
        return Err(RuntimeArtifactError::InvalidManifest(
            "bundle must contain an executable and a shared library".to_string(),
        ));
    }
    Ok(())
}

fn verify_one_file(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    relative_path: &Path,
    expected_size: i64,
    expected_sha256: &str,
    executable: bool,
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
    let has_execute_bit = metadata.mode() & 0o111 != 0;
    if has_execute_bit != executable {
        return Err(RuntimeArtifactError::ModeMismatch {
            path: relative_path.to_path_buf(),
        });
    }
    Ok((digest.sha256, expected_size))
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
    executable: bool,
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
    if (before.mode() & 0o111 != 0) != executable {
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

    fn component(name: &str, path: &str, bytes: &[u8], executable: bool) -> RuntimeBundleComponent {
        RuntimeBundleComponent {
            artifact_name: name.to_string(),
            relative_path: path.to_string(),
            sha256: hex_sha256(bytes),
            size_bytes: i64::try_from(bytes.len()).unwrap(),
            executable,
        }
    }

    fn manifest() -> RuntimeBundleManifest {
        RuntimeBundleManifest {
            schema_version: RUNTIME_BUNDLE_SCHEMA_VERSION,
            bundle_version: "2026.8.5_logan_1".to_string(),
            llama_cpp_commit: "6dc4086b7217cb8c2ccc1945b1e1f3213b9b1941".to_string(),
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            platform: RuntimeBundlePlatform {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                os_id: "ubuntu".to_string(),
                os_version_id: "26.04".to_string(),
                hip_version: "7.1".to_string(),
                gpu_arches: vec!["gfx1151".to_string()],
            },
            components: vec![
                component("libllama", "libllama.so", b"library", false),
                component("llama-server", "llama-server", b"server", true),
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
                "libllama.so" => b"library",
                "llama-server" => b"server",
                _ => panic!("test component bytes missing"),
            };
            let path = directory.join(&component.relative_path);
            std::fs::write(&path, bytes).unwrap();
            let mode = if component.executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let relative = PathBuf::from("logan-runtime/runtime-manifest.json");
        std::fs::write(root.join(&relative), serde_json::to_vec(manifest).unwrap()).unwrap();
        std::fs::set_permissions(root.join(&relative), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        relative
    }

    #[test]
    fn canonical_bundle_emits_only_exact_file_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let relative = write_bundle(root.path(), &manifest());
        let batch = verify_runtime_bundle_at(&identity("logan"), root.path(), &relative).unwrap();

        assert_eq!(batch.manifest_artifact_name, RUNTIME_MANIFEST_ARTIFACT_NAME);
        assert_eq!(batch.artifacts.len(), 3);
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
