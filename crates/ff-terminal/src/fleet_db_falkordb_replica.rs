//! Explicit, fail-closed FalkorDB follower lifecycle.
//!
//! There is deliberately no promotion, failover, or read-routing command in
//! this module. Priya remains the sole authority. Provisioning is split into a
//! mutation-free, drift-bound plan and a target-owned deferred apply. An apply
//! registers topology only after exact replication, graph, write-rejection,
//! container, firewall, and restart proofs all pass.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::BTreeMap;
use std::process::Output;
use std::time::Duration;
use uuid::Uuid;

use crate::fleet_db_redis_replica::{
    docker_output, primary_identity, redis_config_value, redis_connection, redis_info,
};
use crate::{
    shell_escape_single, whoami_tag, FleetDbFalkordbReplicaCommand, CYAN, GREEN, RESET, YELLOW,
};

const PRIMARY_NAME: &str = "priya";
const PRIMARY_PORT: u16 = 63379;
const REPLICA_PORT: u16 = 63380;
const FALKORDB_IMAGE: &str =
    "falkordb/falkordb@sha256:9042fdc4e53f5390ca5a3993aa71506523970efb40ffb9a98e6a4b1a9a4f8862";
const FALKORDB_CONTAINER: &str = "forgefleet-falkordb-replica";
const FALKORDB_VOLUME: &str = "forgefleet-falkordb-replica-data";
const FALKORDB_COMPOSE: &str = "deploy/docker-compose.falkordb-follower.yml";
const EXPECTED_REDIS_VERSION: &str = "8.6.3";
const EXPECTED_GRAPH_MODULE_VERSION: i64 = 42001;
const MAX_BACKUP_AGE_HOURS: i64 = 24;
const MAX_REPLICA_LAG_BYTES: i64 = 16 * 1024 * 1024;
const MIN_TARGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const SSH_TIMEOUT: Duration = Duration::from_secs(30);
const COMPOSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REPLICA_READY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const FULL_NODE_QUERY: &str = "MATCH (n) RETURN labels(n), properties(n)";
const FULL_RELATIONSHIP_QUERY: &str = "MATCH (a)-[r]->(b) RETURN labels(a), properties(a), type(r), properties(r), labels(b), properties(b)";

const UPSERT_PRIMARY_SQL: &str = "INSERT INTO database_replicas (computer_id,database_kind,role,status,lag_bytes,last_sync_at,bootstrapped_from_backup_id,notes) VALUES ($1,'falkordb','primary','running',0,NOW(),$2,$3) ON CONFLICT (computer_id,database_kind) DO UPDATE SET role='primary',status='running',lag_bytes=0,last_sync_at=NOW(),bootstrapped_from_backup_id=$2,notes=$3";
const UPSERT_REPLICA_SQL: &str = "INSERT INTO database_replicas (computer_id,database_kind,role,status,lag_bytes,last_sync_at,bootstrapped_from_backup_id,notes) VALUES ($1,'falkordb','replica','running',$2,NOW(),$3,$4) ON CONFLICT (computer_id,database_kind) DO UPDATE SET role='replica',status='running',lag_bytes=$2,last_sync_at=NOW(),bootstrapped_from_backup_id=$3,notes=$4";

#[derive(Debug, Clone)]
struct Computer {
    id: Uuid,
    name: String,
    ip: String,
    ssh_user: String,
    ssh_port: u16,
    status: String,
    fresh: bool,
    os_family: String,
    reservation_state: String,
    total_ram_gb: i32,
    total_disk_gb: i32,
    failure_domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GraphEvidence {
    nodes: u64,
    relationships: u64,
    node_full_sha256: String,
    relationship_full_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageEvidence {
    image_id: String,
    repo_digests: Vec<String>,
    configured_user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BackupEvidence {
    backup_id: Uuid,
    file_name: String,
    checksum_sha256: String,
    size_bytes: i64,
    drill_id: Uuid,
    restore_image_id: String,
    created_at: DateTime<Utc>,
    verified_restorable_at: DateTime<Utc>,
    distributed_to: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
enum TargetState {
    Absent,
    ExactHealthy { image_id: String },
}

#[derive(Debug, Clone, Serialize)]
struct PlanMaterial {
    version: &'static str,
    target_id: Uuid,
    target_name: String,
    target_ip: String,
    target_ssh_user: String,
    target_ssh_port: u16,
    target_failure_domain: Option<String>,
    primary_id: Uuid,
    primary_name: String,
    primary_ip: String,
    primary_replid: String,
    primary_version: String,
    graph_module_version: i64,
    primary_dbsize: u64,
    graphs: BTreeMap<String, GraphEvidence>,
    backup: BackupEvidence,
    target_state: TargetState,
    image: &'static str,
    primary_image_id: String,
    primary_image_repo_digests: Vec<String>,
    target_image_id: String,
    target_image_repo_digests: Vec<String>,
    image_user: String,
    primary_port: u16,
    replica_port: u16,
    firewall_policy: &'static str,
    automatic_failover: bool,
    read_routing: bool,
}

#[derive(Debug, Clone)]
struct Plan {
    material: PlanMaterial,
}

impl Plan {
    fn id(&self) -> String {
        let canonical = serde_json::to_vec(&self.material)
            .expect("serializing a bounded FalkorDB plan cannot fail");
        sha256(&canonical)
    }

    fn target(&self) -> Computer {
        Computer {
            id: self.material.target_id,
            name: self.material.target_name.clone(),
            ip: self.material.target_ip.clone(),
            ssh_user: self.material.target_ssh_user.clone(),
            ssh_port: self.material.target_ssh_port,
            status: "online".into(),
            fresh: true,
            os_family: "linux".into(),
            reservation_state: "available".into(),
            total_ram_gb: 0,
            total_disk_gb: 0,
            failure_domain: self.material.target_failure_domain.clone(),
        }
    }
}

fn validate_plan_id(plan: &Plan, supplied: &str) -> Result<()> {
    if plan.id() != supplied {
        bail!("FalkorDB replica plan-id is stale or mismatched; run plan again");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetAttestation {
    docker_version: String,
    image_id: String,
    image_repo_digests: Vec<String>,
    image_user: String,
    container: Option<String>,
    volume_present: bool,
    ram_bytes: u64,
    disk_free_bytes: u64,
    replica_port_listeners: u64,
    replica_port_non_loopback: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FirewallEvidence {
    target_id: Uuid,
    target_ip: String,
    primary_id: Uuid,
    primary_ip: String,
    primary_port: u16,
    unit_enabled: bool,
    unit_active: bool,
    unit_result_success: bool,
    docker_lifecycle_bound: bool,
    ipv4_target_allow: bool,
    ipv4_default_deny: bool,
    ipv6_default_deny: bool,
    ipv6_forward_default_deny: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetSourceRouteEvidence {
    target_id: Uuid,
    target_ip: String,
    source_ip: String,
    primary_id: Uuid,
    primary_ip: String,
    primary_port: u16,
    tcp_reachable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectPrimaryAccess {
    url: String,
    target_id: Uuid,
    target_ip: String,
    primary_id: Uuid,
    primary_ip: String,
    primary_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimaryProofTransport {
    AuthorizedTargetDirect,
    StrictSsh,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FirewallStatus {
    ok: bool,
    interface: String,
    source_ipv4: String,
    destination_ipv4: String,
    port: u16,
    allow_v4: bool,
    deny_v4: bool,
    deny_v6: bool,
    allow_v4_position: u64,
    deny_v4_position: u64,
    deny_v6_position: u64,
    deny_v6_forward_position: u64,
    persistence: FirewallPersistence,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FirewallPersistence {
    unit: String,
    unit_active: bool,
    unit_enabled: bool,
    environment_file: String,
    environment_file_present: bool,
    helper: String,
    helper_present: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreReceipt {
    proof: String,
    input_checksum_sha256: String,
    image_reference: String,
    image_id: String,
    network: String,
    query_mode: String,
    expected_min_keys: u64,
    observed_keys: u64,
    expected_min_graph_nodes: u64,
    observed_graphs: u64,
    observed_graph_nodes: u64,
}

#[derive(Debug, Clone)]
struct PurgeEvidence {
    target: Computer,
    primary: Computer,
    primary_image: ImageEvidence,
    backup: BackupEvidence,
}

struct PrimaryProbe {
    url: String,
    tunnel: Option<tokio::process::Child>,
}

impl PrimaryProbe {
    async fn close(mut self) {
        if let Some(child) = &mut self.tunnel {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

struct AuthorizedPrimaryProbe {
    probe: PrimaryProbe,
    transport: PrimaryProofTransport,
    target_id: Uuid,
    target_ip: String,
    primary_id: Uuid,
    primary_ip: String,
    primary_port: u16,
}

impl AuthorizedPrimaryProbe {
    fn url(&self) -> &str {
        &self.probe.url
    }

    fn validate_for_plan(&self, plan: &Plan) -> Result<()> {
        if self.target_id != plan.material.target_id
            || self.target_ip != plan.material.target_ip
            || self.primary_id != plan.material.primary_id
            || self.primary_ip != plan.material.primary_ip
            || self.primary_port != plan.material.primary_port
        {
            bail!("authorized Priya proof transport is not bound to this FalkorDB plan");
        }
        match self.transport {
            PrimaryProofTransport::AuthorizedTargetDirect => {
                if self.probe.tunnel.is_some()
                    || self.probe.url
                        != format!("redis://{}:{}", self.primary_ip, self.primary_port)
                {
                    bail!("authorized target-direct Priya proof transport is malformed");
                }
            }
            PrimaryProofTransport::StrictSsh => {
                if self.probe.tunnel.is_none() || !self.probe.url.starts_with("redis://127.0.0.1:")
                {
                    bail!("strict SSH Priya proof transport is malformed");
                }
            }
        }
        Ok(())
    }

    fn require_target_direct(&self) -> Result<()> {
        if self.transport != PrimaryProofTransport::AuthorizedTargetDirect {
            bail!("target-owned FalkorDB apply requires authorized target-direct Priya proof");
        }
        Ok(())
    }

    async fn close(self) {
        self.probe.close().await;
    }
}

impl PurgeEvidence {
    fn proof(&self) -> String {
        let canonical = format!(
            "falkordb-purge-v2\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            self.target.id,
            self.target.name,
            self.primary.id,
            FALKORDB_VOLUME,
            self.backup.backup_id,
            self.backup.checksum_sha256,
            self.primary_image.image_id,
        );
        sha256(canonical.as_bytes())
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn reject_vinny(name: &str) -> Result<()> {
    if name.eq_ignore_ascii_case("vinny") || name.eq_ignore_ascii_case("taylor") {
        bail!("Vinny (formerly Taylor) is categorically excluded from replica lifecycle work");
    }
    Ok(())
}

fn normalized_url(raw: &str, expected_ip: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(raw).context("parse FalkorDB authority URL")?;
    if parsed.scheme() != "redis"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.host_str() != Some(expected_ip)
        || parsed.port().unwrap_or(6379) != PRIMARY_PORT
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("FalkorDB authority must be credential-free redis://{expected_ip}:{PRIMARY_PORT}");
    }
    Ok(format!("redis://{expected_ip}:{PRIMARY_PORT}"))
}

fn replica_url() -> String {
    format!("redis://127.0.0.1:{REPLICA_PORT}")
}

fn expected_redis_args(primary_ip: &str) -> String {
    format!(
        "--port {REPLICA_PORT} --bind 127.0.0.1 --replicaof {primary_ip} {PRIMARY_PORT} --replica-read-only yes --replica-serve-stale-data no --appendonly yes --appendfsync everysec --save 60 1 --protected-mode yes"
    )
}

async fn resolve_computer(pool: &sqlx::PgPool, name: &str) -> Result<Computer> {
    let rows = sqlx::query(
        "SELECT id,name,primary_ip,ssh_user,ssh_port,status,os_family,
                reservation_state,total_ram_gb,total_disk_gb,
                metadata->>'failure_domain' AS failure_domain,
                last_seen_at >= NOW() - interval '5 minutes' AS fresh
           FROM computers WHERE lower(name)=lower($1) ORDER BY id",
    )
    .bind(name)
    .fetch_all(pool)
    .await?;
    if rows.len() != 1 {
        bail!(
            "computer identity '{name}' resolved to {} rows; require exactly one canonical enrollment",
            rows.len()
        );
    }
    let row = &rows[0];
    let ssh_port_i32 = row.try_get::<Option<i32>, _>("ssh_port")?.unwrap_or(22);
    let ssh_port = u16::try_from(ssh_port_i32).context("computer SSH port is out of range")?;
    Ok(Computer {
        id: row.get("id"),
        name: row.get("name"),
        ip: row
            .try_get::<Option<String>, _>("primary_ip")?
            .filter(|value| !value.trim().is_empty())
            .context("computer has no canonical LAN IP")?,
        ssh_user: row
            .try_get::<Option<String>, _>("ssh_user")?
            .filter(|value| !value.trim().is_empty())
            .context("computer has no canonical SSH user")?,
        ssh_port,
        status: row.get("status"),
        fresh: row.get("fresh"),
        os_family: row
            .try_get::<Option<String>, _>("os_family")?
            .unwrap_or_default(),
        reservation_state: row
            .try_get::<Option<String>, _>("reservation_state")?
            .unwrap_or_else(|| "available".into()),
        total_ram_gb: row.try_get::<Option<i32>, _>("total_ram_gb")?.unwrap_or(0),
        total_disk_gb: row.try_get::<Option<i32>, _>("total_disk_gb")?.unwrap_or(0),
        failure_domain: row
            .try_get::<Option<String>, _>("failure_domain")?
            .filter(|value| !value.trim().is_empty()),
    })
}

fn validate_target_identity(target: &Computer, primary: &Computer) -> Result<()> {
    reject_vinny(&target.name)?;
    if !primary.name.eq_ignore_ascii_case(PRIMARY_NAME) {
        bail!("FalkorDB authority is pinned to canonical Priya; no alternate primary is accepted");
    }
    if target.id == primary.id || target.ip == primary.ip {
        bail!("FalkorDB target and Priya authority must be distinct enrolled computers");
    }
    if target.status != "online" || !target.fresh {
        bail!("FalkorDB target must be online with a heartbeat from the last 5 minutes");
    }
    if primary.status != "online" || !primary.fresh {
        bail!("Priya must be online with a heartbeat from the last 5 minutes");
    }
    if !target.os_family.starts_with("linux") {
        bail!("FalkorDB replica targets must run Linux");
    }
    if target.reservation_state != "available" {
        bail!("FalkorDB target is reserved; release it before infrastructure planning");
    }
    if target.total_ram_gb < 2 || target.total_disk_gb < 2 {
        bail!("FalkorDB target inventory does not attest at least 2 GiB RAM and disk");
    }
    if let (Some(target_domain), Some(primary_domain)) =
        (&target.failure_domain, &primary.failure_domain)
    {
        if target_domain.eq_ignore_ascii_case(primary_domain) {
            bail!("FalkorDB target shares Priya's declared failure domain");
        }
    }
    Ok(())
}

async fn run_on_node(node: &Computer, script: &str, timeout: Duration) -> Result<Output> {
    let local_name = ff_agent::fleet_info::resolve_this_worker_name().await;
    let mut command = if local_name.eq_ignore_ascii_case(&node.name) {
        let mut command = tokio::process::Command::new("bash");
        command.args(["-lc", script]);
        command
    } else {
        let mut command = tokio::process::Command::new("ssh");
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "-p",
            &node.ssh_port.to_string(),
            &format!("{}@{}", node.ssh_user, node.ip),
            script,
        ]);
        command
    };
    tokio::time::timeout(timeout, command.kill_on_drop(true).output())
        .await
        .with_context(|| format!("command on {} timed out", node.name))?
        .with_context(|| format!("run bounded command on {}", node.name))
}

fn primary_proof_transport(local_name: &str, target: &Computer) -> PrimaryProofTransport {
    if local_name.eq_ignore_ascii_case(&target.name) {
        PrimaryProofTransport::AuthorizedTargetDirect
    } else {
        PrimaryProofTransport::StrictSsh
    }
}

fn validate_primary_access_evidence(
    target: &Computer,
    primary: &Computer,
    firewall: &FirewallEvidence,
    route: &TargetSourceRouteEvidence,
) -> Result<()> {
    if target.id == primary.id || target.ip == primary.ip {
        bail!("FalkorDB primary-proof target and authority must be distinct");
    }
    if firewall.target_id != target.id
        || firewall.target_ip != target.ip
        || firewall.primary_id != primary.id
        || firewall.primary_ip != primary.ip
        || firewall.primary_port != PRIMARY_PORT
        || !firewall.unit_enabled
        || !firewall.unit_active
        || !firewall.unit_result_success
        || !firewall.docker_lifecycle_bound
        || !firewall.ipv4_target_allow
        || !firewall.ipv4_default_deny
        || !firewall.ipv6_default_deny
        || !firewall.ipv6_forward_default_deny
    {
        bail!("Priya firewall evidence is not bound to the exact FalkorDB target and authority");
    }
    if route.target_id != target.id
        || route.target_ip != target.ip
        || route.source_ip != target.ip
        || route.primary_id != primary.id
        || route.primary_ip != primary.ip
        || route.primary_port != PRIMARY_PORT
        || !route.tcp_reachable
    {
        bail!(
            "target source-route evidence is not bound to the exact FalkorDB target and authority"
        );
    }
    Ok(())
}

fn authorize_target_direct_primary(
    local_name: &str,
    target: &Computer,
    primary: &Computer,
    canonical_authority: &str,
    firewall: &FirewallEvidence,
    route: &TargetSourceRouteEvidence,
) -> Result<DirectPrimaryAccess> {
    reject_vinny(&target.name)?;
    if !primary.name.eq_ignore_ascii_case(PRIMARY_NAME) {
        bail!("target-direct FalkorDB proof authority must be canonical Priya");
    }
    if !local_name.eq_ignore_ascii_case(&target.name) {
        bail!("target-direct Priya proof is authorized only on the exact target worker");
    }
    validate_primary_access_evidence(target, primary, firewall, route)?;
    let url = normalized_url(canonical_authority, &primary.ip)?;
    Ok(DirectPrimaryAccess {
        url,
        target_id: target.id,
        target_ip: target.ip.clone(),
        primary_id: primary.id,
        primary_ip: primary.ip.clone(),
        primary_port: PRIMARY_PORT,
    })
}

async fn acquire_authorized_primary_probe(
    canonical_authority: &str,
    primary: &Computer,
    target: &Computer,
) -> Result<AuthorizedPrimaryProbe> {
    validate_target_identity(target, primary)?;
    let firewall = attest_firewall(primary, target).await?;
    let route = attest_target_source_route(target, primary).await?;
    validate_primary_access_evidence(target, primary, &firewall, &route)?;
    let local_name = ff_agent::fleet_info::resolve_this_worker_name().await;
    let transport = primary_proof_transport(&local_name, target);
    let probe = match transport {
        PrimaryProofTransport::AuthorizedTargetDirect => {
            let access = authorize_target_direct_primary(
                &local_name,
                target,
                primary,
                canonical_authority,
                &firewall,
                &route,
            )?;
            if access.target_id != target.id
                || access.target_ip != target.ip
                || access.primary_id != primary.id
                || access.primary_ip != primary.ip
                || access.primary_port != PRIMARY_PORT
            {
                bail!("target-direct Priya proof authorization changed during acquisition");
            }
            PrimaryProbe {
                url: access.url,
                tunnel: None,
            }
        }
        PrimaryProofTransport::StrictSsh => {
            normalized_url(canonical_authority, &primary.ip)?;
            strict_ssh_loopback_probe(primary, PRIMARY_PORT, "Priya").await?
        }
    };
    Ok(AuthorizedPrimaryProbe {
        probe,
        transport,
        target_id: target.id,
        target_ip: target.ip.clone(),
        primary_id: primary.id,
        primary_ip: primary.ip.clone(),
        primary_port: PRIMARY_PORT,
    })
}

async fn node_loopback_probe(
    node: &Computer,
    remote_port: u16,
    label: &str,
) -> Result<PrimaryProbe> {
    let local_name = ff_agent::fleet_info::resolve_this_worker_name().await;
    if local_name.eq_ignore_ascii_case(&node.name) {
        return Ok(PrimaryProbe {
            url: format!("redis://127.0.0.1:{remote_port}"),
            tunnel: None,
        });
    }
    strict_ssh_loopback_probe(node, remote_port, label).await
}

fn strict_ssh_loopback_args(node: &Computer, forward: &str) -> Vec<String> {
    vec![
        "-N".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ServerAliveInterval=5".into(),
        "-o".into(),
        "ServerAliveCountMax=2".into(),
        "-p".into(),
        node.ssh_port.to_string(),
        "-L".into(),
        forward.into(),
        format!("{}@{}", node.ssh_user, node.ip),
    ]
}

async fn strict_ssh_loopback_probe(
    node: &Computer,
    remote_port: u16,
    label: &str,
) -> Result<PrimaryProbe> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("reserve local FalkorDB proof tunnel port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let forward = format!("127.0.0.1:{port}:127.0.0.1:{remote_port}");
    let mut child = tokio::process::Command::new("ssh")
        .args(strict_ssh_loopback_args(node, &forward))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("open bounded SSH tunnel for local {label} proof"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(PrimaryProbe {
                url: format!("redis://127.0.0.1:{port}"),
                tunnel: Some(child),
            });
        }
        if child.try_wait()?.is_some() {
            bail!("{label} SSH proof tunnel exited before becoming ready");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("{label} SSH proof tunnel did not become ready");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn field<'a>(raw: &'a str, name: &str) -> Result<&'a str> {
    let prefix = format!("{name}=");
    let matches: Vec<&str> = raw
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .collect();
    if matches.len() != 1 {
        bail!(
            "remote attestation field {name} must appear exactly once (found {})",
            matches.len()
        );
    }
    Ok(matches[0])
}

fn valid_sha256_id(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_image_evidence(evidence: &ImageEvidence) -> Result<()> {
    if !valid_sha256_id(&evidence.image_id) {
        bail!("daemon-resolved FalkorDB image ID is not canonical lowercase SHA-256");
    }
    let exact_digest_count = evidence
        .repo_digests
        .iter()
        .filter(|digest| digest.as_str() == FALKORDB_IMAGE)
        .count();
    if exact_digest_count != 1 {
        bail!("pinned FalkorDB reference does not resolve exactly once in daemon RepoDigests");
    }
    let mut dedup = evidence.repo_digests.clone();
    dedup.sort();
    dedup.dedup();
    if dedup.len() != evidence.repo_digests.len() {
        bail!("daemon FalkorDB RepoDigests contain duplicates");
    }
    Ok(())
}

fn parse_image_evidence(raw: &str) -> Result<ImageEvidence> {
    let mut evidence = ImageEvidence {
        image_id: field(raw, "IMAGE")?.trim().to_string(),
        repo_digests: serde_json::from_str(field(raw, "IMAGE_REPO_DIGESTS")?.trim())
            .context("parse daemon FalkorDB RepoDigests")?,
        configured_user: field(raw, "IMAGE_USER")?.trim().to_string(),
    };
    evidence.repo_digests.sort();
    validate_image_evidence(&evidence)?;
    Ok(evidence)
}

fn image_attestation_script() -> String {
    format!(
        r#"set -eu
printf 'IMAGE='
docker image inspect --format '{{{{.Id}}}}' '{FALKORDB_IMAGE}'
printf 'IMAGE_REPO_DIGESTS='
docker image inspect --format '{{{{json .RepoDigests}}}}' '{FALKORDB_IMAGE}'
printf 'IMAGE_USER='
docker image inspect --format '{{{{.Config.User}}}}' '{FALKORDB_IMAGE}'
"#,
    )
}

async fn attest_image(node: &Computer) -> Result<ImageEvidence> {
    let output = run_on_node(node, &image_attestation_script(), SSH_TIMEOUT)
        .await
        .with_context(|| {
            format!(
                "resolve pinned FalkorDB image on {} Docker daemon",
                node.name
            )
        })?;
    if !output.status.success() {
        bail!(
            "pinned FalkorDB image resolution failed on {}: {}",
            node.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_image_evidence(&String::from_utf8_lossy(&output.stdout))
}

fn parse_target_attestation(raw: &str) -> Result<TargetAttestation> {
    let docker_version = field(raw, "DOCKER")?.trim().to_string();
    let image = parse_image_evidence(raw)?;
    if docker_version.is_empty() {
        bail!("target Docker or pinned-image identity is empty");
    }
    let container = match field(raw, "CONTAINER")?.trim() {
        "absent" => None,
        value if !value.is_empty() => Some(value.to_string()),
        _ => bail!("target container attestation is empty"),
    };
    let volume_present = match field(raw, "VOLUME")?.trim() {
        "present" => true,
        "absent" => false,
        _ => bail!("target volume attestation is invalid"),
    };
    let ram_kib = field(raw, "RAM_KIB")?
        .trim()
        .parse::<u64>()
        .context("parse target RAM attestation")?;
    let disk_kib = field(raw, "DISK_FREE_KIB")?
        .trim()
        .parse::<u64>()
        .context("parse target disk attestation")?;
    Ok(TargetAttestation {
        docker_version,
        image_id: image.image_id,
        image_repo_digests: image.repo_digests,
        image_user: image.configured_user,
        container,
        volume_present,
        ram_bytes: ram_kib.saturating_mul(1024),
        disk_free_bytes: disk_kib.saturating_mul(1024),
        replica_port_listeners: field(raw, "PORT_63380_LISTENERS")?
            .trim()
            .parse::<u64>()
            .context("parse target FalkorDB listener count")?,
        replica_port_non_loopback: field(raw, "PORT_63380_NON_LOOPBACK")?
            .trim()
            .parse::<u64>()
            .context("parse target FalkorDB non-loopback listener count")?,
    })
}

fn target_attestation_script() -> String {
    format!(
        r#"set -eu
printf 'DOCKER='
docker info --format '{{{{.ServerVersion}}}}'
printf 'IMAGE='
docker image inspect --format '{{{{.Id}}}}' '{FALKORDB_IMAGE}'
printf 'IMAGE_REPO_DIGESTS='
docker image inspect --format '{{{{json .RepoDigests}}}}' '{FALKORDB_IMAGE}'
printf 'IMAGE_USER='
docker image inspect --format '{{{{.Config.User}}}}' '{FALKORDB_IMAGE}'
if docker container inspect '{FALKORDB_CONTAINER}' >/dev/null 2>&1; then
  printf 'CONTAINER='
  docker container inspect --format '{{{{.Image}}}}|{{{{.Config.Image}}}}|{{{{.State.Running}}}}|{{{{if .State.Health}}}}{{{{.State.Health.Status}}}}{{{{else}}}}none{{{{end}}}}|{{{{.HostConfig.RestartPolicy.Name}}}}|{{{{.HostConfig.NetworkMode}}}}|{{{{json .HostConfig.PortBindings}}}}|{{{{json .Config.Env}}}}|{{{{range .Mounts}}}}{{{{.Type}}}}:{{{{.Name}}}}:{{{{.Destination}}}}:{{{{.RW}}}};{{{{end}}}}|{{{{.HostConfig.ReadonlyRootfs}}}}|{{{{json .HostConfig.CapDrop}}}}|{{{{json .HostConfig.CapAdd}}}}|{{{{json .HostConfig.SecurityOpt}}}}|{{{{.HostConfig.Privileged}}}}|{{{{json .HostConfig.Devices}}}}|{{{{json .HostConfig.Tmpfs}}}}|{{{{json .HostConfig.Binds}}}}|{{{{.Config.User}}}}|{{{{json .Config.Healthcheck}}}}' '{FALKORDB_CONTAINER}'
else
  echo 'CONTAINER=absent'
fi
if docker volume inspect '{FALKORDB_VOLUME}' >/dev/null 2>&1; then echo 'VOLUME=present'; else echo 'VOLUME=absent'; fi
awk '/MemTotal:/ {{print "RAM_KIB=" $2}}' /proc/meminfo
df -Pk / | awk 'NR==2 {{print "DISK_FREE_KIB=" $4}}'
command -v ss >/dev/null
listeners="$(ss -H -ltn 'sport = :63380')"
printf 'PORT_63380_LISTENERS=%s\n' "$(printf '%s\n' "$listeners" | awk 'NF {{n++}} END {{print n+0}}')"
printf 'PORT_63380_NON_LOOPBACK=%s\n' "$(printf '%s\n' "$listeners" | awk 'NF && $4 != "127.0.0.1:63380" {{n++}} END {{print n+0}}')"
"#,
    )
}

fn validate_existing_container(
    fingerprint: &str,
    image_id: &str,
    image_user: &str,
    primary_ip: &str,
) -> Result<()> {
    let fields: Vec<&str> = fingerprint.splitn(19, '|').collect();
    if fields.len() != 19 {
        bail!("existing FalkorDB container fingerprint is incomplete");
    }
    if fields[0] != image_id
        || fields[1] != FALKORDB_IMAGE
        || fields[2] != "true"
        || fields[3] != "healthy"
        || fields[4] != "unless-stopped"
        || fields[5] != "host"
    {
        bail!("existing FalkorDB container is not exact, running, and healthy");
    }
    let ports: serde_json::Value =
        serde_json::from_str(fields[6]).context("parse FalkorDB port bindings")?;
    if !ports.is_null()
        && ports
            .as_object()
            .is_none_or(|bindings| !bindings.is_empty())
    {
        bail!("existing FalkorDB host-network container has unexpected published-port bindings");
    }
    let env: Vec<String> =
        serde_json::from_str(fields[7]).context("parse FalkorDB container environment")?;
    let redis_args = format!("REDIS_ARGS={}", expected_redis_args(primary_ip));
    let redis_args_count = env
        .iter()
        .filter(|value| value.starts_with("REDIS_ARGS="))
        .count();
    let falkor_args_count = env
        .iter()
        .filter(|value| value.starts_with("FALKORDB_ARGS="))
        .count();
    let browser_count = env
        .iter()
        .filter(|value| value.starts_with("BROWSER="))
        .count();
    if redis_args_count != 1
        || !env.contains(&redis_args)
        || falkor_args_count != 1
        || !env.contains(&"FALKORDB_ARGS=THREAD_COUNT 8 CACHE_SIZE 100".to_string())
        || browser_count != 1
        || !env.contains(&"BROWSER=0".to_string())
    {
        bail!("existing FalkorDB container has non-canonical replication arguments");
    }
    if fields[8] != format!("volume:{FALKORDB_VOLUME}:/var/lib/falkordb/data:true;") {
        bail!("existing FalkorDB container does not have the exact durable volume");
    }
    if fields[9] != "true" {
        bail!("existing FalkorDB container root filesystem is not read-only");
    }
    let normalized_set = |raw: &str, label: &str| -> Result<Vec<String>> {
        let mut values: Vec<String> = if raw == "null" {
            Vec::new()
        } else {
            serde_json::from_str(raw).with_context(|| format!("parse {label}"))?
        };
        values.sort();
        values.dedup();
        Ok(values)
    };
    if normalized_set(fields[10], "FalkorDB CapDrop")? != ["ALL"] {
        bail!("existing FalkorDB container must drop all Linux capabilities");
    }
    if !normalized_set(fields[11], "FalkorDB CapAdd")?.is_empty() {
        bail!("existing FalkorDB container unexpectedly adds Linux capabilities");
    }
    if normalized_set(fields[12], "FalkorDB security options")? != ["no-new-privileges:true"] {
        bail!("existing FalkorDB container lacks exact no-new-privileges hardening");
    }
    if fields[13] != "false" {
        bail!("existing FalkorDB container is privileged");
    }
    let devices: serde_json::Value =
        serde_json::from_str(fields[14]).context("parse FalkorDB device mappings")?;
    if !devices.is_null() && devices.as_array().is_none_or(|items| !items.is_empty()) {
        bail!("existing FalkorDB container has unexpected device mappings");
    }
    let tmpfs: BTreeMap<String, String> =
        serde_json::from_str(fields[15]).context("parse FalkorDB tmpfs mounts")?;
    if tmpfs.len() != 1 {
        bail!("existing FalkorDB container must have exactly one approved tmpfs mount");
    }
    let tmp_options = tmpfs
        .get("/tmp")
        .context("existing FalkorDB container has no /tmp tmpfs")?;
    let option_set: std::collections::BTreeSet<&str> = tmp_options.split(',').collect();
    for required in ["rw", "nodev", "nosuid", "noexec"] {
        if !option_set.contains(required) {
            bail!("existing FalkorDB /tmp tmpfs lacks {required}");
        }
    }
    if !option_set
        .iter()
        .any(|value| *value == "size=64m" || *value == "size=67108864")
    {
        bail!("existing FalkorDB /tmp tmpfs size is not exactly 64 MiB");
    }
    let binds: serde_json::Value =
        serde_json::from_str(fields[16]).context("parse FalkorDB bind mounts")?;
    if !binds.is_null() && binds.as_array().is_none_or(|items| !items.is_empty()) {
        bail!("existing FalkorDB container has unexpected bind mounts");
    }
    if fields[17] != image_user {
        bail!("existing FalkorDB runtime user differs from the pinned image contract");
    }
    let health: serde_json::Value =
        serde_json::from_str(fields[18]).context("parse FalkorDB healthcheck")?;
    if health.get("Test")
        != Some(&serde_json::json!([
            "CMD",
            "redis-cli",
            "-p",
            "63380",
            "PING"
        ]))
        || health.get("Interval").and_then(serde_json::Value::as_u64) != Some(5_000_000_000)
        || health.get("Timeout").and_then(serde_json::Value::as_u64) != Some(3_000_000_000)
        || health.get("Retries").and_then(serde_json::Value::as_u64) != Some(24)
        || health
            .get("StartPeriod")
            .and_then(serde_json::Value::as_u64)
            != Some(30_000_000_000)
    {
        bail!("existing FalkorDB container healthcheck is not exact");
    }
    Ok(())
}

async fn attest_target(
    target: &Computer,
    primary_ip: &str,
) -> Result<(TargetAttestation, TargetState)> {
    let output = run_on_node(target, &target_attestation_script(), SSH_TIMEOUT)
        .await
        .context("read-only FalkorDB target Docker preflight")?;
    if !output.status.success() {
        bail!(
            "target Docker/image preflight failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let attestation = parse_target_attestation(&String::from_utf8_lossy(&output.stdout))?;
    let state = if let Some(fingerprint) = &attestation.container {
        validate_existing_container(
            fingerprint,
            &attestation.image_id,
            &attestation.image_user,
            primary_ip,
        )?;
        if attestation.replica_port_listeners != 1 || attestation.replica_port_non_loopback != 0 {
            bail!("exact FalkorDB container is not listening once on IPv4 loopback 63380");
        }
        if !attestation.volume_present {
            bail!("exact FalkorDB container has no attested durable volume");
        }
        TargetState::ExactHealthy {
            image_id: attestation.image_id.clone(),
        }
    } else if attestation.volume_present {
        bail!(
            "FalkorDB volume exists without its exact container; preserve and audit the orphan before retrying"
        );
    } else {
        if attestation.replica_port_listeners != 0 {
            bail!("FalkorDB target loopback port 63380 is already occupied");
        }
        TargetState::Absent
    };
    Ok((attestation, state))
}

fn validate_plan_image(plan: &Plan, attestation: &TargetAttestation) -> Result<()> {
    if attestation.image_id != plan.material.target_image_id
        || attestation.image_repo_digests != plan.material.target_image_repo_digests
        || attestation.image_user != plan.material.image_user
        || plan.material.image != FALKORDB_IMAGE
    {
        bail!("pinned FalkorDB image resolution changed; the lifecycle plan is stale");
    }
    Ok(())
}

fn validate_primary_plan_image(plan: &Plan, evidence: &ImageEvidence) -> Result<()> {
    if evidence.image_id != plan.material.primary_image_id
        || evidence.repo_digests != plan.material.primary_image_repo_digests
        || evidence.configured_user != plan.material.image_user
        || plan.material.image != FALKORDB_IMAGE
    {
        bail!("Priya Docker image resolution changed after planning; refusing target mutation");
    }
    Ok(())
}

fn firewall_attestation_script() -> &'static str {
    r#"set -eu
unit='forgefleet-falkordb-source-firewall.service'
sudo -n sh -c 'set -a; . /etc/forgefleet/falkordb-source-firewall.env; exec /usr/local/sbin/forgefleet-falkor-source-firewall --json status'
echo "SERVICE_AFTER=$(systemctl show "$unit" -p After --value)"
echo "SERVICE_REQUIRES=$(systemctl show "$unit" -p Requires --value)"
echo "SERVICE_PARTOF=$(systemctl show "$unit" -p PartOf --value)"
"#
}

fn firewall_status(raw: &str) -> Result<(FirewallStatus, &str)> {
    if raw.chars().next().is_some_and(char::is_whitespace) {
        bail!("firewall helper status has unexpected stdout before its JSON document");
    }
    let mut documents = serde_json::Deserializer::from_str(raw).into_iter::<FirewallStatus>();
    let status = documents
        .next()
        .context("firewall helper emitted no JSON status document")?
        .context("parse strict firewall helper status JSON")?;
    let remainder = &raw[documents.byte_offset()..];
    let lines: Vec<&str> = remainder
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() != 3
        || !lines[0].starts_with("SERVICE_AFTER=")
        || !lines[1].starts_with("SERVICE_REQUIRES=")
        || !lines[2].starts_with("SERVICE_PARTOF=")
    {
        bail!("firewall attestation suffix is not the exact three systemd dependency fields");
    }
    Ok((status, remainder))
}

fn validate_firewall_attestation(
    raw: &str,
    target: &Computer,
    primary: &Computer,
) -> Result<FirewallEvidence> {
    let (status, systemd) = firewall_status(raw)?;
    let unit_enabled = status.persistence.unit_enabled;
    let unit_active = status.persistence.unit_active;
    let unit_result_success = status.ok;
    let after = field(systemd, "SERVICE_AFTER")?;
    let requires = field(systemd, "SERVICE_REQUIRES")?;
    let part_of = field(systemd, "SERVICE_PARTOF")?;
    let docker_lifecycle_bound = after
        .split_whitespace()
        .any(|item| item == "docker.service")
        && requires
            .split_whitespace()
            .any(|item| item == "docker.service")
        && part_of
            .split_whitespace()
            .any(|item| item == "docker.service");

    let identity_exact = status.interface == "enp3s0"
        && status.source_ipv4 == target.ip
        && status.destination_ipv4 == primary.ip
        && status.port == PRIMARY_PORT;
    let persistence_exact = status.persistence.unit
        == "forgefleet-falkordb-source-firewall.service"
        && status.persistence.environment_file == "/etc/forgefleet/falkordb-source-firewall.env"
        && status.persistence.helper == "/usr/local/sbin/forgefleet-falkor-source-firewall"
        && status.persistence.environment_file_present
        && status.persistence.helper_present;
    let ipv4_target_allow = status.allow_v4 && status.allow_v4_position == 1 && identity_exact;
    let ipv4_default_deny = status.deny_v4 && status.deny_v4_position == 2 && identity_exact;
    let ipv6_default_deny = status.deny_v6 && status.deny_v6_position == 1 && identity_exact;
    let ipv6_forward_default_deny =
        status.deny_v6 && status.deny_v6_forward_position == 1 && identity_exact;

    let evidence = FirewallEvidence {
        target_id: target.id,
        target_ip: target.ip.clone(),
        primary_id: primary.id,
        primary_ip: primary.ip.clone(),
        primary_port: PRIMARY_PORT,
        unit_enabled,
        unit_active,
        unit_result_success,
        docker_lifecycle_bound,
        ipv4_target_allow,
        ipv4_default_deny,
        ipv6_default_deny,
        ipv6_forward_default_deny,
    };
    if !evidence.unit_enabled
        || !evidence.unit_active
        || !evidence.unit_result_success
        || !evidence.docker_lifecycle_bound
        || !evidence.ipv4_target_allow
        || !evidence.ipv4_default_deny
        || !evidence.ipv6_default_deny
        || !evidence.ipv6_forward_default_deny
        || !persistence_exact
    {
        bail!("Priya source-specific FalkorDB firewall gate is not exact and Docker-persistent");
    }
    Ok(evidence)
}

async fn attest_firewall(primary: &Computer, target: &Computer) -> Result<FirewallEvidence> {
    let output = run_on_node(primary, firewall_attestation_script(), SSH_TIMEOUT)
        .await
        .context("read Priya FalkorDB firewall authority")?;
    if !output.status.success() {
        bail!("Priya firewall attestation command failed");
    }
    validate_firewall_attestation(&String::from_utf8_lossy(&output.stdout), target, primary)
}

async fn attest_target_source_route(
    target: &Computer,
    primary: &Computer,
) -> Result<TargetSourceRouteEvidence> {
    let script = format!(
        r#"set -eu
command -v ip >/dev/null
command -v timeout >/dev/null
route="$(ip -4 route get '{primary_ip}')"
printf 'ROUTE=%s\n' "$route"
timeout 5 bash -c '</dev/tcp/{primary_ip}/{PRIMARY_PORT}'
echo 'TCP_REACHABLE=yes'
"#,
        primary_ip = primary.ip,
    );
    let output = run_on_node(target, &script, SSH_TIMEOUT)
        .await
        .context("probe target-to-Priya FalkorDB source route")?;
    if !output.status.success() {
        bail!(
            "target cannot reach Priya FalkorDB through the exact source firewall: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let route = field(&raw, "ROUTE")?;
    if !route
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair == ["src", target.ip.as_str()])
        || field(&raw, "TCP_REACHABLE")?.trim() != "yes"
    {
        bail!("target-to-Priya FalkorDB route does not use the canonical target source IP");
    }
    Ok(TargetSourceRouteEvidence {
        target_id: target.id,
        target_ip: target.ip.clone(),
        source_ip: target.ip.clone(),
        primary_id: primary.id,
        primary_ip: primary.ip.clone(),
        primary_port: PRIMARY_PORT,
        tcp_reachable: true,
    })
}

async fn redis_value(url: &str, command: &str, args: &[&str]) -> Result<redis::Value> {
    let mut connection = redis_connection(url).await?;
    let mut query = redis::cmd(command);
    for arg in args {
        query.arg(*arg);
    }
    tokio::time::timeout(IO_TIMEOUT, query.query_async(&mut connection))
        .await
        .with_context(|| format!("{command} timed out"))?
        .with_context(|| format!("query {command}"))
}

fn encode_redis_value(value: &redis::Value, output: &mut Vec<u8>) -> Result<()> {
    use redis::Value;
    match value {
        Value::Nil => output.extend_from_slice(b"n;"),
        Value::Int(value) => output.extend_from_slice(format!("i{value};").as_bytes()),
        Value::BulkString(value) => {
            output.extend_from_slice(format!("b{}:", value.len()).as_bytes());
            output.extend_from_slice(value);
            output.push(b';');
        }
        Value::Array(values) => {
            output.extend_from_slice(format!("a{}[", values.len()).as_bytes());
            for value in values {
                encode_redis_value(value, output)?;
            }
            output.extend_from_slice(b"];");
        }
        Value::SimpleString(value) => {
            output.extend_from_slice(format!("s{}:{value};", value.len()).as_bytes());
        }
        Value::Okay => output.extend_from_slice(b"ok;"),
        Value::Double(value) if value.is_finite() => {
            output.extend_from_slice(format!("d{value:e};").as_bytes());
        }
        Value::Boolean(value) => {
            output.extend_from_slice(if *value { b"t;" } else { b"f;" });
        }
        // GRAPH.RO_QUERY proofs must not rely on unordered or protocol-push
        // shapes. Refusing them keeps representative hashes deterministic.
        Value::Map(_)
        | Value::Attribute { .. }
        | Value::Set(_)
        | Value::VerbatimString { .. }
        | Value::BigNumber(_)
        | Value::Push { .. }
        | Value::ServerError(_)
        | Value::Double(_) => bail!("unsupported Redis value in deterministic graph proof"),
    }
    Ok(())
}

fn graph_full_hash(
    value: &redis::Value,
    expected_rows: u64,
    require_unique_rows: bool,
) -> Result<String> {
    let redis::Value::Array(parts) = value else {
        bail!("FalkorDB query proof is not an array");
    };
    let rows_index = if matches!(parts.first(), Some(redis::Value::Int(_))) {
        2
    } else {
        1
    };
    if parts.len() <= rows_index {
        bail!("FalkorDB query proof lacks header and rows");
    }
    let redis::Value::Array(rows) = &parts[rows_index] else {
        bail!("FalkorDB full graph proof rows are not an array");
    };
    if u64::try_from(rows.len()).ok() != Some(expected_rows) {
        bail!(
            "FalkorDB full graph query returned {} rows, expected exact count {expected_rows}",
            rows.len()
        );
    }

    // Hash a sorted multiset of complete canonical Redis rows. This avoids
    // internal FalkorDB IDs and query-return order while retaining duplicate
    // nodes/relationships. Every scalar is type-tagged and length-delimited by
    // encode_redis_value; unsupported/nondeterministic RESP shapes fail closed.
    let mut encoded_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut encoded = Vec::new();
        encode_redis_value(row, &mut encoded)?;
        encoded_rows.push(encoded);
    }
    encoded_rows.sort();
    if require_unique_rows && encoded_rows.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!(
            "FalkorDB node content is not a unique portable endpoint identity; complete cross-daemon relationship equality is unavailable"
        );
    }
    let mut digest = Sha256::new();
    digest.update(expected_rows.to_be_bytes());
    for row in encoded_rows {
        digest.update((row.len() as u64).to_be_bytes());
        digest.update(row);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn graph_count(value: &redis::Value) -> Result<u64> {
    let redis::Value::Array(parts) = value else {
        bail!("FalkorDB count response is not an array");
    };
    // FalkorDB compact responses prefix the header with its column count;
    // older response shapes start directly with the header.
    let rows_index = if matches!(parts.first(), Some(redis::Value::Int(_))) {
        2
    } else {
        1
    };
    let rows = parts
        .get(rows_index)
        .context("FalkorDB count response has no rows")?;
    let redis::Value::Array(rows) = rows else {
        bail!("FalkorDB count rows are not an array");
    };
    let row = rows.first().context("FalkorDB count response is empty")?;
    let redis::Value::Array(row) = row else {
        bail!("FalkorDB count row is not an array");
    };
    let value = row.first().context("FalkorDB count row has no scalar")?;
    match value {
        redis::Value::Int(value) if *value >= 0 => Ok(*value as u64),
        redis::Value::BulkString(value) => std::str::from_utf8(value)?
            .parse::<u64>()
            .context("parse FalkorDB count"),
        redis::Value::Array(encoded)
            if encoded.len() == 2 && encoded.first() == Some(&redis::Value::Int(3)) =>
        {
            // FalkorDB compact scalars are [type-code, value]; type 3 is an
            // integer. The second element is therefore the actual count.
            match &encoded[1] {
                redis::Value::Int(value) if *value >= 0 => Ok(*value as u64),
                redis::Value::BulkString(value) => std::str::from_utf8(value)?
                    .parse::<u64>()
                    .context("parse compact FalkorDB count"),
                value => bail!("compact FalkorDB count value is invalid: {value:?}"),
            }
        }
        value => bail!("FalkorDB count scalar has an unexpected type: {value:?}"),
    }
}

async fn graph_query(url: &str, graph: &str, query: &str) -> Result<redis::Value> {
    redis_value(url, "GRAPH.RO_QUERY", &[graph, query, "--compact"]).await
}

async fn graph_inventory(url: &str) -> Result<BTreeMap<String, GraphEvidence>> {
    let mut connection = redis_connection(url).await?;
    let mut graphs: Vec<String> = tokio::time::timeout(
        IO_TIMEOUT,
        redis::cmd("GRAPH.LIST").query_async(&mut connection),
    )
    .await
    .context("GRAPH.LIST timed out")?
    .context("query GRAPH.LIST")?;
    graphs.sort();
    if graphs.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("FalkorDB authority returned duplicate graph identities");
    }
    if graphs.is_empty() {
        bail!("FalkorDB authority reports no graphs");
    }

    let mut evidence = BTreeMap::new();
    for graph in graphs {
        if graph.trim().is_empty() {
            bail!("FalkorDB authority returned a blank graph identity");
        }
        let nodes = graph_count(&graph_query(url, &graph, "MATCH (n) RETURN count(n)").await?)?;
        let relationships =
            graph_count(&graph_query(url, &graph, "MATCH ()-[r]->() RETURN count(r)").await?)?;
        let node_full_sha256 = graph_full_hash(
            &graph_query(url, &graph, FULL_NODE_QUERY).await?,
            nodes,
            true,
        )?;
        let relationship_full_sha256 = graph_full_hash(
            &graph_query(url, &graph, FULL_RELATIONSHIP_QUERY).await?,
            relationships,
            false,
        )?;
        evidence.insert(
            graph,
            GraphEvidence {
                nodes,
                relationships,
                node_full_sha256,
                relationship_full_sha256,
            },
        );
    }
    Ok(evidence)
}

async fn graph_module_version(url: &str) -> Result<i64> {
    fn text(value: &redis::Value) -> Option<&str> {
        match value {
            redis::Value::BulkString(value) => std::str::from_utf8(value).ok(),
            redis::Value::SimpleString(value) => Some(value),
            _ => None,
        }
    }
    fn integer(value: &redis::Value) -> Option<i64> {
        match value {
            redis::Value::Int(value) => Some(*value),
            value => text(value)?.parse().ok(),
        }
    }
    let response = redis_value(url, "MODULE", &["LIST"]).await?;
    let redis::Value::Array(modules) = response else {
        bail!("MODULE LIST did not return an array");
    };
    let mut graph_versions = Vec::new();
    for module in modules {
        let pairs: Vec<(&redis::Value, &redis::Value)> = match &module {
            redis::Value::Array(fields) => {
                if fields.len() % 2 != 0 {
                    bail!("MODULE LIST entry has an odd field count");
                }
                fields
                    .chunks_exact(2)
                    .map(|pair| (&pair[0], &pair[1]))
                    .collect()
            }
            redis::Value::Map(fields) => fields.iter().map(|(key, value)| (key, value)).collect(),
            _ => bail!("MODULE LIST entry is neither an array nor map"),
        };
        let name = pairs
            .iter()
            .find(|(key, _)| text(key) == Some("name"))
            .and_then(|(_, value)| text(value));
        if name.is_some_and(|name| name.eq_ignore_ascii_case("graph")) {
            let version = pairs
                .iter()
                .find(|(key, _)| text(key) == Some("ver"))
                .and_then(|(_, value)| integer(value))
                .context("graph module has no numeric version")?;
            graph_versions.push(version);
        }
    }
    if graph_versions.len() != 1 {
        bail!("expected exactly one loaded FalkorDB graph module");
    }
    Ok(graph_versions[0])
}

async fn dbsize(url: &str) -> Result<u64> {
    let value = redis_value(url, "DBSIZE", &[]).await?;
    match value {
        redis::Value::Int(value) if value >= 0 => Ok(value as u64),
        _ => bail!("FalkorDB DBSIZE returned an invalid value"),
    }
}

async fn validate_unauthenticated_primary(url: &str) -> Result<()> {
    if !redis_config_value(url, "requirepass").await?.is_empty() {
        bail!(
            "authenticated FalkorDB primary requires reviewed 0600 secret-file support; credentials are never copied into plans, argv, env, or task logs"
        );
    }
    let mut connection = redis_connection(url).await?;
    let acl: Vec<String> = tokio::time::timeout(
        IO_TIMEOUT,
        redis::cmd("ACL").arg("LIST").query_async(&mut connection),
    )
    .await
    .context("ACL LIST timed out")?
    .context("query FalkorDB ACL")?;
    let default_users: Vec<&String> = acl
        .iter()
        .filter(|line| line.starts_with("user default "))
        .collect();
    if default_users.len() != 1
        || !default_users[0].contains(" on ")
        || !default_users[0].contains(" nopass ")
    {
        bail!("FalkorDB ACL authority is not the expected unauthenticated default user");
    }
    Ok(())
}

fn restore_receipt(detail: &str, expected_checksum: &str) -> Result<String> {
    if detail.match_indices("receipt=").count() != 1 {
        bail!("restore drill detail must contain exactly one receipt envelope");
    }
    let framed = detail
        .strip_prefix("restore drill passed; receipt=")
        .context("restore drill detail does not use the exact receipt envelope")?;
    let mut documents = serde_json::Deserializer::from_str(framed).into_iter::<RestoreReceipt>();
    let receipt = documents
        .next()
        .context("restore drill receipt JSON is missing")?
        .context("parse strict FalkorDB restore receipt")?;
    let capacity = &framed[documents.byte_offset()..];
    if capacity != "; capacity=ok" {
        let normalized = capacity.trim_start_matches("; ").replace(';', "");
        let tokens: Vec<&str> = normalized.split_whitespace().collect();
        let numeric = |token: &str, prefix: &str| {
            token
                .strip_prefix(prefix)
                .and_then(|value| value.parse::<u64>().ok())
                .is_some()
        };
        if tokens.len() != 11
            || tokens[0] != "capacity"
            || !numeric(tokens[1], "required=")
            || !numeric(tokens[2], "available=")
            || tokens[3] != "bytes"
            || tokens[4] != "policy"
            || !numeric(tokens[5], "encrypted_max=")
            || !numeric(tokens[6], "extracted_max=")
            || !numeric(tokens[7], "effective_extracted=")
            || !numeric(tokens[8], "files_max=")
            || !numeric(tokens[9], "expansion_ratio=")
            || !numeric(tokens[10], "reserve=")
        {
            bail!("restore drill receipt has a malformed capacity evidence suffix");
        }
    }
    if receipt.proof != "falkordb_exact_restore_v1"
        || receipt.input_checksum_sha256 != expected_checksum
        || receipt.image_reference != FALKORDB_IMAGE
        || receipt.network != "none"
        || receipt.query_mode != "GRAPH.RO_QUERY"
    {
        bail!("FalkorDB restore receipt identity is not exact");
    }
    if !valid_sha256_id(&receipt.image_id)
        || expected_checksum.len() != 64
        || !expected_checksum
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("FalkorDB restore receipt digest is not canonical lowercase SHA-256");
    }
    if receipt.expected_min_keys == 0
        || receipt.expected_min_graph_nodes == 0
        || receipt.observed_keys < receipt.expected_min_keys
        || receipt.observed_graph_nodes < receipt.expected_min_graph_nodes
        || receipt.observed_graphs == 0
    {
        bail!("FalkorDB restore receipt did not meet its exact minimum dataset proof");
    }
    Ok(receipt.image_id)
}

async fn validate_topology_for_plan(
    pool: &sqlx::PgPool,
    target: &Computer,
    primary: &Computer,
) -> Result<()> {
    let primary_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT computer_id FROM database_replicas
          WHERE database_kind='falkordb' AND role='primary' ORDER BY computer_id",
    )
    .fetch_all(pool)
    .await?;
    if primary_ids.len() > 1
        || primary_ids
            .first()
            .is_some_and(|computer_id| *computer_id != primary.id)
    {
        bail!("FalkorDB topology does not have zero-or-one canonical Priya primary row");
    }
    if let Some(role) = sqlx::query_scalar::<_, String>(
        "SELECT role FROM database_replicas WHERE computer_id=$1 AND database_kind='falkordb'",
    )
    .bind(target.id)
    .fetch_optional(pool)
    .await?
    {
        if role != "replica" {
            bail!("target has incompatible FalkorDB topology role {role:?}");
        }
    }
    Ok(())
}

async fn authority_url(pool: &sqlx::PgPool, primary: &Computer) -> Result<String> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM fleet_secrets
          WHERE key='falkordb.url' AND disabled_reason IS NULL ORDER BY updated_at DESC",
    )
    .fetch_all(pool)
    .await?;
    if rows.len() != 1 {
        bail!(
            "FalkorDB URL authority resolved to {} active secret rows; require exactly one",
            rows.len()
        );
    }
    let canonical = normalized_url(&rows[0], &primary.ip)?;
    if let Ok(environment) = std::env::var("FALKORDB_URL") {
        if normalized_url(&environment, &primary.ip)? != canonical {
            bail!("FALKORDB_URL disagrees with fleet_secrets FalkorDB authority");
        }
    }
    Ok(canonical)
}

async fn backup_evidence(pool: &sqlx::PgPool, primary: &Computer) -> Result<BackupEvidence> {
    let policy_rows = sqlx::query(
        "SELECT source_host,dest_hosts,encrypt,enabled
           FROM fleet_backup_config WHERE kind='falkordb'",
    )
    .fetch_all(pool)
    .await?;
    if policy_rows.len() != 1 {
        bail!("FalkorDB backup policy must have exactly one row");
    }
    let policy = &policy_rows[0];
    if !policy.get::<bool, _>("enabled") || !policy.get::<bool, _>("encrypt") {
        bail!("FalkorDB backup policy must be enabled and encrypted");
    }
    let source = policy
        .try_get::<Option<String>, _>("source_host")?
        .filter(|value| !value.trim().is_empty())
        .context("FalkorDB backup source is not pinned")?;
    if !source.eq_ignore_ascii_case(&primary.name) {
        bail!("FalkorDB backup source policy is not canonical Priya");
    }
    let destinations = policy.get::<Vec<String>, _>("dest_hosts");
    if destinations.len() != 2 || destinations[0].eq_ignore_ascii_case(&destinations[1]) {
        bail!("FalkorDB backup policy must name exactly two distinct destinations");
    }
    let mut destination_nodes = Vec::new();
    for destination in destinations {
        let computer = resolve_computer(pool, &destination).await?;
        reject_vinny(&computer.name)?;
        if computer.id == primary.id {
            bail!("FalkorDB backup destination cannot be Priya");
        }
        destination_nodes.push(computer);
    }
    destination_nodes.sort_by_key(|computer| computer.id);

    let candidates = sqlx::query(
        "SELECT b.id,b.file_name,b.checksum_sha256,b.size_bytes,b.created_at,
                b.verified_restorable_at,b.distribution_status,
                d.id AS drill_id,d.detail,d.finished_at
           FROM backups b
           JOIN LATERAL (
             SELECT id,detail,finished_at FROM backup_drills d
              WHERE d.backup_id=b.id AND d.database_kind='falkordb'
                AND d.backup_file=b.file_name AND d.success AND d.stage='done'
              ORDER BY d.finished_at DESC LIMIT 1
           ) d ON true
          WHERE b.database_kind='falkordb' AND b.source_computer_id=$1
            AND b.size_bytes>0 AND b.checksum_sha256 ~ '^[0-9a-fA-F]{64}$'
            AND b.file_name LIKE '%.age'
            AND b.created_at BETWEEN NOW() - make_interval(hours => $2::int) AND NOW()
            AND b.verified_restorable_at BETWEEN b.created_at AND NOW()
          ORDER BY b.created_at DESC",
    )
    .bind(primary.id)
    .bind(MAX_BACKUP_AGE_HOURS)
    .fetch_all(pool)
    .await?;

    let now = Utc::now();
    let mut failures = Vec::new();
    for row in candidates {
        let backup_id: Uuid = row.get("id");
        let file_name: String = row.get("file_name");
        let checksum_sha256: String = row.get::<String, _>("checksum_sha256").to_lowercase();
        let size_bytes: i64 = row.get("size_bytes");
        let created_at: DateTime<Utc> = row.get("created_at");
        let verified_restorable_at: DateTime<Utc> = row.get("verified_restorable_at");
        let distribution_status: serde_json::Value = row.get("distribution_status");
        let drill_id: Uuid = row.get("drill_id");
        let detail: String = row.get("detail");
        let proof = (|| -> Result<String> {
            let restore_image_id = restore_receipt(&detail, &checksum_sha256)?;
            for destination in &destination_nodes {
                crate::fleet_cmd::validate_remote_backup_receipt(
                    &distribution_status,
                    backup_id,
                    destination.id,
                    &destination.name,
                    &checksum_sha256,
                    size_bytes,
                    "falkordb",
                    &file_name,
                    created_at,
                    now,
                )
                .map_err(anyhow::Error::msg)?;
            }
            Ok(restore_image_id)
        })();
        match proof {
            Ok(restore_image_id) => {
                return Ok(BackupEvidence {
                    backup_id,
                    file_name,
                    checksum_sha256,
                    size_bytes,
                    drill_id,
                    restore_image_id,
                    created_at,
                    verified_restorable_at,
                    distributed_to: destination_nodes
                        .iter()
                        .map(|computer| computer.id)
                        .collect(),
                });
            }
            Err(error) => failures.push(format!("{backup_id}: {error:#}")),
        }
    }
    let detail = if failures.is_empty() {
        "no recent source backup has an exact successful restore drill".to_string()
    } else {
        failures.join("; ")
    };
    bail!(
        "no recent encrypted FalkorDB backup has exact restore proof plus exact receipts on both configured destinations: {detail}"
    )
}

async fn primary_evidence(
    url: &str,
) -> Result<(
    String,
    String,
    i64,
    u64,
    u64,
    BTreeMap<String, GraphEvidence>,
)> {
    validate_unauthenticated_primary(url).await?;
    let replication = redis_info(url, "replication").await?;
    let (replid, offset_before) = primary_identity(&replication)?;
    let server = redis_info(url, "server").await?;
    let version = server
        .get("redis_version")
        .context("FalkorDB authority has no Redis version")?
        .clone();
    if version != EXPECTED_REDIS_VERSION {
        bail!(
            "FalkorDB authority Redis version is {version}, expected exact {EXPECTED_REDIS_VERSION}"
        );
    }
    let graph_version = graph_module_version(url).await?;
    if graph_version != EXPECTED_GRAPH_MODULE_VERSION {
        bail!(
            "FalkorDB graph module version is {graph_version}, expected {EXPECTED_GRAPH_MODULE_VERSION}"
        );
    }
    let used_memory = primary_used_memory(url).await?;
    let primary_dbsize = dbsize(url).await?;
    let graphs = graph_inventory(url).await?;
    let replication_after = redis_info(url, "replication").await?;
    let (replid_after, offset_after) = primary_identity(&replication_after)?;
    if replid_after != replid
        || offset_after != offset_before
        || dbsize(url).await? != primary_dbsize
    {
        bail!(
            "Priya changed during the complete FalkorDB graph scan; retry from a fresh quiescent proof"
        );
    }
    Ok((
        replid,
        version,
        graph_version,
        primary_dbsize,
        used_memory,
        graphs,
    ))
}

async fn primary_used_memory(url: &str) -> Result<u64> {
    let memory = redis_info(url, "memory").await?;
    let used_memory = memory
        .get("used_memory")
        .context("FalkorDB authority has no used_memory")?
        .parse::<u64>()
        .context("parse FalkorDB used_memory")?;
    if used_memory == 0 {
        bail!("FalkorDB authority reported zero used memory");
    }
    Ok(used_memory)
}

fn validate_target_capacity(target: &TargetAttestation, primary_used_memory: u64) -> Result<()> {
    if primary_used_memory == 0 {
        bail!("FalkorDB authority reported zero used memory");
    }
    let required_bytes = primary_used_memory.saturating_mul(2).max(MIN_TARGET_BYTES);
    if target.ram_bytes < required_bytes || target.disk_free_bytes < required_bytes {
        bail!(
            "FalkorDB target needs at least {required_bytes} bytes RAM and free disk (2x source data with 2 GiB floor)"
        );
    }
    Ok(())
}

async fn build_plan(pool: &sqlx::PgPool, to: &str, primary_name: &str) -> Result<Plan> {
    reject_vinny(to)?;
    let target = resolve_computer(pool, to).await?;
    let primary = resolve_computer(pool, primary_name).await?;
    validate_target_identity(&target, &primary)?;
    validate_topology_for_plan(pool, &target, &primary).await?;

    let canonical_authority = authority_url(pool, &primary).await?;
    let probe = acquire_authorized_primary_probe(&canonical_authority, &primary, &target).await?;
    let (
        primary_replid,
        primary_version,
        graph_module_version,
        primary_dbsize,
        primary_used_memory,
        graphs,
    ) = match primary_evidence(probe.url()).await {
        Ok(evidence) => {
            probe.close().await;
            evidence
        }
        Err(error) => {
            probe.close().await;
            return Err(error)
                .context("probe canonical Priya FalkorDB through authorized transport");
        }
    };
    let primary_image = attest_image(&primary).await?;
    let backup = backup_evidence(pool, &primary).await?;
    let (target_attestation, target_state) = attest_target(&target, &primary.ip).await?;
    if target_attestation.image_user != primary_image.configured_user {
        bail!(
            "target and canonical Priya Docker daemons disagree on the pinned FalkorDB image user"
        );
    }

    validate_target_capacity(&target_attestation, primary_used_memory)?;

    Ok(Plan {
        material: PlanMaterial {
            version: "falkordb-replica-plan-v4",
            target_id: target.id,
            target_name: target.name,
            target_ip: target.ip,
            target_ssh_user: target.ssh_user,
            target_ssh_port: target.ssh_port,
            target_failure_domain: target.failure_domain,
            primary_id: primary.id,
            primary_name: primary.name,
            primary_ip: primary.ip,
            primary_replid,
            primary_version,
            graph_module_version,
            primary_dbsize,
            graphs,
            backup,
            target_state,
            image: FALKORDB_IMAGE,
            primary_image_id: primary_image.image_id,
            primary_image_repo_digests: primary_image.repo_digests,
            target_image_id: target_attestation.image_id,
            target_image_repo_digests: target_attestation.image_repo_digests,
            image_user: primary_image.configured_user,
            primary_port: PRIMARY_PORT,
            replica_port: REPLICA_PORT,
            firewall_policy: "forgefleet-falkordb-source-firewall-v1-four-rule",
            automatic_failover: false,
            read_routing: false,
        },
    })
}

fn local_apply_command(plan: &Plan) -> String {
    format!(
        "cd \"$HOME/projects/forge-fleet\" && \"$HOME/.local/bin/ff\" fleet db falkordb-replica local-apply --to {} --primary {} --plan-id {}",
        shell_escape_single(&plan.material.target_name),
        shell_escape_single(&plan.material.primary_name),
        shell_escape_single(&plan.id()),
    )
}

fn lifecycle_signature(target_id: Uuid) -> String {
    sha256(format!("falkordb-replica-lifecycle-v1\0{target_id}").as_bytes())
}

fn lifecycle_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

fn validate_lifecycle_payload(
    payload: &serde_json::Value,
    target: &Computer,
    action: &str,
    proof_id: &str,
    command: &str,
) -> Result<()> {
    let deferred = payload
        .get("deferred_payload")
        .and_then(serde_json::Value::as_object)
        .context("existing FalkorDB lifecycle task has no structured deferred payload")?;
    let exact = |key: &str, expected: &str| {
        deferred.get(key).and_then(serde_json::Value::as_str) == Some(expected)
    };
    if !exact("action", action)
        || !exact("command", command)
        || !exact("command_sha256", &sha256(command.as_bytes()))
        || !exact("proof_id", proof_id)
        || deferred
            .get("target_computer_id")
            .and_then(serde_json::Value::as_str)
            != Some(target.id.to_string().as_str())
        || deferred
            .get("database_kind")
            .and_then(serde_json::Value::as_str)
            != Some("falkordb")
        || deferred
            .get("automatic_failover")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
        || deferred
            .get("read_routing")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
    {
        bail!("existing FalkorDB lifecycle task command/proof/target binding is mismatched");
    }
    Ok(())
}

async fn enqueue_lifecycle_action(
    pool: &sqlx::PgPool,
    target: &Computer,
    action: &str,
    proof_id: &str,
    command: String,
) -> Result<Uuid> {
    let signature = lifecycle_signature(target.id);
    let summary = format!("FalkorDB replica {action} on {}", target.name);
    let required_caps = serde_json::json!([]);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("falkordb-replica:{}", target.id))
        .execute(&mut *tx)
        .await?;
    let existing = sqlx::query(
        "SELECT id,status,payload
           FROM fleet_tasks WHERE dedup_signature=$1
          ORDER BY created_at DESC FOR UPDATE",
    )
    .bind(&signature)
    .fetch_all(&mut *tx)
    .await?;
    if existing.len() > 1 {
        bail!("multiple FalkorDB lifecycle rows carry the exact target signature");
    }
    if let Some(row) = existing.first() {
        let id: Uuid = row.get("id");
        let status: String = row.get("status");
        if lifecycle_terminal(&status) {
            let cleared = sqlx::query(
                "UPDATE fleet_tasks SET dedup_signature=NULL
                  WHERE id=$1 AND dedup_signature=$2
                    AND status IN ('completed','failed','cancelled')",
            )
            .bind(id)
            .bind(&signature)
            .execute(&mut *tx)
            .await?;
            if cleared.rows_affected() != 1 {
                bail!("terminal FalkorDB lifecycle signature changed during locked rollover");
            }
        } else {
            let payload: serde_json::Value = row.get("payload");
            validate_lifecycle_payload(&payload, target, action, proof_id, &command)?;
            let id: Uuid = row.get("id");
            tx.commit().await?;
            return Ok(id);
        }
    }
    let deferred_payload = serde_json::json!({
        "command": command.clone(),
        "command_sha256": sha256(command.as_bytes()),
        "action": action,
        "proof_id": proof_id,
        "summary": summary,
        "operation": format!("falkordb_replica_{action}"),
        "target_computer_id": target.id,
        "database_kind": "falkordb",
        "automatic_failover": false,
        "read_routing": false,
    });
    let payload = serde_json::json!({
        "deferred_payload": deferred_payload,
        "created_by": whoami_tag(),
        "kind": "shell",
        "trigger_type": "node_online",
        "trigger_spec": {"node": target.name},
        "preferred_node": target.name,
        "required_caps": required_caps,
        "attempts": 0,
        "max_attempts": 1,
    });
    let new_id = Uuid::new_v4();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO fleet_tasks
           (id,task_type,summary,payload,priority,requires_capability,
            preferred_computer_id,status,created_at,task_class,dedup_signature)
         VALUES ($1,'shell',$2,$3,50,$4,$5,'pending',NOW(),'deferred',$6)
         ON CONFLICT (dedup_signature) WHERE dedup_signature IS NOT NULL DO NOTHING
         RETURNING id",
    )
    .bind(new_id)
    .bind(&summary)
    .bind(&payload)
    .bind(&required_caps)
    .bind(target.id)
    .bind(&signature)
    .fetch_optional(&mut *tx)
    .await?;
    let id = if let Some(id) = inserted {
        id
    } else {
        let rows = sqlx::query(
            "SELECT id,status,payload FROM fleet_tasks
              WHERE dedup_signature=$1 ORDER BY created_at DESC FOR UPDATE",
        )
        .bind(&signature)
        .fetch_all(&mut *tx)
        .await?;
        if rows.len() != 1 {
            bail!("FalkorDB lifecycle unique-index race did not resolve to exactly one row");
        }
        let row = &rows[0];
        let status: String = row.get("status");
        if lifecycle_terminal(&status) {
            bail!("FalkorDB lifecycle unique-index race resolved to a terminal row");
        }
        let payload: serde_json::Value = row.get("payload");
        validate_lifecycle_payload(&payload, target, action, proof_id, &command)?;
        row.get("id")
    };
    tx.commit().await?;
    Ok(id)
}

async fn validate_compose(plan: &Plan) -> Result<()> {
    if !std::path::Path::new(FALKORDB_COMPOSE).is_file() {
        bail!("run from a ForgeFleet checkout containing the FalkorDB follower compose template");
    }
    let runtime_compose = tokio::fs::read_to_string(FALKORDB_COMPOSE)
        .await
        .context("read runtime FalkorDB follower Compose template")?;
    if runtime_compose != include_str!("../../../deploy/docker-compose.falkordb-follower.yml") {
        bail!("runtime FalkorDB follower Compose differs from the reviewed binary contract");
    }
    let status = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("docker")
            .args(["compose", "-f", FALKORDB_COMPOSE, "config", "--quiet"])
            .env("FALKORDB_PRIMARY_HOST", &plan.material.primary_ip)
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("FalkorDB follower Compose validation timed out")?
    .context("validate FalkorDB follower Compose")?;
    if !status.success() {
        bail!("FalkorDB follower Compose validation failed");
    }
    Ok(())
}

async fn start_replica(plan: &Plan) -> Result<()> {
    validate_compose(plan).await?;
    let status = tokio::time::timeout(
        COMPOSE_TIMEOUT,
        tokio::process::Command::new("docker")
            .args([
                "compose",
                "-f",
                FALKORDB_COMPOSE,
                "up",
                "-d",
                "--pull",
                "never",
            ])
            .env("FALKORDB_PRIMARY_HOST", &plan.material.primary_ip)
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("FalkorDB follower Compose timed out")?
    .context("start FalkorDB follower Compose")?;
    if !status.success() {
        bail!("FalkorDB follower start failed; any durable volume was preserved");
    }
    Ok(())
}

async fn wait_exact_healthy(plan: &Plan) -> Result<()> {
    let target = plan.target();
    let deadline = std::time::Instant::now() + Duration::from_secs(3 * 60);
    loop {
        if let Ok((_, TargetState::ExactHealthy { .. })) =
            attest_target(&target, &plan.material.primary_ip).await
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "FalkorDB container did not become exactly attested and healthy within 3 minutes"
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn replica_identity(
    info: &BTreeMap<String, String>,
    plan: &Plan,
    primary_offset: i64,
) -> Result<i64> {
    if info.get("role").map(String::as_str) != Some("slave")
        || info.get("master_host").map(String::as_str) != Some(plan.material.primary_ip.as_str())
        || info.get("master_port").map(String::as_str) != Some("63379")
        || info.get("master_link_status").map(String::as_str) != Some("up")
        || info.get("master_sync_in_progress").map(String::as_str) != Some("0")
        || info.get("master_replid") != Some(&plan.material.primary_replid)
    {
        bail!("FalkorDB replica INFO replication identity is not exact and linked");
    }
    let offset = info
        .get("slave_repl_offset")
        .or_else(|| info.get("master_repl_offset"))
        .context("FalkorDB replica has no replication offset")?
        .parse::<i64>()
        .context("parse FalkorDB replica offset")?;
    let lag = primary_offset.saturating_sub(offset).max(0);
    if lag > MAX_REPLICA_LAG_BYTES {
        bail!("FalkorDB replica lag is {lag} bytes, above the safe limit");
    }
    Ok(lag)
}

async fn prove_safe_config(url: &str) -> Result<()> {
    for (key, expected) in [
        ("replica-read-only", "yes"),
        ("replica-serve-stale-data", "no"),
        ("appendonly", "yes"),
        ("appendfsync", "everysec"),
        ("protected-mode", "yes"),
        ("port", "63380"),
    ] {
        let actual = redis_config_value(url, key).await?;
        if actual != expected {
            bail!("FalkorDB replica config {key}={actual:?}, expected {expected:?}");
        }
    }
    if redis_config_value(url, "bind").await? != "127.0.0.1" {
        bail!("FalkorDB replica is not bound exactly to IPv4 loopback");
    }
    Ok(())
}

async fn expect_readonly_error(
    connection: &mut redis::aio::MultiplexedConnection,
    command: &mut redis::Cmd,
    label: &str,
) -> Result<()> {
    let result = tokio::time::timeout(IO_TIMEOUT, command.query_async::<redis::Value>(connection))
        .await
        .with_context(|| format!("{label} write-rejection proof timed out"))?;
    match result {
        Err(error) if error.to_string().to_ascii_uppercase().contains("READONLY") => Ok(()),
        Err(error) => bail!("{label} failed for a reason other than READONLY: {error}"),
        Ok(_) => bail!("{label} unexpectedly succeeded on the read-only replica"),
    }
}

async fn prove_write_rejection(url: &str, graph: &str) -> Result<()> {
    let token = Uuid::new_v4().simple().to_string();
    let key = format!("__ff_falkor_replica_write_probe:{token}");
    let mut connection = redis_connection(url).await?;
    let mut set = redis::cmd("SET");
    set.arg(&key).arg("must-not-exist");
    expect_readonly_error(&mut connection, &mut set, "SET").await?;
    let value: redis::Value = tokio::time::timeout(
        IO_TIMEOUT,
        redis::cmd("GET").arg(&key).query_async(&mut connection),
    )
    .await
    .context("GET write-probe verification timed out")?
    .context("verify rejected SET")?;
    if value != redis::Value::Nil {
        bail!("rejected SET probe key nevertheless exists");
    }

    let mutation = format!("CREATE (:__FF_REPLICA_WRITE_PROBE {{token:'{token}'}})");
    let mut graph_write = redis::cmd("GRAPH.QUERY");
    graph_write.arg(graph).arg(&mutation);
    expect_readonly_error(&mut connection, &mut graph_write, "GRAPH.QUERY").await?;
    let verify = format!("MATCH (n:__FF_REPLICA_WRITE_PROBE {{token:'{token}'}}) RETURN count(n)");
    if graph_count(&graph_query(url, graph, &verify).await?)? != 0 {
        bail!("rejected GRAPH.QUERY probe nevertheless created data");
    }
    Ok(())
}

async fn prove_replica_once(plan: &Plan, primary_url: &str, replica_url: &str) -> Result<i64> {
    let primary_before = redis_info(primary_url, "replication").await?;
    let (replid, primary_offset) = primary_identity(&primary_before)?;
    if replid != plan.material.primary_replid {
        bail!("Priya FalkorDB replication identity changed; plan is stale");
    }
    let replica_before = redis_info(replica_url, "replication").await?;
    let lag = replica_identity(&replica_before, plan, primary_offset)?;
    let server = redis_info(replica_url, "server").await?;
    if server.get("redis_version").map(String::as_str)
        != Some(plan.material.primary_version.as_str())
        || graph_module_version(replica_url).await? != plan.material.graph_module_version
        || dbsize(primary_url).await? != plan.material.primary_dbsize
        || dbsize(replica_url).await? != plan.material.primary_dbsize
    {
        bail!("FalkorDB replica server/module/key identity does not match the plan");
    }
    let primary_graphs = graph_inventory(primary_url).await?;
    if primary_graphs != plan.material.graphs {
        bail!("Priya complete graph inventory changed; generate a fresh plan");
    }
    let replica_graphs = graph_inventory(replica_url).await?;
    if replica_graphs != plan.material.graphs {
        bail!("FalkorDB replica complete graph inventory differs from canonical Priya");
    }
    let primary_after = redis_info(primary_url, "replication").await?;
    let (replid_after, primary_offset_after) = primary_identity(&primary_after)?;
    let replica_after = redis_info(replica_url, "replication").await?;
    let lag_after = replica_identity(&replica_after, plan, primary_offset_after)?;
    if replid_after != replid || primary_offset_after != primary_offset || lag_after != lag {
        bail!(
            "FalkorDB replication advanced during the complete graph proof; retry from a quiescent snapshot"
        );
    }
    prove_safe_config(replica_url).await?;
    let graph = plan
        .material
        .graphs
        .keys()
        .next()
        .context("plan has no graph for write-rejection proof")?;
    prove_write_rejection(replica_url, graph).await?;
    Ok(lag)
}

async fn prove_replica(plan: &Plan, primary_probe: &AuthorizedPrimaryProbe) -> Result<i64> {
    primary_probe.validate_for_plan(plan)?;
    let replica_url = replica_url();
    let deadline = std::time::Instant::now() + REPLICA_READY_TIMEOUT;
    loop {
        if let Ok(lag) = prove_replica_once(plan, primary_probe.url(), &replica_url).await {
            return Ok(lag);
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "FalkorDB replica did not reach exact INFO, complete graph, and write-rejection proof within 10 minutes"
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn restart_and_reprove(plan: &Plan, primary_probe: &AuthorizedPrimaryProbe) -> Result<i64> {
    primary_probe.validate_for_plan(plan)?;
    let output = docker_output(&["restart", FALKORDB_CONTAINER]).await?;
    if !output.status.success() {
        bail!("exact FalkorDB replica container restart failed");
    }
    wait_exact_healthy(plan).await?;
    prove_replica(plan, primary_probe).await
}

async fn register_topology(pool: &sqlx::PgPool, plan: &Plan, lag_bytes: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('forgefleet-falkordb-primary-authority'))")
        .execute(&mut *tx)
        .await?;
    let active_urls: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM fleet_secrets
          WHERE key='falkordb.url' AND disabled_reason IS NULL FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    if active_urls.len() != 1
        || normalized_url(&active_urls[0], &plan.material.primary_ip)?
            != format!("redis://{}:{PRIMARY_PORT}", plan.material.primary_ip)
    {
        bail!("FalkorDB URL authority changed before topology registration");
    }
    let primary_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT computer_id FROM database_replicas
          WHERE database_kind='falkordb' AND role='primary' FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    if primary_ids.len() > 1
        || primary_ids
            .first()
            .is_some_and(|computer_id| *computer_id != plan.material.primary_id)
    {
        bail!("FalkorDB topology authority changed before registration");
    }
    let target_role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM database_replicas
          WHERE computer_id=$1 AND database_kind='falkordb' FOR UPDATE",
    )
    .bind(plan.material.target_id)
    .fetch_optional(&mut *tx)
    .await?;
    if target_role.as_deref().is_some_and(|role| role != "replica") {
        bail!("target acquired an incompatible FalkorDB topology role before registration");
    }
    let backup_valid: bool = sqlx::query_scalar(
        "SELECT EXISTS(
           SELECT 1 FROM backups WHERE id=$1 AND database_kind='falkordb'
             AND source_computer_id=$2 AND checksum_sha256=$3
             AND verified_restorable_at IS NOT NULL)",
    )
    .bind(plan.material.backup.backup_id)
    .bind(plan.material.primary_id)
    .bind(&plan.material.backup.checksum_sha256)
    .fetch_one(&mut *tx)
    .await?;
    if !backup_valid {
        bail!("exact FalkorDB backup evidence changed before registration");
    }
    sqlx::query(UPSERT_PRIMARY_SQL)
        .bind(plan.material.primary_id)
        .bind(plan.material.backup.backup_id)
        .bind(format!(
            "authority=fleet_secrets:falkordb.url;plan={};endpoint={}:{};redis_version={};graph_module={};automatic_failover=disabled;read_routing=disabled",
            plan.id(),
            plan.material.primary_ip,
            PRIMARY_PORT,
            plan.material.primary_version,
            plan.material.graph_module_version,
        ))
        .execute(&mut *tx)
        .await?;
    sqlx::query(UPSERT_REPLICA_SQL)
        .bind(plan.material.target_id)
        .bind(lag_bytes)
        .bind(plan.material.backup.backup_id)
        .bind(format!(
            "primary={};plan={};image={};primary_image_id={};target_image_id={};restore_image_id={};primary_repo_digest={};target_repo_digest={};backup={};drill={};endpoint=127.0.0.1:{};read_only=yes;rootfs_read_only=yes;cap_drop=ALL;no_new_privileges=yes;stale_data=no;automatic_failover=disabled;read_routing=disabled",
            plan.material.primary_name,
            plan.id(),
            FALKORDB_IMAGE,
            plan.material.primary_image_id,
            plan.material.target_image_id,
            plan.material.backup.restore_image_id,
            FALKORDB_IMAGE,
            FALKORDB_IMAGE,
            plan.material.backup.backup_id,
            plan.material.backup.drill_id,
            REPLICA_PORT,
        ))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn local_apply_inner(pool: &sqlx::PgPool, plan: &Plan) -> Result<()> {
    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
    if !me.eq_ignore_ascii_case(&plan.material.target_name) {
        bail!(
            "local FalkorDB apply must run on '{}' (running on '{me}')",
            plan.material.target_name
        );
    }
    let primary = resolve_computer(pool, &plan.material.primary_name).await?;
    let target = resolve_computer(pool, &plan.material.target_name).await?;
    let canonical_authority = authority_url(pool, &primary).await?;
    let first_primary_probe =
        acquire_authorized_primary_probe(&canonical_authority, &primary, &target).await?;
    first_primary_probe.validate_for_plan(plan)?;
    first_primary_probe.require_target_direct()?;
    let primary_image = attest_image(&primary).await?;
    validate_primary_plan_image(plan, &primary_image)?;
    let (attestation, state) = attest_target(&target, &primary.ip).await?;
    validate_plan_image(plan, &attestation)?;
    let current_primary_used_memory = primary_used_memory(first_primary_probe.url()).await?;
    validate_target_capacity(&attestation, current_primary_used_memory)?;
    match state {
        TargetState::Absent => {
            start_replica(plan).await?;
            wait_exact_healthy(plan).await?;
        }
        TargetState::ExactHealthy { .. } => {
            // An exact, healthy deployment is the only retry state accepted.
        }
    }
    let first_lag = prove_replica(plan, &first_primary_probe).await?;
    first_primary_probe.close().await;

    let restart_primary_probe =
        acquire_authorized_primary_probe(&canonical_authority, &primary, &target).await?;
    restart_primary_probe.validate_for_plan(plan)?;
    restart_primary_probe.require_target_direct()?;
    let second_lag = restart_and_reprove(plan, &restart_primary_probe).await?;
    restart_primary_probe.close().await;
    let (final_attestation, final_state) = attest_target(&target, &primary.ip).await?;
    validate_plan_image(plan, &final_attestation)?;
    if !matches!(final_state, TargetState::ExactHealthy { .. }) {
        bail!("FalkorDB target stopped being exact-healthy before topology registration");
    }
    let final_primary_probe =
        acquire_authorized_primary_probe(&canonical_authority, &primary, &target).await?;
    final_primary_probe.validate_for_plan(plan)?;
    final_primary_probe.require_target_direct()?;
    let final_lag = prove_replica(plan, &final_primary_probe).await?;
    final_primary_probe.close().await;

    let registration_primary_probe =
        acquire_authorized_primary_probe(&canonical_authority, &primary, &target).await?;
    registration_primary_probe.validate_for_plan(plan)?;
    registration_primary_probe.require_target_direct()?;
    let registration_used_memory = primary_used_memory(registration_primary_probe.url()).await?;
    let (registration_attestation, registration_state) =
        attest_target(&target, &primary.ip).await?;
    validate_plan_image(plan, &registration_attestation)?;
    validate_target_capacity(&registration_attestation, registration_used_memory)?;
    if !matches!(registration_state, TargetState::ExactHealthy { .. }) {
        bail!("FalkorDB target stopped being exact-healthy before topology registration");
    }
    registration_primary_probe.close().await;
    register_topology(pool, plan, first_lag.max(second_lag).max(final_lag)).await
}

async fn local_apply(pool: &sqlx::PgPool, plan: &Plan) -> Result<()> {
    let key = format!("falkordb-replica:{}", plan.material.target_id);
    let mut lock = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock(hashtext($1))")
        .bind(&key)
        .execute(&mut *lock)
        .await?;
    let outcome = local_apply_inner(pool, plan).await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&key)
        .execute(&mut *lock)
        .await;
    if let Err(error) = unlock {
        return Err(error).context("release FalkorDB target lifecycle lock");
    }
    outcome
}

async fn read_target_attestation(target: &Computer) -> Result<TargetAttestation> {
    let output = run_on_node(target, &target_attestation_script(), SSH_TIMEOUT).await?;
    if !output.status.success() {
        bail!(
            "target Docker attestation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_target_attestation(&String::from_utf8_lossy(&output.stdout))
}

fn decommission_proof(target: &Computer, primary: &Computer, image_id: &str) -> String {
    sha256(
        format!(
            "falkordb-decommission-v2\0{}\0{}\0{}\0{}\0{}\0{}",
            target.id, target.name, primary.id, FALKORDB_IMAGE, image_id, FALKORDB_VOLUME,
        )
        .as_bytes(),
    )
}

fn local_decommission_command(target: &Computer, primary: &Computer, proof: &str) -> String {
    format!(
        "cd \"$HOME/projects/forge-fleet\" && \"$HOME/.local/bin/ff\" fleet db falkordb-replica local-decommission --to {} --primary {} --proof {}",
        shell_escape_single(&target.name),
        shell_escape_single(&primary.name),
        shell_escape_single(proof),
    )
}

async fn topology_role(pool: &sqlx::PgPool, target_id: Uuid) -> Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT role FROM database_replicas WHERE computer_id=$1 AND database_kind='falkordb'",
    )
    .bind(target_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn decommission_preflight(
    pool: &sqlx::PgPool,
    target: &Computer,
    primary: &Computer,
) -> Result<TargetAttestation> {
    match topology_role(pool, target.id).await? {
        Some(role) if role == "replica" => {}
        Some(role) => bail!("refusing to decommission incompatible FalkorDB role {role:?}"),
        None => bail!("target has no registered FalkorDB replica topology row"),
    }
    let attestation = read_target_attestation(target).await?;
    match &attestation.container {
        Some(fingerprint) => {
            validate_existing_container(
                fingerprint,
                &attestation.image_id,
                &attestation.image_user,
                &primary.ip,
            )?;
            if !attestation.volume_present {
                bail!("exact FalkorDB replica has no durable volume to preserve");
            }
        }
        None if attestation.volume_present => {
            // A prior target-side attempt may already have removed the exact
            // container. The preserved volume makes this safe to resume.
        }
        None => bail!("FalkorDB replica container and preservation volume are both absent"),
    }
    Ok(attestation)
}

async fn local_decommission(
    pool: &sqlx::PgPool,
    target: &Computer,
    primary: &Computer,
    proof: &str,
) -> Result<()> {
    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
    if !me.eq_ignore_ascii_case(&target.name) {
        bail!(
            "local decommission must run on canonical target {}",
            target.name
        );
    }
    let key = format!("falkordb-replica:{}", target.id);
    let mut lock = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock(hashtext($1))")
        .bind(&key)
        .execute(&mut *lock)
        .await?;
    let outcome = async {
        let role = topology_role(pool, target.id).await?;
        let attestation = read_target_attestation(target).await?;
        if proof != decommission_proof(target, primary, &attestation.image_id) {
            bail!("FalkorDB decommission proof is stale or mismatched");
        }
        match (&role, &attestation.container) {
            (Some(role), _) if role != "replica" => {
                bail!("FalkorDB topology role changed before decommission")
            }
            (None, Some(_)) => {
                bail!("unregistered FalkorDB container requires manual audit, not hidden cleanup")
            }
            (_, Some(fingerprint)) => {
                validate_existing_container(
                    fingerprint,
                    &attestation.image_id,
                    &attestation.image_user,
                    &primary.ip,
                )?;
                if !attestation.volume_present {
                    bail!("FalkorDB preservation volume disappeared before decommission");
                }
                let removed =
                    docker_output(&["container", "rm", "--force", FALKORDB_CONTAINER]).await?;
                if !removed.status.success() {
                    bail!("remove exact FalkorDB replica container failed");
                }
            }
            (Some(_), None) if !attestation.volume_present => {
                bail!("FalkorDB volume is absent; refusing to erase topology evidence")
            }
            _ => {}
        }
        let after = read_target_attestation(target).await?;
        if after.container.is_some() || !after.volume_present {
            bail!("decommission did not leave exactly an absent container and preserved volume");
        }

        let mut tx = pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtext('forgefleet-falkordb-primary-authority'))",
        )
        .execute(&mut *tx)
        .await?;
        let primary_rows: Vec<Uuid> = sqlx::query_scalar(
            "SELECT computer_id FROM database_replicas
              WHERE database_kind='falkordb' AND role='primary' FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await?;
        if primary_rows.len() != 1 || primary_rows[0] != primary.id {
            bail!("Priya FalkorDB authority is ambiguous; preserving topology rows");
        }
        sqlx::query(
            "DELETE FROM database_replicas
              WHERE computer_id=$1 AND database_kind='falkordb' AND role='replica'",
        )
        .bind(target.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&key)
        .execute(&mut *lock)
        .await;
    if let Err(error) = unlock {
        return Err(error).context("release FalkorDB decommission lock");
    }
    outcome
}

async fn purge_evidence(
    pool: &sqlx::PgPool,
    target_name: &str,
    primary_name: &str,
) -> Result<PurgeEvidence> {
    reject_vinny(target_name)?;
    let target = resolve_computer(pool, target_name).await?;
    let primary = resolve_computer(pool, primary_name).await?;
    if !primary.name.eq_ignore_ascii_case(PRIMARY_NAME) {
        bail!("FalkorDB purge authority must be canonical Priya");
    }
    if topology_role(pool, target.id).await?.is_some() {
        bail!("remove the FalkorDB topology row with decommission before volume purge");
    }
    let attestation = read_target_attestation(&target).await?;
    if attestation.container.is_some() || !attestation.volume_present {
        bail!("purge proof requires an absent container and exactly one preserved volume");
    }
    let primary_image = attest_image(&primary).await?;
    let backup = backup_evidence(pool, &primary).await?;
    Ok(PurgeEvidence {
        target,
        primary,
        primary_image,
        backup,
    })
}

fn local_purge_command(evidence: &PurgeEvidence) -> String {
    format!(
        "cd \"$HOME/projects/forge-fleet\" && \"$HOME/.local/bin/ff\" fleet db falkordb-replica local-purge-volume --to {} --primary {} --proof {}",
        shell_escape_single(&evidence.target.name),
        shell_escape_single(&evidence.primary.name),
        shell_escape_single(&evidence.proof()),
    )
}

fn require_purge_confirmation(yes: bool) -> Result<()> {
    if !yes {
        bail!(
            "PREVIEW: permanent FalkorDB volume purge was not executed; pass --yes with the exact status proof (preview intentionally exits nonzero)"
        );
    }
    Ok(())
}

async fn local_purge_volume(pool: &sqlx::PgPool, evidence: &PurgeEvidence) -> Result<()> {
    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
    if !me.eq_ignore_ascii_case(&evidence.target.name) {
        bail!(
            "local purge must run on canonical target {}",
            evidence.target.name
        );
    }
    let key = format!("falkordb-replica:{}", evidence.target.id);
    let mut lock = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock(hashtext($1))")
        .bind(&key)
        .execute(&mut *lock)
        .await?;
    let outcome = async {
        if topology_role(pool, evidence.target.id).await?.is_some() {
            bail!("FalkorDB topology reappeared; refusing volume purge");
        }
        let current_image = attest_image(&evidence.primary).await?;
        if current_image != evidence.primary_image {
            bail!("Priya pinned FalkorDB image resolution changed; issue a fresh purge proof");
        }
        let current_backup = backup_evidence(pool, &evidence.primary).await?;
        if current_backup != evidence.backup {
            bail!("FalkorDB backup/receipt proof changed; issue a fresh purge proof");
        }
        let before = read_target_attestation(&evidence.target).await?;
        if before.container.is_some() || !before.volume_present {
            bail!("FalkorDB purge precondition changed");
        }
        let removed = docker_output(&["volume", "rm", FALKORDB_VOLUME]).await?;
        if !removed.status.success() {
            bail!("exact preserved FalkorDB volume purge failed");
        }
        let after = read_target_attestation(&evidence.target).await?;
        if after.container.is_some() || after.volume_present {
            bail!("FalkorDB volume still exists after purge");
        }
        Ok(())
    }
    .await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&key)
        .execute(&mut *lock)
        .await;
    if let Err(error) = unlock {
        return Err(error).context("release FalkorDB purge lock");
    }
    outcome
}

async fn prove_replica_via_bounded_probes(
    pool: &sqlx::PgPool,
    plan: &Plan,
    primary: &Computer,
    target: &Computer,
) -> Result<i64> {
    let canonical_authority = authority_url(pool, primary).await?;
    let primary_probe =
        acquire_authorized_primary_probe(&canonical_authority, primary, target).await?;
    primary_probe.validate_for_plan(plan)?;
    let replica_probe = match node_loopback_probe(target, REPLICA_PORT, "target FalkorDB").await {
        Ok(probe) => probe,
        Err(error) => {
            primary_probe.close().await;
            return Err(error);
        }
    };
    let result = prove_replica_once(plan, primary_probe.url(), &replica_probe.url).await;
    replica_probe.close().await;
    primary_probe.close().await;
    result
}

fn validate_status_topology(
    role: &str,
    status: &str,
    registered_backup: Option<Uuid>,
    plan: &Plan,
) -> Result<()> {
    if role != "replica" || status != "running" {
        bail!("BLOCKED: FalkorDB topology row is not exact replica/running");
    }
    if registered_backup != Some(plan.material.backup.backup_id) {
        bail!("BLOCKED: FalkorDB topology backup identity is stale or missing");
    }
    let TargetState::ExactHealthy { image_id } = &plan.material.target_state else {
        bail!("BLOCKED: registered FalkorDB replica container is not exact-healthy");
    };
    if image_id != &plan.material.target_image_id {
        bail!("BLOCKED: running FalkorDB image differs from daemon-resolved plan image");
    }
    Ok(())
}

fn preserved_volume_status_result() -> Result<()> {
    bail!(
        "BLOCKED: preserved FalkorDB volume is not an exact-healthy replica; the emitted purge proof is a non-mutating preview"
    )
}

async fn show_status(pool: &sqlx::PgPool, to: &str, primary_name: &str) -> Result<()> {
    reject_vinny(to)?;
    let target = resolve_computer(pool, to).await?;
    let primary = resolve_computer(pool, primary_name).await?;
    if !primary.name.eq_ignore_ascii_case(PRIMARY_NAME) {
        bail!("FalkorDB status authority must be canonical Priya");
    }
    let topology = sqlx::query(
        "SELECT role,status,bootstrapped_from_backup_id
           FROM database_replicas WHERE computer_id=$1 AND database_kind='falkordb'",
    )
    .bind(target.id)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = topology {
        let role: String = row.get("role");
        let status: String = row.get("status");
        let registered_backup = row.try_get::<Option<Uuid>, _>("bootstrapped_from_backup_id")?;
        let plan = build_plan(pool, to, primary_name).await?;
        validate_status_topology(&role, &status, registered_backup, &plan)?;
        let fresh_lag = prove_replica_via_bounded_probes(pool, &plan, &primary, &target)
            .await
            .context("BLOCKED: fresh live FalkorDB replica proof failed")?;
        let (after, after_state) = attest_target(&target, &primary.ip).await?;
        validate_plan_image(&plan, &after)?;
        if !matches!(after_state, TargetState::ExactHealthy { .. }) {
            bail!("BLOCKED: FalkorDB target changed after fresh live proof");
        }
        attest_firewall(&primary, &target).await?;
        println!(
            "{CYAN}FalkorDB replica status (fresh live proof; Priya authoritative){RESET}\n  target: {} ({})\n  container: exact-healthy\n  durable volume: present\n  topology: role=replica status=running backup={}\n  fresh lag_bytes: {}\n  image: {}\n  complete graphs: {}\n  automatic failover: disabled\n  read routing: disabled",
            target.name,
            target.id,
            registered_backup.expect("validated registered backup identity"),
            fresh_lag,
            plan.material.target_image_id,
            plan.material.graphs.len(),
        );
    } else {
        let attestation = read_target_attestation(&target).await?;
        if attestation.container.is_some() {
            bail!("BLOCKED: FalkorDB container exists without a topology row");
        }
        if attestation.replica_port_listeners != 0 || attestation.replica_port_non_loopback != 0 {
            bail!("BLOCKED: topology-absent target still has FalkorDB listener evidence");
        }
        if attestation.volume_present {
            let evidence = purge_evidence(pool, to, primary_name)
                .await
                .context("BLOCKED: preserved-volume purge proof is unavailable")?;
            println!(
                "{CYAN}FalkorDB replica status (read-only; Priya authoritative){RESET}\n  target: {} ({})\n  container: decommissioned-volume-preserved\n  durable volume: present\n  topology: absent\n  purge-proof: {}\n  purge is permanent and still requires --yes\n  automatic failover: disabled\n  read routing: disabled",
                target.name,
                target.id,
                evidence.proof(),
            );
            preserved_volume_status_result()?;
        } else {
            println!(
                "{CYAN}FalkorDB replica status (read-only; Priya authoritative){RESET}\n  target: {} ({})\n  container: absent\n  durable volume: absent\n  topology: absent\n  automatic failover: disabled\n  read routing: disabled",
                target.name, target.id,
            );
        }
    }
    Ok(())
}

pub async fn handle(pool: &sqlx::PgPool, command: FleetDbFalkordbReplicaCommand) -> Result<()> {
    match command {
        FleetDbFalkordbReplicaCommand::Plan { to, primary } => {
            let plan = build_plan(pool, &to, &primary).await?;
            println!(
                "{CYAN}FalkorDB replica plan (read-only; no promotion/failover/read routing){RESET}\n  target: {} ({})\n  primary authority: {} ({}:{PRIMARY_PORT})\n  replica endpoint: 127.0.0.1:{REPLICA_PORT}\n  immutable image: {FALKORDB_IMAGE}\n  primary daemon image id: {}\n  target daemon image id: {}\n  graphs: {} (complete deterministic hashes)\n  restore-proven distributed backup: {} (drill {})\n  source firewall: exact target allow + IPv4 deny + IPv6 INPUT/FORWARD deny + Docker-persistent unit\n  target state: {:?}\n  plan-id: {}\n\nApply with:\n  ff fleet db falkordb-replica apply --to {} --primary {} --plan-id {} --yes",
                plan.material.target_name,
                plan.material.target_id,
                plan.material.primary_name,
                plan.material.primary_ip,
                plan.material.primary_image_id,
                plan.material.target_image_id,
                plan.material.graphs.len(),
                plan.material.backup.backup_id,
                plan.material.backup.drill_id,
                plan.material.target_state,
                plan.id(),
                plan.material.target_name,
                plan.material.primary_name,
                plan.id(),
            );
        }
        FleetDbFalkordbReplicaCommand::Apply {
            to,
            primary,
            plan_id,
            yes,
        } => {
            if !yes {
                bail!("FalkorDB replica apply requires --yes; no changes made");
            }
            let plan = build_plan(pool, &to, &primary).await?;
            validate_plan_id(&plan, &plan_id)?;
            let target = plan.target();
            let task = enqueue_lifecycle_action(
                pool,
                &target,
                "apply",
                &plan.id(),
                local_apply_command(&plan),
            )
            .await?;
            println!(
                "{GREEN}✓{RESET} queued exact FalkorDB replica apply on '{}' as deferred task {}; Priya remains authoritative",
                target.name, task
            );
        }
        FleetDbFalkordbReplicaCommand::Status { to, primary } => {
            show_status(pool, &to, &primary).await?;
        }
        FleetDbFalkordbReplicaCommand::Decommission {
            to,
            primary,
            preserve_volume,
            yes,
        } => {
            if !yes || !preserve_volume {
                bail!("decommission requires both --preserve-volume and --yes; no changes made");
            }
            reject_vinny(&to)?;
            let target = resolve_computer(pool, &to).await?;
            let primary = resolve_computer(pool, &primary).await?;
            if !primary.name.eq_ignore_ascii_case(PRIMARY_NAME) {
                bail!("FalkorDB decommission authority must be canonical Priya");
            }
            let attestation = decommission_preflight(pool, &target, &primary).await?;
            let proof = decommission_proof(&target, &primary, &attestation.image_id);
            let task = enqueue_lifecycle_action(
                pool,
                &target,
                "decommission",
                &proof,
                local_decommission_command(&target, &primary, &proof),
            )
            .await?;
            println!(
                "{GREEN}✓{RESET} queued non-destructive FalkorDB decommission as task {task}; the exact volume will be preserved and Priya will not be changed"
            );
        }
        FleetDbFalkordbReplicaCommand::PurgeVolume {
            to,
            primary,
            proof,
            yes,
        } => {
            require_purge_confirmation(yes)?;
            let evidence = purge_evidence(pool, &to, &primary).await?;
            if evidence.proof() != proof {
                bail!("FalkorDB purge proof is stale or mismatched; run status again");
            }
            let task = enqueue_lifecycle_action(
                pool,
                &evidence.target,
                "purge-volume",
                &evidence.proof(),
                local_purge_command(&evidence),
            )
            .await?;
            println!("{YELLOW}queued permanent exact-volume purge as task {task}{RESET}");
        }
        FleetDbFalkordbReplicaCommand::LocalApply {
            to,
            primary,
            plan_id,
        } => {
            let plan = build_plan(pool, &to, &primary).await?;
            validate_plan_id(&plan, &plan_id)?;
            println!(
                "{YELLOW}Applying FalkorDB replica plan {} on {}; Priya remains authoritative{RESET}",
                plan.id(),
                plan.material.target_name
            );
            local_apply(pool, &plan).await?;
            println!(
                "{GREEN}✓ FalkorDB replica passed exact INFO/graph/write/restart proofs and was registered; automatic failover and read routing remain disabled{RESET}"
            );
        }
        FleetDbFalkordbReplicaCommand::LocalDecommission { to, primary, proof } => {
            reject_vinny(&to)?;
            let target = resolve_computer(pool, &to).await?;
            let primary = resolve_computer(pool, &primary).await?;
            if !primary.name.eq_ignore_ascii_case(PRIMARY_NAME) {
                bail!("FalkorDB decommission authority must be canonical Priya");
            }
            local_decommission(pool, &target, &primary, &proof).await?;
            println!(
                "{GREEN}✓ exact FalkorDB replica container and target topology row removed; durable volume preserved{RESET}"
            );
        }
        FleetDbFalkordbReplicaCommand::LocalPurgeVolume { to, primary, proof } => {
            let evidence = purge_evidence(pool, &to, &primary).await?;
            if evidence.proof() != proof {
                bail!("FalkorDB purge proof changed before target execution");
            }
            local_purge_volume(pool, &evidence).await?;
            println!("{GREEN}✓ exact preserved FalkorDB replica volume purged{RESET}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computer(id: u128, name: &str, ip: &str) -> Computer {
        Computer {
            id: Uuid::from_u128(id),
            name: name.into(),
            ip: ip.into(),
            ssh_user: name.into(),
            ssh_port: 22,
            status: "online".into(),
            fresh: true,
            os_family: "linux-ubuntu".into(),
            reservation_state: "available".into(),
            total_ram_gb: 32,
            total_disk_gb: 1024,
            failure_domain: None,
        }
    }

    fn backup() -> BackupEvidence {
        BackupEvidence {
            backup_id: Uuid::from_u128(10),
            file_name: "falkordb-20260805T003449Z.tar.zst.age".into(),
            checksum_sha256: "a".repeat(64),
            size_bytes: 7040,
            drill_id: Uuid::from_u128(11),
            restore_image_id: format!("sha256:{}", "9".repeat(64)),
            created_at: DateTime::parse_from_rfc3339("2026-08-05T00:34:49Z")
                .unwrap()
                .with_timezone(&Utc),
            verified_restorable_at: DateTime::parse_from_rfc3339("2026-08-05T01:24:28Z")
                .unwrap()
                .with_timezone(&Utc),
            distributed_to: vec![Uuid::from_u128(12), Uuid::from_u128(13)],
        }
    }

    fn plan() -> Plan {
        let mut graphs = BTreeMap::new();
        graphs.insert(
            "cortex".into(),
            GraphEvidence {
                nodes: 10,
                relationships: 2,
                node_full_sha256: "b".repeat(64),
                relationship_full_sha256: "c".repeat(64),
            },
        );
        Plan {
            material: PlanMaterial {
                version: "falkordb-replica-plan-v4",
                target_id: Uuid::from_u128(1),
                target_name: "sophie".into(),
                target_ip: "192.168.5.103".into(),
                target_ssh_user: "sophie".into(),
                target_ssh_port: 22,
                target_failure_domain: Some("rack-b".into()),
                primary_id: Uuid::from_u128(2),
                primary_name: "priya".into(),
                primary_ip: "192.168.5.104".into(),
                primary_replid: "0123456789abcdef0123456789abcdef01234567".into(),
                primary_version: EXPECTED_REDIS_VERSION.into(),
                graph_module_version: EXPECTED_GRAPH_MODULE_VERSION,
                primary_dbsize: 6,
                graphs,
                backup: backup(),
                target_state: TargetState::Absent,
                image: FALKORDB_IMAGE,
                primary_image_id: format!("sha256:{}", "d".repeat(64)),
                primary_image_repo_digests: vec![FALKORDB_IMAGE.into()],
                target_image_id: format!("sha256:{}", "e".repeat(64)),
                target_image_repo_digests: vec![FALKORDB_IMAGE.into()],
                image_user: String::new(),
                primary_port: PRIMARY_PORT,
                replica_port: REPLICA_PORT,
                firewall_policy: "forgefleet-falkordb-source-firewall-v1-four-rule",
                automatic_failover: false,
                read_routing: false,
            },
        }
    }

    fn firewall_evidence(target: &Computer, primary: &Computer) -> FirewallEvidence {
        FirewallEvidence {
            target_id: target.id,
            target_ip: target.ip.clone(),
            primary_id: primary.id,
            primary_ip: primary.ip.clone(),
            primary_port: PRIMARY_PORT,
            unit_enabled: true,
            unit_active: true,
            unit_result_success: true,
            docker_lifecycle_bound: true,
            ipv4_target_allow: true,
            ipv4_default_deny: true,
            ipv6_default_deny: true,
            ipv6_forward_default_deny: true,
        }
    }

    fn route_evidence(target: &Computer, primary: &Computer) -> TargetSourceRouteEvidence {
        TargetSourceRouteEvidence {
            target_id: target.id,
            target_ip: target.ip.clone(),
            source_ip: target.ip.clone(),
            primary_id: primary.id,
            primary_ip: primary.ip.clone(),
            primary_port: PRIMARY_PORT,
            tcp_reachable: true,
        }
    }

    #[test]
    fn authority_url_is_exact_and_credential_free() {
        assert_eq!(
            normalized_url("redis://192.168.5.104:63379", "192.168.5.104").unwrap(),
            "redis://192.168.5.104:63379"
        );
        for bad in [
            "redis://192.168.5.103:63379",
            "redis://192.168.5.104:6379",
            "redis://user:secret@192.168.5.104:63379",
            "rediss://192.168.5.104:63379",
            "redis://192.168.5.104:63379/1",
            "redis://192.168.5.104:63379?x=1",
        ] {
            assert!(normalized_url(bad, "192.168.5.104").is_err(), "{bad}");
        }
    }

    #[test]
    fn vinny_and_legacy_name_are_categorically_rejected() {
        assert!(reject_vinny("vinny").is_err());
        assert!(reject_vinny("VINNY").is_err());
        assert!(reject_vinny("taylor").is_err());
        assert!(reject_vinny("sophie").is_ok());
    }

    #[test]
    fn plan_id_is_stable_and_binds_graph_backup_and_target_state() {
        let mut plan = plan();
        let id = plan.id();
        assert_eq!(id, plan.id());
        plan.material.graphs.get_mut("cortex").unwrap().nodes += 1;
        assert_ne!(id, plan.id());
        let graph_changed = plan.id();
        plan.material.backup.backup_id = Uuid::from_u128(99);
        assert_ne!(graph_changed, plan.id());
        let backup_changed = plan.id();
        plan.material.target_state = TargetState::ExactHealthy {
            image_id: "sha256:image".into(),
        };
        assert_ne!(backup_changed, plan.id());
        let state_changed = plan.id();
        plan.material.primary_image_id = format!("sha256:{}", "f".repeat(64));
        assert_ne!(state_changed, plan.id());
        let primary_image_changed = plan.id();
        plan.material.target_image_id = format!("sha256:{}", "a".repeat(64));
        assert_ne!(primary_image_changed, plan.id());
        assert!(validate_plan_id(&plan, &plan.id()).is_ok());
        assert!(validate_plan_id(&plan, &state_changed).is_err());
    }

    #[test]
    fn plan_id_binds_version_and_each_cross_daemon_image_identity() {
        let original = plan();
        let id = original.id();

        let mut changed = original.clone();
        changed.material.version = "falkordb-replica-plan-v3";
        assert_ne!(id, changed.id());

        let mut changed = original.clone();
        changed.material.primary_image_id = format!("sha256:{}", "1".repeat(64));
        assert_ne!(id, changed.id());

        let mut changed = original.clone();
        changed.material.target_image_id = format!("sha256:{}", "2".repeat(64));
        assert_ne!(id, changed.id());

        let mut changed = original.clone();
        changed.material.primary_image_repo_digests = vec!["changed-primary".into()];
        assert_ne!(id, changed.id());

        let mut changed = original;
        changed.material.target_image_repo_digests = vec!["changed-target".into()];
        assert_ne!(id, changed.id());
    }

    fn target_attestation(plan: &Plan) -> TargetAttestation {
        TargetAttestation {
            docker_version: "29.6.0".into(),
            image_id: plan.material.target_image_id.clone(),
            image_repo_digests: plan.material.target_image_repo_digests.clone(),
            image_user: plan.material.image_user.clone(),
            container: None,
            volume_present: false,
            ram_bytes: 32 * 1024 * 1024 * 1024,
            disk_free_bytes: 100 * 1024 * 1024 * 1024,
            replica_port_listeners: 0,
            replica_port_non_loopback: 0,
        }
    }

    #[test]
    fn target_direct_primary_authorization_is_exact_and_evidence_bound() {
        let target = computer(1, "sophie", "192.168.5.103");
        let primary = computer(2, "priya", "192.168.5.104");
        let firewall = firewall_evidence(&target, &primary);
        let route = route_evidence(&target, &primary);
        let canonical = "redis://192.168.5.104:63379";
        let access = authorize_target_direct_primary(
            "sophie", &target, &primary, canonical, &firewall, &route,
        )
        .unwrap();
        assert_eq!(
            access,
            DirectPrimaryAccess {
                url: canonical.into(),
                target_id: target.id,
                target_ip: target.ip.clone(),
                primary_id: primary.id,
                primary_ip: primary.ip.clone(),
                primary_port: PRIMARY_PORT,
            }
        );
        assert!(authorize_target_direct_primary(
            "SOPHIE", &target, &primary, canonical, &firewall, &route,
        )
        .is_ok());
        for wrong in ["sophie-worker", "priya", "adele", ""] {
            assert!(authorize_target_direct_primary(
                wrong, &target, &primary, canonical, &firewall, &route,
            )
            .is_err());
        }

        for bad_authority in [
            "redis://127.0.0.1:63379",
            "redis://192.168.5.103:63379",
            "redis://192.168.5.104:6379",
            "redis://192.168.5.104:63380",
            "redis://user@192.168.5.104:63379",
            "rediss://192.168.5.104:63379",
            "redis://192.168.5.104:63379/1",
            "redis://192.168.5.104:63379?x=1",
            "redis://192.168.5.104:63379/#fragment",
        ] {
            assert!(authorize_target_direct_primary(
                "sophie",
                &target,
                &primary,
                bad_authority,
                &firewall,
                &route,
            )
            .is_err());
        }

        let mut changed = firewall.clone();
        changed.target_id = Uuid::from_u128(99);
        assert!(authorize_target_direct_primary(
            "sophie", &target, &primary, canonical, &changed, &route,
        )
        .is_err());
        let mut changed = firewall.clone();
        changed.primary_id = Uuid::from_u128(99);
        assert!(authorize_target_direct_primary(
            "sophie", &target, &primary, canonical, &changed, &route,
        )
        .is_err());
        let mut changed = firewall.clone();
        changed.primary_port = 6379;
        assert!(authorize_target_direct_primary(
            "sophie", &target, &primary, canonical, &changed, &route,
        )
        .is_err());
        let mut changed = firewall;
        changed.ipv4_target_allow = false;
        assert!(authorize_target_direct_primary(
            "sophie", &target, &primary, canonical, &changed, &route,
        )
        .is_err());

        let mut changed = route.clone();
        changed.source_ip = "192.168.5.102".into();
        assert!(authorize_target_direct_primary(
            "sophie",
            &target,
            &primary,
            canonical,
            &firewall_evidence(&target, &primary),
            &changed,
        )
        .is_err());
        let mut changed = route;
        changed.tcp_reachable = false;
        assert!(authorize_target_direct_primary(
            "sophie",
            &target,
            &primary,
            canonical,
            &firewall_evidence(&target, &primary),
            &changed,
        )
        .is_err());
    }

    #[test]
    fn primary_probe_mode_is_direct_only_on_exact_target() {
        let target = computer(1, "sophie", "192.168.5.103");
        assert_eq!(
            primary_proof_transport("sophie", &target),
            PrimaryProofTransport::AuthorizedTargetDirect
        );
        assert_eq!(
            primary_proof_transport("SOPHIE", &target),
            PrimaryProofTransport::AuthorizedTargetDirect
        );
        for worker in ["adele", "priya", "lily", ""] {
            assert_eq!(
                primary_proof_transport(worker, &target),
                PrimaryProofTransport::StrictSsh
            );
        }
    }

    #[test]
    fn authorized_primary_probe_cannot_cross_plan_identity() {
        let original = plan();
        let probe = AuthorizedPrimaryProbe {
            probe: PrimaryProbe {
                url: "redis://192.168.5.104:63379".into(),
                tunnel: None,
            },
            transport: PrimaryProofTransport::AuthorizedTargetDirect,
            target_id: original.material.target_id,
            target_ip: original.material.target_ip.clone(),
            primary_id: original.material.primary_id,
            primary_ip: original.material.primary_ip.clone(),
            primary_port: PRIMARY_PORT,
        };
        probe.validate_for_plan(&original).unwrap();

        let mut changed = original.clone();
        changed.material.target_id = Uuid::from_u128(99);
        assert!(probe.validate_for_plan(&changed).is_err());
        let mut changed = original.clone();
        changed.material.target_ip = "192.168.5.109".into();
        assert!(probe.validate_for_plan(&changed).is_err());
        let mut changed = original.clone();
        changed.material.primary_id = Uuid::from_u128(98);
        assert!(probe.validate_for_plan(&changed).is_err());
        let mut changed = original;
        changed.material.primary_port = 6379;
        assert!(probe.validate_for_plan(&changed).is_err());

        assert!(probe.require_target_direct().is_ok());
        let mut wrong_transport = probe;
        wrong_transport.transport = PrimaryProofTransport::StrictSsh;
        assert!(wrong_transport.require_target_direct().is_err());
    }

    #[test]
    fn strict_primary_tunnel_is_fail_closed_and_exact() {
        let primary = computer(2, "priya", "192.168.5.104");
        let args = strict_ssh_loopback_args(&primary, "127.0.0.1:45678:127.0.0.1:63379");
        for exact in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ConnectTimeout=5",
            "ExitOnForwardFailure=yes",
            "ServerAliveInterval=5",
            "ServerAliveCountMax=2",
        ] {
            assert!(args.iter().any(|arg| arg == exact), "missing {exact}");
        }
        assert!(args.windows(2).any(|pair| pair == ["-p", "22"]));
        assert!(args
            .windows(2)
            .any(|pair| { pair == ["-L", "127.0.0.1:45678:127.0.0.1:63379"] }));
        assert_eq!(args.last().map(String::as_str), Some("priya@192.168.5.104"));
    }

    #[test]
    fn capacity_is_live_not_plan_bound_and_enforces_floor_and_double_source() {
        let plan = plan();
        let plan_id = plan.id();
        let serialized = serde_json::to_value(&plan.material).unwrap();
        assert_eq!(serialized["version"], "falkordb-replica-plan-v4");
        assert!(serialized.get("primary_used_memory").is_none());

        let mut target = target_attestation(&plan);
        assert!(validate_target_capacity(&target, 0).is_err());
        validate_target_capacity(&target, 1).unwrap();
        validate_target_capacity(&target, 8 * 1024 * 1024 * 1024).unwrap();
        assert_eq!(plan_id, plan.id());

        target.ram_bytes = MIN_TARGET_BYTES;
        target.disk_free_bytes = MIN_TARGET_BYTES;
        validate_target_capacity(&target, 1).unwrap();
        target.ram_bytes -= 1;
        assert!(validate_target_capacity(&target, 1).is_err());
        target.ram_bytes = MIN_TARGET_BYTES;
        target.disk_free_bytes -= 1;
        assert!(validate_target_capacity(&target, 1).is_err());

        let used_memory = 4 * 1024 * 1024 * 1024;
        let required = used_memory * 2;
        target.ram_bytes = required;
        target.disk_free_bytes = required;
        validate_target_capacity(&target, used_memory).unwrap();
        target.ram_bytes -= 1;
        assert!(validate_target_capacity(&target, used_memory).is_err());
        target.ram_bytes = required;
        target.disk_free_bytes -= 1;
        assert!(validate_target_capacity(&target, used_memory).is_err());
    }

    #[test]
    fn cross_daemon_local_ids_pass_but_each_daemon_drift_fails() {
        let plan = plan();
        assert_ne!(
            plan.material.primary_image_id,
            plan.material.target_image_id
        );
        let primary = ImageEvidence {
            image_id: plan.material.primary_image_id.clone(),
            repo_digests: plan.material.primary_image_repo_digests.clone(),
            configured_user: plan.material.image_user.clone(),
        };
        let target = target_attestation(&plan);
        validate_image_evidence(&primary).unwrap();
        validate_image_evidence(&ImageEvidence {
            image_id: target.image_id.clone(),
            repo_digests: target.image_repo_digests.clone(),
            configured_user: target.image_user.clone(),
        })
        .unwrap();
        validate_primary_plan_image(&plan, &primary).unwrap();
        validate_plan_image(&plan, &target).unwrap();

        let mut changed = primary.clone();
        changed.image_id = format!("sha256:{}", "3".repeat(64));
        assert!(validate_primary_plan_image(&plan, &changed).is_err());
        let mut changed = primary.clone();
        changed.repo_digests = vec!["changed-primary".into()];
        assert!(validate_primary_plan_image(&plan, &changed).is_err());
        let mut changed = primary;
        changed.configured_user = "1000:1000".into();
        assert!(validate_primary_plan_image(&plan, &changed).is_err());

        let mut changed = target.clone();
        changed.image_id = format!("sha256:{}", "4".repeat(64));
        assert!(validate_plan_image(&plan, &changed).is_err());
        let mut changed = target.clone();
        changed.image_repo_digests = vec!["changed-target".into()];
        assert!(validate_plan_image(&plan, &changed).is_err());
        let mut changed = target;
        changed.image_user = "1000:1000".into();
        assert!(validate_plan_image(&plan, &changed).is_err());

        let mut changed_plan = plan;
        changed_plan.material.image = "falkordb/falkordb:latest";
        assert!(validate_primary_plan_image(&changed_plan, &primary_image(&changed_plan)).is_err());
        assert!(validate_plan_image(&changed_plan, &target_attestation(&changed_plan)).is_err());
    }

    fn primary_image(plan: &Plan) -> ImageEvidence {
        ImageEvidence {
            image_id: plan.material.primary_image_id.clone(),
            repo_digests: plan.material.primary_image_repo_digests.clone(),
            configured_user: plan.material.image_user.clone(),
        }
    }

    #[test]
    fn pinned_image_evidence_requires_exact_digest_mapping_and_canonical_id() {
        let image_id = format!("sha256:{}", "d".repeat(64));
        let raw =
            format!("IMAGE={image_id}\nIMAGE_REPO_DIGESTS=[\"{FALKORDB_IMAGE}\"]\nIMAGE_USER=\n");
        assert_eq!(parse_image_evidence(&raw).unwrap().image_id, image_id);
        assert!(parse_image_evidence(&raw.replace(FALKORDB_IMAGE, "falkordb:latest")).is_err());
        assert!(parse_image_evidence(&raw.replace(
            &format!("[\"{FALKORDB_IMAGE}\"]"),
            &format!("[\"{FALKORDB_IMAGE}\",\"{FALKORDB_IMAGE}\"]"),
        ))
        .is_err());
        assert!(
            parse_image_evidence(&raw.replace(&format!("[\"{FALKORDB_IMAGE}\"]"), "[]",)).is_err()
        );
        assert!(
            parse_image_evidence(&raw.replace(&format!("[\"{FALKORDB_IMAGE}\"]"), "null",))
                .is_err()
        );
        assert!(parse_image_evidence(&raw.replace("sha256:dddd", "sha256:DDDD")).is_err());
        assert!(parse_image_evidence(&format!("{raw}IMAGE={image_id}\n")).is_err());

        let alias = format!("example.invalid/falkor@sha256:{}", "1".repeat(64));
        let first = raw.replace(
            &format!("[\"{FALKORDB_IMAGE}\"]"),
            &format!("[\"{alias}\",\"{FALKORDB_IMAGE}\"]"),
        );
        let second = raw.replace(
            &format!("[\"{FALKORDB_IMAGE}\"]"),
            &format!("[\"{FALKORDB_IMAGE}\",\"{alias}\"]"),
        );
        assert_eq!(
            parse_image_evidence(&first).unwrap(),
            parse_image_evidence(&second).unwrap()
        );
    }

    #[test]
    fn status_topology_rejects_stale_cached_or_nonhealthy_state() {
        let mut plan = plan();
        plan.material.target_state = TargetState::ExactHealthy {
            image_id: plan.material.target_image_id.clone(),
        };
        assert!(validate_status_topology(
            "replica",
            "running",
            Some(plan.material.backup.backup_id),
            &plan,
        )
        .is_ok());
        assert!(
            validate_status_topology("replica", "running", Some(Uuid::from_u128(999)), &plan,)
                .is_err()
        );
        assert!(validate_status_topology(
            "replica",
            "failed",
            Some(plan.material.backup.backup_id),
            &plan,
        )
        .is_err());
        plan.material.target_state = TargetState::Absent;
        assert!(validate_status_topology(
            "replica",
            "running",
            Some(plan.material.backup.backup_id),
            &plan,
        )
        .is_err());
    }

    #[test]
    fn complete_graph_hash_excludes_statistics_binds_all_rows_and_ignores_row_order() {
        let first = redis::Value::Array(vec![
            redis::Value::Array(vec![redis::Value::BulkString(b"properties(n)".to_vec())]),
            redis::Value::Array(vec![redis::Value::Array(vec![redis::Value::Int(1)])]),
            redis::Value::Array(vec![redis::Value::BulkString(
                b"Cached execution: 0".to_vec(),
            )]),
        ]);
        let mut timing_changed = first.clone();
        if let redis::Value::Array(parts) = &mut timing_changed {
            parts[2] = redis::Value::SimpleString("Query internal execution time: 99".into());
        }
        assert_eq!(
            graph_full_hash(&first, 1, true).unwrap(),
            graph_full_hash(&timing_changed, 1, true).unwrap()
        );
        let mut row_changed = first.clone();
        if let redis::Value::Array(parts) = &mut row_changed {
            parts[1] = redis::Value::Array(vec![redis::Value::Array(vec![redis::Value::Int(2)])]);
        }
        assert_ne!(
            graph_full_hash(&first, 1, true).unwrap(),
            graph_full_hash(&row_changed, 1, true).unwrap()
        );
        assert!(graph_full_hash(&first, 2, true).is_err());

        let mut many_rows: Vec<redis::Value> = (0..129)
            .map(|value| redis::Value::Array(vec![redis::Value::Int(value)]))
            .collect();
        let original = redis::Value::Array(vec![
            redis::Value::Array(vec![redis::Value::BulkString(b"properties(n)".to_vec())]),
            redis::Value::Array(many_rows.clone()),
            redis::Value::SimpleString("Query internal execution time: 0".into()),
        ]);
        let mut reordered_rows = if let redis::Value::Array(parts) = &original {
            if let redis::Value::Array(rows) = &parts[1] {
                rows.clone()
            } else {
                unreachable!()
            }
        } else {
            unreachable!()
        };
        reordered_rows.reverse();
        let reordered = redis::Value::Array(vec![
            redis::Value::Array(vec![redis::Value::BulkString(b"properties(n)".to_vec())]),
            redis::Value::Array(reordered_rows),
            redis::Value::SimpleString("Query internal execution time: 9".into()),
        ]);
        assert_eq!(
            graph_full_hash(&original, 129, true).unwrap(),
            graph_full_hash(&reordered, 129, true).unwrap()
        );
        many_rows[128] = redis::Value::Array(vec![redis::Value::Int(999)]);
        many_rows.reverse();
        let changed_last = redis::Value::Array(vec![
            redis::Value::Array(vec![redis::Value::BulkString(b"properties(n)".to_vec())]),
            redis::Value::Array(many_rows),
            redis::Value::SimpleString("Query internal execution time: 0".into()),
        ]);
        assert_ne!(
            graph_full_hash(&original, 129, true).unwrap(),
            graph_full_hash(&changed_last, 129, true).unwrap()
        );
        let duplicate_nodes = redis::Value::Array(vec![
            redis::Value::Array(vec![redis::Value::BulkString(b"properties(n)".to_vec())]),
            redis::Value::Array(vec![
                redis::Value::Array(vec![redis::Value::Int(1)]),
                redis::Value::Array(vec![redis::Value::Int(1)]),
            ]),
            redis::Value::SimpleString("Query internal execution time: 0".into()),
        ]);
        assert!(graph_full_hash(&duplicate_nodes, 2, true).is_err());
        assert!(graph_full_hash(&duplicate_nodes, 2, false).is_ok());
        assert!(!FULL_NODE_QUERY.contains("LIMIT"));
        assert!(!FULL_RELATIONSHIP_QUERY.contains("LIMIT"));
        assert!(!FULL_NODE_QUERY.contains("id("));
        assert!(!FULL_RELATIONSHIP_QUERY.contains("id("));
    }

    #[test]
    fn compact_falkor_count_uses_typed_scalar_value_not_type_code() {
        let response = redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::Array(vec![redis::Value::BulkString(b"count(n)".to_vec())]),
            redis::Value::Array(vec![redis::Value::Array(vec![redis::Value::Array(vec![
                redis::Value::Int(3),
                redis::Value::Int(56),
            ])])]),
            redis::Value::Array(vec![redis::Value::BulkString(
                b"Query internal execution time: 0.1".to_vec(),
            )]),
        ]);
        assert_eq!(graph_count(&response).unwrap(), 56);
    }

    #[test]
    fn exact_restore_receipt_is_checksum_image_and_count_bound() {
        let checksum = "a".repeat(64);
        let image_id = format!("sha256:{}", "b".repeat(64));
        let receipt = serde_json::json!({
            "proof": "falkordb_exact_restore_v1",
            "input_checksum_sha256": checksum,
            "image_reference": FALKORDB_IMAGE,
            "image_id": image_id,
            "network": "none",
            "query_mode": "GRAPH.RO_QUERY",
            "expected_min_keys": 1,
            "observed_keys": 3,
            "expected_min_graph_nodes": 1,
            "observed_graphs": 3,
            "observed_graph_nodes": 56
        });
        let detail = format!("restore drill passed; receipt={receipt}; capacity=ok");
        assert_eq!(
            restore_receipt(&detail, &"a".repeat(64)).unwrap(),
            format!("sha256:{}", "b".repeat(64))
        );
        let production_detail = format!(
            "restore drill passed; receipt={receipt}; capacity required=10 available=20 bytes; policy encrypted_max=30 extracted_max=40 effective_extracted=50 files_max=60 expansion_ratio=70 reserve=80"
        );
        restore_receipt(&production_detail, &"a".repeat(64)).unwrap();
        assert!(restore_receipt(&detail, &"c".repeat(64)).is_err());
        assert!(restore_receipt(
            &detail.replace(
                &format!("sha256:{}", "b".repeat(64)),
                &format!("sha256:{}", "c".repeat(64)),
            ),
            &"a".repeat(64),
        )
        .is_ok());
        assert!(restore_receipt(
            &detail.replace(
                &format!("sha256:{}", "b".repeat(64)),
                "sha256:NOT-CANONICAL",
            ),
            &"a".repeat(64),
        )
        .is_err());
        assert!(restore_receipt(
            &detail.replace(FALKORDB_IMAGE, "falkordb:latest"),
            &"a".repeat(64),
        )
        .is_err());
        assert!(restore_receipt(&format!("prefixed {detail}"), &"a".repeat(64),).is_err());
        assert!(
            restore_receipt(&format!("{detail}; receipt={receipt}"), &"a".repeat(64),).is_err()
        );
        assert!(restore_receipt(
            &detail.replace("\"proof\":", "\"proof\":\"duplicate\",\"proof\":"),
            &"a".repeat(64),
        )
        .is_err());
        assert!(restore_receipt(
            &detail.replace("\"proof\":", "\"unknown\":true,\"proof\":"),
            &"a".repeat(64),
        )
        .is_err());
    }

    #[test]
    fn firewall_gate_requires_exact_helper_identity_rules_and_docker_unit() {
        let target = computer(1, "sophie", "192.168.5.103");
        let primary = computer(2, "priya", "192.168.5.104");
        let status = serde_json::json!({
            "ok": true,
            "interface": "enp3s0",
            "source_ipv4": "192.168.5.103",
            "destination_ipv4": "192.168.5.104",
            "port": 63379,
            "allow_v4": true,
            "deny_v4": true,
            "deny_v6": true,
            "allow_v4_position": 1,
            "deny_v4_position": 2,
            "deny_v6_position": 1,
            "deny_v6_forward_position": 1,
            "persistence": {
                "unit": "forgefleet-falkordb-source-firewall.service",
                "unit_active": true,
                "unit_enabled": true,
                "environment_file": "/etc/forgefleet/falkordb-source-firewall.env",
                "environment_file_present": true,
                "helper": "/usr/local/sbin/forgefleet-falkor-source-firewall",
                "helper_present": true
            }
        });
        let raw = format!(
            "{status}\nSERVICE_AFTER=network-online.target docker.service\nSERVICE_REQUIRES=docker.service\nSERVICE_PARTOF=docker.service\n"
        );
        validate_firewall_attestation(&raw, &target, &primary).unwrap();
        assert!(validate_firewall_attestation(
            &raw.replace("192.168.5.103", "192.168.5.199"),
            &target,
            &primary
        )
        .is_err());
        assert!(validate_firewall_attestation(
            &raw.replace("SERVICE_PARTOF=docker.service", "SERVICE_PARTOF="),
            &target,
            &primary
        )
        .is_err());
        assert!(validate_firewall_attestation(
            &raw.replace("\"allow_v4_position\":1", "\"allow_v4_position\":3"),
            &target,
            &primary
        )
        .is_err());
        assert!(validate_firewall_attestation(
            &raw.replace("\"unit_active\":true", "\"unit_active\":false"),
            &target,
            &primary
        )
        .is_err());
        assert!(validate_firewall_attestation(
            &raw.replace(
                "\"deny_v6_forward_position\":1",
                "\"deny_v6_forward_position\":2"
            ),
            &target,
            &primary
        )
        .is_err());
        assert!(
            validate_firewall_attestation(&format!("garbage{raw}"), &target, &primary).is_err()
        );
        assert!(validate_firewall_attestation(
            &raw.replace("\"ok\":true", "\"extra\":true,\"ok\":true"),
            &target,
            &primary
        )
        .is_err());
        assert!(validate_firewall_attestation(
            &raw.replace("\"ok\":true", "\"ok\":true,\"ok\":true"),
            &target,
            &primary
        )
        .is_err());
    }

    #[test]
    fn exact_existing_container_accepts_only_safe_shape() {
        let ports = serde_json::Value::Null;
        let env = serde_json::json!([
            format!("REDIS_ARGS={}", expected_redis_args("192.168.5.104")),
            "FALKORDB_ARGS=THREAD_COUNT 8 CACHE_SIZE 100",
            "BROWSER=0",
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        ]);
        let image_id = format!("sha256:{}", "d".repeat(64));
        let cap_drop = serde_json::json!(["ALL"]);
        let cap_add = serde_json::Value::Null;
        let security = serde_json::json!(["no-new-privileges:true"]);
        let devices = serde_json::json!([]);
        let tmpfs = serde_json::json!({"/tmp": "rw,nodev,nosuid,noexec,size=67108864"});
        let binds = serde_json::Value::Null;
        let health = serde_json::json!({
            "Test": ["CMD", "redis-cli", "-p", "63380", "PING"],
            "Interval": 5_000_000_000_u64,
            "Timeout": 3_000_000_000_u64,
            "Retries": 24,
            "StartPeriod": 30_000_000_000_u64
        });
        let fingerprint = format!(
            "{image_id}|{FALKORDB_IMAGE}|true|healthy|unless-stopped|host|{ports}|{env}|volume:{FALKORDB_VOLUME}:/var/lib/falkordb/data:true;|true|{cap_drop}|{cap_add}|{security}|false|{devices}|{tmpfs}|{binds}||{health}"
        );
        validate_existing_container(&fingerprint, &image_id, "", "192.168.5.104").unwrap();
        assert!(validate_existing_container(
            &fingerprint.replace("--bind 127.0.0.1", "--bind 0.0.0.0"),
            &image_id,
            "",
            "192.168.5.104"
        )
        .is_err());
        assert!(validate_existing_container(
            &fingerprint.replace(
                "replica-serve-stale-data no",
                "replica-serve-stale-data yes"
            ),
            &image_id,
            "",
            "192.168.5.104"
        )
        .is_err());
        assert!(validate_existing_container(
            &fingerprint.replacen("|true|[\"ALL\"]", "|false|[\"ALL\"]", 1),
            &image_id,
            "",
            "192.168.5.104"
        )
        .is_err());
        assert!(validate_existing_container(
            &fingerprint.replace("[\"ALL\"]", "[]"),
            &image_id,
            "",
            "192.168.5.104"
        )
        .is_err());
        assert!(validate_existing_container(
            &fingerprint.replace(FALKORDB_IMAGE, "falkordb/falkordb:latest"),
            &image_id,
            "",
            "192.168.5.104"
        )
        .is_err());
    }

    #[test]
    fn deferred_commands_contain_no_authority_url_or_credentials() {
        let plan = plan();
        let command = local_apply_command(&plan);
        assert!(command.contains("falkordb-replica local-apply"));
        assert!(!command.contains("redis://"));
        assert!(!command.to_ascii_lowercase().contains("password"));
        assert!(!command.to_ascii_lowercase().contains("secret"));
    }

    #[test]
    fn lifecycle_payload_rejects_command_proof_action_and_target_mismatch() {
        let target = computer(1, "sophie", "192.168.5.103");
        let command = "ff fleet db falkordb-replica local-apply --to sophie";
        let payload = serde_json::json!({
            "deferred_payload": {
                "command": command,
                "command_sha256": sha256(command.as_bytes()),
                "action": "apply",
                "proof_id": "plan-1",
                "target_computer_id": target.id,
                "database_kind": "falkordb",
                "automatic_failover": false,
                "read_routing": false
            }
        });
        validate_lifecycle_payload(&payload, &target, "apply", "plan-1", command).unwrap();
        assert!(validate_lifecycle_payload(&payload, &target, "apply", "stale", command).is_err());
        assert!(
            validate_lifecycle_payload(&payload, &target, "decommission", "plan-1", command)
                .is_err()
        );
        assert!(
            validate_lifecycle_payload(&payload, &target, "apply", "plan-1", "different").is_err()
        );
        let other = computer(9, "other", "192.168.5.109");
        assert!(validate_lifecycle_payload(&payload, &other, "apply", "plan-1", command).is_err());
    }

    #[test]
    fn purge_preview_is_nonmutating_and_nonzero_by_contract() {
        let preview = require_purge_confirmation(false).unwrap_err().to_string();
        assert!(preview.contains("PREVIEW"));
        assert!(preview.contains("nonzero"));
        assert!(require_purge_confirmation(true).is_ok());
        let blocked_status = preserved_volume_status_result().unwrap_err().to_string();
        assert!(blocked_status.contains("BLOCKED"));
        assert!(blocked_status.contains("preview"));
    }

    #[tokio::test]
    async fn lifecycle_enqueue_reuses_active_then_rolls_terminal_signature() {
        let db_url = match std::env::var("DATABASE_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
        {
            Ok(url) => url,
            Err(_) => {
                let config_text = tokio::fs::read_to_string(
                    dirs::home_dir()
                        .expect("home directory")
                        .join(".forgefleet/fleet.toml"),
                )
                .await
                .expect("read fleet config for session-local PostgreSQL test");
                let config: ff_core::config::FleetConfig =
                    toml::from_str(&config_text).expect("parse fleet config");
                config.database.url
            }
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect PostgreSQL for session-local lifecycle test");
        sqlx::query(
            "CREATE TEMP TABLE fleet_tasks (
                id UUID PRIMARY KEY,
                task_type TEXT NOT NULL,
                summary TEXT NOT NULL,
                payload JSONB NOT NULL,
                priority INTEGER NOT NULL,
                requires_capability JSONB NOT NULL,
                preferred_computer_id UUID,
                status TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                task_class TEXT,
                dedup_signature TEXT
             ) ON COMMIT PRESERVE ROWS",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX idx_fleet_tasks_dedup_signature
               ON fleet_tasks(dedup_signature) WHERE dedup_signature IS NOT NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        let target = computer(1, "sophie", "192.168.5.103");
        let command = "exact-command".to_string();
        let first = enqueue_lifecycle_action(&pool, &target, "apply", "plan-1", command.clone())
            .await
            .unwrap();
        let duplicate =
            enqueue_lifecycle_action(&pool, &target, "apply", "plan-1", command.clone())
                .await
                .unwrap();
        assert_eq!(first, duplicate);
        assert!(
            enqueue_lifecycle_action(&pool, &target, "apply", "plan-2", command.clone())
                .await
                .is_err()
        );
        assert!(enqueue_lifecycle_action(
            &pool,
            &target,
            "apply",
            "plan-1",
            "different-command".into(),
        )
        .await
        .is_err());
        sqlx::query("UPDATE fleet_tasks SET status='completed' WHERE id=$1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        let second = enqueue_lifecycle_action(&pool, &target, "apply", "plan-1", command)
            .await
            .unwrap();
        assert_ne!(first, second);
        let signed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fleet_tasks WHERE dedup_signature IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let cleared: Option<String> =
            sqlx::query_scalar("SELECT dedup_signature FROM fleet_tasks WHERE id=$1")
                .bind(first)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(signed, 1);
        assert!(cleared.is_none());
    }

    #[test]
    fn compose_is_immutable_loopback_durable_read_only_and_failover_free() {
        let compose = include_str!("../../../deploy/docker-compose.falkordb-follower.yml");
        assert!(compose.contains(FALKORDB_IMAGE));
        assert!(compose.contains("pull_policy: never"));
        assert!(compose.contains("network_mode: host"));
        assert!(compose.contains("read_only: true"));
        assert!(compose.contains("cap_drop:"));
        assert!(compose.contains("- ALL"));
        assert!(compose.contains("no-new-privileges:true"));
        assert!(compose.contains("/tmp:rw,nodev,nosuid,noexec,size=64m"));
        assert!(compose.contains("--port 63380"));
        assert!(compose.contains("--bind 127.0.0.1"));
        assert!(compose.contains("BROWSER: \"0\""));
        assert!(compose.contains("replica-read-only yes"));
        assert!(compose.contains("replica-serve-stale-data no"));
        assert!(compose.contains("appendfsync everysec"));
        assert!(compose.contains("/var/lib/falkordb/data"));
        assert!(compose.contains("restart: unless-stopped"));
        assert!(!compose.contains("3000:3000"));
        assert!(!compose.to_ascii_lowercase().contains("sentinel"));
        assert!(!compose.to_ascii_lowercase().contains("replicaof no one"));
        assert!(!compose.to_ascii_lowercase().contains("masterauth"));
    }

    #[test]
    fn topology_sql_can_only_register_falkordb_primary_and_replica() {
        assert!(UPSERT_PRIMARY_SQL.contains("'falkordb','primary','running'"));
        assert!(UPSERT_REPLICA_SQL.contains("'falkordb','replica','running'"));
        assert!(!UPSERT_PRIMARY_SQL.to_ascii_lowercase().contains("promot"));
        assert!(!UPSERT_REPLICA_SQL.to_ascii_lowercase().contains("promot"));
    }
}
