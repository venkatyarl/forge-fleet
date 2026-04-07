//! Secret reference interfaces.
//!
//! This module intentionally does **not** store or expose secret values.
//! Resolvers return metadata/handles only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecretRef {
    /// Resolver/provider identifier (e.g. `1password`, `vault`, `env`).
    pub provider: String,
    /// Logical key or path in the provider.
    pub key: String,
    /// Optional version label.
    pub version: Option<String>,
}

impl SecretRef {
    pub fn new(
        provider: impl Into<String>,
        key: impl Into<String>,
        version: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            key: key.into(),
            version,
        }
    }
}

/// Opaque, non-sensitive metadata describing a resolved reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretHandle {
    pub provider: String,
    pub key: String,
    pub version: Option<String>,
    pub lease_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Error)]
pub enum SecretResolutionError {
    #[error("secret provider is unavailable: {provider}")]
    ProviderUnavailable { provider: String },

    #[error("secret reference not found: {provider}/{key}")]
    NotFound { provider: String, key: String },

    #[error("invalid secret reference: {0}")]
    InvalidReference(String),

    #[error("permission denied for secret reference: {provider}/{key}")]
    PermissionDenied { provider: String, key: String },

    #[error("secret resolver failure: {0}")]
    Other(String),
}

pub type SecretResolutionResult<T> = std::result::Result<T, SecretResolutionError>;

/// Async resolver contract.
///
/// Implementations may fetch secrets from external systems, but must only
/// return `SecretHandle` metadata in this API.
pub trait SecretResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        reference: &'a SecretRef,
    ) -> Pin<Box<dyn Future<Output = SecretResolutionResult<SecretHandle>> + Send + 'a>>;
}
