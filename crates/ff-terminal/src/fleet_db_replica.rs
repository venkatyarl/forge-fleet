//! Supported, fail-closed LAN PostgreSQL physical-replica bootstrap.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::postgres::PgConnectOptions;
use std::io::Write;
use std::str::FromStr;
use uuid::Uuid;

use crate::{CYAN, FleetDbReplicaCommand, GREEN, RESET, YELLOW, shell_escape_single, whoami_tag};

const PORT: i32 = 55432;
const MAX_BACKUP_AGE_HOURS: i64 = 24;
const MAX_POSTCHECK_LAG_BYTES: i64 = 1 << 30;
const PREFLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const COMPOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
struct Plan {
    target_id: Uuid,
    target_name: String,
    target_ip: String,
    primary_name: String,
    primary_ip: String,
    slot: String,
    backup_id: Uuid,
    pg_major: i32,
}

impl Plan {
    fn id(&self) -> String {
        let canonical = format!(
            "v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            self.target_id,
            self.target_name,
            self.target_ip,
            self.primary_name,
            self.primary_ip,
            self.slot,
            self.backup_id,
            self.pg_major
        );
        Sha256::digest(canonical.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn physical_slot(id: Uuid) -> String {
    format!("ff_{}", id.simple())
}

fn connected_primary_matches(
    connected_ip: Option<&str>,
    configured_host: Option<&str>,
    named_primary_ip: &str,
    local_worker_matches: bool,
    server_is_primary: bool,
) -> bool {
    if !server_is_primary {
        return false;
    }
    if configured_host.is_some_and(|host| host.eq_ignore_ascii_case(named_primary_ip)) {
        return true;
    }
    match connected_ip {
        Some(ip) if ip == named_primary_ip => true,
        Some("127.0.0.1" | "::1") | None => local_worker_matches,
        Some(_) => false,
    }
}

fn configured_database_host() -> Result<String> {
    let config_root = std::env::var_os("FORGEFLEET_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".forgefleet")))
        .context("no ForgeFleet home directory")?;
    let config_path = config_root.join("fleet.toml");
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let config: ff_core::config::FleetConfig =
        toml::from_str(&raw).with_context(|| format!("parse {}", config_path.display()))?;
    let options = PgConnectOptions::from_str(config.database.url.trim())
        .context("parse configured PostgreSQL URL")?;
    Ok(options.get_host().to_string())
}

fn pgpass_field(value: &str) -> Result<String> {
    if value.contains(['\n', '\r', '\0']) {
        bail!("replication credential contains an invalid control character");
    }
    Ok(value.replace('\\', "\\\\").replace(':', "\\:"))
}

fn write_replication_passfile(plan: &Plan, password: &str) -> Result<std::path::PathBuf> {
    let secret_dir = dirs::home_dir()
        .context("HOME not set")?
        .join(".forgefleet")
        .join("secrets");
    std::fs::create_dir_all(&secret_dir)
        .with_context(|| format!("create {}", secret_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", secret_dir.display()))?;

        let path = secret_dir.join(format!(
            "postgres-replication-{}.pgpass",
            plan.target_id.simple()
        ));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let line = format!(
            "{}:{}:*:replicator:{}\n",
            pgpass_field(&plan.primary_ip)?,
            PORT,
            pgpass_field(password)?
        );
        file.write_all(line.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
        Ok(path)
    }
    #[cfg(not(unix))]
    {
        let _ = (plan, password);
        bail!("PostgreSQL replica bootstrap currently requires a Unix target")
    }
}

async fn build_plan(pool: &sqlx::PgPool, to: &str, primary: &str) -> Result<Plan> {
    if to.eq_ignore_ascii_case(primary) {
        bail!("replica target and primary must be different nodes");
    }
    let target =
        sqlx::query("SELECT id, name, primary_ip FROM computers WHERE lower(name)=lower($1)")
            .bind(to)
            .fetch_optional(pool)
            .await?
            .context("target is not an enrolled fleet node")?;
    let primary_row =
        sqlx::query("SELECT id, name, primary_ip FROM computers WHERE lower(name)=lower($1)")
            .bind(primary)
            .fetch_optional(pool)
            .await?
            .context("primary is not an enrolled fleet node")?;
    let primary_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM database_replicas WHERE database_kind='postgres' AND role='primary'",
    )
    .fetch_one(pool)
    .await?;
    if primary_count > 1 {
        bail!("multiple PostgreSQL primary authority rows exist; reconcile before planning");
    }
    let connected_ip: Option<String> = sqlx::query_scalar("SELECT host(inet_server_addr())")
        .fetch_one(pool)
        .await?;
    let server_is_primary: bool = sqlx::query_scalar("SELECT NOT pg_is_in_recovery()")
        .fetch_one(pool)
        .await?;
    let configured_host = configured_database_host()?;
    let named_primary_ip: String = primary_row
        .try_get::<Option<String>, _>("primary_ip")?
        .filter(|ip| !ip.trim().is_empty())
        .context("primary has no usable LAN IP")?;
    let local_worker_matches = ff_agent::fleet_info::resolve_this_worker_name()
        .await
        .eq_ignore_ascii_case(primary);
    if !connected_primary_matches(
        connected_ip.as_deref(),
        Some(&configured_host),
        &named_primary_ip,
        local_worker_matches,
        server_is_primary,
    ) {
        let connected_authority = connected_ip.as_deref().unwrap_or("local Unix socket");
        bail!(
            "named primary '{primary}' ({named_primary_ip}) is not the connected PostgreSQL authority ({connected_authority})"
        );
    }
    if primary_count == 1 {
        let authority: String = sqlx::query_scalar(
            "SELECT c.name FROM database_replicas d JOIN computers c ON c.id=d.computer_id WHERE d.database_kind='postgres' AND d.role='primary'"
        ).fetch_one(pool).await?;
        if !authority.eq_ignore_ascii_case(primary) {
            bail!("explicit primary '{primary}' disagrees with database authority '{authority}'");
        }
    }
    // A bootstrap is only authorized when there is recent, checksummed,
    // restore-verified evidence. Planning remains read-only.
    let primary_id: Uuid = primary_row.get("id");
    let backup_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM backups
          WHERE database_kind='postgres' AND source_computer_id=$2
            AND size_bytes > 0
            AND checksum_sha256 ~ '^[0-9a-fA-F]{64}$'
            AND created_at BETWEEN NOW() - make_interval(hours => $1::int) AND NOW()
            AND verified_restorable_at BETWEEN NOW() - make_interval(hours => $1::int) AND NOW()
          ORDER BY verified_restorable_at DESC LIMIT 1",
    )
    .bind(MAX_BACKUP_AGE_HOURS)
    .bind(primary_id)
    .fetch_optional(pool)
    .await?
    .context("no restore-verified PostgreSQL backup from the last 24 hours; refusing bootstrap")?;
    let pg_major: i32 =
        sqlx::query_scalar("SELECT current_setting('server_version_num')::int / 10000")
            .fetch_one(pool)
            .await?;
    let target_id: Uuid = target.get("id");
    let target_ip = target
        .try_get::<Option<String>, _>("primary_ip")?
        .filter(|ip| !ip.trim().is_empty())
        .context("target has no usable LAN IP")?;
    if target_ip == named_primary_ip {
        bail!("replica target and primary resolve to the same LAN IP");
    }
    Ok(Plan {
        target_id,
        target_name: target.get("name"),
        target_ip,
        primary_name: primary_row.get("name"),
        primary_ip: named_primary_ip,
        slot: physical_slot(target_id),
        backup_id,
        pg_major,
    })
}

fn local_apply_command(plan: &Plan) -> String {
    format!(
        "cd \"$HOME/projects/forge-fleet\" && \"$HOME/.local/bin/ff\" fleet db replica local-apply --to {} --primary {} --plan-id {}",
        shell_escape_single(&plan.target_name),
        shell_escape_single(&plan.primary_name),
        shell_escape_single(&plan.id())
    )
}

async fn enqueue_apply(pool: &sqlx::PgPool, plan: &Plan) -> Result<String> {
    let title = format!("bootstrap PostgreSQL replica on {}", plan.target_name);
    let payload = serde_json::json!({"command": local_apply_command(plan), "summary": "PostgreSQL replica bootstrap"});
    let trigger = serde_json::json!({"node": plan.target_name});
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(plan.id())
        .execute(&mut *tx)
        .await?;
    let like = format!("%--plan-id {}%", plan.id());
    let id = if let Some(id) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM deferred_tasks WHERE title=$1 AND status IN ('pending','dispatchable','running','completed') AND payload->>'command' LIKE $2 ORDER BY created_at DESC LIMIT 1")
        .bind(&title).bind(&like).fetch_optional(&mut *tx).await? { id } else {
        sqlx::query_scalar("INSERT INTO deferred_tasks (created_by,title,kind,payload,trigger_type,trigger_spec,preferred_node,required_caps,max_attempts) VALUES ($1,$2,'shell',$3,'node_online',$4,$5,'[]'::jsonb,1) RETURNING id")
            .bind(whoami_tag()).bind(&title).bind(&payload).bind(&trigger).bind(&plan.target_name).fetch_one(&mut *tx).await?
    };
    tx.commit().await?;
    Ok(id.to_string())
}

async fn local_apply(pool: &sqlx::PgPool, plan: &Plan) -> Result<()> {
    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
    if !me.eq_ignore_ascii_case(&plan.target_name) {
        bail!(
            "local apply must run on '{}' (running on '{me}')",
            plan.target_name
        );
    }
    let compose = std::path::Path::new("deploy/docker-compose.follower.yml");
    let script = std::path::Path::new("deploy/docker/replica-bootstrap.sh");
    if !compose.is_file() || !script.is_file() {
        bail!(
            "run from a ForgeFleet checkout containing the follower compose and bootstrap script"
        );
    }
    let replication_password = ff_db::pg_get_secret(
        pool,
        ff_agent::ha::backup::POSTGRES_REPLICATION_PASSWORD_SECRET,
    )
    .await?
    .filter(|value| !value.is_empty())
    .context("missing PostgreSQL replication password in fleet secrets")?;
    let replication_passfile = write_replication_passfile(plan, &replication_password)?;
    let docker_ok = tokio::process::Command::new("docker")
        .args(["compose", "version"])
        .status()
        .await
        .context("docker compose preflight")?
        .success();
    if !docker_ok {
        bail!("docker compose preflight failed");
    }
    let version = tokio::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "pgvector/pgvector:pg16",
            "postgres",
            "--version",
        ])
        .output()
        .await
        .context("PostgreSQL image version preflight")?;
    if !version.status.success()
        || !String::from_utf8_lossy(&version.stdout)
            .contains(&format!("PostgreSQL {}.", plan.pg_major))
    {
        bail!(
            "target PostgreSQL image major does not match primary major {}",
            plan.pg_major
        );
    }
    let required_bytes: i64 = sqlx::query_scalar("SELECT pg_database_size(current_database()) * 2")
        .fetch_one(pool)
        .await?;
    let docker_info = tokio::process::Command::new("docker")
        .args(["info", "--format", "{{.DockerRootDir}}"])
        .output()
        .await
        .context("locate Docker storage")?;
    if !docker_info.status.success() {
        bail!("could not locate Docker storage for disk preflight");
    }
    let docker_root = String::from_utf8_lossy(&docker_info.stdout)
        .trim()
        .to_string();
    if docker_root.is_empty() {
        bail!("Docker reported an empty storage root");
    }
    let disk = tokio::process::Command::new("df")
        .args(["-Pk", &docker_root])
        .output()
        .await
        .context("target disk preflight")?;
    let available_kib = String::from_utf8_lossy(&disk.stdout)
        .lines()
        .last()
        .and_then(|line| line.split_whitespace().nth(3))
        .and_then(|v| v.parse::<i64>().ok())
        .context("could not determine target free disk")?;
    if available_kib.saturating_mul(1024) < required_bytes {
        bail!("target free disk is below the required 2x primary database size margin");
    }
    let reachable = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect((plan.primary_ip.as_str(), PORT as u16)),
    )
    .await;
    if !matches!(reachable, Ok(Ok(_))) {
        bail!(
            "primary {}:{} is unreachable from target",
            plan.primary_ip,
            PORT
        );
    }
    // Prove the supplied credential can open the replication protocol before
    // creating a slot or touching the follower volume. Only the protected
    // passfile path is visible in argv/container metadata, never the password.
    let replication_dsn = format!(
        "host={} port={} user=replicator dbname=replication replication=true",
        plan.primary_ip, PORT
    );
    let credential = tokio::time::timeout(
        PREFLIGHT_TIMEOUT,
        tokio::process::Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "host",
                "--mount",
                &format!(
                    "type=bind,src={},dst=/run/secrets/postgres_replication_pgpass,readonly",
                    replication_passfile.display()
                ),
                "-e",
                "PGPASSFILE=/run/secrets/postgres_replication_pgpass",
                "pgvector/pgvector:pg16",
                "psql",
                "-XAt",
                "-d",
                &replication_dsn,
                "-c",
                "IDENTIFY_SYSTEM",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("replication credential preflight timed out")?
    .context("replication credential preflight")?;
    if !credential.status.success() {
        bail!("replication credential/authentication preflight failed");
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(&plan.slot)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_create_physical_replication_slot($1) WHERE NOT EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name=$1)")
        .bind(&plan.slot).execute(&mut *tx).await.context("create/reuse physical replication slot")?;
    tx.commit().await?;
    let status = tokio::time::timeout(
        COMPOSE_TIMEOUT,
        tokio::process::Command::new("docker")
            .args([
                "compose",
                "-f",
                "deploy/docker-compose.follower.yml",
                "up",
                "-d",
            ])
            .env("POSTGRES_PRIMARY_HOST", &plan.primary_ip)
            .env("POSTGRES_PRIMARY_PORT", PORT.to_string())
            .env("POSTGRES_REPLICATION_PGPASS_FILE", &replication_passfile)
            .env("POSTGRES_REPLICATION_SLOT", &plan.slot)
            .env("FORGEFLEET_REPLICA_BACKUP_ID", plan.backup_id.to_string())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("follower compose timed out")?
    .context("start follower compose")?;
    if !status.success() {
        bail!("follower compose failed; PGDATA and slot were preserved for retry");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        let output = tokio::time::timeout(PREFLIGHT_TIMEOUT, tokio::process::Command::new("docker").args(["exec", "forgefleet-postgres-replica", "psql", "-U", "forgefleet", "-d", "forgefleet", "-Atc", "SELECT pg_is_in_recovery() AND current_setting('transaction_read_only')::bool AND EXISTS (SELECT 1 FROM pg_stat_wal_receiver WHERE status='streaming') AND pg_last_wal_replay_lsn() IS NOT NULL"]).output())
            .await.context("replica postcheck timed out")?.context("replica postcheck")?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "t" {
            break;
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "replica did not become recovery/read-only/streaming within 10 minutes; PGDATA and slot preserved"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    let slot_active: bool =
        sqlx::query_scalar("SELECT active FROM pg_replication_slots WHERE slot_name=$1")
            .bind(&plan.slot)
            .fetch_optional(pool)
            .await?
            .unwrap_or(false);
    if !slot_active {
        bail!("primary slot '{}' is not active", plan.slot);
    }
    let lag_bytes: i64 = sqlx::query_scalar(
        "SELECT GREATEST(0, pg_wal_lsn_diff(pg_current_wal_lsn(), r.replay_lsn)::bigint)
           FROM pg_replication_slots s
           JOIN pg_stat_replication r ON r.pid = s.active_pid
          WHERE s.slot_name=$1 AND s.slot_type='physical'
            AND r.state='streaming' AND r.replay_lsn IS NOT NULL",
    )
    .bind(&plan.slot)
    .fetch_optional(pool)
    .await?
    .context("active slot has no streaming standby replay position")?;
    if lag_bytes > MAX_POSTCHECK_LAG_BYTES {
        bail!("replica lag {lag_bytes} bytes exceeds postcheck limit {MAX_POSTCHECK_LAG_BYTES}");
    }
    sqlx::query("INSERT INTO database_replicas (computer_id,database_kind,role,status,lag_bytes,last_sync_at,bootstrapped_from_backup_id,notes) VALUES ($1,'postgres','replica','running',$4,NOW(),$2,$3) ON CONFLICT (computer_id,database_kind) DO UPDATE SET role='replica',status='running',lag_bytes=$4,last_sync_at=NOW(),bootstrapped_from_backup_id=$2,notes=$3")
        .bind(plan.target_id).bind(plan.backup_id).bind(format!("primary={};slot={};plan={};pg_major={}", plan.primary_name, plan.slot, plan.id(), plan.pg_major)).bind(lag_bytes).execute(pool).await?;
    Ok(())
}

pub async fn handle(pool: &sqlx::PgPool, command: FleetDbReplicaCommand) -> Result<()> {
    match command {
        FleetDbReplicaCommand::Plan { to, primary } => {
            let p = build_plan(pool, &to, &primary).await?;
            println!(
                "{CYAN}PostgreSQL replica plan (read-only){RESET}\n  target: {} ({})\n  primary: {} ({}:{PORT})\n  slot: {}\n  PostgreSQL major: {}\n  backup evidence: {}\n  plan-id: {}\n\nApply with:\n  ff fleet db replica apply --to {} --primary {} --plan-id {} --yes",
                p.target_name,
                p.target_ip,
                p.primary_name,
                p.primary_ip,
                p.slot,
                p.pg_major,
                p.backup_id,
                p.id(),
                p.target_name,
                p.primary_name,
                p.id()
            );
        }
        FleetDbReplicaCommand::Apply {
            to,
            primary,
            plan_id,
            yes,
        } => {
            if !yes {
                bail!("apply requires --yes; no changes made");
            }
            let p = build_plan(pool, &to, &primary).await?;
            if p.id() != plan_id {
                bail!("plan-id is stale or does not match current preflight; run plan again");
            }
            let id = enqueue_apply(pool, &p).await?;
            println!(
                "{GREEN}✓{RESET} queued replica bootstrap on '{}' as deferred task {id}",
                p.target_name
            );
        }
        FleetDbReplicaCommand::LocalApply {
            to,
            primary,
            plan_id,
        } => {
            let p = build_plan(pool, &to, &primary).await?;
            if p.id() != plan_id {
                bail!("plan-id is stale or does not match current preflight");
            }
            println!(
                "{YELLOW}Applying PostgreSQL replica plan {} on {}{RESET}",
                p.id(),
                p.target_name
            );
            local_apply(pool, &p).await?;
            println!("{GREEN}✓ PostgreSQL replica is streaming, read-only, and registered{RESET}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slot_is_unique_stable_and_valid() {
        let id = Uuid::nil();
        let s = physical_slot(id);
        assert_eq!(s, "ff_00000000000000000000000000000000");
        assert!(s.len() <= 63);
    }
    #[test]
    fn connected_primary_authority_is_fail_closed() {
        assert!(connected_primary_matches(
            Some("192.168.5.104"),
            Some("192.168.5.104"),
            "192.168.5.104",
            false,
            true,
        ));
        assert!(connected_primary_matches(
            Some("127.0.0.1"),
            Some("127.0.0.1"),
            "192.168.5.104",
            true,
            true,
        ));
        assert!(connected_primary_matches(
            None,
            None,
            "192.168.5.104",
            true,
            true,
        ));
        assert!(!connected_primary_matches(
            None,
            None,
            "192.168.5.104",
            false,
            true,
        ));
        assert!(!connected_primary_matches(
            Some("192.168.5.100"),
            Some("192.168.5.100"),
            "192.168.5.104",
            true,
            true,
        ));
    }

    #[test]
    fn connected_primary_accepts_configured_host_through_container_nat() {
        assert!(connected_primary_matches(
            Some("172.18.0.4"),
            Some("192.168.5.104"),
            "192.168.5.104",
            false,
            true,
        ));
    }

    #[test]
    fn connected_primary_rejects_standby_and_configured_host_mismatch() {
        assert!(!connected_primary_matches(
            Some("172.18.0.4"),
            Some("192.168.5.104"),
            "192.168.5.104",
            false,
            false,
        ));
        assert!(!connected_primary_matches(
            Some("172.18.0.4"),
            Some("192.168.5.103"),
            "192.168.5.104",
            false,
            true,
        ));
    }
    #[test]
    fn deferred_command_has_no_credentials() {
        let p = Plan {
            target_id: Uuid::nil(),
            target_name: "node one".into(),
            target_ip: "10.0.0.2".into(),
            primary_name: "primary".into(),
            primary_ip: "10.0.0.1".into(),
            slot: physical_slot(Uuid::nil()),
            backup_id: Uuid::nil(),
            pg_major: 16,
        };
        let c = local_apply_command(&p);
        assert!(!c.to_lowercase().contains("password"));
        assert!(c.contains("local-apply"));
        assert!(c.starts_with("cd \"$HOME/projects/forge-fleet\""));
    }
    #[test]
    fn plan_id_is_stable_and_sensitive() {
        let mut p = Plan {
            target_id: Uuid::nil(),
            target_name: "n".into(),
            target_ip: "1".into(),
            primary_name: "p".into(),
            primary_ip: "2".into(),
            slot: physical_slot(Uuid::nil()),
            backup_id: Uuid::nil(),
            pg_major: 16,
        };
        let a = p.id();
        assert_eq!(a, p.id());
        p.primary_ip = "3".into();
        assert_ne!(a, p.id());
        let b = p.id();
        p.pg_major = 17;
        assert_ne!(b, p.id());
    }
    #[test]
    fn pgpass_fields_escape_delimiters_and_reject_lines() {
        assert_eq!(pgpass_field(r"a:b\c").unwrap(), r"a\:b\\c");
        assert!(pgpass_field("secret\nsecond-line").is_err());
        assert!(pgpass_field("secret\rsecond-line").is_err());
        assert!(pgpass_field("secret\0tail").is_err());
    }
    #[test]
    fn bootstrap_script_is_non_destructive_and_slot_bound() {
        let script = include_str!("../../../deploy/docker/replica-bootstrap.sh");
        assert!(!script.contains("rm -rf"));
        assert!(script.contains("POSTGRES_REPLICATION_SLOT"));
        assert!(script.contains("-S \"${POSTGRES_REPLICATION_SLOT}\""));
        assert!(script.contains("refusing partial/non-empty PGDATA"));
        assert!(script.contains("FORGEFLEET_REPLICA_BACKUP_ID"));
        assert!(script.contains("BOOTSTRAP_EVIDENCE"));
        assert!(script.contains("CURRENT_PRIMARY_SLOT"));
        assert!(script.contains("different primary host"));
        let compose = include_str!("../../../deploy/docker-compose.follower.yml");
        assert!(!compose.contains("192.168.5.100"));
        assert!(!compose.contains("POSTGRES_PASSWORD: forgefleet"));
        assert!(!compose.contains("POSTGRES_REPLICATION_PASSWORD:"));
        assert!(compose.contains("POSTGRES_REPLICATION_PGPASS_FILE"));
    }
}
