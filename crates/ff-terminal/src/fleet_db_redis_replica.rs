//! Fail-closed, operator-initiated Redis streaming-replica bootstrap.
//!
//! This module intentionally has no promotion or failover path. The replica is
//! loopback-only, read-only, and unavailable while stale; Priya remains the
//! explicit authority until a separate, reviewed cutover design exists.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::{
    CYAN, FleetDbRedisReplicaCommand, GREEN, RESET, YELLOW, shell_escape_single, whoami_tag,
};

const PRIMARY_PORT: u16 = 56379;
const REPLICA_PORT: u16 = 56380;
const REPLICA_URL: &str = "redis://127.0.0.1:56380";
const REDIS_IMAGE: &str =
    "redis@sha256:6ab0b6e7381779332f97b8ca76193e45b0756f38d4c0dcda72dbb3c32061ab99";
const REDIS_CONTAINER: &str = "forgefleet-redis-replica";
const REDIS_VOLUME: &str = "forgefleet-redis-replica-data";
const REDIS_COMPOSE: &str = "deploy/docker-compose.redis-follower.yml";
const MAX_BACKUP_AGE_HOURS: i64 = 24;
const MAX_REPLICA_LAG_BYTES: i64 = 16 * 1024 * 1024;
const REDIS_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const COMPOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const REPLICA_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

const FIND_EXISTING_TASK_SQL: &str = "SELECT id FROM fleet_tasks WHERE task_class='deferred' AND summary=$1 AND status IN ('pending','dispatchable','running') AND payload->'deferred_payload'->>'command' LIKE $2 ORDER BY created_at DESC LIMIT 1";
const INSERT_TASK_SQL: &str = "INSERT INTO fleet_tasks (task_type,summary,payload,priority,requires_capability,status,created_at,task_class) VALUES ('shell',$1,$2,50,$3,'pending',NOW(),'deferred') RETURNING id";
const UPSERT_PRIMARY_SQL: &str = "INSERT INTO database_replicas (computer_id,database_kind,role,status,lag_bytes,last_sync_at,notes) VALUES ($1,'redis','primary','running',0,NOW(),$2) ON CONFLICT (computer_id,database_kind) DO UPDATE SET role='primary',status='running',lag_bytes=0,last_sync_at=NOW(),notes=$2";
const UPSERT_REPLICA_SQL: &str = "INSERT INTO database_replicas (computer_id,database_kind,role,status,lag_bytes,last_sync_at,notes) VALUES ($1,'redis','replica','running',$2,NOW(),$3) ON CONFLICT (computer_id,database_kind) DO UPDATE SET role='replica',status='running',lag_bytes=$2,last_sync_at=NOW(),notes=$3";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RedisEndpoint {
    host: String,
    port: u16,
}

#[derive(Debug, Clone)]
struct Plan {
    target_id: Uuid,
    target_name: String,
    target_ip: String,
    primary_id: Uuid,
    primary_name: String,
    primary_ip: String,
    backup_id: Uuid,
    primary_replid: String,
    primary_version: String,
}

impl Plan {
    fn id(&self) -> String {
        let canonical = format!(
            "redis-replica-v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            self.target_id,
            self.target_name,
            self.target_ip,
            self.primary_id,
            self.primary_name,
            self.primary_ip,
            PRIMARY_PORT,
            REPLICA_PORT,
            self.backup_id,
            self.primary_replid,
            self.primary_version,
            REDIS_IMAGE,
        );
        Sha256::digest(canonical.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn validate_authority_url(raw: &str, expected_ip: &str) -> Result<RedisEndpoint> {
    let parsed = reqwest::Url::parse(raw).context("parse Redis authority URL")?;
    if parsed.scheme() != "redis" {
        bail!("Redis replica bootstrap currently requires a redis:// authority URL");
    }
    // v1 deliberately refuses credentials rather than copying them into
    // Compose environment, argv, or container metadata. Add secret-file ACL
    // support before enabling an authenticated primary.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!(
            "credential-bearing Redis authority URLs are unsupported by the safe replica workflow"
        );
    }
    let host = parsed
        .host_str()
        .filter(|host| !host.trim().is_empty())
        .context("Redis authority URL has no host")?
        .to_string();
    let port = parsed.port().unwrap_or(6379);
    if host != expected_ip || port != PRIMARY_PORT {
        bail!(
            "Redis authority endpoint does not match the named primary's LAN IP and canonical port"
        );
    }
    Ok(RedisEndpoint { host, port })
}

fn parse_replication_info(raw: &str) -> BTreeMap<String, String> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_once(':')
                .map(|(key, value)| (key.to_string(), value.to_string()))
        })
        .collect()
}

async fn redis_connection(url: &str) -> Result<redis::aio::MultiplexedConnection> {
    let client = redis::Client::open(url).context("open Redis client")?;
    tokio::time::timeout(REDIS_IO_TIMEOUT, client.get_multiplexed_async_connection())
        .await
        .context("Redis connection timed out")?
        .context("connect Redis")
}

async fn redis_info(url: &str, section: &str) -> Result<BTreeMap<String, String>> {
    let mut connection = redis_connection(url).await?;
    let raw: String = tokio::time::timeout(
        REDIS_IO_TIMEOUT,
        redis::cmd("INFO").arg(section).query_async(&mut connection),
    )
    .await
    .context("Redis INFO replication timed out")?
    .context("query Redis INFO replication")?;
    Ok(parse_replication_info(&raw))
}

async fn replication_info(url: &str) -> Result<BTreeMap<String, String>> {
    redis_info(url, "replication").await
}

async fn redis_config_value(url: &str, key: &str) -> Result<String> {
    let mut connection = redis_connection(url).await?;
    let values: Vec<String> = tokio::time::timeout(
        REDIS_IO_TIMEOUT,
        redis::cmd("CONFIG")
            .arg("GET")
            .arg(key)
            .query_async(&mut connection),
    )
    .await
    .context("Redis CONFIG GET timed out")?
    .context("query Redis configuration")?;
    if values.len() != 2 || values[0] != key {
        bail!("Redis CONFIG GET returned an unexpected response for '{key}'");
    }
    Ok(values[1].clone())
}

fn primary_identity(info: &BTreeMap<String, String>) -> Result<(String, i64)> {
    if info.get("role").map(String::as_str) != Some("master") {
        bail!("named Redis authority does not report role=master");
    }
    let replid = info
        .get("master_replid")
        .filter(|value| !value.is_empty())
        .context("Redis primary has no replication ID")?
        .clone();
    let offset = info
        .get("master_repl_offset")
        .context("Redis primary has no replication offset")?
        .parse::<i64>()
        .context("parse Redis primary replication offset")?;
    if offset < 0 {
        bail!("Redis primary reported a negative replication offset");
    }
    Ok((replid, offset))
}

async fn build_plan(pool: &sqlx::PgPool, to: &str, primary: &str) -> Result<Plan> {
    if to.eq_ignore_ascii_case(primary) {
        bail!("Redis replica target and primary must be different nodes");
    }

    let target = sqlx::query(
        "SELECT id,name,primary_ip,status,os_family,reservation_state,
                last_seen_at >= NOW() - interval '5 minutes' AS fresh
           FROM computers WHERE lower(name)=lower($1)",
    )
    .bind(to)
    .fetch_optional(pool)
    .await?
    .context("target is not an enrolled fleet node")?;
    let primary_row = sqlx::query(
        "SELECT id,name,primary_ip,status,
                last_seen_at >= NOW() - interval '5 minutes' AS fresh
           FROM computers WHERE lower(name)=lower($1)",
    )
    .bind(primary)
    .fetch_optional(pool)
    .await?
    .context("primary is not an enrolled fleet node")?;

    if target.get::<String, _>("status") != "online" || !target.get::<bool, _>("fresh") {
        bail!("Redis replica target must be online with a heartbeat from the last 5 minutes");
    }
    let os_family = target
        .try_get::<Option<String>, _>("os_family")?
        .unwrap_or_default();
    if !os_family.starts_with("linux") {
        bail!("Redis replica bootstrap currently requires a Linux target");
    }
    let reservation = target
        .try_get::<Option<String>, _>("reservation_state")?
        .unwrap_or_else(|| "available".to_string());
    if reservation != "available" {
        bail!("Redis replica target is reserved; release it before planning infrastructure");
    }
    if primary_row.get::<String, _>("status") != "online" || !primary_row.get::<bool, _>("fresh") {
        bail!("Redis primary must be online with a heartbeat from the last 5 minutes");
    }

    let target_ip = target
        .try_get::<Option<String>, _>("primary_ip")?
        .filter(|value| !value.trim().is_empty())
        .context("target has no usable LAN IP")?;
    let primary_ip = primary_row
        .try_get::<Option<String>, _>("primary_ip")?
        .filter(|value| !value.trim().is_empty())
        .context("primary has no usable LAN IP")?;
    if target_ip == primary_ip {
        bail!("Redis replica target and primary resolve to the same LAN IP");
    }
    let target_id: Uuid = target.get("id");
    let primary_id: Uuid = primary_row.get("id");

    if let Some(role) = sqlx::query_scalar::<_, String>(
        "SELECT role FROM database_replicas WHERE computer_id=$1 AND database_kind='redis'",
    )
    .bind(target_id)
    .fetch_optional(pool)
    .await?
    {
        if role != "replica" {
            bail!("target already has an incompatible Redis topology role '{role}'");
        }
    }

    let registered_primaries: Vec<String> = sqlx::query_scalar(
        "SELECT c.name FROM database_replicas r JOIN computers c ON c.id=r.computer_id
          WHERE r.database_kind='redis' AND r.role='primary' ORDER BY c.name",
    )
    .fetch_all(pool)
    .await?;
    if registered_primaries.len() > 1 {
        bail!("multiple Redis primary authority rows exist; reconcile before planning");
    }
    if registered_primaries
        .first()
        .is_some_and(|name| !name.eq_ignore_ascii_case(primary))
    {
        bail!("explicit Redis primary disagrees with the registered topology authority");
    }

    let backup_policy = sqlx::query(
        "SELECT source_host,encrypt,enabled FROM fleet_backup_config WHERE kind='redis'",
    )
    .fetch_optional(pool)
    .await?
    .context("Redis backup policy is missing")?;
    if !backup_policy.get::<bool, _>("enabled") || !backup_policy.get::<bool, _>("encrypt") {
        bail!("Redis backup policy must be enabled and encrypted before replica bootstrap");
    }
    if let Some(source) = backup_policy
        .try_get::<Option<String>, _>("source_host")?
        .filter(|value| !value.trim().is_empty())
    {
        if !source.eq_ignore_ascii_case(primary) {
            bail!("Redis backup source policy disagrees with the named primary");
        }
    }
    let backup_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM backups
          WHERE database_kind='redis' AND source_computer_id=$2
            AND size_bytes > 0
            AND checksum_sha256 ~ '^[0-9a-fA-F]{64}$'
            AND file_name LIKE '%.age'
            AND created_at BETWEEN NOW() - make_interval(hours => $1::int) AND NOW()
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(MAX_BACKUP_AGE_HOURS)
    .bind(primary_id)
    .fetch_optional(pool)
    .await?
    .context("no recent encrypted, checksummed Redis backup from the named primary")?;

    let authority_url: String = sqlx::query_scalar(
        "SELECT redis_url FROM fleet_leader_state WHERE singleton_key='current'",
    )
    .fetch_optional(pool)
    .await?
    .flatten()
    .context("fleet leader state has no Redis authority URL")?;
    validate_authority_url(&authority_url, &primary_ip)?;
    let primary_info = replication_info(&authority_url)
        .await
        .context("probe named Redis primary")?;
    let (primary_replid, _) = primary_identity(&primary_info)?;
    let primary_version = redis_info(&authority_url, "server")
        .await
        .context("probe Redis primary version")?
        .get("redis_version")
        .filter(|version| version.starts_with("7."))
        .context("Redis primary is not a supported Redis 7 server")?
        .clone();

    Ok(Plan {
        target_id,
        target_name: target.get("name"),
        target_ip,
        primary_id,
        primary_name: primary_row.get("name"),
        primary_ip,
        backup_id,
        primary_replid,
        primary_version,
    })
}

fn local_apply_command(plan: &Plan) -> String {
    format!(
        "cd \"$HOME/projects/forge-fleet\" && \"$HOME/.local/bin/ff\" fleet db redis-replica local-apply --to {} --primary {} --plan-id {}",
        shell_escape_single(&plan.target_name),
        shell_escape_single(&plan.primary_name),
        shell_escape_single(&plan.id()),
    )
}

async fn enqueue_apply(pool: &sqlx::PgPool, plan: &Plan) -> Result<String> {
    let title = format!("bootstrap Redis replica on {}", plan.target_name);
    let payload = serde_json::json!({
        "command": local_apply_command(plan),
        "summary": "loopback-only read-only Redis replica bootstrap",
    });
    let required_caps = serde_json::json!([]);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(plan.id())
        .execute(&mut *tx)
        .await?;
    let like = format!("%--plan-id {}%", plan.id());
    let id = if let Some(id) = sqlx::query_scalar::<_, Uuid>(FIND_EXISTING_TASK_SQL)
        .bind(&title)
        .bind(&like)
        .fetch_optional(&mut *tx)
        .await?
    {
        id
    } else {
        let canonical_payload = serde_json::json!({
            "deferred_payload": payload,
            "created_by": whoami_tag(),
            "kind": "shell",
            "trigger_type": "node_online",
            "trigger_spec": {"node": plan.target_name},
            "preferred_node": plan.target_name,
            "required_caps": required_caps,
            "attempts": 0,
            "max_attempts": 1,
        });
        sqlx::query_scalar(INSERT_TASK_SQL)
            .bind(&title)
            .bind(&canonical_payload)
            .bind(&required_caps)
            .fetch_one(&mut *tx)
            .await?
    };
    tx.commit().await?;
    Ok(id.to_string())
}

async fn docker_output(args: &[&str]) -> Result<std::process::Output> {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("docker")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Docker command timed out")?
    .context("run Docker command")
}

fn expected_redis_command(plan: &Plan) -> Vec<String> {
    [
        "redis-server",
        "--port",
        "56380",
        "--bind",
        "127.0.0.1",
        "--protected-mode",
        "yes",
        "--replicaof",
        plan.primary_ip.as_str(),
        "56379",
        "--replica-read-only",
        "yes",
        "--replica-serve-stale-data",
        "no",
        "--save",
        "60",
        "1",
        "--appendonly",
        "yes",
        "--appendfsync",
        "everysec",
        "--loglevel",
        "warning",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn validate_existing_container(fingerprint: &str, image_id: &str, plan: &Plan) -> Result<()> {
    let mut fields = fingerprint.trim().splitn(4, '|');
    let container_image_id = fields.next().unwrap_or_default();
    let network_mode = fields.next().unwrap_or_default();
    let command = fields.next().unwrap_or_default();
    let mounts = fields.next().unwrap_or_default();
    if container_image_id != image_id || network_mode != "host" {
        bail!("existing Redis replica container has the wrong image or network mode");
    }
    let command: Vec<String> =
        serde_json::from_str(command).context("parse existing Redis replica container command")?;
    if command != expected_redis_command(plan) {
        bail!("existing Redis replica container command is not the exact safe plan");
    }
    if !mounts.contains(&format!("{REDIS_VOLUME}:/data")) {
        bail!("existing Redis replica container is not using the expected durable volume");
    }
    Ok(())
}

async fn preflight_docker(plan: &Plan) -> Result<bool> {
    let compose = std::path::Path::new(REDIS_COMPOSE);
    if !compose.is_file() {
        bail!("run from a ForgeFleet checkout containing the Redis follower compose template");
    }
    let docker = docker_output(&["info", "--format", "{{.ServerVersion}}"])
        .await
        .context("Docker daemon preflight")?;
    if !docker.status.success() || String::from_utf8_lossy(&docker.stdout).trim().is_empty() {
        bail!("Docker daemon preflight failed");
    }
    let image = docker_output(&["image", "inspect", "--format", "{{.Id}}", REDIS_IMAGE])
        .await
        .context("pinned Redis image preflight")?;
    if !image.status.success() {
        bail!("exact pinned Redis image is not available on the target; pre-stage it first");
    }
    let image_id = String::from_utf8_lossy(&image.stdout).trim().to_string();
    if image_id.is_empty() {
        bail!("Docker returned an empty identity for the pinned Redis image");
    }
    let version = docker_output(&[
        "run",
        "--rm",
        "--network",
        "none",
        REDIS_IMAGE,
        "redis-server",
        "--version",
    ])
    .await
    .context("pinned Redis 7 runtime preflight")?;
    if !version.status.success() || !String::from_utf8_lossy(&version.stdout).contains("v=7.") {
        bail!("pinned Redis image does not attest as Redis 7");
    }

    let existing = docker_output(&[
        "container",
        "inspect",
        "--format",
        "{{.Image}}|{{.HostConfig.NetworkMode}}|{{json .Config.Cmd}}|{{range .Mounts}}{{.Name}}:{{.Destination}};{{end}}",
        REDIS_CONTAINER,
    ])
    .await?;
    if existing.status.success() {
        validate_existing_container(&String::from_utf8_lossy(&existing.stdout), &image_id, plan)?;
        return Ok(true);
    }

    let volume = docker_output(&["volume", "inspect", REDIS_VOLUME]).await?;
    if volume.status.success() {
        bail!(
            "Redis replica volume exists without its attested container; preserve and audit it before retrying"
        );
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", REPLICA_PORT))
        .await
        .context("Redis replica loopback port 56380 is already in use")?;
    drop(listener);

    let config = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("docker")
            .args(["compose", "-f", REDIS_COMPOSE, "config", "--quiet"])
            .env("REDIS_PRIMARY_HOST", &plan.primary_ip)
            .env("REDIS_PRIMARY_PORT", PRIMARY_PORT.to_string())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("Redis follower Compose validation timed out")?
    .context("validate Redis follower Compose")?;
    if !config.success() {
        bail!("Redis follower Compose validation failed");
    }
    Ok(false)
}

async fn start_replica(plan: &Plan) -> Result<()> {
    let status = tokio::time::timeout(
        COMPOSE_TIMEOUT,
        tokio::process::Command::new("docker")
            .args([
                "compose",
                "-f",
                REDIS_COMPOSE,
                "up",
                "-d",
                "--pull",
                "never",
            ])
            .env("REDIS_PRIMARY_HOST", &plan.primary_ip)
            .env("REDIS_PRIMARY_PORT", PRIMARY_PORT.to_string())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("Redis follower Compose timed out")?
    .context("start Redis follower Compose")?;
    if !status.success() {
        bail!("Redis follower Compose failed; the durable volume was preserved for audit");
    }
    Ok(())
}

async fn prove_replica(plan: &Plan, authority_url: &str) -> Result<i64> {
    let deadline = std::time::Instant::now() + REPLICA_READY_TIMEOUT;
    loop {
        let primary = replication_info(authority_url)
            .await
            .context("re-probe Redis primary during replica bootstrap")?;
        let (replid, primary_offset) = primary_identity(&primary)?;
        if replid != plan.primary_replid {
            bail!("Redis primary replication identity changed during bootstrap; plan is stale");
        }

        if let Ok(replica) = replication_info(REPLICA_URL).await {
            let replica_offset = replica
                .get("slave_repl_offset")
                .or_else(|| replica.get("master_repl_offset"))
                .and_then(|value| value.parse::<i64>().ok());
            let identity_matches = replica.get("master_replid") == Some(&plan.primary_replid);
            let ready = replica.get("role").map(String::as_str) == Some("slave")
                && replica.get("master_host").map(String::as_str) == Some(plan.primary_ip.as_str())
                && replica.get("master_port").map(String::as_str) == Some("56379")
                && replica.get("master_link_status").map(String::as_str) == Some("up")
                && replica.get("master_sync_in_progress").map(String::as_str) == Some("0")
                && identity_matches;
            if ready {
                if let Some(replica_offset) = replica_offset {
                    let lag = primary_offset.saturating_sub(replica_offset).max(0);
                    if lag <= MAX_REPLICA_LAG_BYTES {
                        return Ok(lag);
                    }
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "Redis replica did not become linked, identity-matched, and sufficiently caught up within 10 minutes"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

async fn prove_safe_config() -> Result<()> {
    if redis_config_value(REPLICA_URL, "replica-read-only").await? != "yes" {
        bail!("Redis replica is not configured replica-read-only=yes");
    }
    if redis_config_value(REPLICA_URL, "replica-serve-stale-data").await? != "no" {
        bail!("Redis replica does not refuse stale reads");
    }
    if redis_config_value(REPLICA_URL, "protected-mode").await? != "yes" {
        bail!("Redis replica protected mode is not enabled");
    }
    let bind = redis_config_value(REPLICA_URL, "bind").await?;
    if !bind
        .split_whitespace()
        .any(|address| address == "127.0.0.1")
    {
        bail!("Redis replica is not bound to loopback");
    }
    Ok(())
}

async fn register_topology(pool: &sqlx::PgPool, plan: &Plan, lag_bytes: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Serialize the zero-primary bootstrap case as well as updates to an
    // existing row; SELECT ... FOR UPDATE alone cannot lock an absent row.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('forgefleet-redis-primary-authority'))")
        .execute(&mut *tx)
        .await?;
    let authority_url: String = sqlx::query_scalar(
        "SELECT redis_url FROM fleet_leader_state WHERE singleton_key='current' FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?
    .flatten()
    .context("fleet leader state lost Redis authority during bootstrap")?;
    validate_authority_url(&authority_url, &plan.primary_ip)?;

    let registered_primary: Vec<Uuid> = sqlx::query_scalar(
        "SELECT computer_id FROM database_replicas
          WHERE database_kind='redis' AND role='primary' FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    if registered_primary.len() > 1
        || registered_primary
            .first()
            .is_some_and(|id| *id != plan.primary_id)
    {
        bail!("Redis primary topology authority changed during replica bootstrap");
    }

    sqlx::query(UPSERT_PRIMARY_SQL)
        .bind(plan.primary_id)
        .bind(format!(
            "authority=fleet_leader_state;plan={};endpoint={}:{};redis_version={};automatic_failover=disabled",
            plan.id(),
            plan.primary_ip,
            PRIMARY_PORT,
            plan.primary_version,
        ))
        .execute(&mut *tx)
        .await?;
    sqlx::query(UPSERT_REPLICA_SQL)
        .bind(plan.target_id)
        .bind(lag_bytes)
        .bind(format!(
            "primary={};plan={};image={};backup_evidence={};endpoint=loopback-only;automatic_failover=disabled",
            plan.primary_name,
            plan.id(),
            REDIS_IMAGE,
            plan.backup_id,
        ))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn local_apply(pool: &sqlx::PgPool, plan: &Plan) -> Result<()> {
    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
    if !me.eq_ignore_ascii_case(&plan.target_name) {
        bail!(
            "local Redis replica apply must run on '{}' (running on '{me}')",
            plan.target_name
        );
    }

    let authority_url: String = sqlx::query_scalar(
        "SELECT redis_url FROM fleet_leader_state WHERE singleton_key='current'",
    )
    .fetch_optional(pool)
    .await?
    .flatten()
    .context("fleet leader state has no Redis authority URL")?;
    validate_authority_url(&authority_url, &plan.primary_ip)?;

    let _existing = preflight_docker(plan).await?;
    start_replica(plan).await?;
    let lag_bytes = prove_replica(plan, &authority_url).await?;
    prove_safe_config().await?;

    // Re-attest both immutable container shape and primary identity immediately
    // before publishing the topology row.
    preflight_docker(plan).await?;
    let primary = replication_info(&authority_url).await?;
    let (replid, _) = primary_identity(&primary)?;
    if replid != plan.primary_replid {
        bail!("Redis primary identity changed before topology registration");
    }
    register_topology(pool, plan, lag_bytes).await
}

pub async fn handle(pool: &sqlx::PgPool, command: FleetDbRedisReplicaCommand) -> Result<()> {
    match command {
        FleetDbRedisReplicaCommand::Plan { to, primary } => {
            let plan = build_plan(pool, &to, &primary).await?;
            println!(
                "{CYAN}Redis replica plan (read-only; no failover){RESET}\n  target: {} ({})\n  primary authority: {} ({}:{PRIMARY_PORT}, Redis {})\n  replica endpoint: loopback-only 127.0.0.1:{REPLICA_PORT}\n  immutable image: {REDIS_IMAGE}\n  encrypted backup safety evidence: {}\n  plan-id: {}\n\nApply with:\n  ff fleet db redis-replica apply --to {} --primary {} --plan-id {} --yes",
                plan.target_name,
                plan.target_ip,
                plan.primary_name,
                plan.primary_ip,
                plan.primary_version,
                plan.backup_id,
                plan.id(),
                plan.target_name,
                plan.primary_name,
                plan.id(),
            );
        }
        FleetDbRedisReplicaCommand::Apply {
            to,
            primary,
            plan_id,
            yes,
        } => {
            if !yes {
                bail!("Redis replica apply requires --yes; no changes made");
            }
            let plan = build_plan(pool, &to, &primary).await?;
            if plan.id() != plan_id {
                bail!("Redis replica plan-id is stale or mismatched; run plan again");
            }
            let id = enqueue_apply(pool, &plan).await?;
            println!(
                "{GREEN}✓{RESET} queued loopback-only Redis replica bootstrap on '{}' as deferred task {id}; no failover was enabled",
                plan.target_name
            );
        }
        FleetDbRedisReplicaCommand::LocalApply {
            to,
            primary,
            plan_id,
        } => {
            let plan = build_plan(pool, &to, &primary).await?;
            if plan.id() != plan_id {
                bail!("Redis replica plan-id is stale or mismatched");
            }
            println!(
                "{YELLOW}Applying Redis replica plan {} on {}; Priya remains authoritative{RESET}",
                plan.id(),
                plan.target_name
            );
            local_apply(pool, &plan).await?;
            println!(
                "{GREEN}✓ Redis replica is linked, caught up, loopback-only, read-only, and registered; automatic failover remains disabled{RESET}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            target_id: Uuid::nil(),
            target_name: "marcus".into(),
            target_ip: "192.168.5.102".into(),
            primary_id: Uuid::from_u128(1),
            primary_name: "priya".into(),
            primary_ip: "192.168.5.104".into(),
            backup_id: Uuid::from_u128(2),
            primary_replid: "0123456789abcdef0123456789abcdef01234567".into(),
            primary_version: "7.4.7".into(),
        }
    }

    #[test]
    fn authority_url_is_exact_and_never_accepts_embedded_credentials() {
        assert_eq!(
            validate_authority_url("redis://192.168.5.104:56379", "192.168.5.104").unwrap(),
            RedisEndpoint {
                host: "192.168.5.104".into(),
                port: 56379,
            }
        );
        assert!(validate_authority_url("redis://192.168.5.105:56379", "192.168.5.104").is_err());
        assert!(validate_authority_url("redis://192.168.5.104:6379", "192.168.5.104").is_err());
        assert!(
            validate_authority_url("redis://user:secret@192.168.5.104:56379", "192.168.5.104")
                .is_err()
        );
    }

    #[test]
    fn replication_info_parser_ignores_sections_and_preserves_values() {
        let info = parse_replication_info(
            "# Replication\r\nrole:slave\r\nmaster_host:192.168.5.104\r\nmaster_link_status:up\r\n",
        );
        assert_eq!(info.get("role").map(String::as_str), Some("slave"));
        assert_eq!(
            info.get("master_host").map(String::as_str),
            Some("192.168.5.104")
        );
        assert_eq!(
            info.get("master_link_status").map(String::as_str),
            Some("up")
        );
    }

    #[test]
    fn primary_identity_fails_closed() {
        let mut info = BTreeMap::new();
        info.insert("role".into(), "master".into());
        info.insert("master_replid".into(), "abc".into());
        info.insert("master_repl_offset".into(), "42".into());
        assert_eq!(primary_identity(&info).unwrap(), ("abc".into(), 42));
        info.insert("role".into(), "slave".into());
        assert!(primary_identity(&info).is_err());
    }

    #[test]
    fn plan_id_is_stable_and_sensitive_to_authority_identity() {
        let mut plan = plan();
        let first = plan.id();
        assert_eq!(first, plan.id());
        plan.primary_replid.push('x');
        assert_ne!(first, plan.id());
        let second = plan.id();
        plan.backup_id = Uuid::from_u128(3);
        assert_ne!(second, plan.id());
    }

    #[test]
    fn deferred_command_contains_no_url_or_credentials() {
        let command = local_apply_command(&plan());
        assert!(command.contains("redis-replica local-apply"));
        assert!(!command.contains("redis://"));
        assert!(!command.to_lowercase().contains("password"));
        assert!(!command.to_lowercase().contains("secret"));
    }

    #[test]
    fn topology_sql_can_only_register_redis_primary_and_replica() {
        assert!(UPSERT_PRIMARY_SQL.contains("'redis','primary','running'"));
        assert!(UPSERT_REPLICA_SQL.contains("'redis','replica','running'"));
        assert!(!UPSERT_PRIMARY_SQL.to_lowercase().contains("promot"));
        assert!(!UPSERT_REPLICA_SQL.to_lowercase().contains("promot"));
    }

    #[test]
    fn compose_is_pinned_loopback_read_only_and_has_no_failover() {
        let compose = include_str!("../../../deploy/docker-compose.redis-follower.yml");
        assert!(compose.contains(REDIS_IMAGE));
        assert!(compose.contains("network_mode: host"));
        assert!(compose.contains("--bind\n      - 127.0.0.1"));
        assert!(compose.contains("--protected-mode\n      - \"yes\""));
        assert!(compose.contains("--replica-read-only\n      - \"yes\""));
        assert!(compose.contains("--replica-serve-stale-data\n      - \"no\""));
        assert!(compose.contains("name: forgefleet-redis-replica-data"));
        assert!(!compose.to_lowercase().contains("redis-sentinel"));
        assert!(!compose.to_lowercase().contains("sentinel:"));
        assert!(!compose.to_lowercase().contains("replicaof no one"));
        assert!(!compose.to_lowercase().contains("masterauth"));
    }

    #[test]
    fn exact_existing_container_shape_is_accepted() {
        let plan = plan();
        let command = serde_json::to_string(&expected_redis_command(&plan)).unwrap();
        let fingerprint =
            format!("sha256:image|host|{command}|forgefleet-redis-replica-data:/data;");
        validate_existing_container(&fingerprint, "sha256:image", &plan).unwrap();
        assert!(
            validate_existing_container(
                &fingerprint.replace("|host|", "|bridge|"),
                "sha256:image",
                &plan
            )
            .is_err()
        );
    }
}
