//! API key authentication and simple rate limiting for ForgeFleet.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU32, Ordering};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Permission scope attached to an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Admin,
    Read,
    Write,
    Proxy,
}

// ---------------------------------------------------------------------------
// ApiKey
// ---------------------------------------------------------------------------

/// Stored representation of an API key (never stores the raw key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub name: String,
    /// Hex-encoded SHA-256 hash of the raw key.
    pub key_hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<Scope>,
}

impl ApiKey {
    /// Returns `true` when the key has the given scope.
    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope) || self.scopes.contains(&Scope::Admin)
    }

    /// Returns `true` when the key has not yet expired.
    pub fn is_valid(&self) -> bool {
        match self.expires_at {
            Some(exp) => Utc::now() < exp,
            None => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Key generation / verification
// ---------------------------------------------------------------------------

/// Prefix used for generated API keys so they are easy to grep in logs.
const KEY_PREFIX: &str = "ff_";

/// Length of the random portion (bytes) — 32 bytes → 44 base64 chars.
const KEY_RANDOM_BYTES: usize = 32;

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Generate a new API key.
///
/// Returns `(plaintext_key, ApiKey)`.  The plaintext key is shown to the
/// caller exactly once; afterwards only the hash is stored.
pub fn generate_api_key(
    name: &str,
    scopes: Vec<Scope>,
    expires_at: Option<DateTime<Utc>>,
) -> (String, ApiKey) {
    let mut rng = rand::thread_rng();
    let random_bytes: Vec<u8> = (0..KEY_RANDOM_BYTES).map(|_| rng.r#gen()).collect();
    let raw_key = format!(
        "{KEY_PREFIX}{}",
        base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            &random_bytes,
        )
    );
    let key_hash = sha256_hex(&raw_key);

    let api_key = ApiKey {
        id: Uuid::new_v4(),
        name: name.to_string(),
        key_hash,
        created_at: Utc::now(),
        expires_at,
        scopes,
    };

    (raw_key, api_key)
}

/// Verify that `provided` hashes to the same value as `stored_hash`.
pub fn verify_key(provided: &str, stored_hash: &str) -> bool {
    sha256_hex(provided) == stored_hash
}

// ---------------------------------------------------------------------------
// ApiKeyStore
// ---------------------------------------------------------------------------

/// Thread-safe in-memory key store.
#[derive(Debug, Default)]
pub struct ApiKeyStore {
    /// Keyed by `key_hash` for O(1) lookup on every request.
    keys: DashMap<String, ApiKey>,
}

impl ApiKeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a key into the store.
    pub fn insert(&self, key: ApiKey) {
        self.keys.insert(key.key_hash.clone(), key);
    }

    /// Look up a key by hashing the provided plaintext.
    pub fn lookup(&self, plaintext: &str) -> Option<ApiKey> {
        let hash = sha256_hex(plaintext);
        self.keys.get(&hash).map(|r| r.value().clone())
    }

    /// Look up directly by hash.
    pub fn lookup_by_hash(&self, hash: &str) -> Option<ApiKey> {
        self.keys.get(hash).map(|r| r.value().clone())
    }

    /// Remove a key by its UUID id.
    pub fn remove_by_id(&self, id: Uuid) -> bool {
        let maybe_hash = self
            .keys
            .iter()
            .find(|r| r.value().id == id)
            .map(|r| r.key().clone());
        if let Some(hash) = maybe_hash {
            self.keys.remove(&hash);
            true
        } else {
            false
        }
    }

    /// Number of stored keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Returns `true` when the store is empty.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Header extraction
// ---------------------------------------------------------------------------

/// Extract an API key from HTTP headers.
///
/// Checks (in order):
/// 1. `Authorization: Bearer <key>`
/// 2. `X-API-Key: <key>`
pub fn extract_api_key_from_headers(headers: &http::HeaderMap) -> Option<String> {
    // Try Authorization: Bearer first
    if let Some(val) = headers.get(http::header::AUTHORIZATION)
        && let Ok(s) = val.to_str()
    {
        let s = s.trim();
        if let Some(token) = s.strip_prefix("Bearer ") {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }

    // Fallback: X-API-Key
    if let Some(val) = headers.get("x-api-key")
        && let Ok(s) = val.to_str()
    {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Simple token-bucket rate limiter (per key id)
// ---------------------------------------------------------------------------

/// Per-key sliding-window counter for rate limiting.
#[derive(Debug)]
struct KeyBucket {
    count: AtomicU32,
    window_start: std::sync::Mutex<DateTime<Utc>>,
}

/// Simple fixed-window rate limiter keyed by arbitrary string (typically key id).
#[derive(Debug)]
pub struct ApiRateLimiter {
    pub max_requests: u32,
    pub window_secs: i64,
    buckets: DashMap<String, KeyBucket>,
}

impl ApiRateLimiter {
    pub fn new(max_requests: u32, window_secs: i64) -> Self {
        Self {
            max_requests,
            window_secs,
            buckets: DashMap::new(),
        }
    }

    /// Returns `true` if the request is allowed, `false` if rate-limited.
    pub fn check(&self, key_id: &str) -> bool {
        let now = Utc::now();

        // Get or create bucket
        let bucket = self
            .buckets
            .entry(key_id.to_string())
            .or_insert_with(|| KeyBucket {
                count: AtomicU32::new(0),
                window_start: std::sync::Mutex::new(now),
            });

        let mut start = bucket.window_start.lock().unwrap();
        let elapsed = (now - *start).num_seconds();

        if elapsed >= self.window_secs {
            // Reset window
            *start = now;
            bucket.count.store(1, Ordering::SeqCst);
            return true;
        }

        let current = bucket.count.fetch_add(1, Ordering::SeqCst);
        current < self.max_requests
    }
}

// ---------------------------------------------------------------------------
// Hex encoding helper (no extra dep needed — tiny)
// ---------------------------------------------------------------------------

mod hex {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        let bytes = bytes.as_ref();
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX_CHARS[(b >> 4) as usize] as char);
            s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_verify() {
        let (plaintext, api_key) = generate_api_key("test-key", vec![Scope::Read], None);

        assert!(plaintext.starts_with("ff_"));
        assert!(!api_key.key_hash.is_empty());
        assert!(verify_key(&plaintext, &api_key.key_hash));
        assert!(!verify_key("wrong-key", &api_key.key_hash));
    }

    #[test]
    fn test_scope_check() {
        let (_, key) = generate_api_key("admin", vec![Scope::Admin], None);
        assert!(key.has_scope(Scope::Read));
        assert!(key.has_scope(Scope::Write));
        assert!(key.has_scope(Scope::Admin));

        let (_, key2) = generate_api_key("reader", vec![Scope::Read], None);
        assert!(key2.has_scope(Scope::Read));
        assert!(!key2.has_scope(Scope::Write));
        assert!(!key2.has_scope(Scope::Admin));
    }

    #[test]
    fn test_expiry() {
        let past = Utc::now() - chrono::Duration::hours(1);
        let (_, expired) = generate_api_key("old", vec![Scope::Read], Some(past));
        assert!(!expired.is_valid());

        let future = Utc::now() + chrono::Duration::hours(1);
        let (_, fresh) = generate_api_key("new", vec![Scope::Read], Some(future));
        assert!(fresh.is_valid());

        let (_, forever) = generate_api_key("forever", vec![Scope::Read], None);
        assert!(forever.is_valid());
    }

    #[test]
    fn test_key_store_lookup() {
        let store = ApiKeyStore::new();
        let (plaintext, key) = generate_api_key("svc", vec![Scope::Write], None);
        let id = key.id;
        store.insert(key);

        assert_eq!(store.len(), 1);

        let found = store.lookup(&plaintext).expect("should find key");
        assert_eq!(found.name, "svc");

        assert!(store.lookup("bogus").is_none());

        assert!(store.remove_by_id(id));
        assert!(store.is_empty());
    }

    #[test]
    fn test_extract_bearer_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer my-secret-key"),
        );

        assert_eq!(
            extract_api_key_from_headers(&headers),
            Some("my-secret-key".to_string())
        );
    }

    #[test]
    fn test_extract_x_api_key_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-api-key", http::HeaderValue::from_static("another-key"));

        assert_eq!(
            extract_api_key_from_headers(&headers),
            Some("another-key".to_string())
        );
    }

    #[test]
    fn test_bearer_takes_precedence() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer bearer-val"),
        );
        headers.insert("x-api-key", http::HeaderValue::from_static("xapi-val"));

        assert_eq!(
            extract_api_key_from_headers(&headers),
            Some("bearer-val".to_string())
        );
    }

    #[test]
    fn test_no_key_in_headers() {
        let headers = http::HeaderMap::new();
        assert!(extract_api_key_from_headers(&headers).is_none());
    }

    #[test]
    fn test_rate_limiter_allows_within_budget() {
        let limiter = ApiRateLimiter::new(3, 60);
        assert!(limiter.check("k1"));
        assert!(limiter.check("k1"));
        assert!(limiter.check("k1"));
        // 4th should be denied
        assert!(!limiter.check("k1"));
    }

    #[test]
    fn test_rate_limiter_separate_keys() {
        let limiter = ApiRateLimiter::new(1, 60);
        assert!(limiter.check("a"));
        assert!(!limiter.check("a")); // exhausted
        assert!(limiter.check("b")); // different key, fresh bucket
    }
}
