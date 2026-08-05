//! Verified local release-artifact registration.
//!
//! Paths are accepted only relative to a fixed release-build root. The shared
//! descriptor-relative verifier hashes an open file and performs a final
//! identity pass before this module immediately submits immutable evidence to
//! PostgreSQL. Re-verification is still required when an artifact is consumed:
//! userspace cannot make the filesystem pass and database commit one atomic
//! snapshot.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ff_core::model_integrity::{
    ModelArtifactKind, ModelIntegrityError, ModelIntegrityLimits, constant_time_sha256_eq,
    parse_sha256_hex, verify_model_path,
};
use ff_db::{
    DbError, PgPool, ReleaseArtifactAssertion, ReleaseArtifactRegistration,
    pg_register_release_artifact,
};

use crate::fleet_info::LocalComputerIdentity;

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
    #[error(transparent)]
    Filesystem(#[from] ModelIntegrityError),
    #[error(transparent)]
    Database(#[from] DbError),
}

/// The fixed authority root used by `ff artifact register`.
fn local_release_build_root() -> Result<PathBuf, ArtifactRegistryError> {
    authority_home_dir()
        .map(|home| home.join(".forgefleet").join("release-builds"))
        .ok_or(ArtifactRegistryError::HomeUnavailable)
}

/// Resolve the effective user's account home without consulting `HOME` or
/// `FORGEFLEET_HOME`. Those environment variables are convenient configuration
/// inputs but are not suitable roots for a custody assertion.
#[cfg(unix)]
fn authority_home_dir() -> Option<PathBuf> {
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
fn authority_home_dir() -> Option<PathBuf> {
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
    let assertion = verify_release_evidence(identity, &release_root, spec)?;
    Ok(pg_register_release_artifact(pool, &assertion).await?)
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

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn verifies_a_normal_relative_file_under_the_fixed_root() {
        let root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("ff-build/artifact/ff");
        std::fs::create_dir_all(root.path().join("ff-build/artifact")).unwrap();
        std::fs::write(root.path().join(&relative), b"abc").unwrap();

        let evidence = verify_release_evidence(&identity(), root.path(), &spec(relative)).unwrap();
        assert_eq!(evidence.sha256, ABC_SHA256);
        assert_eq!(evidence.size_bytes, 3);
        assert_eq!(evidence.relative_path, "ff-build/artifact/ff");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_absolute_parent_and_symlink_paths() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real"), b"abc").unwrap();
        symlink(root.path().join("real"), root.path().join("link")).unwrap();

        for rejected in [
            PathBuf::from("../real"),
            root.path().join("real"),
            PathBuf::from("link"),
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
