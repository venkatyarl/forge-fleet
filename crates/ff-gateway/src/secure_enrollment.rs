//! Dedicated, leader-only TLS enrollment transport (V289).
//!
//! This module is intentionally separate from the ordinary gateway router.
//! Port 51002 keeps every credential-bearing onboarding route quarantined;
//! only the currently elected leader may bind 51443, and every request is
//! re-fenced against the authoritative Postgres leader epoch.

use std::{
    io::Cursor,
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use rustls::{
    RootCertStore,
    client::{WebPkiServerVerifier, danger::ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use zeroize::Zeroizing;

use crate::{
    onboard::{
        BootstrapQuery, EnrollmentProgress, PeerSshIdentity, SecureBootstrapRender,
        SelfEnrollPayload, SelfEnrollResponse, compute_default_sub_agents, derive_os_family,
        parse_pubkey_meta, publish_redis_at, render_secure_bootstrap_script,
    },
    server::GatewayState,
};

pub const ENROLLMENT_TLS_PORT: u16 = 51_443;
const TOKEN_PREFIX: &str = "ffe1_";
const TOKEN_BYTES: usize = 32;
const LEADER_FRESHNESS_SECS: i64 = 45;
const OP_SERVICE_ACCOUNT_TOKEN_KEY: &str = "1Password:service_account_token";
const TLS_CERT_REF_KEY: &str = "enrollment.tls_cert_ref";
const TLS_PRIVATE_KEY_REF_KEY: &str = "enrollment.tls_private_key_ref";
const TLS_CA_REF_KEY: &str = "enrollment.tls_ca_ref";
const TLS_SPKI_PIN_REF_KEY: &str = "enrollment.tls_spki_pin_ref";
const TLS_SERVER_NAME_KEY: &str = "enrollment.tls_server_name";
const MAX_TLS_MATERIAL_BYTES: usize = 1024 * 1024;
const TRUSTED_OP_PATHS: &[&str] = &[
    "/usr/bin/op",
    "/usr/local/bin/op",
    "/opt/homebrew/bin/op",
    "/opt/1Password/op",
];

#[cfg(unix)]
fn validate_trusted_op_path_component(
    path: &Path,
    metadata: &std::fs::Metadata,
    executable: bool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "1Password path component {} is a symlink",
        path.display()
    );
    anyhow::ensure!(
        metadata.mode() & 0o022 == 0,
        "1Password path component {} is group/world writable",
        path.display()
    );
    anyhow::ensure!(
        metadata.uid() == 0,
        "1Password path component {} is not root-owned",
        path.display()
    );
    if executable {
        anyhow::ensure!(
            metadata.mode() & 0o111 != 0,
            "trusted 1Password binary is not executable"
        );
    }
    Ok(())
}

/// A validated public trust bundle embedded into the authenticated bootstrap.
#[derive(Clone)]
struct ClientTrustMaterial {
    ca_pem_b64: Arc<str>,
    spki_pin: Arc<str>,
    server_name: Arc<str>,
}

#[derive(Clone)]
struct SecureEnrollmentState {
    gateway: Arc<GatewayState>,
    local_name: Arc<str>,
    trust: ClientTrustMaterial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenClaims {
    node_name: String,
    intended_ip: String,
    ssh_user: String,
    role: String,
    runtime: String,
    leader_name: String,
    leader_epoch: i64,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

fn error_response(status: StatusCode, message: &'static str) -> Response {
    (status, Json(ErrorBody { error: message })).into_response()
}

/// Node names are identities, not display strings. Issuers and consumers use
/// this exact predicate and reject aliases/case folding/Unicode substitution.
pub(crate) fn canonical_node_name(input: &str) -> Option<&str> {
    if input.is_empty()
        || input.len() > 63
        || input.starts_with('-')
        || input.ends_with('-')
        || !input
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        None
    } else {
        Some(input)
    }
}

fn canonical_ssh_user(input: &str) -> Option<&str> {
    if input.is_empty()
        || input.len() > 64
        || !input
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || *byte == b'_')
        || !input.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        None
    } else {
        Some(input)
    }
}

pub(crate) fn normalize_peer_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ipv6) => ipv6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ipv6)),
        ipv4 => ipv4,
    }
}

fn parse_bound_ip(input: &str) -> Option<IpAddr> {
    let ip = normalize_peer_ip(input.parse().ok()?);
    (!ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast()).then_some(ip)
}

fn canonical_claim(input: &str, max_len: usize) -> bool {
    !input.is_empty()
        && input.len() <= max_len
        && input.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

/// Decode the versioned bearer and return only its SHA-256 digest. No caller
/// compares plaintext tokens in application code; Postgres compares the fixed
/// 32-byte digest while atomically consuming the row.
pub(crate) fn hash_enrollment_token(raw: &str) -> Option<[u8; 32]> {
    let encoded = raw.strip_prefix(TOKEN_PREFIX)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    if decoded.len() != TOKEN_BYTES {
        return None;
    }
    Some(Sha256::digest(decoded).into())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, value) = value.split_once(' ')?;
    let value = value.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !value.is_empty()).then_some(value)
}

fn has_proxy_identity_headers(headers: &HeaderMap) -> bool {
    [
        "forwarded",
        "x-forwarded-for",
        "x-real-ip",
        "cf-connecting-ip",
        "true-client-ip",
    ]
    .iter()
    .any(|name| headers.contains_key(*name))
}

async fn current_leader_epoch(
    pool: &sqlx::PgPool,
    local_name: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT l.epoch, c.primary_ip FROM fleet_leader_state l \
         JOIN computers c ON c.id = l.computer_id AND c.name = l.member_name \
         JOIN fleet_workers w ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip \
         WHERE l.singleton_key = 'current' \
           AND l.member_name = $1 \
           AND heartbeat_at > clock_timestamp() - make_interval(secs => $2) \
           AND (relinquishing_until IS NULL OR relinquishing_until <= clock_timestamp())",
    )
    .bind(local_name)
    .bind(LEADER_FRESHNESS_SECS as i32)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(epoch, ip)| {
        let authority_ip = parse_bound_ip(&ip)?;
        (route_selected_local_ip(authority_ip).ok()? == authority_ip).then_some(epoch)
    }))
}

async fn lock_current_leader(
    tx: &mut Transaction<'_, Postgres>,
    local_name: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ff_db::SECURE_ENROLLMENT_XACT_LOCK_KEY)
        .execute(&mut **tx)
        .await?;
    sqlx::query("LOCK TABLE computers, fleet_workers IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut **tx)
        .await?;
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT l.epoch, c.primary_ip FROM fleet_leader_state l \
         JOIN computers c ON c.id = l.computer_id AND c.name = l.member_name \
         JOIN fleet_workers w ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip \
         WHERE l.singleton_key = 'current' \
           AND l.member_name = $1 \
           AND heartbeat_at > clock_timestamp() - make_interval(secs => $2) \
           AND (relinquishing_until IS NULL OR relinquishing_until <= clock_timestamp()) \
         FOR UPDATE OF l, c, w",
    )
    .bind(local_name)
    .bind(LEADER_FRESHNESS_SECS as i32)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(|(epoch, ip)| {
        let authority_ip = parse_bound_ip(&ip)?;
        (route_selected_local_ip(authority_ip).ok()? == authority_ip).then_some(epoch)
    }))
}

fn route_selected_local_ip(destination: IpAddr) -> std::io::Result<IpAddr> {
    let bind_addr = match destination {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind_addr)?;
    socket.connect(SocketAddr::new(destination, 9))?;
    Ok(socket.local_addr()?.ip())
}

async fn require_enrollment_schema(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    ff_db::validate_secure_enrollment_schema(pool)
        .await
        .map_err(|error| sqlx::Error::Protocol(format!("unsafe enrollment schema: {error}")))
}

async fn leader_guard(
    State(state): State<SecureEnrollmentState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(pool) = state
        .gateway
        .operational_store
        .as_ref()
        .and_then(|store| store.pg_pool())
    else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "enrollment store unavailable",
        );
    };

    match current_leader_epoch(pool, &state.local_name).await {
        Ok(Some(_)) => next.run(request).await,
        Ok(None) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "not the current fleet leader",
        ),
        Err(error) => {
            warn!(%error, "secure enrollment leader fence failed");
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "leader authority unavailable",
            )
        }
    }
}

fn secure_router(state: SecureEnrollmentState) -> Router {
    Router::new()
        .route("/health", get(secure_health))
        .route("/onboard/bootstrap.sh", get(secure_bootstrap))
        .route("/api/fleet/self-enroll", post(secure_self_enroll))
        .route(
            "/api/fleet/enrollment-progress",
            post(secure_enrollment_progress),
        )
        .layer(middleware::from_fn_with_state(state.clone(), leader_guard))
        .with_state(state)
}

async fn secure_health() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({"status":"ok","transport":"tls"})),
    )
}

async fn lookup_bootstrap_claims(
    pool: &sqlx::PgPool,
    token_hash: &[u8; 32],
    node_name: &str,
    peer_ip: IpAddr,
) -> Result<Option<TokenClaims>, sqlx::Error> {
    require_enrollment_schema(pool).await?;
    let row = sqlx::query(
        "SELECT t.node_name, host(t.intended_ip) AS intended_ip, t.ssh_user, \
                t.role, t.runtime, t.leader_name, t.leader_epoch \
         FROM fleet_enrollment_tokens t \
         JOIN fleet_leader_state l \
           ON l.member_name = t.leader_name AND l.epoch = t.leader_epoch \
         WHERE t.token_hash = $1 \
           AND t.purpose = 'node-enrollment' \
           AND t.consumed_at IS NULL \
           AND t.revoked_at IS NULL \
           AND t.expires_at > clock_timestamp() \
           AND t.node_name = $2 \
           AND t.intended_ip = $3::inet \
           AND l.heartbeat_at > clock_timestamp() - make_interval(secs => $4) \
           AND (l.relinquishing_until IS NULL OR l.relinquishing_until <= clock_timestamp())",
    )
    .bind(token_hash.as_slice())
    .bind(node_name)
    .bind(peer_ip.to_string())
    .bind(LEADER_FRESHNESS_SECS as i32)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(TokenClaims {
            node_name: row.try_get("node_name")?,
            intended_ip: row.try_get("intended_ip")?,
            ssh_user: row.try_get("ssh_user")?,
            role: row.try_get("role")?,
            runtime: row.try_get("runtime")?,
            leader_name: row.try_get("leader_name")?,
            leader_epoch: row.try_get("leader_epoch")?,
        })
    })
    .transpose()
}

async fn secure_bootstrap(
    State(state): State<SecureEnrollmentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<BootstrapQuery>,
) -> Response {
    if has_proxy_identity_headers(&headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "proxy identity headers are forbidden on enrollment",
        );
    }
    let Some(node_name) = query.name.as_deref().and_then(canonical_node_name) else {
        return error_response(StatusCode::BAD_REQUEST, "canonical node name is required");
    };
    let peer_ip = normalize_peer_ip(peer.ip());
    if query
        .ip
        .as_deref()
        .and_then(parse_bound_ip)
        .is_none_or(|requested| requested != peer_ip)
    {
        return error_response(StatusCode::FORBIDDEN, "request IP does not match TLS peer");
    }
    let Some(token_hash) = bearer_token(&headers).and_then(hash_enrollment_token) else {
        return error_response(StatusCode::UNAUTHORIZED, "invalid enrollment credential");
    };
    let Some(pool) = state
        .gateway
        .operational_store
        .as_ref()
        .and_then(|store| store.pg_pool())
    else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "enrollment store unavailable",
        );
    };

    let claims = match lookup_bootstrap_claims(pool, &token_hash, node_name, peer_ip).await {
        Ok(Some(claims)) => claims,
        Ok(None) => {
            return error_response(StatusCode::UNAUTHORIZED, "invalid enrollment credential");
        }
        Err(error) => {
            warn!(%error, "secure bootstrap token lookup failed");
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "enrollment store unavailable",
            );
        }
    };

    let leader_host: Option<String> = sqlx::query_scalar(
        "SELECT COALESCE(NULLIF(c.primary_ip, ''), NULLIF(w.ip, '')) \
         FROM fleet_leader_state l \
         LEFT JOIN computers c ON c.name = l.member_name \
         LEFT JOIN fleet_workers w ON w.name = l.member_name \
         WHERE l.member_name = $1 AND l.epoch = $2",
    )
    .bind(&claims.leader_name)
    .bind(claims.leader_epoch)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some(leader_host) =
        leader_host.and_then(|value| parse_bound_ip(&value).map(|ip| ip.to_string()))
    else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "leader enrollment address unavailable",
        );
    };

    render_secure_bootstrap_script(
        &state.gateway,
        &SecureBootstrapRender {
            leader_host: &leader_host,
            tls_server_name: &state.trust.server_name,
            tls_ca_pem_b64: &state.trust.ca_pem_b64,
            tls_spki_pin: &state.trust.spki_pin,
            name: &claims.node_name,
            ip: &claims.intended_ip,
            ssh_user: &claims.ssh_user,
            role: &claims.role,
            runtime: &claims.runtime,
        },
    )
    .await
}

fn validate_payload(payload: &SelfEnrollPayload, peer_ip: IpAddr) -> Result<(), &'static str> {
    if !payload.token.is_empty() {
        return Err("body enrollment tokens are forbidden");
    }
    canonical_node_name(&payload.name).ok_or("node name is not canonical")?;
    canonical_ssh_user(&payload.ssh_user).ok_or("ssh user is not canonical")?;
    let role = payload.role.as_deref().unwrap_or("builder");
    if !matches!(role, "builder" | "gateway" | "testbed") {
        return Err("role is not an enrollment role");
    }
    if !canonical_claim(&payload.runtime, 32) {
        return Err("runtime is not canonical");
    }
    let payload_ip = parse_bound_ip(&payload.ip).ok_or("invalid node IP")?;
    if payload_ip != peer_ip {
        return Err("payload IP does not match TLS peer");
    }
    if !(1..=4096).contains(&payload.cpu_cores) || !(1..=1_048_576).contains(&payload.ram_gb) {
        return Err("hardware values are outside accepted bounds");
    }
    if payload.ssh_identity.user_public_key.len() > 16 * 1024
        || payload.ssh_identity.host_public_keys.len() > 16
        || payload
            .ssh_identity
            .host_public_keys
            .iter()
            .any(|key| key.len() > 16 * 1024)
    {
        return Err("SSH identity payload is too large");
    }
    Ok(())
}

/// Token consumption, leader fencing, canonical node creation, SSH identity,
/// and mesh-queue publication share one transaction. Locking the singleton
/// leader row prevents a demotion commit from interleaving with enrollment.
async fn consume_and_create_node(
    pool: &sqlx::PgPool,
    local_name: &str,
    token_hash: &[u8; 32],
    peer_ip: IpAddr,
    payload: &SelfEnrollPayload,
) -> Result<Option<String>, sqlx::Error> {
    require_enrollment_schema(pool).await?;
    let role = payload.role.as_deref().unwrap_or("builder");
    let mut tx = pool.begin().await?;
    let Some(leader_epoch) = lock_current_leader(&mut tx, local_name).await? else {
        tx.rollback().await?;
        return Ok(None);
    };
    let claimed_name: Option<String> = sqlx::query_scalar(
        "UPDATE fleet_enrollment_tokens t SET \
             consumed_at = clock_timestamp(), consumed_peer_ip = $3::inet \
         WHERE t.token_hash = $1 \
           AND t.purpose = 'node-enrollment' \
           AND t.consumed_at IS NULL \
           AND t.revoked_at IS NULL \
           AND t.expires_at > clock_timestamp() \
           AND t.node_name = $2 \
           AND t.intended_ip = $3::inet \
           AND t.ssh_user = $4 \
           AND t.role = $5 \
           AND t.runtime = $6 \
           AND t.leader_name = $7 \
           AND t.leader_epoch = $8 \
           AND NOT EXISTS ( \
               SELECT 1 FROM fleet_workers w \
               WHERE (lower(w.name) = lower(t.node_name) AND w.name <> t.node_name) \
                  OR (w.ip = host(t.intended_ip) AND w.name <> t.node_name) \
                  OR (w.name = t.node_name AND w.ip <> host(t.intended_ip)) \
           ) \
           AND NOT EXISTS ( \
               SELECT 1 FROM computers c \
               WHERE (lower(c.name) = lower(t.node_name) AND c.name <> t.node_name) \
                  OR (c.primary_ip = host(t.intended_ip) AND c.name <> t.node_name) \
                  OR (c.name = t.node_name \
                      AND COALESCE(c.primary_ip, '') NOT IN ('', host(t.intended_ip))) \
           ) \
         RETURNING t.node_name",
    )
    .bind(token_hash.as_slice())
    .bind(&payload.name)
    .bind(peer_ip.to_string())
    .bind(&payload.ssh_user)
    .bind(role)
    .bind(&payload.runtime)
    .bind(local_name)
    .bind(leader_epoch)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(name) = claimed_name else {
        tx.rollback().await?;
        return Ok(None);
    };

    let next_priority: i32 =
        sqlx::query_scalar("SELECT COALESCE(MAX(election_priority), 100) + 10 FROM fleet_workers")
            .fetch_one(&mut *tx)
            .await?;
    let has_nvidia = payload.has_nvidia.unwrap_or(false);
    let sub_agent_count = payload.sub_agent_count.unwrap_or_else(|| {
        compute_default_sub_agents(payload.cpu_cores, payload.ram_gb, has_nvidia)
    });

    sqlx::query(
        "INSERT INTO fleet_workers (name, ip, ssh_user, ram_gb, cpu_cores, os, role, \
                election_priority, hardware, alt_ips, capabilities, preferences, resources, status, \
                runtime, models_dir, disk_quota_pct, sub_agent_count, gh_account, tooling, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'[]'::jsonb,'{}'::jsonb,'{}'::jsonb,$10, \
                 'online',$11,'~/models',80,$12,$13,'{}'::jsonb,NOW()) \
         ON CONFLICT (name) DO UPDATE SET \
            ip=EXCLUDED.ip, ssh_user=EXCLUDED.ssh_user, ram_gb=EXCLUDED.ram_gb, \
            cpu_cores=EXCLUDED.cpu_cores, os=EXCLUDED.os, role=EXCLUDED.role, \
            hardware=EXCLUDED.hardware, resources=EXCLUDED.resources, status='online', \
            runtime=EXCLUDED.runtime, sub_agent_count=EXCLUDED.sub_agent_count, \
            gh_account=COALESCE(EXCLUDED.gh_account,fleet_workers.gh_account), updated_at=NOW()",
    )
    .bind(&name)
    .bind(&payload.ip)
    .bind(&payload.ssh_user)
    .bind(payload.ram_gb)
    .bind(payload.cpu_cores)
    .bind(&payload.os)
    .bind(role)
    .bind(next_priority)
    .bind(payload.os_id.as_deref().unwrap_or_default())
    .bind(json!({"has_nvidia": has_nvidia}))
    .bind(&payload.runtime)
    .bind(sub_agent_count)
    .bind(&payload.gh_account)
    .execute(&mut *tx)
    .await?;

    let os_family = derive_os_family(payload.os_id.as_deref(), payload.kernel.as_deref());
    sqlx::query(
        "INSERT INTO computers (name,primary_ip,os_family,os_distribution,os_version, \
             cpu_cores,total_ram_gb,has_gpu,gpu_kind,ssh_user,status,source_tree_path,metadata) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'online', \
             '~/.forgefleet/sub-agents/sub-agent-0/forge-fleet',$11) \
         ON CONFLICT (name) DO UPDATE SET \
             primary_ip=EXCLUDED.primary_ip, os_family=EXCLUDED.os_family, \
             os_distribution=COALESCE(computers.os_distribution,EXCLUDED.os_distribution), \
             os_version=COALESCE(computers.os_version,EXCLUDED.os_version), \
             cpu_cores=EXCLUDED.cpu_cores,total_ram_gb=EXCLUDED.total_ram_gb, \
             has_gpu=EXCLUDED.has_gpu,gpu_kind=COALESCE(computers.gpu_kind,EXCLUDED.gpu_kind), \
             ssh_user=EXCLUDED.ssh_user,status='online', \
             source_tree_path=COALESCE(computers.source_tree_path,EXCLUDED.source_tree_path)",
    )
    .bind(&name)
    .bind(&payload.ip)
    .bind(&os_family)
    .bind(payload.os_id.as_deref())
    .bind(&payload.os)
    .bind(payload.cpu_cores)
    .bind(payload.ram_gb)
    .bind(has_nvidia)
    .bind(has_nvidia.then_some("nvidia_cuda"))
    .bind(&payload.ssh_user)
    .bind(json!({
        "kernel": payload.kernel,
        "enrolled_via": "tls-one-time-token",
        "runtime": payload.runtime,
        "leader_epoch": leader_epoch,
    }))
    .execute(&mut *tx)
    .await?;

    let user_key = payload.ssh_identity.user_public_key.trim();
    if !user_key.is_empty() {
        let (key_type, fingerprint) = parse_pubkey_meta(user_key);
        insert_ssh_key(&mut tx, &name, "user", user_key, &key_type, &fingerprint).await?;
    }
    for host_key in &payload.ssh_identity.host_public_keys {
        let host_key = host_key.trim();
        if !host_key.is_empty() {
            let (key_type, fingerprint) = parse_pubkey_meta(host_key);
            insert_ssh_key(&mut tx, &name, "host", host_key, &key_type, &fingerprint).await?;
        }
    }

    let mesh_payload = json!({
        "new_node": name,
        "new_node_ip": payload.ip,
        "new_node_ssh_user": payload.ssh_user,
        "user_public_key": user_key,
        "host_public_keys": payload.ssh_identity.host_public_keys,
    });
    sqlx::query(
        "INSERT INTO fleet_tasks \
            (task_type,summary,payload,priority,requires_capability,status,created_at,task_class) \
         VALUES ('internal',$1, \
             jsonb_build_object('deferred_payload',$2,'created_by','tls-self-enroll', \
                 'kind','internal','trigger_type','now','trigger_spec','{}'::jsonb, \
                 'preferred_node',$3,'required_caps','[]'::jsonb,'attempts',0,'max_attempts',5), \
             50,'[]'::jsonb,'pending',NOW(),'deferred')",
    )
    .bind(format!("Mesh propagate SSH for {name}"))
    .bind(mesh_payload)
    .bind(local_name)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some(name))
}

async fn insert_ssh_key(
    tx: &mut Transaction<'_, Postgres>,
    node_name: &str,
    purpose: &str,
    public_key: &str,
    key_type: &str,
    fingerprint: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO fleet_workers_ssh_keys \
             (worker_name,key_purpose,public_key,key_type,fingerprint) \
         VALUES ($1,$2,$3,$4,$5) \
         ON CONFLICT (worker_name,fingerprint) DO UPDATE SET \
             public_key=EXCLUDED.public_key,key_type=EXCLUDED.key_type, \
             key_purpose=EXCLUDED.key_purpose",
    )
    .bind(node_name)
    .bind(purpose)
    .bind(public_key)
    .bind(key_type)
    .bind(fingerprint)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn secure_self_enroll(
    State(state): State<SecureEnrollmentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SelfEnrollPayload>,
) -> Response {
    if has_proxy_identity_headers(&headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "proxy identity headers are forbidden on enrollment",
        );
    }
    let peer_ip = normalize_peer_ip(peer.ip());
    if let Err(message) = validate_payload(&payload, peer_ip) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    let Some(token_hash) = bearer_token(&headers).and_then(hash_enrollment_token) else {
        return error_response(StatusCode::UNAUTHORIZED, "invalid enrollment credential");
    };
    let Some(pool) = state
        .gateway
        .operational_store
        .as_ref()
        .and_then(|store| store.pg_pool())
    else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "enrollment store unavailable",
        );
    };

    let name = match consume_and_create_node(
        pool,
        &state.local_name,
        &token_hash,
        peer_ip,
        &payload,
    )
    .await
    {
        Ok(Some(name)) => name,
        Ok(None) => {
            return error_response(StatusCode::UNAUTHORIZED, "invalid enrollment credential");
        }
        Err(error) => {
            error!(operation = "secure enrollment transaction", %error, "enrollment failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "enrollment transaction failed",
            );
        }
    };

    let nodes = ff_db::pg_list_nodes(pool).await.unwrap_or_default();
    let mut peers = Vec::new();
    for peer in nodes.into_iter().filter(|peer| peer.name != name) {
        let user_public_key = ff_db::pg_list_node_ssh_keys(pool, &peer.name, Some("user"))
            .await
            .unwrap_or_default()
            .into_iter()
            .next()
            .map(|key| key.public_key);
        let host_public_keys = ff_db::pg_list_node_ssh_keys(pool, &peer.name, Some("host"))
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|key| key.public_key)
            .collect();
        peers.push(PeerSshIdentity {
            name: peer.name,
            ip: peer.ip,
            ssh_user: peer.ssh_user,
            user_public_key,
            host_public_keys,
        });
    }
    let _ = ff_agent::fleet_events::publish_node_online(&name).await;

    Json(SelfEnrollResponse {
        assigned_name: name,
        peer_ssh_identities: peers,
        postgres_url: None,
        redis_url: None,
    })
    .into_response()
}

async fn secure_enrollment_progress(
    State(state): State<SecureEnrollmentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<EnrollmentProgress>,
) -> Response {
    if has_proxy_identity_headers(&headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "proxy identity headers are forbidden",
        );
    }
    let Some(name) = canonical_node_name(&payload.name) else {
        return error_response(StatusCode::BAD_REQUEST, "node name is not canonical");
    };
    let Some(token_hash) = bearer_token(&headers).and_then(hash_enrollment_token) else {
        return error_response(StatusCode::UNAUTHORIZED, "invalid enrollment credential");
    };
    let Some(pool) = state
        .gateway
        .operational_store
        .as_ref()
        .and_then(|store| store.pg_pool())
    else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "enrollment store unavailable",
        );
    };
    if require_enrollment_schema(pool).await.is_err() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "enrollment authority schema unavailable",
        );
    }
    let peer_ip = normalize_peer_ip(peer.ip());
    let authorized: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM fleet_enrollment_tokens t \
          JOIN fleet_leader_state l \
            ON l.member_name=t.leader_name AND l.epoch=t.leader_epoch \
          WHERE t.token_hash=$1 AND t.node_name=$2 AND t.intended_ip=$3::inet \
            AND t.leader_name=$4 AND t.purpose='node-enrollment' \
            AND t.revoked_at IS NULL \
            AND l.heartbeat_at > clock_timestamp() - make_interval(secs => $5) \
            AND (l.relinquishing_until IS NULL OR l.relinquishing_until <= clock_timestamp()) \
            AND ((t.consumed_at IS NULL AND t.expires_at > clock_timestamp()) \
              OR t.consumed_at > clock_timestamp() - interval '2 minutes'))",
    )
    .bind(token_hash.as_slice())
    .bind(name)
    .bind(peer_ip.to_string())
    .bind(state.local_name.as_ref())
    .bind(LEADER_FRESHNESS_SECS as i32)
    .fetch_one(pool)
    .await
    .unwrap_or(false);
    if !authorized {
        return error_response(StatusCode::UNAUTHORIZED, "invalid enrollment credential");
    }

    let channel = format!("fleet:enrollment:{name}");
    let message = json!({
        "step": payload.step,
        "status": payload.status,
        "detail": payload.detail,
        "at": chrono::Utc::now().to_rfc3339(),
    })
    .to_string();
    let _ = publish_redis_at(
        &channel,
        &message,
        crate::onboard::redis_url_from_state(&state.gateway).await.as_deref(),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

struct LoadedTlsMaterial {
    config: axum_server::tls_rustls::RustlsConfig,
    trust: ClientTrustMaterial,
}

fn valid_op_reference(reference: &str) -> bool {
    reference.starts_with("op://")
        && reference.len() <= 512
        && !reference.chars().any(char::is_control)
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.len() <= 63
        })
}

fn valid_spki_pin(pin: &str) -> bool {
    let Some(encoded) = pin.strip_prefix("sha256//") else {
        return false;
    };
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .is_ok_and(|digest| digest.len() == 32)
}

async fn required_fleet_secret(pool: &sqlx::PgPool, key: &str) -> anyhow::Result<String> {
    ff_db::pg_get_secret(pool, key)
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("required enrollment authority key {key} is not configured"))
}

fn validate_trusted_op_candidate(candidate: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(candidate);
    anyhow::ensure!(path.is_absolute(), "1Password binary path is not absolute");
    let link_metadata = path
        .symlink_metadata()
        .map_err(|error| anyhow::anyhow!("inspect trusted 1Password binary: {error}"))?;
    anyhow::ensure!(
        !link_metadata.file_type().is_symlink(),
        "trusted 1Password binary must not be a symlink"
    );
    anyhow::ensure!(
        link_metadata.is_file(),
        "trusted 1Password path is not a file"
    );
    let canonical = path
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("resolve trusted 1Password binary: {error}"))?;
    anyhow::ensure!(
        canonical == path,
        "trusted 1Password binary resolves outside its approved path"
    );

    #[cfg(unix)]
    {
        let mut component = Some(path);
        while let Some(current) = component {
            let metadata = current.symlink_metadata().map_err(|error| {
                anyhow::anyhow!(
                    "inspect 1Password path component {}: {error}",
                    current.display()
                )
            })?;
            validate_trusted_op_path_component(current, &metadata, current == path)?;
            component = current.parent().filter(|parent| *parent != current);
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("trusted 1Password execution is unsupported on this platform");

    Ok(canonical)
}

fn trusted_op_binary() -> anyhow::Result<PathBuf> {
    for candidate in TRUSTED_OP_PATHS {
        if let Ok(path) = validate_trusted_op_candidate(candidate) {
            return Ok(path);
        }
    }
    anyhow::bail!(
        "1Password CLI is not a root-owned, non-symlink executable at an approved path ({})",
        TRUSTED_OP_PATHS.join(", ")
    )
}

async fn op_read_in_memory(
    service_token: &Zeroizing<String>,
    reference: &str,
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    anyhow::ensure!(valid_op_reference(reference), "invalid 1Password reference");
    let op_binary = trusted_op_binary()?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(op_binary)
            .arg("read")
            .arg(reference)
            .env("OP_SERVICE_ACCOUNT_TOKEN", service_token.as_str())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("1Password read timed out"))??;
    anyhow::ensure!(output.status.success(), "1Password read failed");
    anyhow::ensure!(
        !output.stdout.is_empty() && output.stdout.len() <= MAX_TLS_MATERIAL_BYTES,
        "1Password material has an invalid size"
    );
    Ok(Zeroizing::new(output.stdout))
}

fn rustls_config_from_pem(
    cert_pem: &[u8],
    private_key_pem: &[u8],
) -> anyhow::Result<rustls::ServerConfig> {
    let certificates =
        rustls_pemfile::certs(&mut Cursor::new(cert_pem)).collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(!certificates.is_empty(), "TLS certificate chain is empty");
    let private_key = rustls_pemfile::private_key(&mut Cursor::new(private_key_pem))?
        .ok_or_else(|| anyhow::anyhow!("TLS private key is missing"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)?)
}

fn validate_server_trust_material(
    certificates: &[CertificateDer<'static>],
    ca_pem: &[u8],
    server_name: &str,
    spki_pin: &str,
) -> anyhow::Result<()> {
    let leaf = certificates
        .first()
        .ok_or_else(|| anyhow::anyhow!("TLS certificate chain is empty"))?;
    let ca_certificates =
        rustls_pemfile::certs(&mut Cursor::new(ca_pem)).collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(!ca_certificates.is_empty(), "TLS CA bundle is empty");

    let mut roots = RootCertStore::empty();
    for certificate in ca_certificates {
        roots
            .add(certificate)
            .map_err(|error| anyhow::anyhow!("invalid TLS CA certificate: {error}"))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|error| anyhow::anyhow!("invalid TLS CA trust store: {error}"))?;
    let server_name = ServerName::try_from(server_name.to_owned())
        .map_err(|_| anyhow::anyhow!("invalid TLS server name"))?;
    verifier
        .verify_server_cert(leaf, &certificates[1..], &server_name, &[], UnixTime::now())
        .map_err(|error| {
            anyhow::anyhow!("TLS certificate is not valid for configured CA/name: {error}")
        })?;

    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|error| anyhow::anyhow!("parse TLS leaf certificate: {error}"))?;
    let actual_spki: [u8; 32] = Sha256::digest(parsed.tbs_certificate.subject_pki.raw).into();
    let expected_spki = spki_pin
        .strip_prefix("sha256//")
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .ok_or_else(|| anyhow::anyhow!("invalid TLS SPKI pin"))?;
    anyhow::ensure!(
        bool::from(actual_spki.ct_eq(expected_spki.as_slice())),
        "TLS leaf certificate SPKI does not match configured pin"
    );
    Ok(())
}

async fn load_tls_material(pool: &sqlx::PgPool) -> anyhow::Result<LoadedTlsMaterial> {
    let service_token =
        Zeroizing::new(required_fleet_secret(pool, OP_SERVICE_ACCOUNT_TOKEN_KEY).await?);
    let cert_ref = required_fleet_secret(pool, TLS_CERT_REF_KEY).await?;
    let key_ref = required_fleet_secret(pool, TLS_PRIVATE_KEY_REF_KEY).await?;
    let ca_ref = required_fleet_secret(pool, TLS_CA_REF_KEY).await?;
    let pin_ref = required_fleet_secret(pool, TLS_SPKI_PIN_REF_KEY).await?;
    let server_name = required_fleet_secret(pool, TLS_SERVER_NAME_KEY).await?;
    anyhow::ensure!(
        valid_server_name(server_name.trim()),
        "invalid TLS server name"
    );

    let cert_pem = op_read_in_memory(&service_token, cert_ref.trim()).await?;
    let key_pem = op_read_in_memory(&service_token, key_ref.trim()).await?;
    let ca_pem = op_read_in_memory(&service_token, ca_ref.trim()).await?;
    let pin = op_read_in_memory(&service_token, pin_ref.trim()).await?;
    let pin = std::str::from_utf8(&pin)?.trim();
    anyhow::ensure!(valid_spki_pin(pin), "invalid TLS SPKI pin");

    let certificates =
        rustls_pemfile::certs(&mut Cursor::new(&*cert_pem)).collect::<Result<Vec<_>, _>>()?;
    validate_server_trust_material(&certificates, &ca_pem, server_name.trim(), pin)?;
    let config = rustls_config_from_pem(&cert_pem, &key_pem)?;
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(config));
    Ok(LoadedTlsMaterial {
        config: tls_config,
        trust: ClientTrustMaterial {
            ca_pem_b64: Arc::from(base64::engine::general_purpose::STANDARD.encode(&*ca_pem)),
            spki_pin: Arc::from(pin),
            server_name: Arc::from(server_name.trim()),
        },
    })
}

struct ActiveTlsServer {
    epoch: i64,
    handle: axum_server::Handle,
    task: tokio::task::JoinHandle<()>,
}

async fn stop_active_server(active: ActiveTlsServer) {
    active.handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(3), active.task).await;
}

/// Poll the authoritative leader row and own the TLS listener only for the
/// local, fresh epoch. Secret material is loaded on promotion and discarded on
/// demotion; followers never read the TLS private key from 1Password.
pub(crate) async fn run_supervisor(
    gateway: Arc<GatewayState>,
    local_name: String,
    cancel: CancellationToken,
) {
    let Some(pool) = gateway
        .operational_store
        .as_ref()
        .and_then(|store| store.pg_pool())
        .cloned()
    else {
        warn!("secure enrollment disabled: Postgres store unavailable");
        return;
    };
    let Some(local_name) = canonical_node_name(&local_name).map(str::to_owned) else {
        warn!("secure enrollment disabled: local node name is not canonical");
        return;
    };
    if let Err(error) = ff_db::validate_secure_enrollment_schema(&pool).await {
        warn!(%error, "secure enrollment disabled: authority schema is unavailable or unsafe");
        return;
    }

    let mut active: Option<ActiveTlsServer> = None;
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let epoch = match current_leader_epoch(&pool, &local_name).await {
                    Ok(epoch) => epoch,
                    Err(error) => {
                        warn!(%error, "secure enrollment supervisor cannot verify leader");
                        None
                    }
                };
                if active.as_ref().is_some_and(|server| server.task.is_finished()) {
                    let finished = active.take().expect("active server exists");
                    let _ = finished.task.await;
                }
                if active.as_ref().map(|server| server.epoch) != epoch {
                    if let Some(server) = active.take() {
                        info!(epoch = server.epoch, "stopping secure enrollment TLS listener");
                        stop_active_server(server).await;
                    }
                    if let Some(epoch) = epoch {
                        match load_tls_material(&pool).await {
                            Ok(material) => {
                                let state = SecureEnrollmentState {
                                    gateway: gateway.clone(),
                                    local_name: Arc::from(local_name.as_str()),
                                    trust: material.trust,
                                };
                                let app = secure_router(state);
                                let handle = axum_server::Handle::new();
                                let server_handle = handle.clone();
                                let address = SocketAddr::from(([0, 0, 0, 0], ENROLLMENT_TLS_PORT));
                                let task = tokio::spawn(async move {
                                    if let Err(error) = axum_server::bind_rustls(address, material.config)
                                        .handle(server_handle)
                                        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                                        .await
                                    {
                                        error!(%error, "secure enrollment TLS listener exited");
                                    }
                                });
                                info!(%address, epoch, "secure enrollment TLS listener starting");
                                active = Some(ActiveTlsServer { epoch, handle, task });
                            }
                            Err(error) => {
                                // Do not print secret values, command output, or references.
                                warn!(%error, "secure enrollment TLS material unavailable; listener remains closed");
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(server) = active.take() {
        stop_active_server(server).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    struct TestPki {
        cert_pem: String,
        key_pem: String,
        ca_pem: String,
        pin: String,
    }

    fn test_pki(server_name: &str) -> TestPki {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};

        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let server_key = KeyPair::generate().unwrap();
        let server_params = CertificateParams::new(vec![server_name.to_owned()]).unwrap();
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .unwrap();
        let (_, parsed) = x509_parser::parse_x509_certificate(server_cert.der()).unwrap();
        let pin = format!(
            "sha256//{}",
            base64::engine::general_purpose::STANDARD
                .encode(Sha256::digest(parsed.tbs_certificate.subject_pki.raw))
        );
        TestPki {
            cert_pem: server_cert.pem(),
            key_pem: server_key.serialize_pem(),
            ca_pem: ca_cert.pem(),
            pin,
        }
    }

    async fn isolated_postgres() -> Option<sqlx::PgPool> {
        let database_url = match std::env::var("FF_ENROLLMENT_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_) => {
                eprintln!(
                    "skipping real PostgreSQL enrollment test: FF_ENROLLMENT_TEST_DATABASE_URL is unset"
                );
                return None;
            }
        };
        let admin_options = PgConnectOptions::from_str(&database_url).unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(admin_options.clone())
            .await
            .unwrap();
        let database = format!("ff_enrollment_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        PgPoolOptions::new()
            .max_connections(8)
            .connect_with(admin_options.database(&database))
            .await
            .ok()
    }

    async fn install_test_schema(pool: &sqlx::PgPool) {
        sqlx::raw_sql(
            r#"
            CREATE EXTENSION IF NOT EXISTS pgcrypto;
            CREATE TABLE computers (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL UNIQUE,
                primary_ip TEXT,
                os_family TEXT,
                os_distribution TEXT,
                os_version TEXT,
                cpu_cores INT,
                total_ram_gb INT,
                has_gpu BOOL,
                gpu_kind TEXT,
                ssh_user TEXT,
                status TEXT,
                source_tree_path TEXT,
                metadata JSONB NOT NULL DEFAULT '{}'::jsonb
            );
            CREATE TABLE fleet_workers (
                name TEXT PRIMARY KEY,
                ip TEXT NOT NULL,
                ssh_user TEXT NOT NULL,
                ram_gb INT NOT NULL DEFAULT 1,
                cpu_cores INT NOT NULL DEFAULT 1,
                os TEXT NOT NULL DEFAULT 'test',
                role TEXT NOT NULL DEFAULT 'builder',
                election_priority INT NOT NULL DEFAULT 100,
                hardware TEXT NOT NULL DEFAULT '',
                alt_ips JSONB NOT NULL DEFAULT '[]'::jsonb,
                capabilities JSONB NOT NULL DEFAULT '{}'::jsonb,
                preferences JSONB NOT NULL DEFAULT '{}'::jsonb,
                resources JSONB NOT NULL DEFAULT '{}'::jsonb,
                status TEXT NOT NULL DEFAULT 'online',
                runtime TEXT NOT NULL DEFAULT 'auto',
                models_dir TEXT NOT NULL DEFAULT '~/models',
                disk_quota_pct INT NOT NULL DEFAULT 80,
                sub_agent_count INT NOT NULL DEFAULT 1,
                gh_account TEXT,
                tooling JSONB NOT NULL DEFAULT '{}'::jsonb,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            CREATE TABLE fleet_leader_state (
                singleton_key TEXT PRIMARY KEY DEFAULT 'current' CHECK (singleton_key='current'),
                computer_id UUID NOT NULL REFERENCES computers(id),
                member_name TEXT NOT NULL,
                epoch BIGINT NOT NULL,
                elected_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                reason TEXT,
                heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                relinquishing_until TIMESTAMPTZ
            );
            CREATE TABLE fleet_workers_ssh_keys (
                worker_name TEXT NOT NULL,
                key_purpose TEXT NOT NULL,
                public_key TEXT NOT NULL,
                key_type TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                UNIQUE(worker_name, fingerprint)
            );
            CREATE TABLE fleet_tasks (
                id BIGSERIAL PRIMARY KEY,
                task_type TEXT NOT NULL,
                summary TEXT NOT NULL,
                payload JSONB NOT NULL,
                priority INT NOT NULL,
                requires_capability JSONB NOT NULL,
                status TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                task_class TEXT NOT NULL
            );
            "#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::raw_sql(ff_db::schema::SCHEMA_V289_SECURE_ENROLLMENT_TOKENS)
            .execute(pool)
            .await
            .unwrap();
        sqlx::raw_sql(ff_db::schema::SCHEMA_V290_SECURE_ENROLLMENT_HARDENING)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn install_test_leader(pool: &sqlx::PgPool, leader_name: &str) {
        let leader_ip = route_selected_local_ip("192.0.2.1".parse().unwrap()).unwrap();
        let computer_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO computers (id,name,primary_ip,status,ssh_user) VALUES ($1,$2,$3,'online',$2)",
        )
        .bind(computer_id)
        .bind(leader_name)
        .bind(leader_ip.to_string())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO fleet_workers (name,ip,ssh_user) VALUES ($1,$2,$1)")
            .bind(leader_name)
            .bind(leader_ip.to_string())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO fleet_leader_state (computer_id,member_name,epoch) VALUES ($1,$2,1)",
        )
        .bind(computer_id)
        .bind(leader_name)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_test_token(
        pool: &sqlx::PgPool,
        byte: u8,
        name: &str,
        ip: &str,
        epoch: i64,
    ) -> [u8; 32] {
        let digest = hash_enrollment_token(&token(byte)).unwrap();
        sqlx::query(
            "INSERT INTO fleet_enrollment_tokens \
             (token_hash,node_name,intended_ip,ssh_user,role,runtime,leader_name,leader_epoch,expires_at) \
             VALUES ($1,$2,$3::inet,$2,'builder','auto','testleader',$4,clock_timestamp()+interval '10 minutes')",
        )
        .bind(digest.as_slice())
        .bind(name)
        .bind(ip)
        .bind(epoch)
        .execute(pool)
        .await
        .unwrap();
        digest
    }

    fn enrollment_payload(name: &str, ip: &str) -> SelfEnrollPayload {
        SelfEnrollPayload {
            token: String::new(),
            name: name.to_owned(),
            hostname: Some(name.to_owned()),
            ip: ip.to_owned(),
            os: "test-os".to_owned(),
            os_id: Some("ubuntu".to_owned()),
            kernel: Some("test-kernel".to_owned()),
            runtime: "auto".to_owned(),
            ram_gb: 16,
            cpu_cores: 4,
            role: Some("builder".to_owned()),
            ssh_user: name.to_owned(),
            sub_agent_count: Some(1),
            gh_account: None,
            has_nvidia: Some(false),
            ssh_identity: crate::onboard::SshIdentity {
                user_public_key: String::new(),
                host_public_keys: Vec::new(),
            },
        }
    }

    fn token(byte: u8) -> String {
        format!(
            "{TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([byte; TOKEN_BYTES])
        )
    }

    #[test]
    fn tokens_are_versioned_fixed_entropy_and_hash_only() {
        let first = token(7);
        let second = token(8);
        assert_eq!(first.len(), TOKEN_PREFIX.len() + 43);
        assert_ne!(
            hash_enrollment_token(&first),
            hash_enrollment_token(&second)
        );
        assert_eq!(hash_enrollment_token(&first), hash_enrollment_token(&first));
        assert!(hash_enrollment_token("shared-secret").is_none());
        assert!(hash_enrollment_token("ffe1_short").is_none());
        assert!(hash_enrollment_token(&format!("{first}x")).is_none());
    }

    #[test]
    fn canonical_names_reject_alias_shapes_case_and_unicode() {
        for accepted in ["sia", "new-node", "node7"] {
            assert_eq!(canonical_node_name(accepted), Some(accepted));
        }
        for rejected in [
            "",
            "Vinny",
            " vinny",
            "vinny.local",
            "vinny_2",
            "vínny",
            "-vinny",
            "vinny-",
        ] {
            assert_eq!(canonical_node_name(rejected), None, "accepted {rejected:?}");
        }
    }

    #[test]
    fn mapped_ipv4_is_normalized_and_proxy_headers_are_rejected() {
        let mapped: IpAddr = "::ffff:192.168.5.116".parse().unwrap();
        assert_eq!(
            normalize_peer_ip(mapped),
            "192.168.5.116".parse::<IpAddr>().unwrap()
        );
        let mut headers = HeaderMap::new();
        assert!(!has_proxy_identity_headers(&headers));
        headers.insert("x-forwarded-for", "192.168.5.116".parse().unwrap());
        assert!(has_proxy_identity_headers(&headers));
    }

    #[test]
    fn trust_material_validators_fail_closed() {
        let pin = format!(
            "sha256//{}",
            base64::engine::general_purpose::STANDARD.encode([5_u8; 32])
        );
        assert!(valid_spki_pin(&pin));
        assert!(!valid_spki_pin("sha256//short"));
        assert!(!valid_spki_pin("md5//anything"));
        assert!(valid_server_name("enroll.forgefleet.local"));
        assert!(!valid_server_name("*.forgefleet.local"));
        assert!(!valid_server_name("FORGEFLEET.local"));
        assert!(valid_op_reference("op://ForgeFleet/enrollment/certificate"));
        assert!(!valid_op_reference("/tmp/private-key.pem"));
        assert!(!valid_op_reference("op://vault/item/key\n--reveal"));
        assert!(
            TRUSTED_OP_PATHS
                .iter()
                .all(|path| std::path::Path::new(path).is_absolute())
        );
        assert!(!include_str!("secure_enrollment.rs").contains("Command::new(\"op\")"));
    }

    #[cfg(unix)]
    #[test]
    fn op_trust_rejects_symlink_canonical_owner_and_mode_violations() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = std::env::temp_dir().join(format!(
            "ff-op-trust-gateway-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&directory).unwrap();
        let attacker = directory.join("attacker-op");
        std::fs::write(&attacker, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&attacker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let approved = directory.join("op");
        symlink(&attacker, &approved).unwrap();
        let error = validate_trusted_op_candidate(approved.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("must not be a symlink"));
        std::fs::remove_file(&approved).unwrap();

        let nested = directory.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let noncanonical = nested.join("..").join("attacker-op");
        let error = validate_trusted_op_candidate(noncanonical.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("outside its approved path"));

        let error = validate_trusted_op_candidate(attacker.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("not root-owned"));
        std::fs::set_permissions(&attacker, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = validate_trusted_op_candidate(attacker.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("group/world writable"));

        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let metadata = directory.symlink_metadata().unwrap();
        let error = validate_trusted_op_path_component(&directory, &metadata, false).unwrap_err();
        assert!(error.to_string().contains("group/world writable"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn startup_self_check_rejects_ca_name_and_spki_mismatch() {
        let pki = test_pki("enroll.test");
        let certificates = rustls_pemfile::certs(&mut Cursor::new(pki.cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        validate_server_trust_material(
            &certificates,
            pki.ca_pem.as_bytes(),
            "enroll.test",
            &pki.pin,
        )
        .unwrap();

        let wrong_ca = test_pki("other.test");
        assert!(
            validate_server_trust_material(
                &certificates,
                wrong_ca.ca_pem.as_bytes(),
                "enroll.test",
                &pki.pin,
            )
            .is_err()
        );
        assert!(
            validate_server_trust_material(
                &certificates,
                pki.ca_pem.as_bytes(),
                "wrong.test",
                &pki.pin,
            )
            .is_err()
        );
        assert!(
            validate_server_trust_material(
                &certificates,
                pki.ca_pem.as_bytes(),
                "enroll.test",
                &wrong_ca.pin,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn tls13_listener_binds_and_restarts_on_51443() {
        let pki = test_pki("enroll.test");
        let mut roots = RootCertStore::empty();
        for certificate in rustls_pemfile::certs(&mut Cursor::new(pki.ca_pem.as_bytes())) {
            roots.add(certificate.unwrap()).unwrap();
        }
        let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let address = SocketAddr::from(([127, 0, 0, 1], ENROLLMENT_TLS_PORT));

        for _ in 0..2 {
            let config =
                rustls_config_from_pem(pki.cert_pem.as_bytes(), pki.key_pem.as_bytes()).unwrap();
            let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(config));
            let handle = axum_server::Handle::new();
            let server_handle = handle.clone();
            let app = Router::new().route("/health", get(secure_health));
            let task = tokio::spawn(async move {
                axum_server::bind_rustls(address, config)
                    .handle(server_handle)
                    .serve(app.into_make_service())
                    .await
            });

            let mut connected = false;
            for _ in 0..30 {
                if let Ok(stream) = tokio::net::TcpStream::connect(address).await {
                    let server_name = ServerName::try_from("enroll.test".to_owned()).unwrap();
                    if let Ok(mut tls) = connector.connect(server_name, stream).await {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        assert_eq!(
                            tls.get_ref().1.protocol_version(),
                            Some(rustls::ProtocolVersion::TLSv1_3)
                        );
                        tls.write_all(
                            b"GET /health HTTP/1.1\r\nHost: enroll.test\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                        let mut response = Vec::new();
                        tls.read_to_end(&mut response).await.unwrap();
                        if response.starts_with(b"HTTP/1.1 200") {
                            connected = true;
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(connected, "TLS 1.3 listener did not become reachable");
            handle.shutdown();
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .expect("listener shutdown timed out")
                .expect("listener task panicked")
                .expect("listener returned an error");
        }
    }

    #[tokio::test]
    async fn postgres_consumption_replay_epoch_demotion_and_rollback_fail_closed() {
        let Some(pool) = isolated_postgres().await else {
            return;
        };
        install_test_schema(&pool).await;
        install_test_leader(&pool, "testleader").await;
        ff_db::validate_secure_enrollment_schema(&pool)
            .await
            .unwrap();

        // Exact-shape validation detects drift and never repairs it as a request
        // side effect.
        sqlx::query("ALTER TABLE fleet_enrollment_tokens ADD COLUMN unsafe_extra TEXT")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            ff_db::validate_secure_enrollment_schema(&pool)
                .await
                .is_err()
        );
        sqlx::query("ALTER TABLE fleet_enrollment_tokens DROP COLUMN unsafe_extra")
            .execute(&pool)
            .await
            .unwrap();
        ff_db::validate_secure_enrollment_schema(&pool)
            .await
            .unwrap();

        // Reusing a reviewed object name with weaker semantics must not bypass
        // the exact-definition check.
        sqlx::query(
            "ALTER TABLE fleet_enrollment_tokens \
             DROP CONSTRAINT fleet_enrollment_tokens_canonical_name",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "ALTER TABLE fleet_enrollment_tokens \
             ADD CONSTRAINT fleet_enrollment_tokens_canonical_name CHECK (true)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            ff_db::validate_secure_enrollment_schema(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::raw_sql(ff_db::schema::SCHEMA_V290_SECURE_ENROLLMENT_HARDENING)
                .execute(&pool)
                .await
                .is_err(),
            "controlled migration must not accept a familiar constraint name with weaker semantics"
        );
        sqlx::query(
            "ALTER TABLE fleet_enrollment_tokens \
             DROP CONSTRAINT fleet_enrollment_tokens_canonical_name",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "ALTER TABLE fleet_enrollment_tokens \
             ADD CONSTRAINT fleet_enrollment_tokens_canonical_name \
             CHECK (node_name ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DROP INDEX idx_computers_enrollment_canonical_name")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX idx_computers_enrollment_canonical_name ON computers (name)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            ff_db::validate_secure_enrollment_schema(&pool)
                .await
                .is_err()
        );
        sqlx::query("DROP INDEX idx_computers_enrollment_canonical_name")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX idx_computers_enrollment_canonical_name \
             ON computers (lower(name))",
        )
        .execute(&pool)
        .await
        .unwrap();
        ff_db::validate_secure_enrollment_schema(&pool)
            .await
            .unwrap();

        let peer_ip: IpAddr = "192.0.2.150".parse().unwrap();
        let digest = insert_test_token(&pool, 11, "node-a", "192.0.2.150", 1).await;
        let payload_a = enrollment_payload("node-a", "192.0.2.150");
        let payload_b = enrollment_payload("node-a", "192.0.2.150");
        let (first, second) = tokio::join!(
            consume_and_create_node(&pool, "testleader", &digest, peer_ip, &payload_a),
            consume_and_create_node(&pool, "testleader", &digest, peer_ip, &payload_b),
        );
        let outcomes = [first.unwrap(), second.unwrap()];
        assert_eq!(outcomes.iter().filter(|result| result.is_some()).count(), 1);
        assert_eq!(
            consume_and_create_node(
                &pool,
                "testleader",
                &digest,
                peer_ip,
                &enrollment_payload("node-a", "192.0.2.150"),
            )
            .await
            .unwrap(),
            None,
            "consumed bearer replay must fail"
        );

        let stale = insert_test_token(&pool, 12, "node-b", "192.0.2.151", 1).await;
        sqlx::query("UPDATE fleet_leader_state SET epoch=2")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            consume_and_create_node(
                &pool,
                "testleader",
                &stale,
                "192.0.2.151".parse().unwrap(),
                &enrollment_payload("node-b", "192.0.2.151"),
            )
            .await
            .unwrap(),
            None,
            "credential from a prior leader epoch must fail"
        );

        sqlx::query(
            "UPDATE fleet_leader_state SET epoch=1, relinquishing_until=clock_timestamp()+interval '1 minute'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let demoted = insert_test_token(&pool, 13, "node-c", "192.0.2.152", 1).await;
        assert_eq!(
            consume_and_create_node(
                &pool,
                "testleader",
                &demoted,
                "192.0.2.152".parse().unwrap(),
                &enrollment_payload("node-c", "192.0.2.152"),
            )
            .await
            .unwrap(),
            None,
            "relinquishing leader must not consume"
        );

        sqlx::query("UPDATE fleet_leader_state SET relinquishing_until=NULL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "ALTER TABLE fleet_tasks ADD CONSTRAINT reject_mesh \
             CHECK (task_type <> 'internal') NOT VALID",
        )
        .execute(&pool)
        .await
        .unwrap();
        let rollback = insert_test_token(&pool, 14, "node-d", "192.0.2.153", 1).await;
        assert!(
            consume_and_create_node(
                &pool,
                "testleader",
                &rollback,
                "192.0.2.153".parse().unwrap(),
                &enrollment_payload("node-d", "192.0.2.153"),
            )
            .await
            .is_err()
        );
        let remains_unconsumed: bool = sqlx::query_scalar(
            "SELECT consumed_at IS NULL FROM fleet_enrollment_tokens WHERE token_hash=$1",
        )
        .bind(rollback.as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            remains_unconsumed,
            "failed node transaction must roll token consumption back"
        );
        let node_was_not_created: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS(SELECT 1 FROM fleet_workers WHERE name='node-d')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(node_was_not_created);

        sqlx::query("DROP TABLE fleet_enrollment_tokens")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            consume_and_create_node(
                &pool,
                "testleader",
                &rollback,
                "192.0.2.153".parse().unwrap(),
                &enrollment_payload("node-d", "192.0.2.153"),
            )
            .await
            .is_err(),
            "authority DB loss must fail closed"
        );
        pool.close().await;
    }

    #[test]
    fn consume_sql_is_one_atomic_epoch_and_identity_fence() {
        let source = include_str!("secure_enrollment.rs");
        let start = source.find("UPDATE fleet_enrollment_tokens t SET").unwrap();
        let tail = &source[start..];
        let end = tail.find("RETURNING t.node_name").unwrap();
        let sql = &tail[..end];
        for guard in [
            "consumed_at IS NULL",
            "revoked_at IS NULL",
            "expires_at > clock_timestamp()",
            "t.node_name = $2",
            "t.intended_ip = $3::inet",
            "t.leader_name = $7",
            "t.leader_epoch = $8",
            "NOT EXISTS",
        ] {
            assert!(sql.contains(guard), "missing atomic guard {guard}");
        }
        assert!(!sql.contains(';'), "consume must remain one SQL statement");
    }
}
