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
    http::{HeaderMap, StatusCode},
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

const BOOTSTRAP_TEMPLATE_PS1: &str =
    include_str!("../../../scripts/bootstrap-computer-template.ps1");

/// New-node enrollment currently has no server-owned TLS listener or trusted
/// transport extension. Keep every credential-bearing onboarding handler
/// quarantined until the TLS lane can supply connection-level evidence. Do not
/// replace this with an environment flag or forwarding header: either would
/// let a plaintext LAN caller self-assert that its request was secure.
fn secure_onboarding_transport_available() -> bool {
    false
}

const ONBOARDING_TRANSPORT_QUARANTINE: &str =
    "new-node onboarding is quarantined until the gateway has server-verified TLS transport";

/// Bootstrap script authorization belongs in a request header, never in a URL
/// query that is routinely retained by browser history, access logs, proxies,
/// and shell history.  This remains dormant behind the transport quarantine,
/// but makes the eventual TLS-only path fail closed when the header is absent.
fn bootstrap_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

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

pub async fn bootstrap_script(
    State(_state): State<Arc<GatewayState>>,
    _headers: HeaderMap,
    Query(_q): Query<BootstrapQuery>,
) -> axum::response::Response {
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
/// Same query params, same placeholder substitutions, different template.
pub async fn bootstrap_script_ps1(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<BootstrapQuery>,
) -> axum::response::Response {
    if !secure_onboarding_transport_available() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            ONBOARDING_TRANSPORT_QUARANTINE,
        )
            .into_response();
    }
    let policy = match state.fleet_config.as_ref() {
        Some(cfg_lock) => cfg_lock.read().await.enrollment.enforcement_policy(),
        None => ff_core::config::EnrollmentEnforcement::MisconfiguredRequired,
    };
    match &policy {
        ff_core::config::EnrollmentEnforcement::Disabled => {
            tracing::warn!(
                endpoint = "/onboard/bootstrap.ps1",
                "enrollment token check DISABLED (require_shared_secret=false) — serving script without auth"
            );
        }
        ff_core::config::EnrollmentEnforcement::Required(expected)
            if bootstrap_bearer_token(&headers) != Some(expected.as_str()) =>
        {
            return (
                StatusCode::UNAUTHORIZED,
                "# enrollment bearer token missing or invalid\n",
            )
                .into_response();
        }
        ff_core::config::EnrollmentEnforcement::Required(_) => {}
        ff_core::config::EnrollmentEnforcement::MisconfiguredRequired => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "# enrollment shared secret not configured\n",
            )
                .into_response();
        }
    }

    let leader_host =
        std::env::var("FORGEFLEET_LEADER_HOST").unwrap_or_else(|_| "192.168.5.100".to_string());
    let leader_port =
        std::env::var("FORGEFLEET_LEADER_PORT").unwrap_or_else(|_| "51002".to_string());
    let ip =
        q.ip.filter(|s| !s.is_empty())
            .or_else(|| {
                headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.split(',').next())
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| "auto".to_string());
    let name = q.name.unwrap_or_else(|| "newnode".into());
    let ssh_user = q.ssh_user.unwrap_or_else(|| name.clone());
    let role = q.role.unwrap_or_else(|| "builder".into());
    let runtime = q.runtime.unwrap_or_else(|| "auto".into());

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
    let name = sanitize_bootstrap_value(&name, 64);
    let ssh_user = sanitize_bootstrap_value(&ssh_user, 64);
    let role = sanitize_bootstrap_value(&role, 32);
    let runtime = sanitize_bootstrap_value(&runtime, 32);
    let ip = sanitize_bootstrap_value(&ip, 64);

    let is_vinny = if name.eq_ignore_ascii_case("vinny") || ip == "192.168.5.100" {
        "true"
    } else {
        "false"
    };
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

    let script = BOOTSTRAP_TEMPLATE_PS1
        .replace("{{LEADER_HOST}}", &leader_host)
        .replace("{{LEADER_PORT}}", &leader_port)
        // PowerShell onboarding remains quarantined and must not receive a
        // server-side enrollment secret through rendered script content.
        .replace("{{TOKEN}}", "")
        .replace("{{COMPUTER_NAME}}", &name)
        .replace("{{COMPUTER_IP}}", &ip)
        .replace("{{SSH_USER}}", &ssh_user)
        .replace("{{ROLE}}", &role)
        .replace("{{RUNTIME}}", &runtime)
        .replace("{{GITHUB_OWNER}}", &github_owner)
        .replace("{{GITHUB_PAT_SECRET_KEY}}", "github.venkat_pat")
        .replace("{{IS_VINNY}}", is_vinny);

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        script,
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

pub async fn self_enroll(
    State(state): State<Arc<GatewayState>>,
    Json(payload): Json<SelfEnrollPayload>,
) -> Result<Json<SelfEnrollResponse>, (StatusCode, Json<Value>)> {
    if !secure_onboarding_transport_available() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": ONBOARDING_TRANSPORT_QUARANTINE})),
        ));
    }
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

    // Consult enrollment policy (require_shared_secret flag + resolved secret).
    let policy = state
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
        .enforcement_policy();

    match &policy {
        ff_core::config::EnrollmentEnforcement::Disabled => {
            tracing::warn!(
                endpoint = "/api/fleet/self-enroll",
                node = %payload.name,
                "enrollment token check DISABLED (require_shared_secret=false) — accepting request without auth"
            );
        }
        ff_core::config::EnrollmentEnforcement::MisconfiguredRequired => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"enrollment secret not configured"})),
            ));
        }
        ff_core::config::EnrollmentEnforcement::Required(expected) => {
            if &payload.token != expected {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error":"invalid enrollment token"})),
                ));
            }
        }
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
        // Read-only hardware fields (joined from `computers` on read); not
        // written through the worker upsert.
        gpu_kind: None,
        gpu_model: None,
        gpu_vram_gb: None,
        gpu_total_vram_gb: None,
        has_gpu: None,
        computer_ram_gb: None,
        computer_cpu_cores: None,
        computer_status: None,
    };

    ff_db::pg_upsert_node(pool, &node_row)
        .await
        .map_err(|e| db_err("pg_upsert_node", e))?;

    // UPSERT the `computers` row so Pulse v2 has a row to check against on
    // first beat (without this, forgefleetd logs "no computers row for this
    // host; Pulse v2 disabled until enrollment" and never publishes). We
    // also derive canonical os_family here rather than trusting whatever
    // string the client sent — the bootstrap script often sends "linux" for
    // DGX Sparks since /etc/dgx-release is absent on Blackwell; we detect
    // via `uname -r` ending in `-nvidia` instead. (Closes #114.)
    let os_family = derive_os_family(payload.os_id.as_deref(), payload.kernel.as_deref());
    let default_source_tree_path = if node_row.role.eq_ignore_ascii_case("leader") {
        "~/projects/forge-fleet"
    } else {
        "~/.forgefleet/sub-agents/sub-agent-0/forge-fleet"
    };
    let has_gpu = payload.has_nvidia.unwrap_or(false);
    let _ = sqlx::query(
        "INSERT INTO computers (
            name, primary_ip, os_family, os_distribution, os_version,
            cpu_cores, total_ram_gb, has_gpu, gpu_kind,
            ssh_user, status, source_tree_path, metadata
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         ON CONFLICT (name) DO UPDATE SET
            primary_ip       = EXCLUDED.primary_ip,
            os_family        = EXCLUDED.os_family,
            os_distribution  = COALESCE(computers.os_distribution, EXCLUDED.os_distribution),
            os_version       = COALESCE(computers.os_version, EXCLUDED.os_version),
            cpu_cores        = EXCLUDED.cpu_cores,
            total_ram_gb     = EXCLUDED.total_ram_gb,
            has_gpu          = EXCLUDED.has_gpu,
            gpu_kind         = COALESCE(computers.gpu_kind, EXCLUDED.gpu_kind),
            ssh_user         = EXCLUDED.ssh_user,
            status           = EXCLUDED.status,
            source_tree_path = COALESCE(computers.source_tree_path, EXCLUDED.source_tree_path)",
    )
    .bind(&name)
    .bind(&payload.ip)
    .bind(&os_family)
    .bind(payload.os_id.as_deref())
    .bind(&payload.os)
    .bind(payload.cpu_cores)
    .bind(payload.ram_gb)
    .bind(has_gpu)
    .bind(if has_gpu { Some("nvidia") } else { None })
    .bind(&payload.ssh_user)
    .bind("online")
    .bind(default_source_tree_path)
    .bind(json!({
        "kernel":         payload.kernel,
        "enrolled_via":   "self_enroll",
        "runtime":        payload.runtime,
    }))
    .execute(pool)
    .await;

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
        "internal", // new kind; executor handles via mesh_check module
        &mesh_payload,
        "now",
        &json!({}),
        Some("vinny"), // leader only
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
        let host_keys: Vec<String> = ff_db::pg_list_node_ssh_keys(pool, &peer.name, Some("host"))
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
        // #44: never hand out this gateway's own DSN env — it pins the
        // enrolling node to the CURRENT primary's IP outside fleet.toml (the
        // vinny-death time bomb: 12 nodes carried a dead .100 DSN in their
        // units). Nodes derive the DSN from fleet.toml + dsn_failover; the
        // fields stay for response-shape compatibility but are always None.
        // No client-side consumer reads them (verified 2026-07-17).
        postgres_url: None,
        redis_url: None,
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
        BOOTSTRAP_TEMPLATE, ONBOARDING_TRANSPORT_QUARANTINE, bootstrap_bearer_token, secret_peek,
        secure_onboarding_transport_available,
    };
    use axum::{
        Json,
        http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION},
    };
    use serde_json::json;
    use std::{fs, process::Command, time::SystemTime};

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
            "OP_BIN=\"$(run_as_user bash -lc 'command -v op'",
        ] {
            assert!(
                BOOTSTRAP_TEMPLATE.contains(required),
                "bootstrap is missing 1Password prerequisite: {required}"
            );
        }
    }

    #[test]
    fn bootstrap_reads_credentials_directly_from_1password_without_http_peek() {
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
        ] {
            assert!(
                !BOOTSTRAP_TEMPLATE.contains(forbidden),
                "bootstrap still contains quarantined credential path: {forbidden}"
            );
        }
        for required in [
            "OP_VAULT_REF=\"${FORGEFLEET_OP_VAULT_REF:-mtfbfuettwrsog55of33tbribq}\"",
            "OP_GITHUB_SSH_ITEM_REF=\"${FORGEFLEET_OP_GITHUB_SSH_ITEM_REF:-ww3gtuioogq3sdfpyfdlqaryhi}\"",
            "OP_CLAUDE_CREDENTIAL_DOCUMENT_REF=\"${FORGEFLEET_OP_CLAUDE_CREDENTIAL_DOCUMENT_REF:-z4zjqnfchtpv5ynody4mesmysa}\"",
            "OP_CODEX_CREDENTIAL_DOCUMENT_REF=\"${FORGEFLEET_OP_CODEX_CREDENTIAL_DOCUMENT_REF:-u432zsofisggkhraw6c2he7yei}\"",
            "OP_KIMI_CREDENTIAL_DOCUMENT_REF=\"${FORGEFLEET_OP_KIMI_CREDENTIAL_DOCUMENT_REF:-wls3pwdwilxbgmuh6qxwc676ga}\"",
            "\"$OP_BIN\" read \"op://${OP_VAULT_REF}/${OP_GITHUB_SSH_ITEM_REF}/private_key\"",
            "\"$OP_BIN\" read \"op://${OP_VAULT_REF}/${OP_GITHUB_SSH_ITEM_REF}/public_key\"",
            "\"$OP_BIN\" document get \"$OP_CLAUDE_CREDENTIAL_DOCUMENT_REF\" --vault \"$OP_VAULT_REF\"",
            "\"$OP_BIN\" document get \"$OP_CODEX_CREDENTIAL_DOCUMENT_REF\" --vault \"$OP_VAULT_REF\"",
            "\"$OP_BIN\" document get \"$OP_KIMI_CREDENTIAL_DOCUMENT_REF\" --vault \"$OP_VAULT_REF\"",
            "unset OP_SERVICE_ACCOUNT_TOKEN",
        ] {
            assert!(
                BOOTSTRAP_TEMPLATE.contains(required),
                "bootstrap is missing direct 1Password authority step: {required}"
            );
        }
    }

    #[test]
    fn bootstrap_validates_and_atomically_installs_cloud_credentials() {
        for required in [
            "atomic_install_json_credential()",
            "mktemp \"$final_dir/.forgefleet-credential.XXXXXX\"",
            "run_as_user chmod 600 \"$temp_path\"",
            "python3 -m json.tool > \"$1\"",
            "run_as_user mv -f \"$temp_path\" \"$final_path\"",
            "atomic_install_json_credential \"$USER_HOME/.claude/.credentials.json\"",
            "atomic_install_json_credential \"$USER_HOME/.codex/auth.json\"",
            "atomic_install_json_credential \"$USER_HOME/.kimi/credentials/kimi-code.json\"",
            "existing credentials were preserved",
        ] {
            assert!(
                BOOTSTRAP_TEMPLATE.contains(required),
                "bootstrap is missing atomic credential contract: {required}"
            );
        }
        for unsafe_write in [
            "tee \"$USER_HOME/.claude/.credentials.json\"",
            "tee \"$USER_HOME/.codex/auth.json\"",
            "tee \"$USER_HOME/.kimi/credentials/kimi-code.json\"",
        ] {
            assert!(
                !BOOTSTRAP_TEMPLATE.contains(unsafe_write),
                "bootstrap still overwrites a credential before validation: {unsafe_write}"
            );
        }
    }

    #[test]
    fn bootstrap_validates_github_key_pair_and_uses_only_pinned_host_trust() {
        for required in [
            "ssh-keygen -y -f \"$VENKAT_PRIV_TMP\"",
            "DERIVED_VENKAT_MATERIAL",
            "STORED_VENKAT_PUB",
            "private/public key pair does not match",
            "publish_github_key_pair()",
            "restore_github_key_pair()",
            "mktemp -d \"$final_dir/.forgefleet-key-pair-backup.XXXXXX\"",
            "the prior canonical pair was restored",
            "mv -f \"$staged_private\" \"$final_private\"",
            "mv -f \"$staged_public\" \"$final_public\"",
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
    }

    #[test]
    fn github_key_pair_publication_rolls_back_each_rename_failure_and_absence() {
        let helper_start = BOOTSTRAP_TEMPLATE
            .find("# BEGIN github-key-pair-publication-helper")
            .expect("key-pair publication helper start");
        let helper_end = BOOTSTRAP_TEMPLATE
            .find("# END github-key-pair-publication-helper")
            .expect("key-pair publication helper end");
        let helper = &BOOTSTRAP_TEMPLATE[helper_start..helper_end];
        let script = [
            r#"
set -eu

# Inject a failure *after* the selected rename has executed. This models the
# most dangerous failure report: a canonical path changed before the caller
# learned the publication step failed.
run_as_user() {
  if [ "${1:-}" = "mv" ] && [ "${3:-}" = "${FAIL_SOURCE:-}" ] && [ "${INJECTED:-}" != "yes" ]; then
    command "$@"
    INJECTED=yes
    return 1
  fi
  command "$@"
}
"#,
            helper,
            r#"

exercise_case() {
  local label="$1" initial_state="$2" fail_half="$3"
  local case_dir="$PAIR_TEST_DIR/$label"
  local final_private="$case_dir/id_venkat" final_public="$case_dir/id_venkat.pub"
  local staged_private="$case_dir/.private.new" staged_public="$case_dir/.public.new"
  mkdir -p "$case_dir"
  if [ "$initial_state" = "present" ]; then
    printf '%s\n' 'old-private' > "$final_private"
    printf '%s\n' 'ssh-ed25519 old-public old-comment' > "$final_public"
    chmod 600 "$final_private"
    chmod 644 "$final_public"
  fi
  printf '%s\n' 'new-private' > "$staged_private"
  printf '%s\n' 'ssh-ed25519 new-public new-comment' > "$staged_public"
  chmod 600 "$staged_private"
  chmod 644 "$staged_public"

  INJECTED=""
  if [ "$fail_half" = "private" ]; then
    FAIL_SOURCE="$staged_private"
  else
    FAIL_SOURCE="$staged_public"
  fi
  if publish_github_key_pair \
    "$staged_private" "$staged_public" "$final_private" "$final_public"; then
    echo "publication unexpectedly succeeded: $label" >&2
    return 20
  else
    local rc=$?
    [ "$rc" -eq 1 ] || {
      echo "rollback was not reported successful: $label rc=$rc" >&2
      return 21
    }
  fi

  if [ "$initial_state" = "present" ]; then
    [ "$(cat "$final_private")" = "old-private" ]
    [ "$(cat "$final_public")" = "ssh-ed25519 old-public old-comment" ]
    [ "$(stat -c %a "$final_private")" = "600" ]
    [ "$(stat -c %a "$final_public")" = "644" ]
  else
    [ ! -e "$final_private" ]
    [ ! -e "$final_public" ]
  fi
}

exercise_case present_public_failure present public
exercise_case present_private_failure present private
exercise_case absent_public_failure absent public
exercise_case absent_private_failure absent private
"#,
        ]
        .concat();

        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let test_dir = std::env::temp_dir().join(format!(
            "forgefleet-key-pair-rollback-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&test_dir).expect("create key-pair rollback test directory");
        let output = Command::new("bash")
            .arg("-c")
            .arg(script)
            .env("PAIR_TEST_DIR", &test_dir)
            .output()
            .expect("execute key-pair rollback fault-injection test");
        fs::remove_dir_all(&test_dir).expect("remove key-pair rollback test directory");
        assert!(
            output.status.success(),
            "key-pair rollback fault-injection failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn bootstrap_script_auth_is_header_only_and_missing_tokens_fail_closed() {
        let headers = HeaderMap::new();
        assert_eq!(bootstrap_bearer_token(&headers), None);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert_eq!(bootstrap_bearer_token(&headers), None);
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bootstrap_bearer_token(&headers), None);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer expected-token"),
        );
        assert_eq!(bootstrap_bearer_token(&headers), Some("expected-token"));

        let source = include_str!("onboard.rs");
        for forbidden_parts in [
            ["pub token:", " Option<String>"],
            ["unwrap_or_else(|| ", "expected_token.clone())"],
            [".replace(\"{{TOKEN}}\", ", "&token)"],
        ] {
            let forbidden = forbidden_parts.concat();
            assert!(
                !source.contains(&forbidden),
                "onboarding source contains query-token fallback/rendering: {forbidden}"
            );
        }
        assert!(source.contains(&[".replace(\"{{TOKEN}}\", ", "\"\")"].concat()));
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

    #[test]
    fn unix_bootstrap_does_not_embed_the_enrollment_secret() {
        assert!(!BOOTSTRAP_TEMPLATE.contains("{{TOKEN}}"));
        assert!(BOOTSTRAP_TEMPLATE.contains("TOKEN=\"${FORGEFLEET_ENROLLMENT_TOKEN:-}\""));
        assert!(
            BOOTSTRAP_TEMPLATE
                .contains("a valid one-time FORGEFLEET_ENROLLMENT_TOKEN is required")
        );
        assert!(BOOTSTRAP_TEMPLATE.contains("--data-binary @-"));
        assert!(
            BOOTSTRAP_TEMPLATE.contains("unset ENROLL_PAYLOAD TOKEN FORGEFLEET_ENROLLMENT_TOKEN")
        );
        assert!(!BOOTSTRAP_TEMPLATE.contains("--data \"$ENROLL_PAYLOAD\""));
    }

    #[test]
    fn service_account_token_is_fail_closed_and_never_forwarded_in_argv_or_reports() {
        assert!(BOOTSTRAP_TEMPLATE.contains("[ -n \"${OP_SERVICE_ACCOUNT_TOKEN:-}\" ]"));
        assert!(BOOTSTRAP_TEMPLATE.contains("\"$OP_BIN\" whoami >/dev/null 2>&1"));
        assert!(
            BOOTSTRAP_TEMPLATE
                .contains("export -n FORGEFLEET_ENROLLMENT_TOKEN OP_SERVICE_ACCOUNT_TOKEN")
        );
        assert!(
            BOOTSTRAP_TEMPLATE.contains("OP_SERVICE_ACCOUNT_TOKEN=\"$OP_SERVICE_ACCOUNT_TOKEN\"")
        );
        for forbidden in [
            "env OP_SERVICE_ACCOUNT_TOKEN=",
            "--token $OP_SERVICE_ACCOUNT_TOKEN",
            "--token \"$OP_SERVICE_ACCOUNT_TOKEN\"",
            "report \"OP_SERVICE_ACCOUNT_TOKEN",
            "report \"1password\" ok \"$OP_SERVICE_ACCOUNT_TOKEN",
        ] {
            assert!(
                !BOOTSTRAP_TEMPLATE.contains(forbidden),
                "service-account token could enter argv/report via: {forbidden}"
            );
        }
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
