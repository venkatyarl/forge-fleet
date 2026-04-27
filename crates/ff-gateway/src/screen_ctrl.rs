//! Screen-control daemon — Pillar 1 (Computer Use) MVP.
//!
//! Listens on `127.0.0.1:51200` per fleet member. Exposes an
//! OpenAI/Anthropic Computer Use-aligned action set:
//!
//!   GET  /screenshot          — current screen as PNG bytes
//!   POST /click  {x, y}       — click absolute coords
//!   POST /double-click {x,y}  — double click
//!   POST /move   {x, y}       — move mouse
//!   POST /type   {text}       — type text into focused element
//!   POST /key    {key}        — keystroke (e.g. "Return", "cmd+c")
//!   POST /goto   {url}        — open url in default browser
//!
//! Backed by subprocess wrappers around the platform's standard
//! tools — no new Rust deps, no headless-browser stack:
//!
//!   macOS:  `screencapture` (built-in), `cliclick` (brew install cliclick), `open`
//!   Linux:  `scrot` or `import` (ImageMagick), `xdotool`, `xdg-open`
//!
//! When a tool is missing, the endpoint returns 503 with an install
//! hint. This lets ff fleet members participate in screen-control
//! workflows when the helper binaries are present, and gracefully
//! decline otherwise.
//!
//! The `screen` capability tag (PR-A3 cap-detect — separate follow-up
//! to register it) gates which fleet_tasks dispatch here. For now any
//! caller can hit the local daemon directly via 127.0.0.1:51200.
//!
//! # Why not headless Chromium / Playwright?
//!
//! The plan called for a headless-browser daemon. In practice:
//! 1. Anthropic's Computer Use API is screen+mouse, not browser.
//! 2. Most fleet members have a real desktop; using their actual
//!    screen lets agents do anything (browsers, apps, file dialogs),
//!    not just web pages.
//! 3. No multi-hundred-MB Chromium dep per member; native tools are
//!    smaller, already-installed-or-easily-installed.
//! Headless-Chromium is a future add (members without a screen, e.g.
//! DGX Sparks) but not the MVP.

use std::net::SocketAddr;

use axum::{
    Json, Router,
    extract::Query,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tracing::{info, warn};

const BIND_ADDR: &str = "127.0.0.1:51200";

/// Spawn the screen-control daemon. Returns a JoinHandle for the
/// daemon main to keep alive. Errors during binding are logged but
/// non-fatal — a member without screen-control privileges (e.g.
/// headless server) just doesn't open the port.
pub fn spawn() -> JoinHandle<()> {
    tokio::spawn(async move {
        let app = Router::new()
            .route("/screenshot", get(screenshot))
            .route("/click", post(click))
            .route("/double-click", post(double_click))
            .route("/move", post(mouse_move))
            .route("/type", post(type_text))
            .route("/key", post(key_press))
            .route("/goto", post(goto));
        let addr: SocketAddr = BIND_ADDR.parse().expect("valid bind addr");
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                info!(%addr, "screen_ctrl daemon listening");
                if let Err(e) = axum::serve(listener, app).await {
                    warn!(%addr, error = %e, "screen_ctrl daemon stopped");
                }
            }
            Err(e) => warn!(%addr, error = %e, "screen_ctrl daemon failed to bind"),
        }
    })
}

#[derive(Debug, Deserialize)]
struct ClickReq {
    x: i32,
    y: i32,
}

#[derive(Debug, Deserialize)]
struct TypeReq {
    text: String,
}

#[derive(Debug, Deserialize)]
struct KeyReq {
    key: String,
}

#[derive(Debug, Deserialize)]
struct GotoReq {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ScreenshotQuery {
    /// Optional region: "x,y,w,h". Default = full screen.
    region: Option<String>,
}

async fn screenshot(Query(q): Query<ScreenshotQuery>) -> impl IntoResponse {
    let tmp = std::env::temp_dir().join(format!(
        "ff-screen-{}.png",
        chrono::Utc::now().timestamp_millis()
    ));
    let tmp_str = tmp.to_string_lossy().to_string();

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("screencapture");
        c.arg("-x"); // no shutter sound
        if let Some(r) = q.region.as_deref() {
            c.arg("-R").arg(r);
        }
        c.arg(&tmp_str);
        c
    };

    #[cfg(target_os = "linux")]
    let mut cmd = {
        // scrot first, fall back to import (ImageMagick) if missing.
        // Keep it simple — use scrot and let the 503 path handle the
        // missing-tool case.
        let mut c = tokio::process::Command::new("scrot");
        c.arg(&tmp_str);
        c
    };

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"screen_ctrl not implemented for this OS"})),
        )
            .into_response();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let _ = q.region; // suppress unused on Linux when region is unused
        let out = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": format!("screen-capture tool not available: {e}"),
                        "hint": "macOS uses `screencapture` (built-in). Linux: `apt install scrot` or equivalent."
                    })),
                )
                    .into_response();
            }
        };
        if !out.status.success() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("screen-capture failed: {}", String::from_utf8_lossy(&out.stderr))
                })),
            )
                .into_response();
        }
        let bytes = match std::fs::read(&tmp) {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("read screenshot: {e}")})),
                )
                    .into_response();
            }
        };
        let _ = std::fs::remove_file(&tmp);
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "image/png".parse().unwrap());
        (StatusCode::OK, headers, bytes).into_response()
    }
}

async fn click(Json(r): Json<ClickReq>) -> impl IntoResponse {
    run_click(r.x, r.y, false).await
}

async fn double_click(Json(r): Json<ClickReq>) -> impl IntoResponse {
    run_click(r.x, r.y, true).await
}

async fn run_click(x: i32, y: i32, double: bool) -> axum::response::Response {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cliclick");
        let action = if double {
            format!("dc:{},{}", x, y)
        } else {
            format!("c:{},{}", x, y)
        };
        c.arg(action);
        c
    };

    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("xdotool");
        c.arg("mousemove").arg(x.to_string()).arg(y.to_string());
        if double {
            c.arg("click").arg("--repeat").arg("2").arg("1");
        } else {
            c.arg("click").arg("1");
        }
        c
    };

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"click not implemented for this OS"})),
        )
            .into_response();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run_subprocess(cmd, "click").await
}

async fn mouse_move(Json(r): Json<ClickReq>) -> impl IntoResponse {
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = tokio::process::Command::new("cliclick");
        c.arg(format!("m:{},{}", r.x, r.y));
        c
    };
    #[cfg(target_os = "linux")]
    let cmd = {
        let mut c = tokio::process::Command::new("xdotool");
        c.arg("mousemove").arg(r.x.to_string()).arg(r.y.to_string());
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"move not implemented for this OS"})),
        )
            .into_response();
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run_subprocess(cmd, "move").await
}

async fn type_text(Json(r): Json<TypeReq>) -> impl IntoResponse {
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = tokio::process::Command::new("cliclick");
        c.arg(format!("t:{}", r.text));
        c
    };
    #[cfg(target_os = "linux")]
    let cmd = {
        let mut c = tokio::process::Command::new("xdotool");
        c.arg("type").arg("--").arg(&r.text);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"type not implemented for this OS"})),
        )
            .into_response();
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run_subprocess(cmd, "type").await
}

async fn key_press(Json(r): Json<KeyReq>) -> impl IntoResponse {
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = tokio::process::Command::new("cliclick");
        // cliclick uses kp:<keyname> for single keys; kd:<mod> kp:<key>
        // ku:<mod> for chords. Pass through unmapped — caller picks
        // the right syntax for cliclick.
        c.arg(format!("kp:{}", r.key));
        c
    };
    #[cfg(target_os = "linux")]
    let cmd = {
        let mut c = tokio::process::Command::new("xdotool");
        c.arg("key").arg(&r.key);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"key not implemented for this OS"})),
        )
            .into_response();
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run_subprocess(cmd, "key").await
}

async fn goto(Json(r): Json<GotoReq>) -> impl IntoResponse {
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = tokio::process::Command::new("open");
        c.arg(&r.url);
        c
    };
    #[cfg(target_os = "linux")]
    let cmd = {
        let mut c = tokio::process::Command::new("xdg-open");
        c.arg(&r.url);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"goto not implemented for this OS"})),
        )
            .into_response();
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run_subprocess(cmd, "goto").await
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn run_subprocess(
    mut cmd: tokio::process::Command,
    action: &'static str,
) -> axum::response::Response {
    cmd.kill_on_drop(true);
    match cmd.output().await {
        Ok(out) if out.status.success() => {
            (StatusCode::OK, Json(json!({"ok": true, "action": action}))).into_response()
        }
        Ok(out) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("{} failed: {}", action, String::from_utf8_lossy(&out.stderr)),
                "exit": out.status.code(),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": format!("{} tool not available: {e}", action),
                "hint": "macOS: `brew install cliclick`. Linux: `apt install xdotool`."
            })),
        )
            .into_response(),
    }
}
