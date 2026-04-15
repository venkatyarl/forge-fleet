//! Onboarding endpoints for new fleet members.
//!
//! See plan: /Users/venkat/.claude/plans/gentle-questing-valley.md
//!
//! Routes registered in `server.rs::build_router`:
//!   GET  /onboard/bootstrap.sh              — render the per-node install script
//!   POST /api/fleet/self-enroll             — full admission flow (writes fleet_nodes)
//!   POST /api/fleet/enrollment-progress     — bootstrap script callbacks for live UI
//!   GET  /api/fleet/check-ip                — server-side ping probe (for verify actions)
//!   GET  /api/fleet/check-tcp               — server-side TCP probe
//!
//! These endpoints are *complementary* to the existing `/api/fleet/enroll` (which
//! only upserts `fleet_node_runtime`). Self-enroll handles first-join flow: it
//! creates the `fleet_nodes` row, stashes the SSH identity, records hardware/
//! tooling metadata, and kicks off mesh-propagation via the deferred queue.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::server::GatewayState;

// ─── Bootstrap script rendering ──────────────────────────────────────────

const BOOTSTRAP_TEMPLATE: &str =
    include_str!("../../../scripts/bootstrap-node-template.sh");

/// Query params accepted by GET /onboard/bootstrap.sh
#[derive(Debug, Deserialize)]
pub struct BootstrapQuery {
    pub token: Option<String>,
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

pub async fn bootstrap_script(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<BootstrapQuery>,
) -> axum::response::Response {
    // Resolve enrollment token — the one embedded in the script must match
    // what self-enroll validates. If the token QS is absent we reuse the
    // server's shared secret (operator probably hit the URL directly).
    let expected_token: String = match state.fleet_config.as_ref() {
        Some(cfg_lock) => cfg_lock
            .read()
            .await
            .enrollment
            .resolve_shared_secret()
            .unwrap_or_default(),
        None => String::new(),
    };

    let token = q.token.unwrap_or_else(|| expected_token.clone());
    if token.is_empty() || token != expected_token {
        return (
            StatusCode::UNAUTHORIZED,
            "enrollment token missing or invalid\n",
        )
            .into_response();
    }

    // Leader host: derive from the operator's browser connection if possible;
    // else fall back to env / config.
    let leader_host = std::env::var("FORGEFLEET_LEADER_HOST")
        .unwrap_or_else(|_| "192.168.5.100".to_string());
    let leader_port = std::env::var("FORGEFLEET_LEADER_PORT")
        .unwrap_or_else(|_| "51002".to_string());

    // Caller's LAN IP: prefer explicit query param, fall back to
    // X-Forwarded-For / X-Real-IP headers (if a reverse proxy added them),
    // then to a generic placeholder the script will override with `hostname -I`.
    let ip = q
        .ip
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next())
                .map(|s| s.trim().to_string())
        })
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "auto".to_string());

    let name = q.name.unwrap_or_else(|| "newnode".into());
    let ssh_user = q.ssh_user.unwrap_or_else(|| name.clone());
    let role = q.role.unwrap_or_else(|| "builder".into());
    let runtime = q.runtime.unwrap_or_else(|| "auto".into());
    let is_taylor = if name.eq_ignore_ascii_case("taylor") || ip == "192.168.5.100" {
        "true"
    } else {
        "false"
    };

    // Read GitHub owner from fleet_settings; fallback to env; fallback to "venkat-oclaw".
    let github_owner: String = {
        let mut found: Option<String> = None;
        if let Some(pool) = state.operational_store.as_ref().and_then(|os| os.pg_pool()) {
            if let Ok(Some(v)) = ff_db::pg_get_setting(pool, "github.default_owner").await {
                if let Some(s) = v.as_str() {
                    found = Some(s.to_string());
                }
            }
        }
        found
            .or_else(|| std::env::var("FORGEFLEET_GITHUB_OWNER").ok())
            .unwrap_or_else(|| "venkat-oclaw".to_string())
    };

    let script = BOOTSTRAP_TEMPLATE
        .replace("{{LEADER_HOST}}", &leader_host)
        .replace("{{LEADER_PORT}}", &leader_port)
        .replace("{{TOKEN}}", &token)
        .replace("{{NODE_NAME}}", &name)
        .replace("{{NODE_IP}}", &ip)
        .replace("{{SSH_USER}}", &ssh_user)
        .replace("{{ROLE}}", &role)
        .replace("{{RUNTIME}}", &runtime)
        .replace("{{GITHUB_OWNER}}", &github_owner)
        .replace("{{GITHUB_PAT_SECRET_KEY}}", "github.venkat_pat")
        .replace("{{IS_TAYLOR}}", is_taylor);

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        script,
    )
        .into_response()
}

// ─── Self-enroll ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SelfEnrollPayload {
    pub token: String,
    pub name: String,
    pub hostname: Option<String>,
    pub ip: String,
    pub os: String,
    pub os_id: Option<String>,
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

pub async fn self_enroll(
    State(state): State<Arc<GatewayState>>,
    Json(payload): Json<SelfEnrollPayload>,
) -> Result<Json<SelfEnrollResponse>, (StatusCode, Json<Value>)> {
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

    // Validate token against fleet config.
    let expected_token = state
        .fleet_config
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"fleet config not loaded"})),
            )
        })?
        .read()
        .await
        .enrollment
        .resolve_shared_secret()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"enrollment secret not configured"})),
            )
        })?;

    if payload.token != expected_token {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"invalid enrollment token"})),
        ));
    }

    let name = payload.name.trim().to_lowercase();
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"name is required"})),
        ));
    }

    // Determine election_priority = max(existing) + 10 (workers only).
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| db_err("pg_list_nodes", e))?;
    let next_priority = nodes
        .iter()
        .map(|n| n.election_priority)
        .max()
        .unwrap_or(100)
        + 10;

    // Compute default sub_agent_count if the script didn't supply one.
    let sub_agent_count = payload.sub_agent_count.unwrap_or_else(|| {
        compute_default_sub_agents(
            payload.cpu_cores,
            payload.ram_gb,
            payload.has_nvidia.unwrap_or(false),
        )
    });

    // Build FleetNodeRow — mostly defaults; runtime/os/ip from payload.
    let node_row = ff_db::FleetNodeRow {
        name: name.clone(),
        ip: payload.ip.clone(),
        ssh_user: payload.ssh_user.clone(),
        ram_gb: payload.ram_gb,
        cpu_cores: payload.cpu_cores,
        os: payload.os.clone(),
        role: payload.role.clone().unwrap_or_else(|| "builder".into()),
        election_priority: next_priority,
        hardware: payload.os_id.clone().unwrap_or_default(),
        alt_ips: json!([]),
        capabilities: json!({}),
        preferences: json!({}),
        resources: json!({
            "has_nvidia": payload.has_nvidia.unwrap_or(false),
        }),
        status: "online".into(),
        runtime: payload.runtime.clone(),
        models_dir: "~/models".into(),
        disk_quota_pct: 80,
        sub_agent_count,
        gh_account: payload.gh_account.clone(),
        tooling: json!({}),
    };

    ff_db::pg_upsert_node(pool, &node_row)
        .await
        .map_err(|e| db_err("pg_upsert_node", e))?;

    // Stash SSH identity.
    let user_pub = payload.ssh_identity.user_public_key.trim();
    if !user_pub.is_empty() {
        let (key_type, fingerprint) = parse_pubkey_meta(user_pub);
        ff_db::pg_insert_node_ssh_key(pool, &name, "user", user_pub, &key_type, &fingerprint)
            .await
            .map_err(|e| db_err("pg_insert_node_ssh_key(user)", e))?;
    }
    for host_pub in &payload.ssh_identity.host_public_keys {
        let host_pub = host_pub.trim();
        if host_pub.is_empty() {
            continue;
        }
        let (key_type, fingerprint) = parse_pubkey_meta(host_pub);
        ff_db::pg_insert_node_ssh_key(pool, &name, "host", host_pub, &key_type, &fingerprint)
            .await
            .map_err(|e| db_err("pg_insert_node_ssh_key(host)", e))?;
    }

    // Kick off mesh-propagation deferred task. Runs on leader with SSH access
    // to every existing peer; appends new node's user pubkey to each peer's
    // authorized_keys and host keys to known_hosts, then ssh-tests reachability.
    // Implementation of the shell command lives in Phase 3 (ff-agent::mesh_check).
    let mesh_payload = json!({
        "new_node": name,
        "new_node_ip": payload.ip,
        "new_node_ssh_user": payload.ssh_user,
        "user_public_key": user_pub,
        "host_public_keys": payload.ssh_identity.host_public_keys,
    });
    let _ = ff_db::pg_enqueue_deferred(
        pool,
        &format!("Mesh propagate SSH for {name}"),
        "internal",        // new kind; executor handles via mesh_check module
        &mesh_payload,
        "now",
        &json!({}),
        Some("taylor"),    // leader only
        &json!([]),
        Some("self-enroll"),
        Some(5),
    )
    .await
    .map_err(|e| db_err("pg_enqueue_deferred(mesh)", e))?;

    // Assemble peer_ssh_identities for the response so the new node can
    // populate its own authorized_keys + known_hosts.
    let mut peers = Vec::with_capacity(nodes.len());
    for peer in &nodes {
        let user_key = ff_db::pg_list_node_ssh_keys(pool, &peer.name, Some("user"))
            .await
            .unwrap_or_default()
            .into_iter()
            .next()
            .map(|k| k.public_key);
        let host_keys: Vec<String> =
            ff_db::pg_list_node_ssh_keys(pool, &peer.name, Some("host"))
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|k| k.public_key)
                .collect();
        peers.push(PeerSshIdentity {
            name: peer.name.clone(),
            ip: peer.ip.clone(),
            ssh_user: peer.ssh_user.clone(),
            user_public_key: user_key,
            host_public_keys: host_keys,
        });
    }

    // Best-effort: announce the new node via Redis so the dashboard sees it live.
    let _ = ff_agent::fleet_events::publish_node_online(&name).await;

    Ok(Json(SelfEnrollResponse {
        assigned_name: name,
        peer_ssh_identities: peers,
        postgres_url: std::env::var("FORGEFLEET_POSTGRES_URL").ok(),
        redis_url: std::env::var("FORGEFLEET_REDIS_URL").ok(),
    }))
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
    // Also log so operators can tail daemon logs.
    tracing::info!(target: "ff-gateway::onboard", node=%payload.name, step=%payload.step, status=%payload.status, "enrollment progress");
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
    let reachable =
        timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(format!("{ip}:22")))
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
/// dedup via unique constraint on (node_name, fingerprint).
fn parse_pubkey_meta(pubkey: &str) -> (String, String) {
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
fn compute_default_sub_agents(cores: i32, ram_gb: i32, has_nvidia: bool) -> i32 {
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
async fn publish_redis(channel: &str, payload: &str) -> Result<(), String> {
    // Read redis URL from env; default localhost:6380.
    let url = std::env::var("FORGEFLEET_REDIS_URL")
        .unwrap_or_else(|_| "redis://192.168.5.100:6380".into());
    // Parse host:port from URL (redis://host:port or redis://host:port/db).
    let (host, port) = parse_redis_hostport(&url).unwrap_or(("192.168.5.100".into(), 6380));
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
