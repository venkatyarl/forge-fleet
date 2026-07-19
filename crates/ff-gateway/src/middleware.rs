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
/// All other routes mandate a valid JWT when FF_JWT_SECRET is set.
const PUBLIC_ROUTES: &[&str] = &[
    "/health",
    "/.well-known/forgefleet.json",
    "/api/webhook",
    "/api/github/webhook",
    "/metrics",
    "/ws",
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
    PUBLIC_ROUTES
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
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
    let is_mutating = matches!(
        method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::PATCH
            | axum::http::Method::DELETE
    );

    // OpenAI-compatible LLM endpoints (chat completions, completions,
    // embeddings, rerank) are inherently LLM dispatch — they don't mutate
    // fleet state in the same sense as the admin/control routes the JWT
    // gate is intended to protect. Without this carve-out, internal fleet
    // callers (ff-pipeline, fleet_crew, the cascade) get 401'd when
    // FF_JWT_SECRET is unset (the common dev/lan-only deploy). With the
    // carve-out: callers can POST to /v1/chat/completions on the loopback
    // gateway without authenticating. When the operator DOES set
    // FF_JWT_SECRET (production deploy behind a reverse proxy), the
    // full JWT check still applies — the carve-out only kicks in when
    // no secret is configured.
    // `/api/jarvis/ask` is JARVIS's query endpoint: it reads fleet state or
    // dispatches the prompt to a local LLM — same spirit as the /v1 LLM routes,
    // and the loopback JARVIS HUD calls it unauthenticated. Carve it out so the
    // LAN deploy (no FF_JWT_SECRET) works; a production deploy that sets the
    // secret still gates it.
    let is_llm_endpoint = matches!(
        path.as_str(),
        "/v1/chat/completions"
            | "/v1/completions"
            | "/v1/embeddings"
            | "/v1/rerank"
            | "/api/jarvis/ask"
    );

    let secret = match std::env::var("FF_JWT_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            // No JWT secret configured — still block mutating routes,
            // except for LLM-dispatch endpoints (see comment above).
            if is_mutating && !is_public_route(&path) && !is_llm_endpoint {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication required for mutating requests",
                )
                .into_response();
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
