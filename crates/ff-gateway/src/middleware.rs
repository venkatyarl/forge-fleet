//! Request logging, timing, trace-ID injection, and error-handling middleware.

use std::time::Instant;

use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use chrono::Utc;
use serde_json::json;
use tracing::{info, warn};

use ff_observability::tracing_ext::{
    SpanExt, TraceSummary, extract_or_generate_trace_id, extract_trace_header, global_trace_store,
    trace_request as create_request_span,
};

// ─── Request timing + logging ────────────────────────────────────────────────

/// Axum middleware that logs every request with method, path, status, and
/// elapsed time.
pub async fn request_logger(request: Request<Body>, next: Next) -> Response<Body> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let start = Instant::now();

    let response = next.run(request).await;
    let elapsed = start.elapsed();
    let status = response.status();

    if status.is_server_error() {
        warn!(
            method = %method,
            path = %uri.path(),
            status = status.as_u16(),
            elapsed_ms = elapsed.as_millis() as u64,
            "request completed with error"
        );
    } else {
        info!(
            method = %method,
            path = %uri.path(),
            status = status.as_u16(),
            elapsed_ms = elapsed.as_millis() as u64,
            "request completed"
        );
    }

    response
}

// ─── Trace-ID middleware ─────────────────────────────────────────────────────

/// Axum middleware that:
/// 1. Extracts or generates an `X-Trace-Id` header on every request
/// 2. Opens a tracing span scoped to the request
/// 3. Injects the trace ID into the response headers
/// 4. Records a [`TraceSummary`] in the global trace store
pub async fn trace_id_middleware(request: Request<Body>, next: Next) -> Response<Body> {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let trace_id = extract_or_generate_trace_id(extract_trace_header(request.headers()).as_deref());

    let span = create_request_span(&trace_id, &method, &path);
    let _guard = span.enter();

    let started_at = Utc::now();
    let start = Instant::now();

    let mut response = next.run(request).await;
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_millis() as u64;
    let status_code = response.status().as_u16();

    // Record on span
    span.record_status(status_code);
    span.record_elapsed_ms(elapsed_ms);

    // Inject trace ID into response
    if let Ok(val) = trace_id.parse() {
        response.headers_mut().insert("x-trace-id", val);
    }

    // Store trace summary
    global_trace_store().record(TraceSummary {
        trace_id,
        span_name: "http_request".into(),
        service: "ff-gateway".into(),
        started_at,
        elapsed_ms,
        status: Some(status_code),
        attributes: serde_json::json!({
            "http.method": method,
            "http.path": path,
        }),
    });

    response
}

// ─── JSON error response helpers ─────────────────────────────────────────────

/// Produce a standardised JSON error body.
pub fn json_error(status: StatusCode, message: &str) -> impl IntoResponse {
    let body = json!({
        "error": {
            "message": message,
            "type": error_type_for_status(status),
        }
    })
    .to_string();

    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            Response::new(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        })
}

// ─── JWT auth middleware ─────────────────────────────────────────────────────

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub exp: Option<usize>,
    pub iat: Option<usize>,
    /// Fleet RBAC role: `admin` | `operator` | `viewer`. Absent ⇒ `viewer`
    /// (a valid token can always READ; mutating/control routes require an
    /// explicit `operator`/`admin` role). Only enforced when `FF_JWT_SECRET`
    /// is set (the production / cloud-exposed deploy).
    #[serde(default)]
    pub role: Option<String>,
}

/// Privilege rank of a fleet role; higher = more privileged. An unknown or
/// absent role is treated as `viewer` (rank 1) so existing read-only tokens
/// keep working — RBAC only *adds* a requirement for mutating/control routes.
fn role_rank(role: Option<&str>) -> u8 {
    match role.map(|r| r.trim().to_ascii_lowercase()).as_deref() {
        Some("admin") => 3,
        Some("operator") => 2,
        Some("viewer") | None | Some("") => 1,
        _ => 1, // unknown role → least privilege (viewer)
    }
}

/// Minimum role rank a route requires. Pure + isolated so the policy is
/// unit-testable. Read requests need `viewer` (any valid token). Mutating
/// requests need `operator`. The most sensitive surfaces (secrets, OAuth
/// material, RBAC/leadership control) need `admin` regardless of method.
fn required_rank(path: &str, is_mutating: bool) -> u8 {
    let admin_sensitive = path.starts_with("/api/secrets")
        || path.starts_with("/api/oauth")
        || path.starts_with("/api/fleet/leader")
        || path.contains("/secrets")
        || path.contains("/rbac");
    if admin_sensitive {
        3 // admin
    } else if is_mutating {
        2 // operator
    } else {
        1 // viewer (read)
    }
}

/// Public routes that do NOT require authentication.
/// All other routes mandate authentication unless trusted-LAN mode is
/// explicitly enabled.
const PUBLIC_ROUTES: &[&str] = &[
    "/health",
    "/.well-known/forgefleet.json",
    "/api/webhook",
    "/api/github/webhook",
    "/api/webhooks/github",
    // Enrollment endpoints: an enrolling node has no fleet credentials YET by
    // definition, and each of these enforces its own shared-secret policy
    // (EnrollmentConfig / onboard.rs) — gating them behind fleet auth made
    // onboarding impossible (found live 2026-07-18: shakira's self-enroll
    // POST 401'd at this middleware before the token check ever ran).
    "/onboard",
    "/api/fleet/self-enroll",
    "/api/fleet/enrollment-progress",
];

fn is_public_route(path: &str) -> bool {
    PUBLIC_ROUTES.contains(&path) || path.starts_with("/onboard/")
}

fn trusted_lan_enabled() -> bool {
    std::env::var("FF_GATEWAY_TRUSTED_LAN").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

/// Axum middleware that validates `Authorization: Bearer <token>`.
///
/// - When `FF_JWT_SECRET` is **set**: all routes except `PUBLIC_ROUTES` require
///   a valid Bearer token with standard claims (exp, iat).
/// - When `FF_JWT_SECRET` is **unset**: only `PUBLIC_ROUTES` pass by default.
/// - `FF_GATEWAY_TRUSTED_LAN=1` explicitly restores anonymous reads and LLM
///   dispatch for deployments whose network boundary is trusted.
pub async fn jwt_auth_middleware(request: Request<Body>, next: Next) -> Response<Body> {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let is_mutating = matches!(
        method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::PATCH
            | axum::http::Method::DELETE
    );

    // These cost-bearing routes are allowed without JWT only in the explicit
    // trusted-LAN compatibility mode.
    let is_llm_endpoint = matches!(
        path.as_str(),
        "/v1/chat/completions"
            | "/v1/completions"
            | "/v1/embeddings"
            | "/v1/rerank"
            | "/api/jarvis/ask"
    );

    let secret = match std::env::var("FF_JWT_SECRET") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            if is_public_route(&path) {
                return next.run(request).await;
            }

            if trusted_lan_enabled() && (!is_mutating || is_llm_endpoint) {
                return next.run(request).await;
            }

            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway authentication is not configured",
            )
            .into_response();
        }
    };

    // Public routes bypass JWT check even when secret is set.
    if is_public_route(&path) {
        return next.run(request).await;
    }

    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let token = match auth_header {
        Some(hdr) if hdr.starts_with("Bearer ") => &hdr[7..],
        _ => {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "missing or malformed Authorization header",
            )
            .into_response();
        }
    };

    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    // Standard JWT security: tokens MUST have an expiration.
    validation.validate_exp = true;
    validation.required_spec_claims = std::collections::HashSet::from(["exp".into(), "iat".into()]);

    match jsonwebtoken::decode::<JwtClaims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    ) {
        Ok(decoded) => {
            // RBAC: a valid token is necessary but not sufficient — the holder's
            // role must meet the route's requirement (item 11). Reads pass for
            // any role; mutations need operator+; sensitive surfaces need admin.
            let have = role_rank(decoded.claims.role.as_deref());
            let need = required_rank(&path, is_mutating);
            if have < need {
                warn!(
                    sub = %decoded.claims.sub,
                    role = decoded.claims.role.as_deref().unwrap_or("viewer"),
                    %path,
                    "rbac: insufficient role for route"
                );
                return json_error(
                    StatusCode::FORBIDDEN,
                    "insufficient role for this operation",
                )
                .into_response();
            }
            next.run(request).await
        }
        Err(_e) => {
            warn!("jwt validation failed");
            // Generic error — do not leak internal validation details.
            json_error(StatusCode::UNAUTHORIZED, "invalid token").into_response()
        }
    }
}

fn error_type_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::SERVICE_UNAVAILABLE => "service_unavailable",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::INTERNAL_SERVER_ERROR => "internal_error",
        _ => "unknown_error",
    }
}

#[cfg(test)]
mod rbac_tests {
    use super::{required_rank, role_rank};

    #[test]
    fn role_rank_orders_admin_operator_viewer() {
        assert_eq!(role_rank(Some("admin")), 3);
        assert_eq!(role_rank(Some("operator")), 2);
        assert_eq!(role_rank(Some("viewer")), 1);
        assert_eq!(role_rank(Some("ADMIN")), 3); // case-insensitive
        assert_eq!(role_rank(Some(" operator ")), 2); // trimmed
    }

    #[test]
    fn absent_or_unknown_role_is_viewer() {
        // A valid token without a role can still READ — RBAC only adds a
        // requirement for elevated routes; it never breaks read access.
        assert_eq!(role_rank(None), 1);
        assert_eq!(role_rank(Some("")), 1);
        assert_eq!(role_rank(Some("superuser")), 1);
    }

    #[test]
    fn read_routes_need_only_viewer() {
        assert_eq!(required_rank("/api/fleet/status", false), 1);
        assert_eq!(required_rank("/api/models", false), 1);
    }

    #[test]
    fn mutations_need_operator() {
        assert_eq!(required_rank("/api/models", true), 2);
        assert_eq!(required_rank("/api/tasks", true), 2);
    }

    #[test]
    fn sensitive_surfaces_need_admin_regardless_of_method() {
        assert_eq!(required_rank("/api/secrets/foo", false), 3);
        assert_eq!(required_rank("/api/oauth/import", true), 3);
        assert_eq!(required_rank("/api/fleet/leader/step-down", true), 3);
    }

    #[test]
    fn enforcement_matrix() {
        // (role, path, mutating) -> allowed?
        let allowed =
            |role: Option<&str>, path: &str, m: bool| role_rank(role) >= required_rank(path, m);
        // viewer can read but not mutate
        assert!(allowed(Some("viewer"), "/api/models", false));
        assert!(!allowed(Some("viewer"), "/api/models", true));
        // operator can mutate but not touch secrets
        assert!(allowed(Some("operator"), "/api/models", true));
        assert!(!allowed(Some("operator"), "/api/secrets/x", false));
        // admin can do everything
        assert!(allowed(Some("admin"), "/api/secrets/x", true));
        // no-role token: reads ok, mutations denied
        assert!(allowed(None, "/api/fleet/status", false));
        assert!(!allowed(None, "/api/tasks", true));
    }
}

#[cfg(test)]
mod router_auth_tests {
    use std::{
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::{get, post},
    };
    use jsonwebtoken::{EncodingKey, Header, encode};
    use tower::ServiceExt;

    use super::{JwtClaims, jwt_auth_middleware};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env(name: &str, value: Option<&str>) {
        // SAFETY: this test serializes all mutations of these process globals.
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    fn test_router() -> Router {
        Router::new()
            .route("/health", get(|| async { StatusCode::OK }))
            .route("/api/fleet/self-enroll", post(|| async { StatusCode::OK }))
            .route("/api/webhooks/github", post(|| async { StatusCode::OK }))
            .route("/api/fleet/status", get(|| async { StatusCode::OK }))
            .route("/v1/chat/completions", post(|| async { StatusCode::OK }))
            .route("/api/config", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn(jwt_auth_middleware))
    }

    async fn status(router: &Router, method: &str, path: &str, token: Option<&str>) -> StatusCode {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn router_fails_closed_without_secret_and_enforces_jwt_when_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_secret = std::env::var("FF_JWT_SECRET").ok();
        let old_trusted_lan = std::env::var("FF_GATEWAY_TRUSTED_LAN").ok();
        let router = test_router();

        set_env("FF_JWT_SECRET", None);
        set_env("FF_GATEWAY_TRUSTED_LAN", None);
        assert_eq!(
            status(&router, "GET", "/health", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&router, "GET", "/health/private", None).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&router, "POST", "/api/fleet/self-enroll", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&router, "POST", "/api/webhooks/github", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&router, "GET", "/api/fleet/status", None).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&router, "POST", "/v1/chat/completions", None).await,
            StatusCode::SERVICE_UNAVAILABLE
        );

        set_env("FF_GATEWAY_TRUSTED_LAN", Some("1"));
        assert_eq!(
            status(&router, "GET", "/api/fleet/status", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&router, "POST", "/v1/chat/completions", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&router, "POST", "/api/config", None).await,
            StatusCode::SERVICE_UNAVAILABLE
        );

        let secret = "router-test-secret";
        set_env("FF_JWT_SECRET", Some(secret));
        set_env("FF_GATEWAY_TRUSTED_LAN", None);
        assert_eq!(
            status(&router, "GET", "/api/fleet/status", None).await,
            StatusCode::UNAUTHORIZED
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize;
        let token = encode(
            &Header::default(),
            &JwtClaims {
                sub: "router-test".into(),
                exp: Some(now + 60),
                iat: Some(now),
                role: Some("admin".into()),
            },
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        assert_eq!(
            status(&router, "GET", "/api/fleet/status", Some(&token)).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&router, "POST", "/v1/chat/completions", Some(&token)).await,
            StatusCode::OK
        );

        set_env("FF_JWT_SECRET", old_secret.as_deref());
        set_env("FF_GATEWAY_TRUSTED_LAN", old_trusted_lan.as_deref());
    }
}
