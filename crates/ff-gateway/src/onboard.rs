//! Onboarding endpoints for new fleet members.
//!
//! Routes registered in `server.rs::build_router`:
//!   GET  /onboard/bootstrap.sh              — render the per-node install script
//!   POST /api/fleet/self-enroll             — full admission flow (writes fleet_workers)
//!   POST /api/fleet/enrollment-progress     — bootstrap script callbacks for live UI
//!   GET  /api/fleet/check-ip                — server-side ping probe (for verify actions)
//!   GET  /api/fleet/check-tcp               — server-side TCP probe
//!
//! These endpoints are *complementary* to the existing `/api/fleet/enroll` (which
//! only upserts `fleet_worker_runtime`). Self-enroll handles first-join flow: it
//! creates the `fleet_workers` row, stashes the SSH identity, records hardware/
//! tooling metadata, and kicks off mesh-propagation via the deferred queue.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::server::GatewayState;

/// Resolve the enrollment policy, falling back to the canonical
/// `enrollment.shared_secret` in the fleet vault (`fleet_secrets`) when the
/// local fleet.toml/env has none. Without this, only nodes with a hand-wired
/// `FORGEFLEET_ENROLLMENT_TOKEN` could serve onboarding — every other
/// gateway 503'd `/onboard/bootstrap.sh` (found live 2026-08-03: zero of 17
/// nodes could onboard vinny until adele was hand-configured).
pub async fn resolve_enrollment_policy(
    state: &GatewayState,
) -> ff_core::config::EnrollmentEnforcement {
    let policy = match state.fleet_config.as_ref() {
        Some(cfg_lock) => cfg_lock.read().await.enrollment.enforcement_policy(),
        None => ff_core::config::EnrollmentEnforcement::MisconfiguredRequired,
    };
    if !matches!(
        policy,
        ff_core::config::EnrollmentEnforcement::MisconfiguredRequired
    ) {
        return policy;
    }
    if let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool())
        && let Ok(Some(secret)) = ff_db::pg_get_secret(pool, "enrollment.shared_secret").await
        && !secret.trim().is_empty()
    {
        return ff_core::config::EnrollmentEnforcement::Required(secret);
    }
    policy
}

// ─── Bootstrap script rendering ──────────────────────────────────────────

pub(crate) const BOOTSTRAP_TEMPLATE: &str =
    include_str!("../../../scripts/bootstrap-computer-template.sh");

/// The ordinary gateway listener has no connection-level TLS evidence. Keep
/// every credential-bearing handler on that listener quarantined; only the
/// dedicated TLS enrollment server may admit nodes. Do not replace this with
/// an environment flag or forwarding header: either would let a plaintext LAN
/// caller self-assert that its request was secure.
fn secure_onboarding_transport_available() -> bool {
    false
}

const ONBOARDING_TRANSPORT_QUARANTINE: &str =
    "new-node onboarding is quarantined until the gateway has server-verified TLS transport";

/// Query params accepted by GET /onboard/bootstrap.sh
#[derive(Debug, Deserialize)]
pub struct BootstrapQuery {
    pub name: Option<String>,
    pub ip: Option<String>,
    pub ssh_user: Option<String>,
    pub role: Option<String>,
    pub runtime: Option<String>,
    /// Optional hardware hints from browser JS; script will re-detect
    /// authoritatively but they help during form rendering.
    pub cores: Option<u32>,
    pub ram_hint: Option<u32>,
}

pub async fn bootstrap_script() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        ONBOARDING_TRANSPORT_QUARANTINE,
    )
        .into_response()
}

pub(crate) struct SecureBootstrapRender<'a> {
    pub leader_host: &'a str,
    pub tls_server_name: &'a str,
    pub tls_ca_pem_b64: &'a str,
    pub tls_spki_pin: &'a str,
    pub name: &'a str,
    pub ip: &'a str,
    pub ssh_user: &'a str,
    pub role: &'a str,
    pub runtime: &'a str,
}

/// Render the Unix bootstrap only after the dedicated TLS handler has
/// authenticated the one-time token, peer IP, canonical node name, and leader
/// epoch. The ordinary HTTP route above never calls this helper.
pub(crate) async fn render_secure_bootstrap_script(
    state: &GatewayState,
    claim: &SecureBootstrapRender<'_>,
) -> axum::response::Response {
    let leader_host = claim.leader_host;
    let leader_port = "51443";
    let name = claim.name;
    let ssh_user = claim.ssh_user;
    let role = claim.role;
    let runtime = claim.runtime;
    let ip = claim.ip;

    // Sanitize bootstrap parameters to prevent shell injection in the rendered script.
    fn sanitize_bootstrap_value(s: &str, max_len: usize) -> String {
        let trimmed = s.trim();
        let valid: String = trimmed
            .chars()
            .take(max_len)
            .filter(|&c| {
                c.is_alphanumeric()
                    || c == '-'
                    || c == '_'
                    || c == '.'
                    || c == '@'
                    || c == '+'
                    || c == ':'
                    || c == '/'
            })
            .collect();
        if valid.is_empty() {
            "unknown".into()
        } else {
            valid
        }
    }
    let name = sanitize_bootstrap_value(name, 64);
    let ssh_user = sanitize_bootstrap_value(ssh_user, 64);
    let role = sanitize_bootstrap_value(role, 32);
    let runtime = sanitize_bootstrap_value(runtime, 32);
    let ip = sanitize_bootstrap_value(ip, 64);
    let is_vinny = if name.eq_ignore_ascii_case("vinny") || ip == "192.168.5.100" {
        "true"
    } else {
        "false"
    };

    // Read GitHub owner from fleet_settings → fleet_secrets → env → fallback.
    // (fleet_secrets is the CLI-managed store; fleet_settings is reserved for
    // structured config and has no `ff` CLI setter yet.)
    let github_owner: String = {
        let mut found: Option<String> = None;
        if let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) {
            if let Ok(Some(v)) = ff_db::pg_get_setting(pool, "github.default_owner").await
                && let Some(s) = v.as_str()
            {
                found = Some(s.to_string());
            }
            if found.is_none()
                && let Ok(Some(s)) = ff_db::pg_get_secret(pool, "github.default_owner").await
                && !s.is_empty()
            {
                found = Some(s);
            }
        }
        found
            .or_else(|| std::env::var("FORGEFLEET_GITHUB_OWNER").ok())
            .unwrap_or_else(|| "venkatyarl".to_string())
    };

    // DB endpoint for the rendered fleet.toml: the serving gateway's own
    // fleet config is authoritative (Postgres/Redis are fleet services that
    // do NOT necessarily live on the gateway/leader host).
    let (db_host, db_port, redis_host, redis_port) = {
        let mut db_h = "192.168.5.104".to_string();
        let mut db_p = "55432".to_string();
        let mut rd_h = db_h.clone();
        let mut rd_p = "56379".to_string();
        if let Some(cfg_lock) = state.fleet_config.as_ref() {
            let cfg = cfg_lock.read().await;
            if let Some(h) = cfg.database.host.as_ref().filter(|h| !h.trim().is_empty()) {
                db_h = h.trim().to_string();
            } else if let Some((h, _)) = cfg
                .database
                .url
                .split('@')
                .next_back()
                .and_then(|s| s.split('/').next())
                .and_then(|s| s.rsplit_once(':'))
            {
                db_h = h.to_string();
            }
            if let Some(p) = cfg.database.port {
                db_p = p.to_string();
            }
            // redis://host:port[/db]
            let redis_rest = cfg
                .redis
                .url
                .strip_prefix("redis://")
                .unwrap_or(&cfg.redis.url);
            if let Some((h, p)) = redis_rest
                .split('/')
                .next()
                .and_then(|s| s.rsplit_once(':'))
            {
                if !h.is_empty() {
                    rd_h = h.to_string();
                }
                if let Ok(port) = p.parse::<u16>() {
                    rd_p = port.to_string();
                }
            }
        }
        (db_h, db_p, rd_h, rd_p)
    };

    let script = BOOTSTRAP_TEMPLATE
        .replace("{{LEADER_HOST}}", &leader_host)
        .replace("{{LEADER_PORT}}", &leader_port)
        .replace("{{TLS_SERVER_NAME}}", claim.tls_server_name)
        .replace("{{TLS_CA_PEM_B64}}", claim.tls_ca_pem_b64)
        .replace("{{TLS_SPKI_PIN}}", claim.tls_spki_pin)
        .replace("{{DB_HOST}}", &db_host)
        .replace("{{DB_PORT}}", &db_port)
        .replace("{{REDIS_HOST}}", &redis_host)
        .replace("{{REDIS_PORT}}", &redis_port)
        .replace("{{COMPUTER_NAME}}", &name)
        .replace("{{COMPUTER_IP}}", &ip)
        .replace("{{SSH_USER}}", &ssh_user)
        .replace("{{ROLE}}", &role)
        .replace("{{RUNTIME}}", &runtime)
        .replace("{{GITHUB_OWNER}}", &github_owner)
        .replace("{{IS_VINNY}}", is_vinny);

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/x-shellscript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        script,
    )
        .into_response()
}

/// GET /onboard/bootstrap.ps1 — Windows PowerShell equivalent of bootstrap.sh.
/// The legacy plaintext listener never renders a credential-bearing script.
pub async fn bootstrap_script_ps1() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        ONBOARDING_TRANSPORT_QUARANTINE,
    )
        .into_response()
}

// ─── Self-enroll ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SelfEnrollPayload {
    /// Legacy body field retained for rolling wire compatibility. The secure
    /// TLS handler requires it to be empty and authenticates only from the
    /// Authorization header.
    #[serde(default)]
    pub token: String,
    pub name: String,
    pub hostname: Option<String>,
    pub ip: String,
    pub os: String,
    pub os_id: Option<String>,
    /// `uname -r` output; e.g. `6.17.0-1014-nvidia` is the tell-tale DGX
    /// Spark kernel (NVIDIA's custom Blackwell kernel layered on Ubuntu).
    /// Used with os_id to derive canonical os_family.
    #[serde(default)]
    pub kernel: Option<String>,
    pub runtime: String,
    pub ram_gb: i32,
    pub cpu_cores: i32,
    pub role: Option<String>,
    pub ssh_user: String,
    pub sub_agent_count: Option<i32>,
    pub gh_account: Option<String>,
    pub has_nvidia: Option<bool>,
    pub ssh_identity: SshIdentity,
}

/// Derive the canonical `computers.os_family` from the enrollment payload.
///
/// os_family drives upgrade_playbook routing (`{os_family}-{install_source}`
/// → `{os_family}` → `all`) so getting this right matters.
///
/// Rules (ordered):
///   1. `os_id=="macos"` → `"macos"`
///   2. `os_id` starts with `dgx` → `"linux-dgx"` (rare — only old DGX
///      servers that still ship `/etc/dgx-release`)
///   3. kernel ends with `-nvidia` → `"linux-dgx"` (DGX Sparks, which show
///      up as `os_id=ubuntu` but their custom NVIDIA kernel is the marker)
///   4. `os_id=="ubuntu"` or `os_id` starts with `linux-ubuntu` → `"linux-ubuntu"`
///   5. `os_id=="windows"` → `"windows"`
///   6. fallback: whatever `os_id` was, or `"linux"`
pub(crate) fn derive_os_family(os_id: Option<&str>, kernel: Option<&str>) -> String {
    let os = os_id.unwrap_or("").to_ascii_lowercase();
    if os == "macos" || os == "darwin" {
        return "macos".into();
    }
    if os.starts_with("dgx") {
        return "linux-dgx".into();
    }
    if kernel
        .map(|k| k.trim_end_matches('\n').ends_with("-nvidia"))
        .unwrap_or(false)
    {
        return "linux-dgx".into();
    }
    if os == "ubuntu" || os.starts_with("linux-ubuntu") || os.starts_with("debian") {
        return "linux-ubuntu".into();
    }
    if os == "windows" {
        return "windows".into();
    }
    if os.is_empty() { "linux".into() } else { os }
}

#[derive(Debug, Deserialize)]
pub struct SshIdentity {
    pub user_public_key: String,
    #[serde(default)]
    pub host_public_keys: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SelfEnrollResponse {
    pub assigned_name: String,
    pub peer_ssh_identities: Vec<PeerSshIdentity>,
    pub postgres_url: Option<String>,
    pub redis_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PeerSshIdentity {
    pub name: String,
    pub ip: String,
    pub ssh_user: String,
    pub user_public_key: Option<String>,
    pub host_public_keys: Vec<String>,
}

/// Plaintext/web enrollment is permanently quarantined. The dedicated TLS
/// listener owns the only supported self-enrollment route.
pub async fn self_enroll() -> Result<Json<SelfEnrollResponse>, (StatusCode, Json<Value>)> {
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": ONBOARDING_TRANSPORT_QUARANTINE})),
    ))
}

// ─── Enrollment progress (script → dashboard) ────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EnrollmentProgress {
    pub name: String,
    pub step: String,
    pub status: String, // 'running'|'ok'|'failed'
    #[serde(default)]
    pub detail: Option<String>,
}

pub async fn enrollment_progress(
    State(_state): State<Arc<GatewayState>>,
    Json(payload): Json<EnrollmentProgress>,
) -> impl IntoResponse {
    if !secure_onboarding_transport_available() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    // Lightweight pass-through: publish to Redis so the dashboard's WS can
    // relay without doing its own Postgres poll. Do NOT block on Redis error.
    let channel = format!("fleet:enrollment:{}", payload.name);
    let message = json!({
        "step": payload.step,
        "status": payload.status,
        "detail": payload.detail,
        "at": chrono::Utc::now().to_rfc3339(),
    })
    .to_string();
    let _ = publish_redis(&channel, &message).await;
    // Also log so operators can tail daemon logs. Include the detail payload
    // for failures — a fatal without its reason is undebuggable (vinny
    // 2026-08-04: mesh_import fatal with no visible cause).
    if payload.status == "failed" {
        tracing::warn!(target: "ff-gateway::onboard", node=%payload.name, step=%payload.step, detail=payload.detail.as_deref().unwrap_or(""), "enrollment step FAILED");
    } else {
        tracing::info!(target: "ff-gateway::onboard", node=%payload.name, step=%payload.step, status=%payload.status, "enrollment progress");
    }
    StatusCode::NO_CONTENT
}

// ─── Check helpers (server-side probes used by the checklist "Verify" buttons) ───

#[derive(Debug, Deserialize)]
pub struct CheckIpQuery {
    pub ip: String,
}

pub async fn check_ip(Query(q): Query<CheckIpQuery>) -> Json<Value> {
    use tokio::time::timeout;
    let ip = q.ip.trim();
    let reachable = timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(format!("{ip}:22")),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);
    Json(json!({"ip": ip, "reachable": reachable}))
}

#[derive(Debug, Deserialize)]
pub struct CheckTcpQuery {
    pub ip: String,
    pub port: u16,
}

// ─── Mesh check endpoint ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MeshCheckQuery {
    pub node: Option<String>,
}

pub async fn get_mesh_check(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<MeshCheckQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"postgres pool not available"})),
            )
        })?;
    let rows = ff_db::pg_list_mesh_status(pool, q.node.as_deref())
        .await
        .map_err(|e| db_err("pg_list_mesh_status", e))?;
    let matrix: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "src_node": r.src_node,
                "dst_node": r.dst_node,
                "status": r.status,
                "last_checked": r.last_checked,
                "last_error": r.last_error,
                "attempts": r.attempts,
            })
        })
        .collect();
    Ok(Json(json!({
        "matrix": matrix.clone(),
        "node_filter": q.node,
        "count": matrix.len(),
    })))
}

// ─── Verify-node endpoint ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct VerifyNodeQuery {
    pub name: String,
}

pub async fn post_verify_computer(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<VerifyNodeQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"postgres pool not available"})),
            )
        })?;
    let report = ff_agent::verify_computer::verify_computer(pool, &q.name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;
    Ok(Json(serde_json::to_value(report).unwrap_or(json!({}))))
}

pub async fn check_tcp(Query(q): Query<CheckTcpQuery>) -> Json<Value> {
    use tokio::time::timeout;
    let reachable = timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(format!("{}:{}", q.ip, q.port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);
    Json(json!({"ip": q.ip, "port": q.port, "reachable": reachable}))
}

// ─── Secret peek quarantine ──────────────────────────────────────────────
//
// Retain the legacy route temporarily so old bootstrap clients receive an
// explicit terminal response, but never consult enrollment policy, Postgres,
// or another secret authority. Unix onboarding reads directly from 1Password;
// the insecure Windows flow is quarantined until it can do the same.
pub async fn secret_peek() -> (StatusCode, Json<Value>) {
    (
        StatusCode::GONE,
        Json(json!({
            "error": "bootstrap secret retrieval is disabled; use direct 1Password authority"
        })),
    )
}

// ─── Fleet tooling matrix (for /versions dashboard page) ────────────────

pub async fn get_fleet_tooling(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"postgres pool not available"})),
            )
        })?;
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| db_err("pg_list_nodes", e))?;
    let out: Vec<Value> = nodes
        .iter()
        .map(|n| json!({ "name": n.name, "tooling": n.tooling }))
        .collect();
    Ok(Json(json!({ "nodes": out })))
}

// ─── Deferred-task endpoints (drift/mesh-retry operator approval) ────────

#[derive(Debug, Deserialize)]
pub struct DeferredQuery {
    pub status: Option<String>,
    pub kind: Option<String>,
    pub node: Option<String>,
    pub tool: Option<String>,
    pub limit: Option<i64>,
}

pub async fn list_deferred(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<DeferredQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"postgres pool not available"})),
            )
        })?;
    let rows = ff_db::pg_list_deferred(pool, q.status.as_deref(), q.limit.unwrap_or(100))
        .await
        .map_err(|e| db_err("pg_list_deferred", e))?;
    let out: Vec<Value> = rows
        .iter()
        .filter(|t| q.kind.as_deref().map(|k| k == t.kind).unwrap_or(true))
        .filter(|t| {
            q.node
                .as_deref()
                .map(|n| t.preferred_node.as_deref() == Some(n))
                .unwrap_or(true)
        })
        .filter(|t| {
            q.tool
                .as_deref()
                .map(|tool| t.payload.get("tool").and_then(|v| v.as_str()) == Some(tool))
                .unwrap_or(true)
        })
        .map(|t| {
            json!({
                "id":             t.id,
                "title":          t.title,
                "kind":           t.kind,
                "status":         t.status,
                "trigger_type":   t.trigger_type,
                "preferred_node": t.preferred_node,
                "payload":        t.payload,
                "attempts":       t.attempts,
                "max_attempts":   t.max_attempts,
                "created_at":     t.created_at,
                "last_error":     t.last_error,
            })
        })
        .collect();
    Ok(Json(json!({ "tasks": out })))
}

#[derive(Debug, Deserialize)]
pub struct PromotePath {
    pub id: String,
}

pub async fn promote_deferred(
    State(state): State<Arc<GatewayState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = state
        .operational_store
        .as_ref()
        .and_then(|os| os.pg_pool())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"postgres pool not available"})),
            )
        })?;
    let promoted = ff_db::pg_promote_deferred(pool, &id)
        .await
        .map_err(|e| db_err("pg_promote_deferred", e))?;
    Ok(Json(json!({ "id": id, "promoted": promoted })))
}

// ─── Internal helpers ────────────────────────────────────────────────────

fn db_err(op: &str, e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!("onboard db error ({op}): {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("{op}: {e}")})),
    )
}

/// Parse the type and fingerprint of an OpenSSH public-key string. Returns
/// ("unknown", sha256-of-key-body) if parsing fails — good enough for DB
/// dedup via unique constraint on (worker_name, fingerprint).
pub(crate) fn parse_pubkey_meta(pubkey: &str) -> (String, String) {
    use sha2::{Digest, Sha256};
    let mut parts = pubkey.split_whitespace();
    let key_type = parts.next().unwrap_or("unknown").to_string();
    let key_body = parts.next().unwrap_or(pubkey);
    let mut hasher = Sha256::new();
    hasher.update(key_body.as_bytes());
    let digest = hasher.finalize();
    let fp = format!("SHA256:{}", hex_encode(&digest));
    (key_type, fp)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Compute default sub_agent_count: `max(1, min(cores/2, ram_gb/16, 4))`,
/// softcap bumped to 8 if the node has an NVIDIA GPU and ≥ 64 GB RAM.
pub(crate) fn compute_default_sub_agents(cores: i32, ram_gb: i32, has_nvidia: bool) -> i32 {
    let from_cores = (cores / 2).max(1);
    let from_ram = (ram_gb / 16).max(1);
    let soft_cap = if has_nvidia && ram_gb >= 64 { 8 } else { 4 };
    let mut n = from_cores.min(from_ram).min(soft_cap);
    if n < 1 {
        n = 1;
    }
    n
}

/// Lightweight Redis publish; no dedicated crate import — we shell out to a
/// tiny helper to avoid adding another dep on ff-gateway (ff-pulse has the
/// redis crate). Best-effort: failures are logged, not raised.
pub(crate) async fn publish_redis(channel: &str, payload: &str) -> Result<(), String> {
    // Read redis URL from env; default localhost:56379.
    let url = std::env::var("FORGEFLEET_REDIS_URL")
        .unwrap_or_else(|_| "redis://192.168.5.100:56379".into());
    // Parse host:port from URL (redis://host:port or redis://host:port/db).
    let (host, port) = parse_redis_hostport(&url).unwrap_or(("192.168.5.100".into(), 56379));
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let mut sock = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    // RESP inline: PUBLISH <channel> <payload>
    let cmd = format!(
        "*3\r\n$7\r\nPUBLISH\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
        channel.len(),
        channel,
        payload.len(),
        payload
    );
    sock.write_all(cmd.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = [0u8; 32];
    let _ = sock.read(&mut buf).await;
    Ok(())
}

fn parse_redis_hostport(url: &str) -> Option<(String, u16)> {
    let s = url.strip_prefix("redis://").unwrap_or(url);
    let s = s.split('/').next()?;
    let mut parts = s.rsplitn(2, ':');
    let port_str = parts.next()?;
    let host = parts.next()?.to_string();
    let port: u16 = port_str.parse().ok()?;
    Some((host, port))
}

#[cfg(test)]
mod bootstrap_lifecycle_tests {
    use super::{
        BOOTSTRAP_TEMPLATE, ONBOARDING_TRANSPORT_QUARANTINE, bootstrap_script,
        bootstrap_script_ps1, secret_peek, secure_onboarding_transport_available, self_enroll,
    };
    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        routing::{get, post},
    };
    use serde_json::json;
    use tower::ServiceExt;

    const SYSTEMD_UNIT: &str = include_str!("../../../deploy/systemd/forgefleetd.service");
    const MCP_SYSTEMD_UNIT: &str = include_str!("../../../deploy/systemd/forgefleet-mcp.service");

    #[test]
    fn linux_bootstrap_uses_canonical_redis_port() {
        assert!(BOOTSTRAP_TEMPLATE.contains("redis://{{REDIS_HOST}}:{{REDIS_PORT}}"));
        assert!(!BOOTSTRAP_TEMPLATE.contains("redis://{{LEADER_HOST}}:6380"));
    }

    #[test]
    fn linux_bootstrap_enforces_reboot_persistent_user_service() {
        for required in [
            "loginctl enable-linger \"$SUDO_INVOKER\"",
            "loginctl show-user \"$SUDO_INVOKER\" -p Linger --value",
            "user_systemctl daemon-reload",
            "user_systemctl enable forgefleetd.service",
            "user_systemctl restart forgefleetd.service",
            "user_systemctl is-enabled forgefleetd.service",
            "user_systemctl is-active forgefleetd.service",
            "user_systemctl enable forgefleetd.service forgefleet-mcp.service",
            "user_systemctl restart forgefleet-mcp.service",
            "user_systemctl is-enabled forgefleet-mcp.service",
            "user_systemctl is-active forgefleet-mcp.service",
        ] {
            assert!(
                BOOTSTRAP_TEMPLATE.contains(required),
                "bootstrap is missing persistence step: {required}"
            );
        }
        assert!(
            !BOOTSTRAP_TEMPLATE
                .contains("loginctl enable-linger \"$SUDO_INVOKER\" 2>/dev/null || true")
        );
        assert!(
            !BOOTSTRAP_TEMPLATE
                .contains("user_systemctl enable forgefleetd.service >/dev/null 2>&1 || true")
        );
    }

    #[test]
    fn canonical_user_unit_starts_at_boot_and_recovers_from_failure() {
        assert!(SYSTEMD_UNIT.contains("Restart=on-failure"));
        assert!(SYSTEMD_UNIT.contains("[Install]\nWantedBy=default.target"));
        assert!(SYSTEMD_UNIT.contains("ExecStart=%h/.local/bin/forgefleetd start"));
        assert!(MCP_SYSTEMD_UNIT.contains("Restart=always"));
        assert!(MCP_SYSTEMD_UNIT.contains("[Install]\nWantedBy=default.target"));
        assert!(
            MCP_SYSTEMD_UNIT
                .contains("ExecStart=%h/.local/bin/forgefleetd mcp --listen 0.0.0.0:50001")
        );
    }

    #[test]
    fn mac_bootstrap_installs_and_loads_a_separate_mcp_launch_agent() {
        for required in [
            "com.forgefleet.forgefleet-mcp.plist",
            "<string>com.forgefleet.forgefleet-mcp</string>",
            "<string>mcp</string>",
            "<string>--listen</string>",
            "<string>0.0.0.0:50001</string>",
            "launchctl bootstrap \"gui/$USER_UID\" \"$MCP_PLIST_TARGET\"",
            "launchctl enable \"$MCP_GUI_DOMAIN\"",
            "launchctl kickstart -k \"$MCP_GUI_DOMAIN\"",
            "launchctl print \"$MCP_GUI_DOMAIN\"",
            "die \"launchd did not register the separate ForgeFleet MCP agent\"",
        ] {
            assert!(
                BOOTSTRAP_TEMPLATE.contains(required),
                "bootstrap is missing separate macOS MCP service step: {required}"
            );
        }
    }

    #[test]
    fn mac_mcp_launch_agent_is_linted_and_atomically_published_before_restart() {
        let stage = BOOTSTRAP_TEMPLATE
            .find("MCP_PLIST_TMP=\"")
            .expect("same-directory MCP plist staging");
        let mcp = &BOOTSTRAP_TEMPLATE[stage..];
        let lint = mcp
            .find("plutil -lint \"$MCP_PLIST_TMP\"")
            .expect("plutil lint");
        let publish = mcp
            .find("mv -f \"$MCP_PLIST_TMP\" \"$MCP_PLIST_TARGET\"")
            .expect("atomic plist publish");
        let bootout = mcp
            .find("launchctl bootout \"gui/$USER_UID\" \"$MCP_PLIST_TARGET\"")
            .expect("launchd bootout");
        let bootstrap = mcp
            .find("launchctl bootstrap \"gui/$USER_UID\" \"$MCP_PLIST_TARGET\"")
            .expect("launchd bootstrap");
        assert!(lint < publish && publish < bootout && bootout < bootstrap);
        assert!(mcp.contains("mktemp \"$PLIST_TARGET_DIR/.com.forgefleet.forgefleet-mcp.XXXXXX\""));
        assert!(!mcp.contains("cat > '$MCP_PLIST_TARGET'"));
    }

    #[test]
    fn bootstrap_verifies_mcp_listener_and_tools_before_client_install() {
        let health = BOOTSTRAP_TEMPLATE
            .find("http://127.0.0.1:50001/mcp/health")
            .expect("MCP health probe");
        let tools = BOOTSTRAP_TEMPLATE
            .find("\\\"method\\\":\\\"tools/list\\\"")
            .expect("MCP tools/list probe");
        let install = BOOTSTRAP_TEMPLATE
            .find("mcp install --for all --no-instructions")
            .expect("MCP client install");
        assert!(health < tools && tools < install);
        assert!(BOOTSTRAP_TEMPLATE.contains("die \"separate ForgeFleet MCP returned no tools\""));
    }

    #[test]
    fn linux_bootstrap_restricts_fleet_toml_for_new_and_existing_files() {
        assert!(BOOTSTRAP_TEMPLATE.contains("umask 077\ncat > '$FLEET_TOML'"));
        assert!(BOOTSTRAP_TEMPLATE.contains("run_as_user chmod 600 \"$FLEET_TOML\""));
        assert!(BOOTSTRAP_TEMPLATE.contains("failed to restrict $FLEET_TOML to mode 0600"));
        assert!(BOOTSTRAP_TEMPLATE.contains("$FLEET_TOML_RESULT; mode=0600"));
    }

    #[test]
    fn linux_bootstrap_delegates_mcp_config_to_typed_installer() {
        assert!(BOOTSTRAP_TEMPLATE.contains(
            "run_as_user \"$USER_HOME/.local/bin/ff\" mcp install --for all --no-instructions"
        ));
        assert!(BOOTSTRAP_TEMPLATE.contains("grep -Fq '✗'"));
        assert!(
            BOOTSTRAP_TEMPLATE
                .contains("report \"mcp-config\" failed \"canonical installer reported")
        );
        assert!(
            BOOTSTRAP_TEMPLATE.contains(
                "die \"ff mcp install reported one or more client configuration failures\""
            )
        );
        assert!(BOOTSTRAP_TEMPLATE.contains("die \"ff mcp install failed\""));
        for stale_writer in [
            "CLAUDE_MCP_FILE",
            "CODEX_CONFIG_FILE",
            "GEMINI_FILE",
            "claude mcp add forgefleet",
        ] {
            assert!(
                !BOOTSTRAP_TEMPLATE.contains(stale_writer),
                "bootstrap still contains bespoke MCP writer: {stale_writer}"
            );
        }
    }

    #[test]
    fn linux_bootstrap_installs_official_1password_cli_for_host_architecture() {
        for required in [
            "https://downloads.1password.com/linux/keys/1password.asc",
            "OP_DEB_ARCH=\"$(dpkg --print-architecture)\"",
            "https://downloads.1password.com/linux/debian/%s stable main",
            "apt-get install -y 1password-cli",
        ] {
            assert!(
                BOOTSTRAP_TEMPLATE.contains(required),
                "bootstrap is missing 1Password prerequisite: {required}"
            );
        }
    }

    #[test]
    fn bootstrap_never_receives_vault_or_centralized_credentials() {
        for forbidden in [
            "peek_secret",
            "/api/fleet/secret-peek",
            "secret-peek?token=",
            "fleet_secrets",
            "ff github sync",
            "github_ssh_id_venkat_priv",
            "github_ssh_id_venkat_pub",
            "anthropic.oauth_token.credentials",
            "openai.oauth_token.credentials",
            "moonshot.oauth_token.credentials",
            "OP_SERVICE_ACCOUNT_TOKEN",
            "OP_VAULT_REF",
            "CREDENTIAL_DOCUMENT_REF",
            "id_venkat.private",
            ".credentials.json",
            ".codex/auth.json",
            ".kimi/credentials",
        ] {
            assert!(
                !BOOTSTRAP_TEMPLATE.contains(forbidden),
                "bootstrap still contains target-side credential path: {forbidden}"
            );
        }
        assert!(BOOTSTRAP_TEMPLATE.contains("https://github.com/${GITHUB_OWNER}/forge-fleet.git"));
        assert!(BOOTSTRAP_TEMPLATE.contains("auth deferred to fleet distributor"));
    }

    #[test]
    fn bootstrap_defers_cloud_auth_until_after_admission() {
        assert!(BOOTSTRAP_TEMPLATE.contains("authentication remains post-enrollment"));
        assert!(BOOTSTRAP_TEMPLATE.contains("auth deferred to fleet distributor"));
        for forbidden in ["atomic_install_json_credential", "document get", "op://"] {
            assert!(!BOOTSTRAP_TEMPLATE.contains(forbidden));
        }
    }

    #[test]
    fn bootstrap_uses_public_https_clone_and_retains_pinned_future_host_trust() {
        for required in [
            "https://github.com/${GITHUB_OWNER}/forge-fleet.git",
            "UserKnownHostsFile ~/.ssh/known_hosts.github",
            "GlobalKnownHostsFile /dev/null",
            "StrictHostKeyChecking yes",
            "SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s",
            "SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM",
            "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU",
        ] {
            assert!(
                BOOTSTRAP_TEMPLATE.contains(required),
                "bootstrap is missing GitHub SSH verification contract: {required}"
            );
        }
        assert!(!BOOTSTRAP_TEMPLATE.contains("ssh-keyscan"));
        assert!(!BOOTSTRAP_TEMPLATE.contains("id_venkat.private"));
        assert!(!BOOTSTRAP_TEMPLATE.contains("git@github.com-venkat"));
    }

    #[tokio::test]
    async fn legacy_secret_peek_route_is_permanently_fail_closed() {
        let (status, Json(body)) = secret_peek().await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(
            body,
            json!({
                "error": "bootstrap secret retrieval is disabled; use direct 1Password authority"
            })
        );
        assert!(body.get("value").is_none());
        assert!(body.get("key").is_none());
    }

    #[test]
    fn credential_bearing_onboarding_stays_quarantined_without_server_tls_evidence() {
        assert!(!secure_onboarding_transport_available());
        assert!(ONBOARDING_TRANSPORT_QUARANTINE.contains("server-verified TLS"));
        assert!(!ONBOARDING_TRANSPORT_QUARANTINE.is_empty());
    }

    #[tokio::test]
    async fn legacy_http_ps1_and_web_enrollment_are_quarantined_over_real_http() {
        let router = Router::new()
            .route("/onboard/bootstrap.sh", get(bootstrap_script))
            .route("/onboard/bootstrap.ps1", get(bootstrap_script_ps1))
            .route("/api/fleet/self-enroll", post(self_enroll));

        for (method, path) in [
            ("GET", "/onboard/bootstrap.sh"),
            ("GET", "/onboard/bootstrap.ps1"),
            ("POST", "/api/fleet/self-enroll"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .expect("legacy onboarding request"),
                )
                .await
                .expect("legacy onboarding response");
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            let body = to_bytes(response.into_body(), 16 * 1024)
                .await
                .expect("legacy onboarding response body");
            let body = String::from_utf8(body.to_vec()).expect("UTF-8 quarantine response");
            assert!(body.contains("server-verified TLS"), "{path}: {body}");
            assert!(
                !body.contains("FORGEFLEET_ENROLLMENT_TOKEN"),
                "{path}: {body}"
            );
        }
    }

    #[test]
    fn unix_bootstrap_does_not_embed_the_enrollment_secret() {
        assert!(!BOOTSTRAP_TEMPLATE.contains("{{TOKEN}}"));
        assert!(BOOTSTRAP_TEMPLATE.contains("TOKEN=\"${FORGEFLEET_ENROLLMENT_TOKEN:-}\""));
        assert!(
            BOOTSTRAP_TEMPLATE.contains("a valid one-time FORGEFLEET_ENROLLMENT_TOKEN is required")
        );
        assert!(BOOTSTRAP_TEMPLATE.contains("--data-binary @-"));
        assert!(
            BOOTSTRAP_TEMPLATE.contains("unset ENROLL_PAYLOAD TOKEN FORGEFLEET_ENROLLMENT_TOKEN")
        );
        assert!(!BOOTSTRAP_TEMPLATE.contains("--data \"$ENROLL_PAYLOAD\""));
    }

    #[test]
    fn service_account_token_is_absent_from_joining_node_script() {
        for forbidden in [
            "OP_SERVICE_ACCOUNT_TOKEN",
            "--preserve-env=FORGEFLEET_ENROLLMENT_TOKEN,",
            "document get",
            "op://",
            "private_key",
        ] {
            assert!(
                !BOOTSTRAP_TEMPLATE.contains(forbidden),
                "joining-node bootstrap still contains broad credential path: {forbidden}"
            );
        }
        assert!(BOOTSTRAP_TEMPLATE.contains("export -n FORGEFLEET_ENROLLMENT_TOKEN"));
    }

    #[test]
    fn linux_bootstrap_never_fetches_or_persists_github_pat() {
        assert!(
            BOOTSTRAP_TEMPLATE
                .contains("API token is injected on demand from fleet/1Password authority")
        );
        for forbidden in [
            "GITHUB_PAT_SECRET_KEY",
            "PAT_VALUE",
            "gh auth login --with-token",
        ] {
            assert!(
                !BOOTSTRAP_TEMPLATE.contains(forbidden),
                "bootstrap still contains plaintext PAT path: {forbidden}"
            );
        }
    }
}
