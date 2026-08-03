//! Web console static file serving.
//!
//! Uses `rust-embed` to embed `web-forge-fleet/out/*` (the Next.js static
//! export) at compile time so the gateway binary is fully self-contained.
//! Falls back to reading from the filesystem when the embed folder is empty
//! (development mode).

use axum::{
    body::Body,
    http::{Request, Response, StatusCode, header},
    response::IntoResponse,
};
use rust_embed::Embed;

/// Embedded web assets (compiled from `web-forge-fleet/out/`).
///
/// In release builds the files are baked into the binary.
/// In debug builds the folder is read at runtime.
#[derive(Embed)]
#[folder = "../../web-forge-fleet/out/"]
struct DashboardAssets;

// ─── Axum handler ────────────────────────────────────────────────────────────

/// Serve an embedded asset.
///
/// Intended to be used as an Axum fallback handler so that:
/// - Known API / WS routes are handled first.
/// - Any remaining path is tried against the embedded web files.
/// - If the path doesn't match a real file, serve `index.html` (SPA fallback).
pub async fn serve_dashboard(req: Request<Body>) -> impl IntoResponse {
    let path = req.uri().path().trim_start_matches('/');

    // API routes should never fall back to the SPA — return a JSON 404.
    if path.starts_with("api/") {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"not found"}"#))
            .unwrap_or_else(|_| Response::new(Body::from("not found")));
    }

    // 1. Try the exact path (files: /_next/static/*, /console.html, …).
    if let Some(resp) = serve_embedded(path) {
        return resp;
    }

    // 2. Directory index — the Next.js export uses trailingSlash, so routes
    //    live at `<route>/index.html` ("" resolves to the root index.html).
    let dir_index = format!("{}/index.html", path.trim_end_matches('/'));
    if let Some(resp) = serve_embedded(&dir_index) {
        return resp;
    }

    // 3. SPA fallback — any non-file path returns index.html.
    if let Some(resp) = serve_embedded("index.html") {
        return resp;
    }

    // 4. Nothing at all (shouldn't happen if the web app was built).
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(
            "web-forge-fleet not built — run `npm run build` in web-forge-fleet/",
        ))
        .unwrap_or_else(|_| Response::new(Body::from("not found")))
}

/// Try to serve a single embedded file, returning `None` if missing.
fn serve_embedded(path: &str) -> Option<Response<Body>> {
    let asset = DashboardAssets::get(path)?;

    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();

    // Cache policy:
    //   - Hashed assets (JS/CSS under _next/static/) → immutable, 1 year
    //   - index.html and other root files → no-store (avoid stale shell/version mismatch)
    let cache = if path.starts_with("_next/static/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-store, max-age=0, must-revalidate"
    };

    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(asset.data.to_vec()))
        .unwrap_or_else(|_| Response::new(Body::from(Vec::new())));

    Some(resp)
}
