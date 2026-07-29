//! Authentication helpers for the ff-agent HTTP control plane.

use axum::http::HeaderMap;
use chrono::Utc;
use ff_security::computer_auth;
use serde_json::Value;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{
    OnceLock,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::Notify;

pub const SIGNATURE_HEADER: &str = "x-forgefleet-signature";
pub const TIMESTAMP_HEADER: &str = "x-forgefleet-timestamp";
const SECRET_ENV: &str = "FORGEFLEET_ENROLLMENT_TOKEN";
const CANONICAL_SECRET_KEY: &str = "enrollment.shared_secret";
const BIND_ENV: &str = "FF_AGENT_HTTP_BIND";
const MAX_REQUEST_AGE_SECS: i64 = 300;
const SECRET_UNINITIALIZED: u8 = 0;
const SECRET_INITIALIZING: u8 = 1;
const SECRET_INITIALIZED: u8 = 2;

struct ControlPlaneSecretCache {
    state: AtomicU8,
    result: OnceLock<Result<String, String>>,
    ready: Notify,
}

impl ControlPlaneSecretCache {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(SECRET_UNINITIALIZED),
            result: OnceLock::new(),
            ready: Notify::const_new(),
        }
    }

    async fn resolve_with<F, Fut>(&self, fetch_secret: F) -> Result<&str, String>
    where
        F: FnOnce(&'static str) -> Fut,
        Fut: Future<Output = Option<String>>,
    {
        let mut fetch_secret = Some(fetch_secret);
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.state.load(Ordering::Acquire) {
                SECRET_INITIALIZED => return self.cached_result(),
                SECRET_UNINITIALIZED => {
                    if self
                        .state
                        .compare_exchange(
                            SECRET_UNINITIALIZED,
                            SECRET_INITIALIZING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }

                    let mut guard = SecretInitializationGuard {
                        cache: self,
                        completed: false,
                    };
                    let result = resolve_control_plane_secret_with(
                        fetch_secret
                            .take()
                            .expect("secret initializer consumed more than once"),
                    )
                    .await;
                    guard.completed = true;
                    self.complete(result);
                    return self.cached_result();
                }
                SECRET_INITIALIZING => notified.await,
                _ => unreachable!("invalid control-plane secret cache state"),
            }
        }
    }

    fn get(&self) -> Result<&str, String> {
        if self.state.load(Ordering::Acquire) != SECRET_INITIALIZED {
            return Err("agent HTTP auth secret was not initialized during startup".to_string());
        }
        self.cached_result()
    }

    fn cached_result(&self) -> Result<&str, String> {
        match self.result.get() {
            Some(Ok(secret)) => Ok(secret),
            Some(Err(error)) => Err(error.clone()),
            None => Err("agent HTTP auth secret initialization state is unavailable".to_string()),
        }
    }

    fn complete(&self, result: Result<String, String>) {
        let _ = self.result.set(result);
        self.state.store(SECRET_INITIALIZED, Ordering::Release);
        self.ready.notify_waiters();
    }
}

impl Default for ControlPlaneSecretCache {
    fn default() -> Self {
        Self::new()
    }
}

struct SecretInitializationGuard<'a> {
    cache: &'a ControlPlaneSecretCache,
    completed: bool,
}

impl Drop for SecretInitializationGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.cache.complete(Err(
                "agent HTTP auth secret initialization was cancelled".to_string()
            ));
        }
    }
}

static CONTROL_PLANE_SECRET: ControlPlaneSecretCache = ControlPlaneSecretCache::new();

/// Resolve and cache the control-plane secret during process startup.
///
/// The returned clone is injected into the inbound router. Outbound request
/// signing reads the same process-local cache and never reaches PostgreSQL.
/// The first success, configuration error, or cancelled initialization is
/// retained for the process lifetime; secret rotation therefore takes effect
/// after process restart.
pub async fn control_plane_secret() -> Result<String, String> {
    CONTROL_PLANE_SECRET
        .resolve_with(crate::fleet_info::fetch_secret)
        .await
        .map(str::to_owned)
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
    send_signed_with_cache(&CONTROL_PLANE_SECRET, client, method, url, body).await
}

async fn send_signed_with_cache(
    cache: &ControlPlaneSecretCache,
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: &str,
) -> anyhow::Result<reqwest::Response> {
    let secret = cache.get().map_err(anyhow::Error::msg)?;
    let parsed = reqwest::Url::parse(url)?;
    let path = parsed.path();
    let timestamp = Utc::now().timestamp();
    let signature = computer_auth::sign_request(secret, method.as_str(), path, timestamp, body);
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    async fn fallback_is_resolved_once_and_reused_for_multiple_signed_requests() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(None);
        let cache = ControlPlaneSecretCache::default();
        let fetches = Arc::new(AtomicUsize::new(0));

        let first_fetches = fetches.clone();
        let first = cache
            .resolve_with(move |_| {
                first_fetches.fetch_add(1, Ordering::SeqCst);
                async { Some(" shared-fallback ".to_string()) }
            })
            .await
            .unwrap();
        assert_eq!(first, "shared-fallback");

        let second_fetches = fetches.clone();
        let second = cache
            .resolve_with(move |_| {
                second_fetches.fetch_add(1, Ordering::SeqCst);
                async { Some("must-not-replace-cache".to_string()) }
            })
            .await
            .unwrap();
        assert_eq!(second, "shared-fallback");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);

        let expected_secret = first.to_string();
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |request: axum::extract::Request| {
                let expected_secret = expected_secret.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let body = axum::body::to_bytes(body, 16 * 1024).await.unwrap();
                    let body = String::from_utf8(body.to_vec()).unwrap();
                    match authorize(
                        &expected_secret,
                        parts.method.as_str(),
                        parts.uri.path(),
                        &parts.headers,
                        &body,
                    ) {
                        Ok(()) => axum::http::StatusCode::OK,
                        Err(_) => axum::http::StatusCode::UNAUTHORIZED,
                    }
                }
            },
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();

        let first_response = send_signed_with_cache(
            &cache,
            &client,
            reqwest::Method::POST,
            &format!("http://{address}/agent/message"),
            r#"{"message":"one"}"#,
        )
        .await
        .unwrap();
        let second_response = send_signed_with_cache(
            &cache,
            &client,
            reqwest::Method::GET,
            &format!("http://{address}/tasks"),
            "",
        )
        .await
        .unwrap();

        assert!(first_response.status().is_success());
        assert!(second_response.status().is_success());
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_startup_resolves_fallback_once() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(None);
        let cache = Arc::new(ControlPlaneSecretCache::default());
        let fetches = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let cache = cache.clone();
            let fetches = fetches.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .resolve_with(move |_| {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        async {
                            tokio::task::yield_now().await;
                            Some("concurrent-secret".to_string())
                        }
                    })
                    .await
                    .map(str::to_owned)
            }));
        }

        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), "concurrent-secret");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_error_is_cached_without_retrying_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(None);
        let cache = ControlPlaneSecretCache::default();
        let fetches = Arc::new(AtomicUsize::new(0));

        let first_fetches = fetches.clone();
        let first_error = cache
            .resolve_with(move |_| {
                first_fetches.fetch_add(1, Ordering::SeqCst);
                async { None }
            })
            .await
            .unwrap_err();
        let second_fetches = fetches.clone();
        let second_error = cache
            .resolve_with(move |_| {
                second_fetches.fetch_add(1, Ordering::SeqCst);
                async { Some("must-not-retry".to_string()) }
            })
            .await
            .unwrap_err();

        assert_eq!(first_error, second_error);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_startup_is_cached_without_retrying_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_secret_env(None);
        let cache = Arc::new(ControlPlaneSecretCache::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let initializing_cache = cache.clone();
        let initializer = tokio::spawn(async move {
            initializing_cache
                .resolve_with(move |_| async move {
                    let _ = started_tx.send(());
                    std::future::pending::<Option<String>>().await
                })
                .await
                .map(str::to_owned)
        });

        started_rx.await.unwrap();
        initializer.abort();
        let _ = initializer.await;

        let retried = Arc::new(AtomicBool::new(false));
        let retried_in_closure = retried.clone();
        let error = cache
            .resolve_with(move |_| {
                retried_in_closure.store(true, Ordering::SeqCst);
                async { Some("must-not-retry".to_string()) }
            })
            .await
            .unwrap_err();
        assert!(error.contains("initialization was cancelled"));
        assert!(!retried.load(Ordering::SeqCst));
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

    #[test]
    fn outbound_signing_fails_closed_before_startup_initialization() {
        let cache = ControlPlaneSecretCache::default();
        let err = cache.get().unwrap_err();
        assert!(err.contains("not initialized during startup"));
        assert!(!err.contains("token"));
        assert!(!err.contains("secret="));
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
