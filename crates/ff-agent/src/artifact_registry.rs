//! Verified local release-artifact registration.
//!
//! Paths are accepted only relative to a fixed release-build root. The shared
//! descriptor-relative verifier hashes an open file and performs a final
//! identity pass before this module immediately submits immutable evidence to
//! PostgreSQL. Re-verification is still required when an artifact is consumed:
//! userspace cannot make the filesystem pass and database commit one atomic
//! snapshot.

use std::ffi::OsStr;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use ff_core::model_integrity::{
    ModelArtifactKind, ModelIntegrityError, ModelIntegrityLimits, constant_time_sha256_eq,
    parse_sha256_hex, verify_model_path,
};
use ff_db::{
    DbError, PgPool, ReleaseArtifactAssertion, ReleaseArtifactRegistration,
    pg_register_release_artifact,
};

use crate::fleet_info::LocalComputerIdentity;

pub(crate) const RELEASE_ARTIFACT_NAMES: [&str; 2] = ["ff", "forgefleetd"];

#[derive(Debug, Clone)]
pub struct LocalReleaseArtifactSpec {
    pub artifact_name: String,
    pub artifact_version: String,
    pub source_commit: String,
    pub target_triple: String,
    pub expected_sha256: String,
    pub expected_size_bytes: i64,
    pub relative_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactRegistryError {
    #[error("could not resolve the current user's home directory")]
    HomeUnavailable,
    #[error("expected sha256 must be exactly 64 lowercase hexadecimal characters")]
    InvalidSha256,
    #[error("expected artifact size must be positive")]
    InvalidSize,
    #[error("verified artifact must be a single regular file")]
    NotARegularFile,
    #[error("verified artifact size mismatch: expected {expected}, found {actual}")]
    SizeMismatch { expected: i64, actual: u64 },
    #[error("verified artifact sha256 does not match the expected digest")]
    DigestMismatch,
    #[error("verified relative path is not valid UTF-8")]
    NonUtf8Path,
    #[error("release artifact name must be exactly ff or forgefleetd")]
    InvalidArtifactName,
    #[error("release artifact relative-path basename must exactly match its artifact name")]
    ArtifactBasenameMismatch,
    #[error("release artifact custody path must be a non-empty normal relative path")]
    InvalidCustodyPath,
    #[error("could not inspect release artifact custody parent {path}: {source}")]
    CustodyParentInspection {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "release artifact custody parent is not a private effective-user-owned directory: {path}"
    )]
    UnsafeCustodyParent { path: PathBuf },
    #[error(transparent)]
    Filesystem(#[from] ModelIntegrityError),
    #[error(transparent)]
    Database(#[from] DbError),
}

/// The fixed authority root used by `ff artifact register`.
pub(crate) fn local_release_build_root() -> Result<PathBuf, ArtifactRegistryError> {
    authority_home_dir()
        .map(|home| home.join(".forgefleet").join("release-builds"))
        .ok_or(ArtifactRegistryError::HomeUnavailable)
}

/// Resolve the effective user's account home without consulting `HOME` or
/// `FORGEFLEET_HOME`. Those environment variables are convenient configuration
/// inputs but are not suitable roots for a custody assertion.
#[cfg(unix)]
pub(crate) fn authority_home_dir() -> Option<PathBuf> {
    use std::ffi::{CStr, OsString};
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStringExt;
    use std::ptr;

    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut capacity = if suggested > 0 {
        usize::try_from(suggested).ok()?.clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    loop {
        let mut password = MaybeUninit::<libc::passwd>::uninit();
        let mut result: *mut libc::passwd = ptr::null_mut();
        let mut buffer = vec![0_u8; capacity];
        // SAFETY: all pointers refer to live writable storage for this call;
        // `result` is checked before reading the initialized passwd record.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                password.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && capacity < 1024 * 1024 {
            capacity = (capacity * 2).min(1024 * 1024);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        // SAFETY: successful getpwuid_r initialized `password`; `pw_dir`
        // points into `buffer`, which remains alive through the CStr copy.
        let password = unsafe { password.assume_init() };
        if password.pw_dir.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(password.pw_dir) }
            .to_bytes()
            .to_vec();
        if bytes.is_empty() {
            return None;
        }
        return Some(PathBuf::from(OsString::from_vec(bytes)));
    }
}

#[cfg(not(unix))]
pub(crate) fn authority_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Verify local bytes and immediately submit immutable artifact/custody
/// evidence. No remote path or caller-supplied holder is accepted.
pub async fn register_local_release_artifact(
    pool: &PgPool,
    identity: &LocalComputerIdentity,
    spec: &LocalReleaseArtifactSpec,
) -> Result<ReleaseArtifactRegistration, ArtifactRegistryError> {
    let release_root = local_release_build_root()?;
    let registration =
        verify_then_register(identity, &release_root, spec, |assertion| async move {
            pg_register_release_artifact(pool, &assertion).await
        })
        .await?;
    validate_release_artifact_basename(
        &registration.artifact.artifact_name,
        Path::new(&registration.custody.relative_path),
    )?;
    Ok(registration)
}

async fn verify_then_register<R, F, Fut>(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    spec: &LocalReleaseArtifactSpec,
    register: F,
) -> Result<R, ArtifactRegistryError>
where
    F: FnOnce(ReleaseArtifactAssertion) -> Fut,
    Fut: Future<Output = Result<R, DbError>>,
{
    let assertion = verify_release_evidence(identity, release_root, spec)?;
    Ok(register(assertion).await?)
}

fn verify_release_evidence(
    identity: &LocalComputerIdentity,
    release_root: &Path,
    spec: &LocalReleaseArtifactSpec,
) -> Result<ReleaseArtifactAssertion, ArtifactRegistryError> {
    if parse_sha256_hex(&spec.expected_sha256).is_none()
        || spec
            .expected_sha256
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    {
        return Err(ArtifactRegistryError::InvalidSha256);
    }
    if spec.expected_size_bytes <= 0 {
        return Err(ArtifactRegistryError::InvalidSize);
    }

    validate_private_custody_parents(release_root, &spec.relative_path)?;

    let expected_bytes =
        u64::try_from(spec.expected_size_bytes).map_err(|_| ArtifactRegistryError::InvalidSize)?;
    let digest = verify_model_path(
        &identity.name,
        release_root,
        &spec.relative_path,
        ModelIntegrityLimits {
            max_files: 1,
            max_bytes: expected_bytes,
            max_depth: 16,
            timeout: Duration::from_secs(5 * 60),
        },
    )?;
    if digest.kind != ModelArtifactKind::File
        || digest.files != 1
        || digest.entries != 1
        || digest.algorithm != "sha256"
    {
        return Err(ArtifactRegistryError::NotARegularFile);
    }
    if digest.bytes != expected_bytes {
        return Err(ArtifactRegistryError::SizeMismatch {
            expected: spec.expected_size_bytes,
            actual: digest.bytes,
        });
    }
    if !constant_time_sha256_eq(&digest.sha256, &spec.expected_sha256) {
        return Err(ArtifactRegistryError::DigestMismatch);
    }
    validate_release_artifact_basename(&spec.artifact_name, &spec.relative_path)?;
    // Re-open the fixed path immediately before producing database evidence.
    // The first pass prevents hashing through an unsafe parent; this pass also
    // catches an ownership or mode change made while the file was hashed.
    validate_private_custody_parents(release_root, &spec.relative_path)?;

    let relative_path = spec
        .relative_path
        .to_str()
        .ok_or(ArtifactRegistryError::NonUtf8Path)?
        .to_string();
    Ok(ReleaseArtifactAssertion {
        artifact_name: spec.artifact_name.clone(),
        artifact_version: spec.artifact_version.clone(),
        source_commit: spec.source_commit.clone(),
        target_triple: spec.target_triple.clone(),
        sha256: digest.sha256,
        size_bytes: spec.expected_size_bytes,
        computer_id: identity.id,
        holder_name: identity.name.clone(),
        relative_path,
    })
}

/// One policy predicate is shared by release registration and activation so
/// the producer and consumer cannot drift on directory ownership or modes.
#[cfg(unix)]
pub(crate) fn is_private_effective_user_directory(mode: u64, uid: u64, effective_uid: u64) -> bool {
    mode & u64::from(libc::S_IFMT) == u64::from(libc::S_IFDIR)
        && uid == effective_uid
        && mode & 0o022 == 0
}

#[cfg(unix)]
fn validate_private_custody_parents(
    release_root: &Path,
    relative_path: &Path,
) -> Result<(), ArtifactRegistryError> {
    let components = relative_path
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(ArtifactRegistryError::InvalidCustodyPath),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.is_empty() {
        return Err(ArtifactRegistryError::InvalidCustodyPath);
    }

    let effective_uid = u64::from(unsafe { libc::geteuid() });
    let mut current_path = release_root.to_path_buf();
    let mut directory = open_private_directory(release_root, &current_path, effective_uid)?;
    for name in &components[..components.len() - 1] {
        current_path.push(name);
        directory =
            open_private_directory_at(directory.as_raw_fd(), name, &current_path, effective_uid)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_custody_parents(
    _release_root: &Path,
    relative_path: &Path,
) -> Result<(), ArtifactRegistryError> {
    if relative_path.as_os_str().is_empty()
        || relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ArtifactRegistryError::InvalidCustodyPath);
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_directory(
    path: &Path,
    display_path: &Path,
    effective_uid: u64,
) -> Result<OwnedFd, ArtifactRegistryError> {
    let encoded = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ArtifactRegistryError::InvalidCustodyPath)?;
    let raw = unsafe {
        libc::open(
            encoded.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    checked_private_directory(raw, display_path, effective_uid)
}

#[cfg(unix)]
fn open_private_directory_at(
    parent: RawFd,
    name: &OsStr,
    display_path: &Path,
    effective_uid: u64,
) -> Result<OwnedFd, ArtifactRegistryError> {
    let encoded =
        CString::new(name.as_bytes()).map_err(|_| ArtifactRegistryError::InvalidCustodyPath)?;
    let raw = unsafe {
        libc::openat(
            parent,
            encoded.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    checked_private_directory(raw, display_path, effective_uid)
}

#[cfg(unix)]
fn checked_private_directory(
    raw: RawFd,
    display_path: &Path,
    effective_uid: u64,
) -> Result<OwnedFd, ArtifactRegistryError> {
    if raw < 0 {
        return Err(ArtifactRegistryError::CustodyParentInspection {
            path: display_path.to_path_buf(),
            source: std::io::Error::last_os_error(),
        });
    }
    // The OwnedFd pins the opened inode for fstat and closes it on every later
    // error path; renaming the directory cannot redirect this descriptor.
    let directory = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(directory.as_raw_fd(), &mut stat) } != 0 {
        return Err(ArtifactRegistryError::CustodyParentInspection {
            path: display_path.to_path_buf(),
            source: std::io::Error::last_os_error(),
        });
    }
    if !is_private_effective_user_directory(
        u64::from(stat.st_mode),
        u64::from(stat.st_uid),
        effective_uid,
    ) {
        return Err(ArtifactRegistryError::UnsafeCustodyParent {
            path: display_path.to_path_buf(),
        });
    }
    Ok(directory)
}

pub(crate) fn validate_release_artifact_basename(
    artifact_name: &str,
    relative_path: &Path,
) -> Result<(), ArtifactRegistryError> {
    if !RELEASE_ARTIFACT_NAMES.contains(&artifact_name) {
        return Err(ArtifactRegistryError::InvalidArtifactName);
    }
    if relative_path.file_name() != Some(OsStr::new(artifact_name)) {
        return Err(ArtifactRegistryError::ArtifactBasenameMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::cell::Cell;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use uuid::Uuid;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn identity() -> LocalComputerIdentity {
        LocalComputerIdentity {
            id: Uuid::new_v4(),
            name: "thalia".to_string(),
        }
    }

    fn spec(path: PathBuf) -> LocalReleaseArtifactSpec {
        LocalReleaseArtifactSpec {
            artifact_name: "ff".to_string(),
            artifact_version: "2026.8.5_1".to_string(),
            source_commit: "6dc4086b7217cb8c2ccc1945b1e1f3213b9b1941".to_string(),
            target_triple: "aarch64-unknown-linux-gnu".to_string(),
            expected_sha256: ABC_SHA256.to_string(),
            expected_size_bytes: 3,
            relative_path: path,
        }
    }

    #[cfg(unix)]
    fn set_directory_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verifies_a_normal_relative_file_under_the_fixed_root() {
        let root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("ff-build/artifact/ff");
        std::fs::create_dir_all(root.path().join("ff-build/artifact")).unwrap();
        std::fs::write(root.path().join(&relative), b"abc").unwrap();
        set_directory_mode(root.path(), 0o755);
        set_directory_mode(&root.path().join("ff-build"), 0o755);
        set_directory_mode(&root.path().join("ff-build/artifact"), 0o755);

        let evidence = verify_release_evidence(&identity(), root.path(), &spec(relative)).unwrap();
        assert_eq!(evidence.sha256, ABC_SHA256);
        assert_eq!(evidence.size_bytes, 3);
        assert_eq!(evidence.relative_path, "ff-build/artifact/ff");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_group_writable_root_unchanged_before_registration() {
        let root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("build/ff");
        std::fs::create_dir_all(root.path().join("build")).unwrap();
        std::fs::write(root.path().join(&relative), b"abc").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
        let registration_called = Cell::new(false);

        let error = verify_then_register(&identity(), root.path(), &spec(relative), |_| {
            registration_called.set(true);
            std::future::ready(Ok::<(), DbError>(()))
        })
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            ArtifactRegistryError::UnsafeCustodyParent { .. }
        ));
        assert!(!registration_called.get());
        assert_eq!(
            std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o775
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_writable_intermediate_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let intermediate = root.path().join("build");
        let relative = PathBuf::from("build/artifact/ff");
        std::fs::create_dir_all(intermediate.join("artifact")).unwrap();
        std::fs::write(root.path().join(&relative), b"abc").unwrap();
        set_directory_mode(root.path(), 0o755);
        set_directory_mode(&intermediate, 0o775);
        set_directory_mode(&intermediate.join("artifact"), 0o755);

        let error = verify_release_evidence(&identity(), root.path(), &spec(relative)).unwrap_err();

        assert!(matches!(
            error,
            ArtifactRegistryError::UnsafeCustodyParent { .. }
        ));
        assert_eq!(
            std::fs::metadata(intermediate)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o775
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepts_non_writable_0755_custody_parents() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let artifact = build.join("artifact");
        let relative = PathBuf::from("build/artifact/ff");
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(root.path().join(&relative), b"abc").unwrap();
        for directory in [root.path(), build.as_path(), artifact.as_path()] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(verify_release_evidence(&identity(), root.path(), &spec(relative)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_custody_parent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("ff"), b"abc").unwrap();
        set_directory_mode(root.path(), 0o755);
        set_directory_mode(&real, 0o755);
        symlink(&real, root.path().join("linked")).unwrap();

        assert!(
            verify_release_evidence(&identity(), root.path(), &spec(PathBuf::from("linked/ff")))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_custody_root() {
        use std::os::unix::fs::symlink;

        let container = tempfile::tempdir().unwrap();
        let real_root = container.path().join("real-root");
        let linked_root = container.path().join("linked-root");
        std::fs::create_dir(&real_root).unwrap();
        std::fs::write(real_root.join("ff"), b"abc").unwrap();
        set_directory_mode(&real_root, 0o755);
        symlink(&real_root, &linked_root).unwrap();

        assert!(matches!(
            verify_release_evidence(&identity(), &linked_root, &spec(PathBuf::from("ff"))),
            Err(ArtifactRegistryError::CustodyParentInspection { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn requires_exact_logical_name_and_matching_relative_path_basename() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("build")).unwrap();
        std::fs::write(root.path().join("build/ff"), b"abc").unwrap();
        std::fs::write(root.path().join("build/forgefleetd"), b"abc").unwrap();
        set_directory_mode(root.path(), 0o755);
        set_directory_mode(&root.path().join("build"), 0o755);

        let mut daemon = spec(PathBuf::from("build/forgefleetd"));
        daemon.artifact_name = "forgefleetd".to_string();
        assert!(verify_release_evidence(&identity(), root.path(), &daemon).is_ok());

        for (artifact_name, relative_path, expected_error) in [
            (
                "agent",
                "build/ff",
                ArtifactRegistryError::InvalidArtifactName,
            ),
            (
                "ff",
                "build/forgefleetd",
                ArtifactRegistryError::ArtifactBasenameMismatch,
            ),
            (
                "forgefleetd",
                "build/ff",
                ArtifactRegistryError::ArtifactBasenameMismatch,
            ),
        ] {
            let mut invalid = spec(PathBuf::from(relative_path));
            invalid.artifact_name = artifact_name.to_string();
            let error = verify_release_evidence(&identity(), root.path(), &invalid).unwrap_err();
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected_error)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_absolute_parent_and_symlink_paths() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real"), b"abc").unwrap();
        symlink(root.path().join("real"), root.path().join("ff")).unwrap();
        set_directory_mode(root.path(), 0o755);

        for rejected in [
            PathBuf::from("../ff"),
            root.path().join("ff"),
            PathBuf::from("ff"),
        ] {
            assert!(
                verify_release_evidence(&identity(), root.path(), &spec(rejected)).is_err(),
                "spoofable path must fail closed"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_digest_size_and_vinny_evidence() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("ff"), b"abc").unwrap();
        set_directory_mode(root.path(), 0o755);

        let mut bad_digest = spec(PathBuf::from("ff"));
        bad_digest.expected_sha256 = "0".repeat(64);
        assert!(verify_release_evidence(&identity(), root.path(), &bad_digest).is_err());

        let mut bad_size = spec(PathBuf::from("ff"));
        bad_size.expected_size_bytes = 4;
        assert!(verify_release_evidence(&identity(), root.path(), &bad_size).is_err());

        let vinny = LocalComputerIdentity {
            id: Uuid::new_v4(),
            name: "VINNY".to_string(),
        };
        assert!(verify_release_evidence(&vinny, root.path(), &spec(PathBuf::from("ff"))).is_err());
    }
}
