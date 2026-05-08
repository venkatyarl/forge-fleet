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
}

/// Public routes that do NOT require authentication.
/// All other routes mandate a valid JWT when FF_JWT_SECRET is set.
const PUBLIC_ROUTES: &[&str] = &[
    "/health",
    "/.well-known/forgefleet.json",
    "/api/webhook",
    "/api/github/webhook",
    "/metrics",
    "/ws",
];

fn is_public_route(path: &str) -> bool {
    PUBLIC_ROUTES.iter().any(|p| path == *p || path.starts_with(&format!("{}/", p)))
}

/// Axum middleware that validates `Authorization: Bearer <token>`.
///
/// - When `FF_JWT_SECRET` is **set**: all routes except `PUBLIC_ROUTES` require
///   a valid Bearer token with standard claims (exp, iat).
/// - When `FF_JWT_SECRET` is **unset**: mutating routes (`POST`, `PUT`, `PATCH`,
///   `DELETE`) still reject with 401. Read-only public routes pass through.
pub async fn jwt_auth_middleware(request: Request<Body>, next: Next) -> Response<Body> {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let is_mutating = matches!(method, axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH | axum::http::Method::DELETE);

    let secret = match std::env::var("FF_JWT_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            // No JWT secret configured — still block mutating routes.
            if is_mutating && !is_public_route(&path) {
                return json_error(StatusCode::UNAUTHORIZED, "authentication required for mutating requests").into_response();
            }
            return next.run(request).await;
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
            return json_error(StatusCode::UNAUTHORIZED, "missing or malformed Authorization header").into_response();
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
        Ok(_decoded) => next.run(request).await,
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
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::INTERNAL_SERVER_ERROR => "internal_error",
        _ => "unknown_error",
    }
}
