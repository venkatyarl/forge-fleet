//! Explicit, fail-closed FalkorDB follower lifecycle.
//!
//! There is deliberately no promotion, failover, or read-routing command in
//! this module. Priya remains the sole authority. Provisioning is split into a
//! mutation-free, drift-bound plan and a target-owned deferred apply. An apply
//! registers topology only after exact replication, graph, write-rejection,
//! container, firewall, and restart proofs all pass.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
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
    node_sample_sha256: String,
    relationship_sample_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BackupEvidence {
    backup_id: Uuid,
    file_name: String,
    checksum_sha256: String,
    size_bytes: i64,
    drill_id: Uuid,
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
    primary_used_memory: u64,
    graphs: BTreeMap<String, GraphEvidence>,
    backup: BackupEvidence,
    target_state: TargetState,
    image: &'static str,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetAttestation {
    docker_version: String,
    image_id: String,
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
    primary_ip: String,
    unit_enabled: bool,
    unit_active: bool,
    unit_result_success: bool,
    docker_lifecycle_bound: bool,
    ipv4_target_allow: bool,
    ipv4_default_deny: bool,
    ipv6_default_deny: bool,
}

#[derive(Debug, Clone)]
struct PurgeEvidence {
    target: Computer,
    primary: Computer,
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

impl PurgeEvidence {
    fn proof(&self) -> String {
        let canonical = format!(
            "falkordb-purge-v1\0{}\0{}\0{}\0{}\0{}\0{}",
            self.target.id,
            self.target.name,
            self.primary.id,
            FALKORDB_VOLUME,
            self.backup.backup_id,
            self.backup.checksum_sha256,
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

async fn primary_probe(primary: &Computer) -> Result<PrimaryProbe> {
    let local_name = ff_agent::fleet_info::resolve_this_worker_name().await;
    if local_name.eq_ignore_ascii_case(&primary.name) {
        return Ok(PrimaryProbe {
            url: format!("redis://127.0.0.1:{PRIMARY_PORT}"),
            tunnel: None,
        });
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("reserve local FalkorDB proof tunnel port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let forward = format!("127.0.0.1:{port}:127.0.0.1:{PRIMARY_PORT}");
    let mut child = tokio::process::Command::new("ssh")
        .args([
            "-N",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "-p",
            &primary.ssh_port.to_string(),
            "-L",
            &forward,
            &format!("{}@{}", primary.ssh_user, primary.ip),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("open bounded SSH tunnel for local Priya proof")?;
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
            bail!("Priya SSH proof tunnel exited before becoming ready");
        }
        if std::time::Instant::now() >= deadline {
            bail!("Priya SSH proof tunnel did not become ready");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn field<'a>(raw: &'a str, name: &str) -> Result<&'a str> {
    raw.lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
        .context(format!("remote attestation field {name} is missing"))
}

fn parse_target_attestation(raw: &str) -> Result<TargetAttestation> {
    let docker_version = field(raw, "DOCKER")?.trim().to_string();
    let image_id = field(raw, "IMAGE")?.trim().to_string();
    if docker_version.is_empty() || image_id.is_empty() {
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
        image_id,
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
if docker container inspect '{FALKORDB_CONTAINER}' >/dev/null 2>&1; then
  printf 'CONTAINER='
  docker container inspect --format '{{{{.Image}}}}|{{{{.State.Running}}}}|{{{{if .State.Health}}}}{{{{.State.Health.Status}}}}{{{{else}}}}none{{{{end}}}}|{{{{.HostConfig.RestartPolicy.Name}}}}|{{{{.HostConfig.NetworkMode}}}}|{{{{json .HostConfig.PortBindings}}}}|{{{{json .Config.Env}}}}|{{{{range .Mounts}}}}{{{{.Name}}}}:{{{{.Destination}}}};{{{{end}}}}' '{FALKORDB_CONTAINER}'
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

fn validate_existing_container(fingerprint: &str, image_id: &str, primary_ip: &str) -> Result<()> {
    let fields: Vec<&str> = fingerprint.splitn(8, '|').collect();
    if fields.len() != 8 {
        bail!("existing FalkorDB container fingerprint is incomplete");
    }
    if fields[0] != image_id
        || fields[1] != "true"
        || fields[2] != "healthy"
        || fields[3] != "unless-stopped"
        || fields[4] != "host"
    {
        bail!("existing FalkorDB container is not exact, running, and healthy");
    }
    let ports: serde_json::Value =
        serde_json::from_str(fields[5]).context("parse FalkorDB port bindings")?;
    if !ports.is_null()
        && ports
            .as_object()
            .is_none_or(|bindings| !bindings.is_empty())
    {
        bail!("existing FalkorDB host-network container has unexpected published-port bindings");
    }
    let env: Vec<String> =
        serde_json::from_str(fields[6]).context("parse FalkorDB container environment")?;
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
    if fields[7] != format!("{FALKORDB_VOLUME}:/var/lib/falkordb/data;") {
        bail!("existing FalkorDB container does not have the exact durable volume");
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
        validate_existing_container(fingerprint, &attestation.image_id, primary_ip)?;
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

fn firewall_attestation_script() -> &'static str {
    r#"set -eu
unit='forgefleet-falkordb-source-firewall.service'
echo FIREWALL_JSON_BEGIN
sudo -n sh -c 'set -a; . /etc/forgefleet/falkordb-source-firewall.env; exec /usr/local/sbin/forgefleet-falkor-source-firewall --json status'
echo FIREWALL_JSON_END
echo "SERVICE_AFTER=$(systemctl show "$unit" -p After --value)"
echo "SERVICE_REQUIRES=$(systemctl show "$unit" -p Requires --value)"
echo "SERVICE_PARTOF=$(systemctl show "$unit" -p PartOf --value)"
"#
}

fn firewall_json(raw: &str) -> Result<serde_json::Value> {
    let (_, tail) = raw
        .split_once("FIREWALL_JSON_BEGIN\n")
        .context("firewall JSON begin marker is missing")?;
    let (json, _) = tail
        .split_once("\nFIREWALL_JSON_END")
        .context("firewall JSON end marker is missing")?;
    serde_json::from_str(json.trim()).context("parse firewall helper status JSON")
}

fn validate_firewall_attestation(
    raw: &str,
    target: &Computer,
    primary: &Computer,
) -> Result<FirewallEvidence> {
    let status = firewall_json(raw)?;
    let status_obj = status
        .as_object()
        .context("firewall helper status must be a JSON object")?;
    let persistence = status
        .get("persistence")
        .and_then(serde_json::Value::as_object)
        .context("firewall helper status is missing persistence evidence")?;
    let bool_field = |object: &serde_json::Map<String, serde_json::Value>, key: &str| {
        object.get(key).and_then(serde_json::Value::as_bool) == Some(true)
    };
    let exact_string = |key: &str, expected: &str| {
        status.get(key).and_then(serde_json::Value::as_str) == Some(expected)
    };
    let exact_position = |key: &str, expected: u64| {
        status.get(key).and_then(serde_json::Value::as_u64) == Some(expected)
    };

    let unit_enabled = bool_field(persistence, "unit_enabled");
    let unit_active = bool_field(persistence, "unit_active");
    let unit_result_success = status.get("ok").and_then(serde_json::Value::as_bool) == Some(true);
    let after = field(raw, "SERVICE_AFTER")?;
    let requires = field(raw, "SERVICE_REQUIRES")?;
    let part_of = field(raw, "SERVICE_PARTOF")?;
    let docker_lifecycle_bound = after
        .split_whitespace()
        .any(|item| item == "docker.service")
        && requires
            .split_whitespace()
            .any(|item| item == "docker.service")
        && part_of
            .split_whitespace()
            .any(|item| item == "docker.service");

    let identity_exact = exact_string("interface", "enp3s0")
        && exact_string("source_ipv4", &target.ip)
        && exact_string("destination_ipv4", &primary.ip)
        && status.get("port").and_then(serde_json::Value::as_u64) == Some(u64::from(PRIMARY_PORT));
    let persistence_exact = persistence.get("unit").and_then(serde_json::Value::as_str)
        == Some("forgefleet-falkordb-source-firewall.service")
        && persistence
            .get("environment_file")
            .and_then(serde_json::Value::as_str)
            == Some("/etc/forgefleet/falkordb-source-firewall.env")
        && persistence
            .get("helper")
            .and_then(serde_json::Value::as_str)
            == Some("/usr/local/sbin/forgefleet-falkor-source-firewall")
        && bool_field(persistence, "environment_file_present")
        && bool_field(persistence, "helper_present");
    let ipv4_target_allow = bool_field(status_obj, "allow_v4")
        && exact_position("allow_v4_position", 1)
        && identity_exact;
    let ipv4_default_deny = bool_field(status_obj, "deny_v4")
        && exact_position("deny_v4_position", 2)
        && identity_exact;
    let ipv6_default_deny = bool_field(status_obj, "deny_v6")
        && exact_position("deny_v6_position", 1)
        && identity_exact;

    let evidence = FirewallEvidence {
        target_id: target.id,
        target_ip: target.ip.clone(),
        primary_ip: primary.ip.clone(),
        unit_enabled,
        unit_active,
        unit_result_success,
        docker_lifecycle_bound,
        ipv4_target_allow,
        ipv4_default_deny,
        ipv6_default_deny,
    };
    if !evidence.unit_enabled
        || !evidence.unit_active
        || !evidence.unit_result_success
        || !evidence.docker_lifecycle_bound
        || !evidence.ipv4_target_allow
        || !evidence.ipv4_default_deny
        || !evidence.ipv6_default_deny
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

async fn attest_target_source_route(target: &Computer, primary: &Computer) -> Result<()> {
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
    Ok(())
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

fn graph_payload_hash(value: &redis::Value) -> Result<String> {
    let redis::Value::Array(parts) = value else {
        bail!("FalkorDB query proof is not an array");
    };
    if parts.len() < 2 {
        bail!("FalkorDB query proof lacks header and rows");
    }
    // FalkorDB's final element is timing/statistics. It is intentionally not
    // hashed; the exact header and rows are.
    let stable = redis::Value::Array(parts[..parts.len() - 1].to_vec());
    let mut encoded = Vec::new();
    encode_redis_value(&stable, &mut encoded)?;
    Ok(sha256(&encoded))
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
    graphs.dedup();
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
        let node_sample_sha256 = graph_payload_hash(
            &graph_query(
                url,
                &graph,
                "MATCH (n) RETURN id(n), labels(n), properties(n) ORDER BY id(n) LIMIT 128",
            )
            .await?,
        )?;
        let relationship_sample_sha256 = graph_payload_hash(
            &graph_query(
                url,
                &graph,
                "MATCH (a)-[r]->(b) RETURN id(a), type(r), properties(r), id(b) ORDER BY id(a), id(b), type(r) LIMIT 128",
            )
            .await?,
        )?;
        evidence.insert(
            graph,
            GraphEvidence {
                nodes,
                relationships,
                node_sample_sha256,
                relationship_sample_sha256,
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

fn restore_receipt(detail: &str, expected_checksum: &str) -> Result<()> {
    let (_, tail) = detail
        .split_once("receipt=")
        .context("restore drill detail has no exact receipt")?;
    let json = tail.split_once("; ").map_or(tail, |(json, _)| json);
    let receipt: serde_json::Value =
        serde_json::from_str(json).context("parse FalkorDB restore receipt")?;
    let exact = |field: &str, expected: &str| -> Result<()> {
        if receipt.get(field).and_then(serde_json::Value::as_str) != Some(expected) {
            bail!("FalkorDB restore receipt field {field} is not exact");
        }
        Ok(())
    };
    exact("proof", "falkordb_exact_restore_v1")?;
    exact("input_checksum_sha256", expected_checksum)?;
    exact("image_reference", FALKORDB_IMAGE)?;
    exact("network", "none")?;
    exact("query_mode", "GRAPH.RO_QUERY")?;
    let image_id = receipt
        .get("image_id")
        .and_then(serde_json::Value::as_str)
        .context("FalkorDB restore receipt has no image identity")?;
    if image_id.len() != 71
        || !image_id.starts_with("sha256:")
        || !image_id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("FalkorDB restore receipt image identity is not a SHA-256 image ID");
    }
    for (observed, expected) in [
        ("observed_keys", "expected_min_keys"),
        ("observed_graph_nodes", "expected_min_graph_nodes"),
    ] {
        let observed = receipt
            .get(observed)
            .and_then(serde_json::Value::as_u64)
            .context("restore receipt observed count is missing")?;
        let expected = receipt
            .get(expected)
            .and_then(serde_json::Value::as_u64)
            .context("restore receipt expected count is missing")?;
        if observed < expected {
            bail!("FalkorDB restore receipt did not meet its minimum count");
        }
    }
    if receipt
        .get("observed_graphs")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        == 0
    {
        bail!("FalkorDB restore receipt did not observe a graph");
    }
    Ok(())
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
        let proof = (|| -> Result<()> {
            restore_receipt(&detail, &checksum_sha256)?;
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
            Ok(())
        })();
        match proof {
            Ok(()) => {
                return Ok(BackupEvidence {
                    backup_id,
                    file_name,
                    checksum_sha256,
                    size_bytes,
                    drill_id,
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
    let (replid, _) = primary_identity(&replication)?;
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
    let memory = redis_info(url, "memory").await?;
    let used_memory = memory
        .get("used_memory")
        .context("FalkorDB authority has no used_memory")?
        .parse::<u64>()
        .context("parse FalkorDB used_memory")?;
    if used_memory == 0 {
        bail!("FalkorDB authority reported zero used memory");
    }
    let primary_dbsize = dbsize(url).await?;
    let graphs = graph_inventory(url).await?;
    Ok((
        replid,
        version,
        graph_version,
        primary_dbsize,
        used_memory,
        graphs,
    ))
}

async fn build_plan(pool: &sqlx::PgPool, to: &str, primary_name: &str) -> Result<Plan> {
    reject_vinny(to)?;
    let target = resolve_computer(pool, to).await?;
    let primary = resolve_computer(pool, primary_name).await?;
    validate_target_identity(&target, &primary)?;
    validate_topology_for_plan(pool, &target, &primary).await?;

    let _canonical_authority = authority_url(pool, &primary).await?;
    let probe = primary_probe(&primary).await?;
    let (
        primary_replid,
        primary_version,
        graph_module_version,
        primary_dbsize,
        primary_used_memory,
        graphs,
    ) = match primary_evidence(&probe.url).await {
        Ok(evidence) => {
            probe.close().await;
            evidence
        }
        Err(error) => {
            probe.close().await;
            return Err(error).context("probe canonical Priya FalkorDB through SSH loopback");
        }
    };
    let backup = backup_evidence(pool, &primary).await?;
    attest_firewall(&primary, &target).await?;
    attest_target_source_route(&target, &primary).await?;
    let (target_attestation, target_state) = attest_target(&target, &primary.ip).await?;

    let required_bytes = primary_used_memory.saturating_mul(2).max(MIN_TARGET_BYTES);
    if target_attestation.ram_bytes < required_bytes
        || target_attestation.disk_free_bytes < required_bytes
    {
        bail!(
            "FalkorDB target needs at least {required_bytes} bytes RAM and free disk (2x source data with 2 GiB floor)"
        );
    }

    Ok(Plan {
        material: PlanMaterial {
            version: "falkordb-replica-plan-v1",
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
            primary_used_memory,
            graphs,
            backup,
            target_state,
            image: FALKORDB_IMAGE,
            primary_port: PRIMARY_PORT,
            replica_port: REPLICA_PORT,
            firewall_policy: "ff-falkordb-firewall-v1",
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

async fn enqueue_lifecycle_action(
    pool: &sqlx::PgPool,
    target: &Computer,
    action: &str,
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
        "SELECT id,payload->'deferred_payload'->>'command' AS command
           FROM fleet_tasks WHERE dedup_signature=$1
            AND status IN ('pending','dispatchable','running')
          ORDER BY created_at DESC",
    )
    .bind(&signature)
    .fetch_all(&mut *tx)
    .await?;
    if existing.len() > 1 {
        bail!("multiple active FalkorDB lifecycle tasks exist for the exact target UUID");
    }
    if let Some(row) = existing.first() {
        let existing_command = row
            .try_get::<Option<String>, _>("command")?
            .unwrap_or_default();
        if existing_command == command {
            let id: Uuid = row.get("id");
            tx.commit().await?;
            return Ok(id);
        }
        bail!(
            "another FalkorDB lifecycle action is active for target {}; wait or reconcile it first",
            target.id
        );
    }
    let deferred_payload = serde_json::json!({
        "command": command,
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
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fleet_tasks
           (task_type,summary,payload,priority,requires_capability,
            preferred_computer_id,status,created_at,task_class,dedup_signature)
         VALUES ('shell',$1,$2,50,$3,$4,'pending',NOW(),'deferred',$5)
         RETURNING id",
    )
    .bind(&summary)
    .bind(&payload)
    .bind(&required_caps)
    .bind(target.id)
    .bind(&signature)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn validate_compose(plan: &Plan) -> Result<()> {
    if !std::path::Path::new(FALKORDB_COMPOSE).is_file() {
        bail!("run from a ForgeFleet checkout containing the FalkorDB follower compose template");
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

async fn prove_replica(plan: &Plan) -> Result<i64> {
    let primary_url = format!(
        "redis://{}:{}",
        plan.material.primary_ip, plan.material.primary_port
    );
    let replica_url = replica_url();
    let deadline = std::time::Instant::now() + REPLICA_READY_TIMEOUT;
    loop {
        let primary_info = redis_info(&primary_url, "replication").await?;
        let (replid, primary_offset) = primary_identity(&primary_info)?;
        if replid != plan.material.primary_replid {
            bail!("Priya FalkorDB replication identity changed; plan is stale");
        }
        if let Ok(replica_info) = redis_info(&replica_url, "replication").await {
            if let Ok(lag) = replica_identity(&replica_info, plan, primary_offset) {
                let server = redis_info(&replica_url, "server").await?;
                if server.get("redis_version").map(String::as_str)
                    != Some(plan.material.primary_version.as_str())
                    || graph_module_version(&replica_url).await?
                        != plan.material.graph_module_version
                    || dbsize(&replica_url).await? != plan.material.primary_dbsize
                {
                    bail!("FalkorDB replica server/module/key identity does not match the plan");
                }
                let primary_graphs = graph_inventory(&primary_url).await?;
                if primary_graphs != plan.material.graphs {
                    bail!("Priya graph inventory changed; generate a fresh plan");
                }
                let replica_graphs = graph_inventory(&replica_url).await?;
                if replica_graphs == plan.material.graphs {
                    prove_safe_config(&replica_url).await?;
                    let graph = plan
                        .material
                        .graphs
                        .keys()
                        .next()
                        .context("plan has no graph for write-rejection proof")?;
                    prove_write_rejection(&replica_url, graph).await?;
                    return Ok(lag);
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "FalkorDB replica did not reach exact INFO, graph-count/hash, and write-rejection proof within 10 minutes"
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn restart_and_reprove(plan: &Plan) -> Result<i64> {
    let output = docker_output(&["restart", FALKORDB_CONTAINER]).await?;
    if !output.status.success() {
        bail!("exact FalkorDB replica container restart failed");
    }
    wait_exact_healthy(plan).await?;
    prove_replica(plan).await
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
            "primary={};plan={};image={};backup={};drill={};endpoint=127.0.0.1:{};read_only=yes;stale_data=no;automatic_failover=disabled;read_routing=disabled",
            plan.material.primary_name,
            plan.id(),
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
    attest_firewall(&primary, &target).await?;
    let (_, state) = attest_target(&target, &primary.ip).await?;
    match state {
        TargetState::Absent => {
            start_replica(plan).await?;
            wait_exact_healthy(plan).await?;
        }
        TargetState::ExactHealthy { .. } => {
            // An exact, healthy deployment is the only retry state accepted.
        }
    }
    let first_lag = prove_replica(plan).await?;
    let second_lag = restart_and_reprove(plan).await?;
    attest_target(&target, &primary.ip).await?;
    attest_firewall(&primary, &target).await?;
    register_topology(pool, plan, first_lag.max(second_lag)).await
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

fn decommission_proof(target: &Computer, primary: &Computer) -> String {
    sha256(
        format!(
            "falkordb-decommission-v1\0{}\0{}\0{}\0{}\0{}",
            target.id, target.name, primary.id, FALKORDB_IMAGE, FALKORDB_VOLUME,
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
            validate_existing_container(fingerprint, &attestation.image_id, &primary.ip)?;
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
    if proof != decommission_proof(target, primary) {
        bail!("FalkorDB decommission proof is stale or mismatched");
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
        match (&role, &attestation.container) {
            (Some(role), _) if role != "replica" => {
                bail!("FalkorDB topology role changed before decommission")
            }
            (None, Some(_)) => {
                bail!("unregistered FalkorDB container requires manual audit, not hidden cleanup")
            }
            (_, Some(fingerprint)) => {
                validate_existing_container(fingerprint, &attestation.image_id, &primary.ip)?;
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
    let backup = backup_evidence(pool, &primary).await?;
    Ok(PurgeEvidence {
        target,
        primary,
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

async fn show_status(pool: &sqlx::PgPool, to: &str, primary_name: &str) -> Result<()> {
    reject_vinny(to)?;
    let target = resolve_computer(pool, to).await?;
    let primary = resolve_computer(pool, primary_name).await?;
    if !primary.name.eq_ignore_ascii_case(PRIMARY_NAME) {
        bail!("FalkorDB status authority must be canonical Priya");
    }
    let topology = sqlx::query(
        "SELECT role,status,lag_bytes,last_sync_at,bootstrapped_from_backup_id,notes
           FROM database_replicas WHERE computer_id=$1 AND database_kind='falkordb'",
    )
    .bind(target.id)
    .fetch_optional(pool)
    .await?;
    let attestation = read_target_attestation(&target).await?;
    let container_state = if let Some(fingerprint) = &attestation.container {
        validate_existing_container(fingerprint, &attestation.image_id, &primary.ip)?;
        "exact-healthy"
    } else if attestation.volume_present {
        "decommissioned-volume-preserved"
    } else {
        "absent"
    };
    println!(
        "{CYAN}FalkorDB replica status (read-only; Priya authoritative){RESET}\n  target: {} ({})\n  container: {}\n  durable volume: {}",
        target.name,
        target.id,
        container_state,
        if attestation.volume_present {
            "present"
        } else {
            "absent"
        },
    );
    if let Some(row) = topology {
        println!(
            "  topology: role={} status={} lag_bytes={} last_sync={} backup={}\n  notes: {}",
            row.get::<String, _>("role"),
            row.get::<String, _>("status"),
            row.try_get::<Option<i64>, _>("lag_bytes")?.unwrap_or(-1),
            row.try_get::<Option<DateTime<Utc>>, _>("last_sync_at")?
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
            row.try_get::<Option<Uuid>, _>("bootstrapped_from_backup_id")?
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".into()),
            row.try_get::<Option<String>, _>("notes")?
                .unwrap_or_default(),
        );
    } else {
        println!("  topology: absent");
        if attestation.container.is_none() && attestation.volume_present {
            match purge_evidence(pool, to, primary_name).await {
                Ok(evidence) => println!(
                    "  purge-proof: {}\n  purge is permanent and still requires --yes",
                    evidence.proof()
                ),
                Err(error) => println!("  purge-proof: BLOCKED ({error:#})"),
            }
        }
    }
    println!("  automatic failover: disabled\n  read routing: disabled");
    Ok(())
}

pub async fn handle(pool: &sqlx::PgPool, command: FleetDbFalkordbReplicaCommand) -> Result<()> {
    match command {
        FleetDbFalkordbReplicaCommand::Plan { to, primary } => {
            let plan = build_plan(pool, &to, &primary).await?;
            println!(
                "{CYAN}FalkorDB replica plan (read-only; no promotion/failover/read routing){RESET}\n  target: {} ({})\n  primary authority: {} ({}:{PRIMARY_PORT})\n  replica endpoint: 127.0.0.1:{REPLICA_PORT}\n  immutable image: {FALKORDB_IMAGE}\n  graphs: {}\n  restore-proven distributed backup: {} (drill {})\n  source firewall: exact target allow + IPv4/IPv6 default deny + Docker-persistent unit\n  target state: {:?}\n  plan-id: {}\n\nApply with:\n  ff fleet db falkordb-replica apply --to {} --primary {} --plan-id {} --yes",
                plan.material.target_name,
                plan.material.target_id,
                plan.material.primary_name,
                plan.material.primary_ip,
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
            if plan.id() != plan_id {
                bail!("FalkorDB replica plan-id is stale or mismatched; run plan again");
            }
            let target = plan.target();
            let task = enqueue_lifecycle_action(pool, &target, "apply", local_apply_command(&plan))
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
            decommission_preflight(pool, &target, &primary).await?;
            let proof = decommission_proof(&target, &primary);
            let task = enqueue_lifecycle_action(
                pool,
                &target,
                "decommission",
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
            if !yes {
                bail!("permanent FalkorDB volume purge requires --yes; no changes made");
            }
            let evidence = purge_evidence(pool, &to, &primary).await?;
            if evidence.proof() != proof {
                bail!("FalkorDB purge proof is stale or mismatched; run status again");
            }
            let task = enqueue_lifecycle_action(
                pool,
                &evidence.target,
                "purge-volume",
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
            if plan.id() != plan_id {
                bail!("FalkorDB replica plan-id is stale or mismatched");
            }
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
                node_sample_sha256: "b".repeat(64),
                relationship_sample_sha256: "c".repeat(64),
            },
        );
        Plan {
            material: PlanMaterial {
                version: "falkordb-replica-plan-v1",
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
                primary_used_memory: 1024,
                graphs,
                backup: backup(),
                target_state: TargetState::Absent,
                image: FALKORDB_IMAGE,
                primary_port: PRIMARY_PORT,
                replica_port: REPLICA_PORT,
                firewall_policy: "ff-falkordb-firewall-v1",
                automatic_failover: false,
                read_routing: false,
            },
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
    }

    #[test]
    fn graph_hash_excludes_only_statistics_and_binds_rows() {
        let first = redis::Value::Array(vec![
            redis::Value::Array(vec![redis::Value::BulkString(b"id(n)".to_vec())]),
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
            graph_payload_hash(&first).unwrap(),
            graph_payload_hash(&timing_changed).unwrap()
        );
        let mut row_changed = first.clone();
        if let redis::Value::Array(parts) = &mut row_changed {
            parts[1] = redis::Value::Array(vec![redis::Value::Array(vec![redis::Value::Int(2)])]);
        }
        assert_ne!(
            graph_payload_hash(&first).unwrap(),
            graph_payload_hash(&row_changed).unwrap()
        );
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
        let receipt = serde_json::json!({
            "proof": "falkordb_exact_restore_v1",
            "input_checksum_sha256": checksum,
            "image_reference": FALKORDB_IMAGE,
            "image_id": format!("sha256:{}", "b".repeat(64)),
            "network": "none",
            "query_mode": "GRAPH.RO_QUERY",
            "expected_min_keys": 1,
            "observed_keys": 3,
            "expected_min_graph_nodes": 1,
            "observed_graphs": 3,
            "observed_graph_nodes": 56
        });
        let detail = format!("restore drill passed; receipt={receipt}; capacity=ok");
        restore_receipt(&detail, &"a".repeat(64)).unwrap();
        assert!(restore_receipt(&detail, &"c".repeat(64)).is_err());
        assert!(restore_receipt(
            &detail.replace(FALKORDB_IMAGE, "falkordb:latest"),
            &"a".repeat(64)
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
            "FIREWALL_JSON_BEGIN\n{status}\nFIREWALL_JSON_END\nSERVICE_AFTER=network-online.target docker.service\nSERVICE_REQUIRES=docker.service\nSERVICE_PARTOF=docker.service\n"
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
        let fingerprint = format!(
            "sha256:image|true|healthy|unless-stopped|host|{ports}|{env}|{FALKORDB_VOLUME}:/var/lib/falkordb/data;"
        );
        validate_existing_container(&fingerprint, "sha256:image", "192.168.5.104").unwrap();
        assert!(validate_existing_container(
            &fingerprint.replace("--bind 127.0.0.1", "--bind 0.0.0.0"),
            "sha256:image",
            "192.168.5.104"
        )
        .is_err());
        assert!(validate_existing_container(
            &fingerprint.replace(
                "replica-serve-stale-data no",
                "replica-serve-stale-data yes"
            ),
            "sha256:image",
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
    fn compose_is_immutable_loopback_durable_read_only_and_failover_free() {
        let compose = include_str!("../../../deploy/docker-compose.falkordb-follower.yml");
        assert!(compose.contains(FALKORDB_IMAGE));
        assert!(compose.contains("pull_policy: never"));
        assert!(compose.contains("network_mode: host"));
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
