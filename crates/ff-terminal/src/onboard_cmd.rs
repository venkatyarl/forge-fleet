use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use base64::Engine;
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use zeroize::Zeroizing;

const TOKEN_PREFIX: &str = "ffe1_";
const TOKEN_BYTES: usize = 32;
const TOKEN_TTL_MINUTES: i32 = 10;
const LEADER_FRESHNESS_SECONDS: i32 = 45;
const ENROLLMENT_TLS_PORT: u16 = 51_443;
const OP_SERVICE_ACCOUNT_TOKEN_KEY: &str = "1Password:service_account_token";
const TLS_CA_REF_KEY: &str = "enrollment.tls_ca_ref";
const TLS_SPKI_PIN_REF_KEY: &str = "enrollment.tls_spki_pin_ref";
const TLS_SERVER_NAME_KEY: &str = "enrollment.tls_server_name";
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
) -> Result<()> {
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

#[derive(Debug)]
struct EnrollmentAuthority {
    leader_name: String,
    leader_epoch: i64,
    leader_ip: IpAddr,
}

#[derive(Debug)]
struct ClientTrust {
    server_name: String,
    ca_pem_b64: String,
    spki_pin: String,
}

fn canonical_node_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn canonical_ssh_user(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || *byte == b'_')
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn canonical_claim(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn bound_ip(value: &str) -> Result<IpAddr> {
    let ip: IpAddr = value.parse().context("--ip must be a literal IP address")?;
    anyhow::ensure!(
        !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast(),
        "--ip must be a non-loopback unicast address"
    );
    Ok(ip)
}

fn valid_op_reference(value: &str) -> bool {
    value.starts_with("op://") && value.len() <= 512 && !value.chars().any(char::is_control)
}

fn valid_server_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn valid_spki_pin(value: &str) -> bool {
    value
        .strip_prefix("sha256//")
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .is_some_and(|digest| digest.len() == 32)
}

async fn required_secret(pool: &sqlx::PgPool, key: &str) -> Result<String> {
    ff_db::pg_get_secret(pool, key)
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("required enrollment authority key {key} is not configured"))
}

fn validate_trusted_op_candidate(candidate: &str) -> Result<PathBuf> {
    let path = Path::new(candidate);
    anyhow::ensure!(path.is_absolute(), "1Password binary path is not absolute");
    let link_metadata = path
        .symlink_metadata()
        .with_context(|| format!("inspect trusted 1Password binary {candidate}"))?;
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
        .with_context(|| format!("resolve trusted 1Password binary {candidate}"))?;
    anyhow::ensure!(
        canonical == path,
        "trusted 1Password binary resolves outside its approved path"
    );

    #[cfg(unix)]
    {
        let mut component = Some(path);
        while let Some(current) = component {
            let metadata = current.symlink_metadata().with_context(|| {
                format!("inspect 1Password path component {}", current.display())
            })?;
            validate_trusted_op_path_component(current, &metadata, current == path)?;
            component = current.parent().filter(|parent| *parent != current);
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("trusted 1Password execution is unsupported on this platform");

    Ok(canonical)
}

fn trusted_op_binary() -> Result<PathBuf> {
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

async fn op_read(service_token: &Zeroizing<String>, reference: &str) -> Result<Zeroizing<Vec<u8>>> {
    anyhow::ensure!(valid_op_reference(reference), "invalid 1Password reference");
    let op_binary = trusted_op_binary()?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(op_binary)
            .arg("read")
            .arg(reference)
            .env("OP_SERVICE_ACCOUNT_TOKEN", service_token.as_str())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("1Password read timed out"))??;
    anyhow::ensure!(output.status.success(), "1Password read failed");
    anyhow::ensure!(
        !output.stdout.is_empty() && output.stdout.len() <= 1024 * 1024,
        "1Password material has an invalid size"
    );
    Ok(Zeroizing::new(output.stdout))
}

async fn load_client_trust(pool: &sqlx::PgPool) -> Result<ClientTrust> {
    let service_token = Zeroizing::new(required_secret(pool, OP_SERVICE_ACCOUNT_TOKEN_KEY).await?);
    let ca_ref = required_secret(pool, TLS_CA_REF_KEY).await?;
    let pin_ref = required_secret(pool, TLS_SPKI_PIN_REF_KEY).await?;
    let server_name = required_secret(pool, TLS_SERVER_NAME_KEY).await?;
    let server_name = server_name.trim();
    anyhow::ensure!(valid_server_name(server_name), "invalid TLS server name");

    let ca_pem = op_read(&service_token, ca_ref.trim()).await?;
    let pin = op_read(&service_token, pin_ref.trim()).await?;
    let pin = std::str::from_utf8(&pin)?.trim();
    anyhow::ensure!(valid_spki_pin(pin), "invalid TLS SPKI pin");
    anyhow::ensure!(
        std::str::from_utf8(&ca_pem)?.contains("-----BEGIN CERTIFICATE-----"),
        "enrollment CA is not PEM encoded"
    );

    Ok(ClientTrust {
        server_name: server_name.to_owned(),
        ca_pem_b64: base64::engine::general_purpose::STANDARD.encode(&*ca_pem),
        spki_pin: pin.to_owned(),
    })
}

async fn lock_enrollment_authority(
    tx: &mut Transaction<'_, Postgres>,
    local_identity: &str,
) -> Result<EnrollmentAuthority> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ff_db::SECURE_ENROLLMENT_XACT_LOCK_KEY)
        .execute(&mut **tx)
        .await?;
    // Acquire the roster table locks before any roster row lock so ordinary
    // roster writers cannot deadlock with enrollment by holding the inverse
    // table/row lock order.
    sqlx::query("LOCK TABLE computers, fleet_workers IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut **tx)
        .await?;
    let row = sqlx::query(
        "SELECT l.member_name, l.epoch, \
                c.primary_ip AS leader_ip \
         FROM fleet_leader_state l \
         JOIN computers c ON c.id = l.computer_id AND c.name = l.member_name \
         JOIN fleet_workers w ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip \
         WHERE l.singleton_key = 'current' \
           AND l.heartbeat_at > clock_timestamp() - make_interval(secs => $1) \
           AND (l.relinquishing_until IS NULL OR l.relinquishing_until <= clock_timestamp()) \
         FOR UPDATE OF l, c, w",
    )
    .bind(LEADER_FRESHNESS_SECONDS)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| anyhow::anyhow!("no fresh elected leader is available for enrollment"))?;

    let leader_name: String = row.try_get("member_name")?;
    anyhow::ensure!(
        canonical_node_name(&leader_name),
        "elected leader name is not canonical"
    );
    anyhow::ensure!(
        leader_name == local_identity,
        "enrollment credentials may only be issued by the locked current leader; local identity is {local_identity}, leader is {leader_name}"
    );
    let leader_ip_raw: String = row
        .try_get("leader_ip")
        .context("elected leader has no authoritative IP")?;
    let leader_ip = bound_ip(&leader_ip_raw).context("elected leader has an invalid IP")?;
    let routed_local_ip = route_selected_local_ip(leader_ip)?;
    anyhow::ensure!(
        routed_local_ip == leader_ip,
        "local network identity {routed_local_ip} does not own elected leader address {leader_ip}"
    );
    Ok(EnrollmentAuthority {
        leader_name,
        leader_epoch: row.try_get("epoch")?,
        leader_ip,
    })
}

fn route_selected_local_ip(destination: IpAddr) -> Result<IpAddr> {
    let bind_addr = match destination {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind_addr).context("bind local identity probe")?;
    socket
        .connect(SocketAddr::new(destination, 9))
        .context("route local identity probe")?;
    Ok(socket.local_addr()?.ip())
}

async fn canonical_local_issuer(pool: &sqlx::PgPool) -> Result<String> {
    let local_identity = ff_agent::fleet_info::resolve_this_worker_name().await;
    anyhow::ensure!(
        canonical_node_name(&local_identity),
        "local ForgeFleet identity is not canonical"
    );
    let exact_projection: bool = sqlx::query_scalar(
        "SELECT EXISTS( \
             SELECT 1 FROM computers c \
             JOIN fleet_workers w ON w.name = c.name AND NULLIF(w.ip, '') = c.primary_ip \
             WHERE c.name = $1 \
         )",
    )
    .bind(&local_identity)
    .fetch_one(pool)
    .await?;
    anyhow::ensure!(
        exact_projection,
        "local ForgeFleet identity is not an exact computers/fleet_workers projection"
    );
    Ok(local_identity)
}

async fn ensure_identity_available(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
    ip: IpAddr,
) -> Result<()> {
    let conflict: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM fleet_workers \
               WHERE lower(name) = lower($1) OR NULLIF(ip, '')::inet = $2::inet \
             UNION ALL \
             SELECT 1 FROM computers \
               WHERE lower(name) = lower($1) OR NULLIF(primary_ip, '')::inet = $2::inet \
         )",
    )
    .bind(name)
    .bind(ip.to_string())
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(
        !conflict,
        "node name or intended IP already belongs to a fleet identity"
    );
    Ok(())
}

fn render_bootstrap_command(
    trust: &ClientTrust,
    authority: &EnrollmentAuthority,
    name: &str,
    intended_ip: IpAddr,
) -> String {
    let resolve_ip = match authority.leader_ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    format!(
        "set -euo pipefail\n\
read -r -s -p 'Enrollment token: ' FORGEFLEET_ENROLLMENT_TOKEN; printf '\\n'\n\
case \"$FORGEFLEET_ENROLLMENT_TOKEN\" in ffe1_???????????????????????????????????????????) ;; *) echo 'Invalid enrollment token' >&2; exit 2 ;; esac\n\
export FORGEFLEET_ENROLLMENT_TOKEN\n\
trap 'unset FORGEFLEET_ENROLLMENT_TOKEN' EXIT\n\
TLS_CA_PEM_B64='{ca}'\n\
tls_curl() {{ curl --proto '=https' --tlsv1.3 --silent --show-error --fail \
  --resolve '{server}:{port}:{resolve_ip}' \
  --cacert <(printf '%s' \"$TLS_CA_PEM_B64\" | base64 --decode) \
  --pinnedpubkey '{pin}' \"$@\"; }}\n\
tls_curl --config <(printf 'header = \"Authorization: Bearer %s\"\\n' \"$FORGEFLEET_ENROLLMENT_TOKEN\") \
  'https://{server}:{port}/onboard/bootstrap.sh?name={name}&ip={intended_ip}' \
  | sudo --preserve-env=FORGEFLEET_ENROLLMENT_TOKEN bash",
        ca = trust.ca_pem_b64,
        server = trust.server_name,
        port = ENROLLMENT_TLS_PORT,
        pin = trust.spki_pin,
    )
}

async fn issue_enrollment(
    pool: &sqlx::PgPool,
    local_identity: &str,
    name: &str,
    ip: IpAddr,
    ssh_user: &str,
    role: &str,
    runtime: &str,
) -> Result<(Zeroizing<String>, EnrollmentAuthority)> {
    let mut random = Zeroizing::new([0_u8; TOKEN_BYTES]);
    OsRng.fill_bytes(&mut *random);
    let token = Zeroizing::new(format!(
        "{TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&*random)
    ));
    let token_hash: [u8; 32] = Sha256::digest(&*random).into();
    let created_by = format!(
        "{}@{}",
        std::env::var("USER").unwrap_or_else(|_| "ff-onboard".to_owned()),
        local_identity
    );

    let mut tx = pool.begin().await?;
    let authority = lock_enrollment_authority(&mut tx, local_identity).await?;
    ensure_identity_available(&mut tx, name, ip).await?;

    // A newly issued credential supersedes any still-pending credential for
    // this exact identity, avoiding ambiguous concurrent claims.
    sqlx::query(
        "UPDATE fleet_enrollment_tokens SET revoked_at = clock_timestamp() \
         WHERE consumed_at IS NULL AND revoked_at IS NULL \
           AND (node_name = $1 OR intended_ip = $2::inet)",
    )
    .bind(name)
    .bind(ip.to_string())
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO fleet_enrollment_tokens \
           (token_hash,node_name,intended_ip,ssh_user,role,runtime,purpose, \
            leader_name,leader_epoch,expires_at,created_by) \
         VALUES ($1,$2,$3::inet,$4,$5,$6,'node-enrollment',$7,$8, \
                 clock_timestamp() + make_interval(mins => $9),$10)",
    )
    .bind(token_hash.as_slice())
    .bind(name)
    .bind(ip.to_string())
    .bind(ssh_user)
    .bind(role)
    .bind(runtime)
    .bind(&authority.leader_name)
    .bind(authority.leader_epoch)
    .bind(TOKEN_TTL_MINUTES)
    .bind(created_by)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((token, authority))
}

pub async fn handle_onboard(cmd: crate::OnboardCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    match cmd {
        crate::OnboardCommand::Show {
            name,
            ip,
            ssh_user,
            role,
            runtime,
        } => {
            ff_db::validate_secure_enrollment_schema(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("secure enrollment schema validation: {e}"))?;
            anyhow::ensure!(
                canonical_node_name(&name),
                "--name must be a canonical lowercase node name"
            );
            let ip =
                ip.ok_or_else(|| anyhow::anyhow!("--ip is required for node-bound enrollment"))?;
            let ip = bound_ip(&ip)?;
            let ssh_user = ssh_user.unwrap_or_else(|| name.clone());
            anyhow::ensure!(canonical_ssh_user(&ssh_user), "--ssh-user is not canonical");
            anyhow::ensure!(
                matches!(role.as_str(), "builder" | "gateway" | "testbed"),
                "--role must be builder, gateway, or testbed"
            );
            anyhow::ensure!(canonical_claim(&runtime, 32), "--runtime is not canonical");

            // Load only public trust material before creating a credential. A
            // 1Password failure therefore cannot leave an unusable live token.
            let trust = load_client_trust(&pool).await?;
            let local_identity = canonical_local_issuer(&pool).await?;
            let (token, authority) = issue_enrollment(
                &pool,
                &local_identity,
                &name,
                ip,
                &ssh_user,
                &role,
                &runtime,
            )
            .await?;
            let command = render_bootstrap_command(&trust, &authority, &name, ip);

            println!("One-time enrollment token (expires in {TOKEN_TTL_MINUTES} minutes):");
            println!("{}", token.as_str());
            println!(
                "\nOn the intended node ({ip}), run this command and paste the token when prompted:\n"
            );
            println!("{command}");
            println!(
                "\nAuthority: {} epoch {} at {}:{}",
                authority.leader_name,
                authority.leader_epoch,
                authority.leader_ip,
                ENROLLMENT_TLS_PORT
            );
        }
        crate::OnboardCommand::List { limit } => {
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            let mut sorted: Vec<&ff_db::FleetNodeRow> = nodes.iter().collect();
            sorted.sort_by(|a, b| b.election_priority.cmp(&a.election_priority));
            println!(
                "{:<15} {:<16} {:<10} {:<6} GH",
                "NAME", "IP", "RUNTIME", "PRIO"
            );
            for n in sorted.into_iter().take(limit as usize) {
                println!(
                    "{:<15} {:<16} {:<10} {:<6} {}",
                    n.name,
                    n.ip,
                    n.runtime,
                    n.election_priority,
                    n.gh_account.clone().unwrap_or_else(|| "-".into())
                );
            }
        }
        crate::OnboardCommand::Revoke { name, yes } => {
            if !yes {
                println!(
                    "This will DELETE fleet_workers row '{name}', all its SSH keys, and mesh-status rows."
                );
                println!("Re-run with --yes to confirm.");
                return Ok(());
            }
            let removed_keys = ff_db::pg_delete_node_ssh_keys(&pool, &name).await?;
            let removed_mesh = ff_db::pg_delete_mesh_status_for_node(&pool, &name).await?;
            let r = sqlx::query("DELETE FROM fleet_workers WHERE name = $1")
                .bind(&name)
                .execute(&pool)
                .await?;
            println!(
                "Revoked '{name}': {} ssh keys, {} mesh rows, {} node row(s)",
                removed_keys,
                removed_mesh,
                r.rows_affected()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    const WEB_ONBOARDING_PAGE: &str =
        include_str!("../../../web-forge-fleet/app/(console)/onboarding/page.tsx");

    fn trust() -> ClientTrust {
        ClientTrust {
            server_name: "enroll.forgefleet.local".into(),
            ca_pem_b64: "UHVibGljIENB".into(),
            spki_pin: format!(
                "sha256//{}",
                base64::engine::general_purpose::STANDARD.encode([7_u8; 32])
            ),
        }
    }

    async fn isolated_postgres() -> Option<sqlx::PgPool> {
        let database_url = match std::env::var("FF_ENROLLMENT_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_) => {
                eprintln!(
                    "skipping real PostgreSQL issuance test: FF_ENROLLMENT_TEST_DATABASE_URL is unset"
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
        let database = format!("ff_issuance_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        PgPoolOptions::new()
            .max_connections(4)
            .connect_with(admin_options.database(&database))
            .await
            .ok()
    }

    async fn install_issuance_schema(pool: &sqlx::PgPool) {
        sqlx::raw_sql(
            r#"
            CREATE EXTENSION IF NOT EXISTS pgcrypto;
            CREATE TABLE computers (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL UNIQUE,
                primary_ip TEXT,
                status TEXT
            );
            CREATE TABLE fleet_workers (
                name TEXT PRIMARY KEY,
                ip TEXT NOT NULL
            );
            CREATE TABLE fleet_leader_state (
                singleton_key TEXT PRIMARY KEY DEFAULT 'current' CHECK (singleton_key='current'),
                computer_id UUID NOT NULL REFERENCES computers(id),
                member_name TEXT NOT NULL,
                epoch BIGINT NOT NULL,
                heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                relinquishing_until TIMESTAMPTZ
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
        // Re-running V290 is allowed only when the existing authority objects
        // still match the exact reviewed definitions.
        sqlx::raw_sql(ff_db::schema::SCHEMA_V290_SECURE_ENROLLMENT_HARDENING)
            .execute(pool)
            .await
            .unwrap();
        let leader_ip = route_selected_local_ip("192.0.2.1".parse().unwrap()).unwrap();
        let computer_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO computers (id,name,primary_ip,status) VALUES ($1,'testleader',$2,'online')")
            .bind(computer_id)
            .bind(leader_ip.to_string())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO fleet_workers (name,ip) VALUES ('testleader',$1)")
            .bind(leader_ip.to_string())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO fleet_leader_state (computer_id,member_name,epoch) VALUES ($1,'testleader',7)",
        )
        .bind(computer_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn generated_command_is_https_pinned_and_carries_only_one_time_token() {
        let command = render_bootstrap_command(
            &trust(),
            &EnrollmentAuthority {
                leader_name: "beyonce".into(),
                leader_epoch: 9,
                leader_ip: "192.168.5.116".parse().unwrap(),
            },
            "new-node",
            "192.168.5.150".parse().unwrap(),
        );
        assert!(command.contains("https://enroll.forgefleet.local:51443"));
        assert!(command.contains("--proto '=https'"));
        assert!(command.contains("--tlsv1.3"));
        assert!(command.contains("--cacert <("));
        assert!(command.contains("--pinnedpubkey 'sha256//"));
        assert!(command.contains("--resolve 'enroll.forgefleet.local:51443:192.168.5.116'"));
        assert!(command.contains("read -r -s -p 'Enrollment token: '"));
        assert!(command.contains("--preserve-env=FORGEFLEET_ENROLLMENT_TOKEN bash"));
        assert!(!command.contains("OP_SERVICE_ACCOUNT_TOKEN"));
        assert!(!command.contains("1Password service-account token"));
        assert!(!command.contains("token="));
        assert!(!command.contains("http://"));
        assert!(!command.contains(" -k"));
        assert!(!command.contains("--insecure"));
    }

    #[test]
    fn identity_and_trust_validators_fail_closed() {
        assert!(canonical_node_name("new-node7"));
        for bad in ["Vinny", "vinny.local", "vínny", "-vinny", "vinny-"] {
            assert!(!canonical_node_name(bad));
        }
        assert!(canonical_ssh_user("deploy_user"));
        assert!(!canonical_ssh_user("root;id"));
        assert!(valid_server_name("enroll.forgefleet.local"));
        assert!(!valid_server_name("*.forgefleet.local"));
        assert!(valid_op_reference("op://ForgeFleet/enrollment/ca"));
        assert!(!valid_op_reference("/tmp/enrollment-ca.pem"));
        assert!(valid_spki_pin(&trust().spki_pin));
        assert!(
            TRUSTED_OP_PATHS
                .iter()
                .all(|path| std::path::Path::new(path).is_absolute())
        );
        assert!(!include_str!("onboard_cmd.rs").contains("Command::new(\"op\")"));
    }

    #[cfg(unix)]
    #[test]
    fn op_trust_rejects_symlink_canonical_owner_and_mode_violations() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = std::env::temp_dir().join(format!(
            "ff-op-trust-terminal-{}",
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
    fn web_onboarding_remains_hard_quarantined() {
        let token_query = ["?", "token="].concat();
        assert!(WEB_ONBOARDING_PAGE.contains("server-verified TLS"));
        assert!(WEB_ONBOARDING_PAGE.contains("No bootstrap command was generated"));
        assert!(!WEB_ONBOARDING_PAGE.contains(&token_query));
        assert!(!WEB_ONBOARDING_PAGE.contains("enrollment.shared_secret"));
    }

    #[test]
    fn issuance_source_requires_leader_lock_and_no_request_side_migration() {
        let source = include_str!("onboard_cmd.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source before test module");
        for required in [
            "pg_advisory_xact_lock",
            "FOR UPDATE OF l, c, w",
            "leader_name == local_identity",
            "routed_local_ip == leader_ip",
            "LOCK TABLE computers, fleet_workers IN SHARE ROW EXCLUSIVE MODE",
            "validate_secure_enrollment_schema",
        ] {
            assert!(
                production.contains(required),
                "missing issuer fence: {required}"
            );
        }
        assert!(!production.contains("run_postgres_migrations(&pool)"));
    }

    #[tokio::test]
    async fn postgres_concurrent_issuance_has_one_active_identity_claim() {
        let Some(pool) = isolated_postgres().await else {
            return;
        };
        install_issuance_schema(&pool).await;
        ff_db::validate_secure_enrollment_schema(&pool)
            .await
            .unwrap();
        let intended_ip: IpAddr = "192.0.2.200".parse().unwrap();
        let (first, second) = tokio::join!(
            issue_enrollment(
                &pool,
                "testleader",
                "new-node",
                intended_ip,
                "new-node",
                "builder",
                "auto",
            ),
            issue_enrollment(
                &pool,
                "testleader",
                "new-node",
                intended_ip,
                "new-node",
                "builder",
                "auto",
            ),
        );
        let (first_token, first_authority) = first.unwrap();
        let (second_token, second_authority) = second.unwrap();
        assert_ne!(first_token.as_str(), second_token.as_str());
        assert_eq!(first_authority.leader_epoch, 7);
        assert_eq!(second_authority.leader_epoch, 7);
        let (total, active, revoked): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE consumed_at IS NULL AND revoked_at IS NULL), \
                    count(*) FILTER (WHERE revoked_at IS NOT NULL) \
             FROM fleet_enrollment_tokens WHERE node_name='new-node'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((total, active, revoked), (2, 1, 1));

        assert!(
            issue_enrollment(
                &pool,
                "notleader",
                "other-node",
                "192.0.2.201".parse().unwrap(),
                "other-node",
                "builder",
                "auto",
            )
            .await
            .is_err(),
            "a non-leader local identity must not issue"
        );
        pool.close().await;
    }
}
