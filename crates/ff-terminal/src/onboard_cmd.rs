use std::{net::IpAddr, process::Stdio, time::Duration};

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

async fn op_read(service_token: &Zeroizing<String>, reference: &str) -> Result<Zeroizing<Vec<u8>>> {
    anyhow::ensure!(valid_op_reference(reference), "invalid 1Password reference");
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new("op")
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
) -> Result<EnrollmentAuthority> {
    let row = sqlx::query(
        "SELECT l.member_name, l.epoch, \
                COALESCE(NULLIF(c.primary_ip, ''), NULLIF(w.ip, '')) AS leader_ip \
         FROM fleet_leader_state l \
         LEFT JOIN computers c ON c.name = l.member_name \
         LEFT JOIN fleet_workers w ON w.name = l.member_name \
         WHERE l.heartbeat_at > clock_timestamp() - make_interval(secs => $1) \
           AND (l.relinquishing_until IS NULL OR l.relinquishing_until <= clock_timestamp()) \
         LIMIT 1 FOR SHARE OF l",
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
    let leader_ip_raw: String = row
        .try_get("leader_ip")
        .context("elected leader has no authoritative IP")?;
    let leader_ip = bound_ip(&leader_ip_raw).context("elected leader has an invalid IP")?;
    Ok(EnrollmentAuthority {
        leader_name,
        leader_epoch: row.try_get("epoch")?,
        leader_ip,
    })
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
read -r -s -p '1Password service-account token: ' OP_SERVICE_ACCOUNT_TOKEN; printf '\\n'\n\
test -n \"$OP_SERVICE_ACCOUNT_TOKEN\" || {{ echo '1Password token is required' >&2; exit 2; }}\n\
export FORGEFLEET_ENROLLMENT_TOKEN OP_SERVICE_ACCOUNT_TOKEN\n\
trap 'unset FORGEFLEET_ENROLLMENT_TOKEN OP_SERVICE_ACCOUNT_TOKEN' EXIT\n\
TLS_CA_PEM_B64='{ca}'\n\
tls_curl() {{ curl --proto '=https' --tlsv1.3 --silent --show-error --fail \
  --resolve '{server}:{port}:{resolve_ip}' \
  --cacert <(printf '%s' \"$TLS_CA_PEM_B64\" | base64 --decode) \
  --pinnedpubkey '{pin}' \"$@\"; }}\n\
tls_curl --config <(printf 'header = \"Authorization: Bearer %s\"\\n' \"$FORGEFLEET_ENROLLMENT_TOKEN\") \
  'https://{server}:{port}/onboard/bootstrap.sh?name={name}&ip={intended_ip}' \
  | sudo --preserve-env=FORGEFLEET_ENROLLMENT_TOKEN,OP_SERVICE_ACCOUNT_TOKEN bash",
        ca = trust.ca_pem_b64,
        server = trust.server_name,
        port = ENROLLMENT_TLS_PORT,
        pin = trust.spki_pin,
    )
}

async fn issue_enrollment(
    pool: &sqlx::PgPool,
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
    let created_by = std::env::var("USER").unwrap_or_else(|_| "ff-onboard".to_owned());

    let mut tx = pool.begin().await?;
    let authority = lock_enrollment_authority(&mut tx).await?;
    ensure_identity_available(&mut tx, name, ip).await?;

    // A newly issued credential supersedes any still-pending credential for
    // this exact identity, avoiding ambiguous concurrent claims.
    sqlx::query(
        "UPDATE fleet_enrollment_tokens SET expires_at = clock_timestamp() \
         WHERE consumed_at IS NULL AND expires_at > clock_timestamp() \
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
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::OnboardCommand::Show {
            name,
            ip,
            ssh_user,
            role,
            runtime,
        } => {
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
            let (token, authority) =
                issue_enrollment(&pool, &name, ip, &ssh_user, &role, &runtime).await?;
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

    #[test]
    fn generated_command_is_https_pinned_and_prompts_for_secret() {
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
        assert!(command.contains("read -r -s -p '1Password service-account token: '"));
        assert!(
            command.contains("--preserve-env=FORGEFLEET_ENROLLMENT_TOKEN,OP_SERVICE_ACCOUNT_TOKEN")
        );
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
    }

    #[test]
    fn web_onboarding_remains_hard_quarantined() {
        let token_query = ["?", "token="].concat();
        assert!(WEB_ONBOARDING_PAGE.contains("server-verified TLS"));
        assert!(WEB_ONBOARDING_PAGE.contains("No bootstrap command was generated"));
        assert!(!WEB_ONBOARDING_PAGE.contains(&token_query));
        assert!(!WEB_ONBOARDING_PAGE.contains("enrollment.shared_secret"));
    }
}
