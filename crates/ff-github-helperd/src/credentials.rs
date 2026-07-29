use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential source is not an approved absolute path")]
    UnapprovedPath,
    #[error("credential must be a root-owned regular file with no group/other access")]
    UnsafeMetadata,
    #[error("credential could not be loaded")]
    Unavailable,
}

/// A credential source chosen by the service administrator, never by a client.
pub struct CredentialFile {
    path: PathBuf,
}

impl CredentialFile {
    pub fn systemd(name: &str) -> Result<Self, CredentialError> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(CredentialError::UnapprovedPath);
        }
        Ok(Self {
            path: Path::new("/run/credentials/ff-github-helperd.service").join(name),
        })
    }

    pub fn root_owned(path: PathBuf) -> Result<Self, CredentialError> {
        if !path.is_absolute() || !path.starts_with("/etc/forgefleet/credentials/") {
            return Err(CredentialError::UnapprovedPath);
        }
        Ok(Self { path })
    }

    pub fn load(&self) -> Result<SecretBytes, CredentialError> {
        let metadata =
            fs::symlink_metadata(&self.path).map_err(|_| CredentialError::Unavailable)?;
        if !metadata.file_type().is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(CredentialError::UnsafeMetadata);
        }
        let bytes = fs::read(&self.path).map_err(|_| CredentialError::Unavailable)?;
        if bytes.is_empty() {
            return Err(CredentialError::Unavailable);
        }
        Ok(SecretBytes(bytes))
    }
}

pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub(crate) fn from_vec(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBytes([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_credential_paths_are_denied() {
        assert!(CredentialFile::root_owned(PathBuf::from("/tmp/app.pem")).is_err());
        assert!(CredentialFile::systemd("../escape").is_err());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretBytes(b"never-print-me".to_vec());
        assert_eq!(format!("{secret:?}"), "SecretBytes([REDACTED])");
    }
}
