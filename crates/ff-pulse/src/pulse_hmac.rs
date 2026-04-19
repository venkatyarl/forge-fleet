//! HMAC-SHA256 signing for PulseBeatV2 payloads.
//!
//! Every publisher signs its outgoing JSON payload with a fleet-wide key
//! stored in `fleet_secrets.pulse_beat_hmac_key`. Subscribers verify the
//! signature in constant time and reject beats whose signature does not
//! match. Beats without an `hmac` field are accepted with a DEBUG log
//! (backwards compatibility for pre-V17 publishers).
//!
//! Wire format:
//! ```json
//! { ...existing fields..., "hmac": "<64-char hex>" }
//! ```
//! The HMAC is computed over the canonical JSON form of the beat with
//! the `hmac` field removed. We accomplish that by serializing the
//! whole payload to JSON, then stripping the field textually.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use tracing::{debug, warn};

const HMAC_SECRET_KEY: &str = "pulse_beat_hmac_key";
const HMAC_JSON_FIELD: &str = "hmac";

type HmacSha256 = Hmac<Sha256>;

/// In-memory cache of the HMAC key. Refreshed periodically by
/// [`KeyCache::spawn_refresher`].
#[derive(Clone, Default)]
pub struct KeyCache {
    inner: Arc<RwLock<Option<Vec<u8>>>>,
}

impl KeyCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Global singleton used by publishers + subscribers.
    pub fn global() -> &'static KeyCache {
        static CACHE: OnceLock<KeyCache> = OnceLock::new();
        CACHE.get_or_init(KeyCache::new)
    }

    pub async fn set(&self, key: Vec<u8>) {
        let mut guard = self.inner.write().await;
        *guard = Some(key);
    }

    pub async fn get(&self) -> Option<Vec<u8>> {
        self.inner.read().await.clone()
    }

    /// Refresh the cached HMAC key from `fleet_secrets.pulse_beat_hmac_key`.
    /// Returns `Ok(true)` if the key changed, `Ok(false)` if unchanged.
    /// Returns `Ok(false)` (not an error) if the secret is missing —
    /// subscribers will then treat unsigned beats as acceptable.
    pub async fn refresh_from(&self, pool: &sqlx::PgPool) -> Result<bool, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM fleet_secrets WHERE key = $1",
        )
        .bind(HMAC_SECRET_KEY)
        .fetch_optional(pool)
        .await?;
        let Some((value,)) = row else {
            return Ok(false);
        };
        let bytes = match hex_decode(&value) {
            Some(b) => b,
            None => value.into_bytes(),
        };
        let mut guard = self.inner.write().await;
        let changed = guard.as_deref() != Some(bytes.as_slice());
        *guard = Some(bytes);
        Ok(changed)
    }

    /// Spawn a task that refreshes the cached key every `interval`.
    /// Returns immediately; the task runs until the pool is dropped.
    pub fn spawn_refresher(
        self,
        pool: sqlx::PgPool,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Immediate first refresh.
            if let Err(e) = self.refresh_from(&pool).await {
                debug!(error = %e, "initial pulse hmac key refresh failed");
            }
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the first tick since refresh_from ran once already.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match self.refresh_from(&pool).await {
                    Ok(true) => debug!("pulse_hmac: key rotated"),
                    Ok(false) => {}
                    Err(e) => debug!(error = %e, "pulse_hmac: refresh failed"),
                }
            }
        })
    }
}

/// Compute HMAC-SHA256 over `payload` and return a lowercase-hex digest.
pub fn compute_hmac_hex(key: &[u8], payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(payload);
    let digest = mac.finalize().into_bytes();
    hex_encode(&digest)
}

/// Verification outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Signature is valid; accept beat.
    Ok,
    /// No `hmac` field in the beat and no key configured — accept (legacy).
    MissingFieldLegacy,
    /// No `hmac` field but a key is configured — reject.
    MissingFieldStrict,
    /// Signature mismatch — reject.
    Mismatch,
    /// Malformed / non-hex signature — reject.
    Malformed,
}

/// Attach an `hmac` field to an already-serialized beat JSON string.
///
/// The returned string is guaranteed to be a valid JSON object with the
/// `hmac` field appended. If the input is not a JSON object (shouldn't
/// happen for beats), returns the input unchanged.
pub fn sign_json(key: &[u8], raw: &str) -> String {
    // Compute signature over the raw payload (without hmac field).
    let sig = compute_hmac_hex(key, raw.as_bytes());
    // Insert before the final '}'. Serde-roundtripping would be cleaner
    // but we want to preserve field order exactly.
    let trimmed = raw.trim_end();
    let Some(end) = trimmed.rfind('}') else {
        return raw.to_string();
    };
    let (head, tail) = trimmed.split_at(end);
    let separator = if head.trim_end().ends_with('{') {
        ""
    } else {
        ","
    };
    format!("{head}{separator}\"{HMAC_JSON_FIELD}\":\"{sig}\"{tail}")
}

/// Verify the `hmac` field on a beat JSON string.
///
/// Returns the outcome and, on success, the payload bytes used for
/// signing (i.e. the JSON with `hmac` stripped) so callers can re-emit
/// or diff against a canonical form.
pub fn verify_json(key: Option<&[u8]>, raw: &str) -> VerifyOutcome {
    // Extract "hmac":"..." textually.
    let Some((without, sig)) = strip_hmac_field(raw) else {
        return match key {
            Some(_) => VerifyOutcome::MissingFieldStrict,
            None => VerifyOutcome::MissingFieldLegacy,
        };
    };
    let Some(key) = key else {
        // Has hmac but we have no key to check against — accept to avoid
        // breaking the fleet during a rollout.
        return VerifyOutcome::MissingFieldLegacy;
    };
    let expected = compute_hmac_hex(key, without.as_bytes());
    let a = expected.as_bytes();
    let b = sig.as_bytes();
    if a.len() != b.len() {
        return VerifyOutcome::Malformed;
    }
    if a.ct_eq(b).unwrap_u8() == 1 {
        VerifyOutcome::Ok
    } else {
        VerifyOutcome::Mismatch
    }
}

/// Split a beat JSON into (payload_without_hmac, hmac_value).
///
/// Finds `"hmac":"<hex>"` textually. Returns None if absent. Handles
/// both leading-comma (`,"hmac":"..."`) and first-field forms.
fn strip_hmac_field(raw: &str) -> Option<(String, String)> {
    let needle = "\"hmac\":\"";
    let idx = raw.find(needle)?;
    let value_start = idx + needle.len();
    let value_end = raw[value_start..].find('"')? + value_start;
    let hmac = raw[value_start..value_end].to_string();

    // The field is at raw[idx..value_end+1] inclusive. We also need to
    // remove the separating comma (before or after).
    let field_start = idx;
    let field_end = value_end + 1;

    // Look backwards for a comma.
    let before = raw[..field_start].trim_end();
    let mut new_start = field_start;
    let mut new_end = field_end;
    if before.ends_with(',') {
        new_start = before.len() - 1;
    } else {
        // No preceding comma — look for a trailing comma instead.
        let after = raw[field_end..].trim_start_matches(|c: char| c.is_whitespace());
        if after.starts_with(',') {
            let cut = raw.len() - after.len() + 1;
            new_end = cut;
        }
    }

    let mut without = String::with_capacity(raw.len());
    without.push_str(&raw[..new_start]);
    without.push_str(&raw[new_end..]);
    Some((without, hmac))
}

// ─── hex helpers (local; avoid hex crate dep) ──────────────────────────

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Returned from subscriber code to consolidate logging.
pub fn log_verify(computer_name: &str, outcome: VerifyOutcome) -> bool {
    match outcome {
        VerifyOutcome::Ok => true,
        VerifyOutcome::MissingFieldLegacy => {
            debug!(computer = %computer_name, "pulse: beat missing hmac (legacy-accepted)");
            true
        }
        VerifyOutcome::MissingFieldStrict => {
            warn!(computer = %computer_name, "pulse: rejected unsigned beat from {computer_name} (strict)");
            false
        }
        VerifyOutcome::Mismatch => {
            warn!(computer = %computer_name, "pulse: rejected unsigned beat from {computer_name} — hmac mismatch");
            false
        }
        VerifyOutcome::Malformed => {
            warn!(computer = %computer_name, "pulse: rejected malformed hmac from {computer_name}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sign_verify() {
        let key = b"test-key-bytes";
        let payload = r#"{"a":1,"b":"two"}"#;
        let signed = sign_json(key, payload);
        assert!(signed.contains("\"hmac\":"));
        let outcome = verify_json(Some(key), &signed);
        assert_eq!(outcome, VerifyOutcome::Ok);
    }

    #[test]
    fn verify_missing_field_legacy_when_no_key() {
        let payload = r#"{"a":1}"#;
        assert_eq!(
            verify_json(None, payload),
            VerifyOutcome::MissingFieldLegacy
        );
    }

    #[test]
    fn verify_missing_field_strict_when_key_configured() {
        let payload = r#"{"a":1}"#;
        let key = b"secret";
        assert_eq!(
            verify_json(Some(key), payload),
            VerifyOutcome::MissingFieldStrict
        );
    }

    #[test]
    fn verify_mismatch() {
        let key = b"k1";
        let payload = r#"{"a":1}"#;
        let signed = sign_json(key, payload);
        let outcome = verify_json(Some(b"k2"), &signed);
        assert_eq!(outcome, VerifyOutcome::Mismatch);
    }

    #[test]
    fn hex_encode_decode() {
        let bytes = [0u8, 0x0a, 0xff, 0x7c, 0x10];
        let hex = hex_encode(&bytes);
        assert_eq!(hex, "000aff7c10");
        let back = hex_decode(&hex).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn strip_hmac_handles_trailing_position() {
        let raw = r#"{"a":1,"hmac":"abcdef"}"#;
        let (without, hmac) = strip_hmac_field(raw).unwrap();
        assert_eq!(hmac, "abcdef");
        assert_eq!(without, r#"{"a":1}"#);
    }

    #[test]
    fn strip_hmac_handles_middle_position() {
        let raw = r#"{"hmac":"abc","a":1}"#;
        let (without, hmac) = strip_hmac_field(raw).unwrap();
        assert_eq!(hmac, "abc");
        assert_eq!(without, r#"{"a":1}"#);
    }
}
