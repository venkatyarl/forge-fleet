use std::time::{Duration, SystemTime};

use thiserror::Error;

use crate::credentials::{CredentialError, CredentialFile, SecretBytes};

const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("GitHub App credential unavailable")]
    Credential,
    #[error("GitHub App token endpoint rejected the request")]
    Endpoint,
    #[error("installation token lifetime is invalid")]
    InvalidLifetime,
}

impl From<CredentialError> for TokenError {
    fn from(_: CredentialError) -> Self {
        Self::Credential
    }
}

/// Secret token bytes have no public accessor and are zeroed on drop.
pub struct InstallationToken {
    bytes: SecretBytes,
    expires_at: SystemTime,
}

impl InstallationToken {
    pub fn with_bytes<T>(&self, callback: impl FnOnce(&[u8]) -> T) -> T {
        callback(self.bytes.expose())
    }

    pub fn expires_at(&self) -> SystemTime {
        self.expires_at
    }
}

impl std::fmt::Debug for InstallationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallationToken")
            .field("bytes", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// In-process HTTPS endpoint seam. Production implementations sign the GitHub
/// App JWT and exchange it without spawning a process. Tests use a fake
/// endpoint, proving that the private key and returned token never enter Git.
pub trait GitHubAppEndpoint: Send + Sync {
    fn mint(
        &self,
        app_id: u64,
        installation_id: u64,
        private_key: &[u8],
    ) -> Result<(Vec<u8>, SystemTime), TokenError>;
}

pub struct TokenBroker<E> {
    app_id: u64,
    installation_id: u64,
    credential: CredentialFile,
    endpoint: E,
}

impl<E: GitHubAppEndpoint> TokenBroker<E> {
    pub fn new(app_id: u64, installation_id: u64, credential: CredentialFile, endpoint: E) -> Self {
        Self {
            app_id,
            installation_id,
            credential,
            endpoint,
        }
    }

    pub fn mint(&self, now: SystemTime) -> Result<InstallationToken, TokenError> {
        let private_key = self.credential.load()?;
        self.exchange(now, private_key)
    }

    fn exchange(
        &self,
        now: SystemTime,
        private_key: SecretBytes,
    ) -> Result<InstallationToken, TokenError> {
        let (token, expires_at) =
            self.endpoint
                .mint(self.app_id, self.installation_id, private_key.expose())?;
        let lifetime = expires_at
            .duration_since(now)
            .map_err(|_| TokenError::InvalidLifetime)?;
        if token.is_empty() || lifetime.is_zero() || lifetime > MAX_TOKEN_LIFETIME {
            return Err(TokenError::InvalidLifetime);
        }
        Ok(InstallationToken {
            bytes: SecretBytes::from_vec(token),
            expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeEndpoint;
    impl GitHubAppEndpoint for FakeEndpoint {
        fn mint(
            &self,
            app_id: u64,
            installation_id: u64,
            private_key: &[u8],
        ) -> Result<(Vec<u8>, SystemTime), TokenError> {
            assert_eq!((app_id, installation_id), (7, 9));
            assert_eq!(private_key, b"hostile-fixture-private-key");
            Ok((
                b"ghs_fake-endpoint-token".to_vec(),
                SystemTime::UNIX_EPOCH + Duration::from_secs(300),
            ))
        }
    }

    #[test]
    fn token_debug_never_discloses_token() {
        let token = InstallationToken {
            bytes: SecretBytes::from_vec(b"ghs_hostile-secret".to_vec()),
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(60),
        };
        let debug = format!("{token:?}");
        assert!(!debug.contains("ghs_"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn fake_endpoint_mints_only_short_lived_in_memory_token() {
        let broker = TokenBroker::new(
            7,
            9,
            CredentialFile::systemd("github-app.pem").unwrap(),
            FakeEndpoint,
        );
        let token = broker
            .exchange(
                SystemTime::UNIX_EPOCH,
                SecretBytes::from_vec(b"hostile-fixture-private-key".to_vec()),
            )
            .unwrap();
        assert_eq!(
            token.expires_at(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(300)
        );
        assert_eq!(token.with_bytes(|bytes| bytes.len()), 23);
    }

    #[test]
    fn fake_endpoint_overlong_token_is_denied() {
        struct Overlong;
        impl GitHubAppEndpoint for Overlong {
            fn mint(&self, _: u64, _: u64, _: &[u8]) -> Result<(Vec<u8>, SystemTime), TokenError> {
                Ok((
                    b"token".to_vec(),
                    SystemTime::UNIX_EPOCH + MAX_TOKEN_LIFETIME + Duration::from_secs(1),
                ))
            }
        }
        let broker = TokenBroker::new(
            7,
            9,
            CredentialFile::systemd("github-app.pem").unwrap(),
            Overlong,
        );
        assert!(matches!(
            broker.exchange(
                SystemTime::UNIX_EPOCH,
                SecretBytes::from_vec(b"key".to_vec())
            ),
            Err(TokenError::InvalidLifetime)
        ));
    }
}
