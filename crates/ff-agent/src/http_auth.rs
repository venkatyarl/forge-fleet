//! Authentication helpers for the ff-agent HTTP control plane.

use axum::http::HeaderMap;
use chrono::Utc;
use ff_security::computer_auth;
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub const SIGNATURE_HEADER: &str = "x-forgefleet-signature";
pub const TIMESTAMP_HEADER: &str = "x-forgefleet-timestamp";
const SECRET_ENV: &str = "FORGEFLEET_ENROLLMENT_TOKEN";
const BIND_ENV: &str = "FF_AGENT_HTTP_BIND";
const MAX_REQUEST_AGE_SECS: i64 = 300;

pub fn control_plane_secret() -> Result<String, String> {
    std::env::var(SECRET_ENV)
        .ok()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| format!("{SECRET_ENV} is not configured"))
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
    let secret = control_plane_secret().map_err(anyhow::Error::msg)?;
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
}
