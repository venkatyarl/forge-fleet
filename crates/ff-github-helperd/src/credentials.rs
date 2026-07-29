use std::{
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use thiserror::Error;
use zeroize::Zeroizing;

const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("systemd credential directory is unavailable")]
    MissingDirectory,
    #[error("credential name is invalid")]
    InvalidName,
    #[error("credential metadata is unsafe")]
    UnsafeMetadata,
    #[error("credential read failed")]
    Read,
}

pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBytes([REDACTED])")
    }
}

pub fn load_systemd_credential(name: &str) -> Result<SecretBytes, CredentialError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(CredentialError::InvalidName);
    }
    let directory =
        std::env::var_os("CREDENTIALS_DIRECTORY").ok_or(CredentialError::MissingDirectory)?;
    let directory = PathBuf::from(directory);
    if directory != Path::new("/run/credentials/forgefleet-github-helperd.service") {
        return Err(CredentialError::MissingDirectory);
    }
    load_exact(&directory.join(name))
}

fn load_exact(path: &Path) -> Result<SecretBytes, CredentialError> {
    let mut file = File::open(path).map_err(|_| CredentialError::Read)?;
    let before = file.metadata().map_err(|_| CredentialError::Read)?;
    if !before.file_type().is_file()
        || before.uid() != 0
        || before.mode() & 0o077 != 0
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > MAX_CREDENTIAL_BYTES
    {
        return Err(CredentialError::UnsafeMetadata);
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(before.len() as usize));
    file.read_to_end(&mut bytes)
        .map_err(|_| CredentialError::Read)?;
    let after = file.metadata().map_err(|_| CredentialError::Read)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || bytes.len() as u64 != before.len()
    {
        return Err(CredentialError::UnsafeMetadata);
    }
    Ok(SecretBytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_names_are_rejected() {
        assert!(matches!(
            load_systemd_credential("../secret"),
            Err(CredentialError::InvalidName)
        ));
    }
}
