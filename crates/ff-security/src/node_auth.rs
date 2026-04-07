//! Node-to-node HMAC-SHA256 authentication for ForgeFleet.
//!
//! Every inter-node HTTP request is signed with a shared secret.  The
//! receiving node verifies the signature and checks that the timestamp is
//! recent (replay protection).

use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Hex encoding helper (matches auth.rs — zero extra deps)
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

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        if !s.len().is_multiple_of(2) {
            return None;
        }
        let mut bytes = Vec::with_capacity(s.len() / 2);
        for chunk in s.as_bytes().chunks(2) {
            let hi = from_hex_char(chunk[0])?;
            let lo = from_hex_char(chunk[1])?;
            bytes.push((hi << 4) | lo);
        }
        Some(bytes)
    }

    fn from_hex_char(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// Build the canonical string that is HMAC-signed.
///
/// Format: `{method}\n{path}\n{timestamp}\n{body}`
fn canonical_string(method: &str, path: &str, timestamp: i64, body: &str) -> String {
    format!("{method}\n{path}\n{timestamp}\n{body}")
}

/// Sign an outgoing request, returning a hex-encoded HMAC-SHA256 signature.
pub fn sign_request(secret: &str, method: &str, path: &str, timestamp: i64, body: &str) -> String {
    let payload = canonical_string(method, path, timestamp, body);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Verify that `signature` (hex) is a valid HMAC for the given request
/// parameters signed with `secret`.
pub fn verify_signature(
    secret: &str,
    method: &str,
    path: &str,
    timestamp: i64,
    body: &str,
    signature: &str,
) -> bool {
    let payload = canonical_string(method, path, timestamp, body);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());

    match hex::decode(signature) {
        Some(sig_bytes) => mac.verify_slice(&sig_bytes).is_ok(),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Replay protection
// ---------------------------------------------------------------------------

/// Check that `timestamp` (unix seconds) is within `max_age_secs` of the
/// current server time.  Protects against replay attacks.
pub fn is_request_fresh(timestamp: i64, max_age_secs: i64) -> bool {
    let now = Utc::now().timestamp();
    let diff = (now - timestamp).abs();
    diff <= max_age_secs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-shared-secret-42";

    #[test]
    fn test_sign_and_verify() {
        let ts = Utc::now().timestamp();
        let sig = sign_request(SECRET, "POST", "/api/v1/task", ts, r#"{"hello":"world"}"#);

        assert!(
            verify_signature(
                SECRET,
                "POST",
                "/api/v1/task",
                ts,
                r#"{"hello":"world"}"#,
                &sig
            ),
            "valid signature must verify"
        );
    }

    #[test]
    fn test_wrong_secret_fails() {
        let ts = Utc::now().timestamp();
        let sig = sign_request(SECRET, "GET", "/health", ts, "");

        assert!(
            !verify_signature("wrong-secret", "GET", "/health", ts, "", &sig),
            "wrong secret must fail"
        );
    }

    #[test]
    fn test_tampered_body_fails() {
        let ts = Utc::now().timestamp();
        let sig = sign_request(SECRET, "POST", "/run", ts, "original");

        assert!(
            !verify_signature(SECRET, "POST", "/run", ts, "tampered", &sig),
            "tampered body must fail"
        );
    }

    #[test]
    fn test_tampered_path_fails() {
        let ts = Utc::now().timestamp();
        let sig = sign_request(SECRET, "GET", "/api/v1/nodes", ts, "");

        assert!(
            !verify_signature(SECRET, "GET", "/api/v1/EVIL", ts, "", &sig),
            "tampered path must fail"
        );
    }

    #[test]
    fn test_invalid_hex_signature_fails() {
        let ts = Utc::now().timestamp();
        assert!(
            !verify_signature(SECRET, "GET", "/x", ts, "", "not-hex!!"),
            "invalid hex must fail"
        );
    }

    #[test]
    fn test_is_request_fresh() {
        let now = Utc::now().timestamp();
        assert!(is_request_fresh(now, 60));
        assert!(is_request_fresh(now - 30, 60));
        assert!(!is_request_fresh(now - 120, 60));
    }

    #[test]
    fn test_future_timestamp_within_window() {
        let future = Utc::now().timestamp() + 10;
        assert!(
            is_request_fresh(future, 60),
            "slightly future timestamps should be accepted"
        );
    }

    #[test]
    fn test_future_timestamp_beyond_window() {
        let far_future = Utc::now().timestamp() + 120;
        assert!(
            !is_request_fresh(far_future, 60),
            "far-future timestamps must be rejected"
        );
    }

    #[test]
    fn test_deterministic_signature() {
        let sig1 = sign_request(SECRET, "PUT", "/a", 1700000000, "body");
        let sig2 = sign_request(SECRET, "PUT", "/a", 1700000000, "body");
        assert_eq!(sig1, sig2, "same inputs must produce same signature");
    }
}
