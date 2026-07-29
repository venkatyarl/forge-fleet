//! Authentication helpers for the ff-agent HTTP control plane.

use axum::http::HeaderMap;
use chrono::Utc;
use ff_security::computer_auth;
use serde_json::Value;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub const SIGNATURE_HEADER: &str = "x-forgefleet-signature";
pub const TIMESTAMP_HEADER: &str = "x-forgefleet-timestamp";
const SECRET_ENV: &str = "FORGEFLEET_ENROLLMENT_TOKEN";
const CANONICAL_SECRET_KEY: &str = "enrollment.shared_secret";
const BIND_ENV: &str = "FF_AGENT_HTTP_BIND";
const MAX_REQUEST_AGE_SECS: i64 = 300;

pub async fn control_plane_secret() -> Result<String, String> {
    resolve_control_plane_secret_with(crate::fleet_info::fetch_secret).await
}

async fn resolve_control_plane_secret_with<F, Fut>(fetch_secret: F) -> Result<String, String>
where
    F: FnOnce(&'static str) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    if let Some(secret) = secret_from_env() {
        return Ok(secret);
    }

    fetch_secret(CANONICAL_SECRET_KEY)
        .await
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| {
            format!(
                "agent HTTP auth secret is not configured: set {SECRET_ENV} or fleet_secrets key {CANONICAL_SECRET_KEY}"
            )
        })
}

fn secret_from_env() -> Option<String> {
    std::env::var(SECRET_ENV)
        .ok()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
}

pub fn bind_addr(port: u16) -> Result<SocketAddr, String> {
    let ip = match std::env::var(BIND_ENV) {
        Ok(value) => value
            .trim()
            .parse::<IpAddr>()
            .map_err(|err| format!("invalid {BIND_ENV} value {value:?}: {err}"))?,
        Err(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    Ok(SocketAddr::new(ip, port))
}

pub fn authorize(
    secret: &str,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<(), &'static str> {
    let timestamp = headers
        .get(TIMESTAMP_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or("missing or invalid authentication timestamp")?;
    if !computer_auth::is_request_fresh(timestamp, MAX_REQUEST_AGE_SECS) {
        return Err("request expired or replay detected");
    }
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or("missing authentication signature")?;
    if !computer_auth::verify_signature(secret, method, path, timestamp, body, signature) {
        return Err("invalid authentication signature");
    }
    Ok(())
}

pub async fn send_signed_json(
    client: &reqwest::Client,
    url: &str,
    payload: &Value,
) -> anyhow::Result<reqwest::Response> {
    let body = serde_json::to_string(payload)?;
    send_signed(client, reqwest::Method::POST, url, &body).await
}

pub async fn send_signed_get(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<reqwest::Response> {
    send_signed(client, reqwest::Method::GET, url, "").await
}

async fn send_signed(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: &str,
) -> anyhow::Result<reqwest::Response> {
    let secret = control_plane_secret().await.map_err(anyhow::Error::msg)?;
    let parsed = reqwest::Url::parse(url)?;
    let path = parsed.path();
    let timestamp = Utc::now().timestamp();
    let signature = computer_auth::sign_request(&secret, method.as_str(), path, timestamp, body);
    Ok(client
        .request(method, parsed)
        .header(TIMESTAMP_HEADER, timestamp)
        .header(SIGNATURE_HEADER, signature)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_secret_env(value: Option<&str>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(SECRET_ENV, value),
                None => std::env::remove_var(SECRET_ENV),
            }
        }
    }

    #[test]
    fn rejects_missing_bad_and_stale_auth_but_accepts_valid_signature() {
        let secret = "fleet-test-secret";
        let body = r#"{"task":"safe"}"#;
        let mut headers = HeaderMap::new();
        assert!(authorize(secret, "POST", "/assign", &headers, body).is_err());

        let timestamp = Utc::now().timestamp();
        headers.insert(TIMESTAMP_HEADER, timestamp.to_string().parse().unwrap());
        headers.insert(SIGNATURE_HEADER, "00".parse().unwrap());
        assert!(authorize(secret, "POST", "/assign", &headers, body).is_err());

        let stale = timestamp - MAX_REQUEST_AGE_SECS - 1;
        let stale_signature = computer_auth::sign_request(secret, "POST", "/assign", stale, body);
        headers.insert(TIMESTAMP_HEADER, stale.to_string().parse().unwrap());
        headers.insert(SIGNATURE_HEADER, stale_signature.parse().unwrap());
        assert!(authorize(secret, "POST", "/assign", &headers, body).is_err());

        let signature = computer_auth::sign_request(secret, "POST", "/assign", timestamp, body);
        headers.insert(TIMESTAMP_HEADER, timestamp.to_string().parse().unwrap());
        headers.insert(SIGNATURE_HEADER, signature.parse().unwrap());
        assert_eq!(authorize(secret, "POST", "/assign", &headers, body), Ok(()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_env_secret_takes_precedence_over_db_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(Some(" env-secret "));
        let fetched = Arc::new(AtomicBool::new(false));
        let fetched_for_closure = fetched.clone();

        let resolved = resolve_control_plane_secret_with(move |_| {
            fetched_for_closure.store(true, Ordering::SeqCst);
            async { Some("db-secret".to_string()) }
        })
        .await
        .unwrap();

        assert_eq!(resolved, "env-secret");
        assert!(!fetched.load(Ordering::SeqCst));
        set_secret_env(None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canonical_fleet_secret_is_used_when_env_is_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(None);
        let requested_key = Arc::new(Mutex::new(String::new()));
        let requested_key_for_closure = requested_key.clone();

        let resolved = resolve_control_plane_secret_with(move |key| {
            *requested_key_for_closure.lock().unwrap() = key.to_string();
            async { Some(" db-secret ".to_string()) }
        })
        .await
        .unwrap();

        assert_eq!(resolved, "db-secret");
        assert_eq!(&*requested_key.lock().unwrap(), CANONICAL_SECRET_KEY);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_env_and_canonical_secret_fails_closed_with_redacted_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(Some("   "));

        let err = resolve_control_plane_secret_with(|_| async { None })
            .await
            .unwrap_err();

        assert!(err.contains(SECRET_ENV));
        assert!(err.contains(CANONICAL_SECRET_KEY));
        assert!(!err.contains("   "));
        assert!(!err.contains("token="));
        set_secret_env(None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolved_fallback_secret_uses_existing_hmac_signing_contract() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(None);
        let secret = resolve_control_plane_secret_with(|_| async {
            Some("fallback-signing-secret".to_string())
        })
        .await
        .unwrap();
        let body = r#"{"task":"safe"}"#;
        let timestamp = Utc::now().timestamp();
        let signature = computer_auth::sign_request(&secret, "POST", "/assign", timestamp, body);
        let mut headers = HeaderMap::new();
        headers.insert(TIMESTAMP_HEADER, timestamp.to_string().parse().unwrap());
        headers.insert(SIGNATURE_HEADER, signature.parse().unwrap());

        assert_eq!(
            authorize(&secret, "POST", "/assign", &headers, body),
            Ok(())
        );
    }
}
