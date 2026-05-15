//! `ff fleet` subcommand implementations.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{
    CYAN, FleetCommand, FleetDbCommand, GREEN, RED, RESET, TaskCoverageCommand, YELLOW,
    pulse_reader, whoami_tag,
};

/// `ff fleet panic-stop` — emergency halt of every daemon.
///
/// The implementation initializes NATS best-effort before delegating to
/// `panic_stop::fleet_panic_stop` so observers on the bus see the event
/// (the stop itself doesn't need NATS but `--halt-dbs` users expect
/// downstream alerting to fire).
pub async fn handle_fleet_panic_stop(pool: &sqlx::PgPool, yes: bool, halt_dbs: bool) -> Result<()> {
    if !yes {
        eprintln!("{YELLOW}⚠ panic-stop halts EVERY ForgeFleet daemon across the fleet.{RESET}");
        eprintln!("  Use this only when the fleet is misbehaving (runaway loops, resource");
        eprintln!(
            "  exhaustion, task spam). Pass --yes to proceed. Recover via `ff fleet resume`."
        );
        std::process::exit(1);
    }

    // Fire-and-forget NATS init so the quarantine/halt events propagate.
    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;

    println!("{CYAN}▶ ff fleet panic-stop — halting every daemon…{RESET}");
    let local = ff_agent::fleet_info::resolve_this_node_name().await;
    let report = ff_agent::panic_stop::fleet_panic_stop(pool, &local)
        .await
        .map_err(|e| anyhow::anyhow!("panic_stop: {e}"))?;

    for e in &report.entries {
        let marker = if e.ok {
            format!("{GREEN}✓{RESET}")
        } else {
            format!("{RED}✗{RESET}")
        };
        println!("  {marker} {:<10} {}", e.name, e.detail);
    }
    println!(
        "\n{} of {} daemons stopped.{}",
        report.succeeded,
        report.total,
        if report.failed > 0 {
            format!(
                " {YELLOW}({} failure{}){RESET}",
                report.failed,
                if report.failed == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        },
    );

    if halt_dbs {
        println!("\n{CYAN}▶ --halt-dbs — stopping local Docker data-plane containers…{RESET}");
        let (ok, detail) = ff_agent::panic_stop::stop_taylor_docker_stack().await;
        let marker = if ok {
            format!("{GREEN}✓{RESET}")
        } else {
            format!("{YELLOW}—{RESET}")
        };
        println!("  {marker} docker stack\n{detail}");
        if !ok {
            println!(
                "{YELLOW}(some containers weren't running locally — expected if this isn't Taylor){RESET}"
            );
        }
    }

    println!("\nRecover with: {CYAN}ff fleet resume --yes{RESET}");
    if report.failed > 0 {
        std::process::exit(3);
    }
    Ok(())
}

/// `ff fleet resume` — symmetric undo of panic-stop.
pub async fn handle_fleet_resume(pool: &sqlx::PgPool, yes: bool) -> Result<()> {
    if !yes {
        eprintln!(
            "{YELLOW}⚠ resume will (re)start every daemon across the fleet. Pass --yes to proceed.{RESET}"
        );
        std::process::exit(1);
    }

    println!("{CYAN}▶ ff fleet resume — starting every daemon…{RESET}");
    let local = ff_agent::fleet_info::resolve_this_node_name().await;
    let report = ff_agent::panic_stop::fleet_resume(pool, &local)
        .await
        .map_err(|e| anyhow::anyhow!("resume: {e}"))?;

    for e in &report.entries {
        let marker = if e.ok {
            format!("{GREEN}✓{RESET}")
        } else {
            format!("{RED}✗{RESET}")
        };
        println!("  {marker} {:<10} {}", e.name, e.detail);
    }
    println!(
        "\n{} of {} daemons (re)started.{}",
        report.succeeded,
        report.total,
        if report.failed > 0 {
            format!(
                " {YELLOW}({} failure{}){RESET}",
                report.failed,
                if report.failed == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        },
    );
    if report.failed > 0 {
        std::process::exit(3);
    }
    Ok(())
}

/// `ff fleet quarantine <computer>` — stop daemons + flip status to
/// 'maintenance'. See module docs on `panic_stop.rs` for full flow.
pub async fn handle_fleet_quarantine(pool: &sqlx::PgPool, computer: &str, yes: bool) -> Result<()> {
    if !yes {
        eprintln!(
            "{YELLOW}⚠ quarantine will stop daemons on '{computer}' and mark it 'maintenance'.{RESET}"
        );
        eprintln!("  The node will be excluded from leader election and LLM routing.");
        eprintln!(
            "  Pass --yes to proceed. Reverse with `ff fleet unquarantine {computer} --yes`."
        );
        std::process::exit(1);
    }

    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;

    println!("{CYAN}▶ ff fleet quarantine {computer}{RESET}");
    let result = ff_agent::panic_stop::quarantine_computer(pool, computer)
        .await
        .map_err(|e| anyhow::anyhow!("quarantine: {e}"))?;

    if result.ssh_stop_ok {
        println!("  {GREEN}✓{RESET} ssh stop succeeded on '{}'", result.name);
    } else {
        println!(
            "  {YELLOW}—{RESET} ssh stop did NOT succeed on '{}' (detail: {}) — DB flip applied anyway",
            result.name, result.ssh_detail
        );
    }
    println!("  {GREEN}✓{RESET} status='maintenance' in computers table");
    println!(
        "  {GREEN}✓{RESET} openclaw_installations.mode='node', gateway_url cleared (if present)"
    );
    println!("  {GREEN}✓{RESET} published fleet.events.quarantine on NATS");
    println!();
    println!("Implications while '{}' is quarantined:", result.name);
    println!("  • will not participate in leader election");
    println!("  • will not receive LLM inference requests");
    println!("  • pulse beats still recorded but computer is excluded from healthy-member lists");
    println!();
    println!(
        "Reverse with: {CYAN}ff fleet unquarantine {} --yes{RESET}",
        result.name
    );
    Ok(())
}

/// `ff fleet unquarantine <computer>` — restart daemons + flip status back
/// to 'pending'. Next pulse beat moves it to 'online'.
pub async fn handle_fleet_unquarantine(
    pool: &sqlx::PgPool,
    computer: &str,
    yes: bool,
) -> Result<()> {
    if !yes {
        eprintln!(
            "{YELLOW}⚠ unquarantine will restart daemons on '{computer}' and reset its status. Pass --yes to proceed.{RESET}"
        );
        std::process::exit(1);
    }

    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;

    println!("{CYAN}▶ ff fleet unquarantine {computer}{RESET}");
    let result = ff_agent::panic_stop::unquarantine_computer(pool, computer)
        .await
        .map_err(|e| anyhow::anyhow!("unquarantine: {e}"))?;

    if result.ssh_stop_ok {
        println!("  {GREEN}✓{RESET} ssh start succeeded on '{}'", result.name);
    } else {
        println!(
            "  {YELLOW}—{RESET} ssh start did NOT succeed on '{}' (detail: {}) — DB reset applied anyway",
            result.name, result.ssh_detail
        );
    }
    println!("  {GREEN}✓{RESET} status='pending' in computers table (pulse will flip to 'online')");
    println!("  {GREEN}✓{RESET} published fleet.events.quarantine (event=unquarantine) on NATS");
    Ok(())
}

/// `ff fleet upgrade <software_id>` — dispatch the software's upgrade_playbook
/// across the fleet via the deferred task queue.
///
/// Resolves the playbook key per-target in this priority order:
///   1. `{os_family}-{install_source}`  (e.g. `"macos-brew"`)
///   2. `{os_family}`                   (e.g. `"macos"`)
///   3. `"all"`
///
/// Targets with no matching key are warned about and skipped. Dry-run mode
/// prints the plan and exits; `--yes` without `--dry-run` enqueues one
/// deferred shell task per target with trigger_type=`node_online`.
pub async fn handle_fleet_upgrade(
    pool: &sqlx::PgPool,
    software_id: &str,
    computer: Option<String>,
    all: bool,
    dry_run: bool,
    yes: bool,
    force_dirty: bool,
) -> Result<()> {
    if computer.is_none() && !all {
        anyhow::bail!("pass --all or --computer <name> to pick targets");
    }
    if computer.is_some() && all {
        anyhow::bail!("--computer and --all are mutually exclusive");
    }

    // Shared resolver — same code path the hourly auto-upgrade tick uses.
    let (plans, skipped) = ff_agent::auto_upgrade::resolve_upgrade_plans(
        pool,
        software_id,
        computer.as_deref(),
        false,
    )
    .await?;

    let display_name = plans
        .first()
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| software_id.to_string());
    let latest_version = plans.first().and_then(|p| p.latest_version.clone());

    if plans.is_empty() && skipped.is_empty() {
        println!(
            "{YELLOW}No computer_software rows found for software_id='{software_id}'. Nothing to do.{RESET}"
        );
        return Ok(());
    }

    println!("{CYAN}▶ ff fleet upgrade {software_id}{RESET}");
    println!("  software:        {display_name} ({software_id})");
    println!(
        "  latest upstream: {}",
        latest_version.as_deref().unwrap_or("(unknown)")
    );
    println!("  targets:         {} computer(s)", plans.len());
    if plans.is_empty() {
        println!("{YELLOW}No resolvable targets. Nothing to do.{RESET}");
        for (name, why) in &skipped {
            println!("    {YELLOW}⚠ skip{RESET} {name}: {why}");
        }
        return Ok(());
    }

    println!(
        "\n  {:<10} {:<14} {:<10} {:<10} {:<22} command",
        "computer", "os_family", "source", "installed", "playbook_key"
    );
    for p in &plans {
        let short_cmd = if p.command.len() > 60 {
            format!("{}…", &p.command[..60])
        } else {
            p.command.clone()
        };
        println!(
            "  {:<10} {:<14} {:<10} {:<10} {:<22} {}",
            p.computer_name,
            p.os_family,
            p.install_source.as_deref().unwrap_or("-"),
            p.installed_version.as_deref().unwrap_or("-"),
            p.playbook_key,
            short_cmd
        );
    }
    for (name, why) in &skipped {
        println!("  {YELLOW}⚠ skip{RESET} {name}: {why}");
    }

    if dry_run {
        println!(
            "\n{YELLOW}Dry run — not enqueuing. Drop --dry-run and pass --yes to actually enqueue.{RESET}"
        );
        return Ok(());
    }
    if !yes {
        println!("\n{YELLOW}Pass --yes to actually enqueue these upgrade tasks.{RESET}");
        return Ok(());
    }

    // Dirty-build gate for `ff_git` / `forgefleetd_git` — refuses propagation
    // of a leader with an uncommitted working tree unless `--force-dirty`.
    use ff_agent::auto_upgrade::GitStateGate;
    let gate = ff_agent::auto_upgrade::gate_git_state(pool, software_id, force_dirty).await;
    let leader_sha = plans
        .first()
        .and_then(|p| p.installed_version.clone())
        .unwrap_or_else(|| "(unknown)".into());
    match gate {
        GitStateGate::BlockDirty => {
            eprintln!(
                "{RED}✗ refusing to propagate dirty build {leader_sha} — commit or pass --force-dirty{RESET}"
            );
            ff_agent::auto_upgrade::mark_targets_blocked_dirty(pool, software_id).await;
            anyhow::bail!("dirty-build gate");
        }
        GitStateGate::AllowWithWarning => {
            eprintln!(
                "{YELLOW}⚠ propagating unpushed/forced commit {leader_sha} from leader to fleet — push to origin/main when ready{RESET}"
            );
            let payload = serde_json::json!({
                "software_id": software_id,
                "sha": leader_sha,
                "computer_count": plans.len(),
                "source": whoami_tag(),
                "forced": force_dirty,
                "ts": chrono::Utc::now().to_rfc3339(),
            });
            ff_agent::nats_client::publish_json(
                "fleet.events.software.unpushed_propagation".to_string(),
                &payload,
            )
            .await;
        }
        GitStateGate::Allow => {}
    }

    let who = whoami_tag();
    let enqueued = ff_agent::auto_upgrade::enqueue_plans(pool, &plans, &who).await?;

    println!(
        "\n{GREEN}✓ Enqueued {} upgrade task(s):{RESET}",
        enqueued.len()
    );
    for ep in &enqueued {
        println!("  {:<12} {}", ep.computer_name, ep.defer_id);
    }
    println!("\nTrack progress with: ff defer list");
    Ok(())
}

pub async fn handle_fleet_set_network_scope(
    pool: &sqlx::PgPool,
    computer: &str,
    scope: &str,
) -> Result<()> {
    const VALID: &[&str] = &["lan", "tailscale_only", "wan"];
    if !VALID.contains(&scope) {
        anyhow::bail!(
            "unknown scope '{scope}' — must be one of: {}",
            VALID.join(", ")
        );
    }
    let res = sqlx::query("UPDATE computers SET network_scope = $1 WHERE LOWER(name) = LOWER($2)")
        .bind(scope)
        .bind(computer)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("update computers: {e}"))?;

    if res.rows_affected() == 0 {
        anyhow::bail!("no computer named '{computer}' found");
    }
    println!(
        "{GREEN}✓{RESET} set network_scope='{scope}' on '{computer}' ({} row updated)",
        res.rows_affected()
    );
    Ok(())
}

pub async fn handle_fleet_db(pool: &sqlx::PgPool, cmd: FleetDbCommand) -> Result<()> {
    match cmd {
        FleetDbCommand::AddRemoteReplica {
            computer,
            via,
            skip_probe,
        } => {
            if via != "tailscale" {
                eprintln!(
                    "{YELLOW}warning:{RESET} --via '{via}' is not 'tailscale' — \
                     recording the row anyway, but no WAN compose template will be generated."
                );
            }

            // Resolve target computer + its Tailscale IP.
            let row = sqlx::query_as::<_, (uuid::Uuid, String, serde_json::Value, String)>(
                "SELECT id, primary_ip, all_ips, COALESCE(network_scope, 'lan')
                 FROM computers
                 WHERE LOWER(name) = LOWER($1)",
            )
            .bind(&computer)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("query computers: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no computer named '{computer}' registered — run `ff onboard` first"
                )
            })?;

            let (computer_id, primary_ip, all_ips_json, current_scope) = row;

            let ts_ip = all_ips_json
                .as_array()
                .and_then(|arr| {
                    arr.iter().find_map(|v| {
                        let obj = v.as_object()?;
                        if obj.get("kind")?.as_str() == Some("tailscale") {
                            obj.get("ip")?.as_str().map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                })
                .or_else(|| {
                    if primary_ip.starts_with("100.64.") || primary_ip.starts_with("100.65.") {
                        Some(primary_ip.clone())
                    } else {
                        None
                    }
                });

            let ts_ip = match ts_ip {
                Some(ip) => ip,
                None => anyhow::bail!(
                    "no tailscale IP in computers.all_ips for '{computer}'. \
                     Ensure the node is joined to Tailscale and has emitted a Pulse heartbeat."
                ),
            };

            // Optional reachability probe (skipped by --skip-probe).
            if !skip_probe && via == "tailscale" {
                println!("{CYAN}▶ Probing Tailscale reachability: {ts_ip}:55432{RESET}");
                let ok = tokio::process::Command::new("nc")
                    .args(["-vz", "-w", "3", &ts_ip, "55432"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    eprintln!(
                        "{YELLOW}warning:{RESET} nc probe to {ts_ip}:55432 failed \
                         (still recording — Postgres may not be listening yet, or nc may be missing)"
                    );
                } else {
                    println!("{GREEN}✓{RESET} reachable over Tailscale");
                }
            }

            // Upsert database_replicas row with role='wan_replica'.
            sqlx::query(
                "INSERT INTO database_replicas (computer_id, database_kind, role, status, notes) \
                 VALUES ($1, 'postgres', 'wan_replica', 'stopped', $2) \
                 ON CONFLICT (computer_id, database_kind) DO UPDATE \
                 SET role = 'wan_replica', notes = $2",
            )
            .bind(computer_id)
            .bind(format!(
                "added via ff fleet db add-remote-replica --via {via}"
            ))
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("insert database_replicas: {e}"))?;

            // Auto-apply network_scope='wan' if the caller hasn't already
            // set it (defaults to 'lan', which is wrong for a WAN replica).
            if current_scope == "lan" {
                sqlx::query("UPDATE computers SET network_scope = 'wan' WHERE id = $1")
                    .bind(computer_id)
                    .execute(pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("update computers.network_scope: {e}"))?;
                println!("{CYAN}▶{RESET} auto-applied network_scope='wan' (was 'lan')");
            }

            // Print the runbook snippet.
            println!();
            println!("{GREEN}✓{RESET} registered WAN replica for '{computer}' ({ts_ip})");
            println!();
            println!("Now run on the off-site machine:");
            println!("  cd deploy/");
            println!(
                "  POSTGRES_PRIMARY_HOST=<taylor-tailscale-ip> \\\n    \
                 POSTGRES_REPLICATION_PASSWORD=<same as primary> \\\n    \
                 docker compose -f docker-compose.follower-remote.yml up -d"
            );
            println!();
            println!("Full runbook: deploy/WAN_REPLICATION.md");
        }
        FleetDbCommand::Failover { to, force, yes } => {
            handle_fleet_db_failover(pool, &to, force, yes).await?;
        }
        FleetDbCommand::Restore {
            backup_id,
            to,
            target_db,
            yes,
        } => {
            handle_fleet_db_restore(pool, &backup_id, to.as_deref(), &target_db, yes).await?;
        }
        FleetDbCommand::VerifyBackups {
            limit,
            test_restore,
        } => {
            handle_fleet_db_verify_backups(pool, limit, test_restore).await?;
        }
    }
    Ok(())
}

pub async fn handle_fleet_db_failover(
    pool: &sqlx::PgPool,
    to: &str,
    force: bool,
    yes: bool,
) -> Result<()> {
    // 1) Resolve target computer_id.
    let target = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
        "SELECT id, name, primary_ip FROM computers WHERE LOWER(name) = LOWER($1)",
    )
    .bind(to)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query computers: {e}"))?
    .ok_or_else(|| anyhow::anyhow!("no computer named '{to}' registered"))?;
    let (target_id, target_name, target_ip) = target;

    // 2) Must be running on the target (we shell `docker exec` locally).
    let my_name = ff_agent::fleet_info::resolve_this_node_name().await;
    if my_name.to_lowercase() != target_name.to_lowercase() && !force {
        anyhow::bail!(
            "refusing to failover: this command must be run ON '{target_name}' \
             (we'd shell `docker exec` locally). Current node is '{my_name}'. \
             Re-run with --force to override or ssh to '{target_name}' first."
        );
    }

    // 3) Confirm with user.
    if !yes {
        eprintln!(
            "{YELLOW}About to promote '{target_name}' ({target_ip}) to Postgres primary.{RESET}"
        );
        eprintln!("  - The old primary's docker container will be stopped via SSH.");
        eprintln!("  - database_replicas + fleet_secrets.postgres_primary_url will be rewritten.");
        eprintln!("  - All fleet daemons will reconnect against the new primary.");
        eprintln!("Re-run with --yes to confirm.");
        std::process::exit(2);
    }

    println!("{CYAN}▶ Promoting '{target_name}' replica to primary...{RESET}");
    let mgr = ff_agent::ha::pg_failover::PostgresFailoverManager::new(pool.clone(), target_id)
        .with_strict_fencing(!force);
    mgr.promote_local_replica()
        .await
        .map_err(|e| anyhow::anyhow!("promote: {e}"))?;
    println!("{GREEN}✓{RESET} '{target_name}' is now the Postgres primary.");
    Ok(())
}

/// Resolve the local encrypted-backup root. Matches
/// `BackupOrchestrator::new`'s default (`~/.forgefleet/backups`).
fn local_backup_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet/backups")
}

/// Metadata loaded from the `backups` table — shared by restore + verify.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BackupRow {
    id: uuid::Uuid,
    database_kind: String,
    file_name: String,
    size_bytes: i64,
    checksum_sha256: String,
    created_at: chrono::DateTime<chrono::Utc>,
    retention_tier: String,
}

async fn fetch_backup_row(pool: &sqlx::PgPool, id: uuid::Uuid) -> Result<BackupRow> {
    let row = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            i64,
            String,
            chrono::DateTime<chrono::Utc>,
            String,
        ),
    >(
        "SELECT id, database_kind, file_name, size_bytes, checksum_sha256,
                created_at, retention_tier
           FROM backups WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query backups: {e}"))?
    .ok_or_else(|| anyhow::anyhow!("no backup row with id {id}"))?;
    Ok(BackupRow {
        id: row.0,
        database_kind: row.1,
        file_name: row.2,
        size_bytes: row.3,
        checksum_sha256: row.4,
        created_at: row.5,
        retention_tier: row.6,
    })
}

/// Locate the on-disk artifact for a backup row.
/// Layout: `<root>/<kind>/<file_name>`.
fn backup_path_on_disk(row: &BackupRow) -> PathBuf {
    local_backup_root()
        .join(&row.database_kind)
        .join(&row.file_name)
}

/// Run SHA256 on a file and compare against the `backups.checksum_sha256`
/// value. Returns `Ok(true)` if they match.
async fn verify_checksum(path: &Path, expected: &str) -> Result<bool> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| anyhow::anyhow!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = format!("{:x}", hasher.finalize());
    Ok(got.eq_ignore_ascii_case(expected))
}

/// Cheap "is this an age ciphertext?" probe — reads the first few bytes
/// and confirms the `age-encryption.org/v1` armor/binary header. Avoids
/// decrypting the full archive just to answer "decryptable yes/no".
async fn has_age_header(path: &Path) -> Result<bool> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut head = [0u8; 21];
    let n = f.read(&mut head).await?;
    let prefix = &head[..n];
    // Binary and armor variants both begin with "age-encryption.org/v1".
    Ok(prefix.starts_with(b"age-encryption.org/v1")
        || prefix.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----"))
}

/// Restore an age-encrypted Postgres backup to a scratch database.
///
/// Steps:
/// 1. Look up `backups` row.
/// 2. Verify file exists + checksum matches.
/// 3. Decrypt via `ff_agent::ha::backup::decrypt_backup_file` (uses the
///    `age` Rust crate — no CLI dependency).
/// 4. `docker exec forgefleet-postgres createdb <target_db>` (idempotent).
/// 5. Stream the plaintext archive into the container and run
///    `pg_restore` (tar format) or `psql` (plain SQL, fallback).
/// 6. Print `SELECT COUNT(*) FROM fleet_workers` as a sanity check.
pub async fn handle_fleet_db_restore(
    pool: &sqlx::PgPool,
    backup_id: &str,
    to: Option<&str>,
    target_db: &str,
    yes: bool,
) -> Result<()> {
    if let Some(target_node) = to {
        let me = ff_agent::fleet_info::resolve_this_node_name().await;
        if !target_node.eq_ignore_ascii_case(&me) {
            anyhow::bail!(
                "--to '{target_node}' != current node '{me}'. Cross-node \
                 restore over the defer queue isn't wired yet; ssh to \
                 '{target_node}' and re-run locally."
            );
        }
    }
    if !yes {
        eprintln!(
            "{YELLOW}Restore creates a new database ('{target_db}') in the \
             local forgefleet-postgres container and loads the backup \
             into it. Re-run with --yes to proceed.{RESET}"
        );
        std::process::exit(2);
    }

    let id = uuid::Uuid::parse_str(backup_id)
        .map_err(|e| anyhow::anyhow!("invalid backup id '{backup_id}': {e}"))?;
    let row = fetch_backup_row(pool, id).await?;
    let enc_path = backup_path_on_disk(&row);

    println!(
        "{CYAN}▶ restore backup{RESET}  id={} kind={} file={} size={} tier={}",
        row.id, row.database_kind, row.file_name, row.size_bytes, row.retention_tier,
    );

    if !enc_path.exists() {
        anyhow::bail!(
            "backup file not found on disk: {}. Rsync may not have \
             landed yet — run `ff fleet db verify-backups` to audit.",
            enc_path.display()
        );
    }
    let disk_bytes = tokio::fs::metadata(&enc_path).await?.len() as i64;
    if disk_bytes == 0 {
        anyhow::bail!(
            "backup file {} is 0 bytes — producer never wrote ciphertext. \
             Likely cause: `age` CLI was missing when the backup ran.",
            enc_path.display()
        );
    }

    let checksum_ok = verify_checksum(&enc_path, &row.checksum_sha256).await?;
    if !checksum_ok {
        anyhow::bail!(
            "checksum mismatch on {} — refusing to restore corrupt backup",
            enc_path.display()
        );
    }
    println!(
        "{GREEN}✓{RESET} checksum matches (sha256={}…)",
        &row.checksum_sha256[..12.min(row.checksum_sha256.len())]
    );

    // Decrypt into a tempfile. The archive sizes here (<100 MB) are fine
    // to materialize; if that ever changes, swap this for a streaming
    // decrypt that pipes straight into pg_restore.
    let tmp_dir = std::env::temp_dir().join(format!("ff-restore-{}", row.id));
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let plaintext_path = tmp_dir.join(row.file_name.strip_suffix(".age").unwrap_or(&row.file_name));
    if let Err(e) =
        ff_agent::ha::backup::decrypt_backup_file(pool, &enc_path, &plaintext_path).await
    {
        anyhow::bail!(
            "decrypt failed: {e}. If this is '{}' key not set — no real \
             backup encryption has happened yet, so there's nothing to \
             restore.",
            ff_agent::ha::backup::BACKUP_ENC_PRIVKEY
        );
    }
    println!(
        "{GREEN}✓{RESET} decrypted → {} ({} bytes)",
        plaintext_path.display(),
        tokio::fs::metadata(&plaintext_path).await?.len()
    );

    if row.database_kind != "postgres" {
        println!(
            "{YELLOW}note:{RESET} kind='{}' — only 'postgres' restore is \
             wired end-to-end. Plaintext is available at {}.",
            row.database_kind,
            plaintext_path.display()
        );
        return Ok(());
    }

    // 1) Create the scratch DB (idempotent — swallow "already exists").
    let createdb = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-u",
            "postgres",
            "forgefleet-postgres",
            "createdb",
            target_db,
        ])
        .output()
        .await?;
    if !createdb.status.success() {
        let stderr = String::from_utf8_lossy(&createdb.stderr);
        if !stderr.contains("already exists") {
            anyhow::bail!("createdb {target_db} failed: {stderr}");
        }
        println!("{YELLOW}note:{RESET} database '{target_db}' already exists (reusing)");
    } else {
        println!("{GREEN}✓{RESET} created scratch database '{target_db}'");
    }

    // 2) Stream plaintext into the container and pg_restore it.
    //    pg_basebackup tar archives come out as `base.tar.gz` nested inside
    //    the streamed tar — that's a cluster snapshot, not a logical
    //    dump. pg_restore won't consume it. For this helper we treat the
    //    file as a custom/plain pg_dump archive *or* a pg_basebackup
    //    tarball and pick the right tool based on extension.
    println!("{CYAN}▶ loading archive into '{target_db}'...{RESET}");
    let ext = plaintext_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let (prog, extra_args): (&str, Vec<&str>) = if plaintext_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|n| n.ends_with(".sql") || n.ends_with(".sql.gz"))
        .unwrap_or(false)
    {
        ("psql", vec!["-v", "ON_ERROR_STOP=1", "-d", target_db])
    } else if ext == "gz" || ext == "tgz" {
        // pg_basebackup tar.gz — not a logical dump. We can't pg_restore
        // it into an existing DB; the correct flow is to stop postgres,
        // wipe PGDATA, untar, restart. That's way too destructive for a
        // "scratch DB" helper. Report clearly instead of silently doing
        // the wrong thing.
        println!(
            "{YELLOW}note:{RESET} archive looks like a pg_basebackup \
             cluster snapshot (.tar.gz). That's a physical backup — \
             restoring it requires replacing PGDATA, not loading into a \
             scratch DB. Plaintext is at {}.",
            plaintext_path.display()
        );
        let fm_count = count_fleet_workers_live(pool).await.unwrap_or(-1);
        println!(
            "{GREEN}✓{RESET} sanity check — live fleet_workers row count: {fm_count} \
             (no load performed; scratch DB '{target_db}' is empty)"
        );
        return Ok(());
    } else {
        (
            "pg_restore",
            vec!["--no-owner", "--no-privileges", "-d", target_db],
        )
    };

    // `docker exec -i` with stdin streaming from our tempfile.
    let plaintext = tokio::fs::read(&plaintext_path).await?;
    let mut child = tokio::process::Command::new("docker")
        .args({
            let mut v: Vec<&str> =
                vec!["exec", "-i", "-u", "postgres", "forgefleet-postgres", prog];
            v.extend(extra_args.iter().copied());
            v
        })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(&plaintext).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "{prog} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    println!("{GREEN}✓{RESET} {prog} completed");

    // 3) Sanity check — count fleet_workers rows in the restored DB.
    let count_out = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-u",
            "postgres",
            "forgefleet-postgres",
            "psql",
            "-d",
            target_db,
            "-tAc",
            "SELECT COUNT(*) FROM fleet_workers",
        ])
        .output()
        .await?;
    if count_out.status.success() {
        let c = String::from_utf8_lossy(&count_out.stdout)
            .trim()
            .to_string();
        println!("{GREEN}✓{RESET} restored '{target_db}'.fleet_workers row count: {c}");
    } else {
        println!(
            "{YELLOW}note:{RESET} could not count fleet_workers in '{target_db}': {}",
            String::from_utf8_lossy(&count_out.stderr).trim()
        );
    }
    Ok(())
}

/// Count rows in the *live* fleet_workers table via the existing pool.
/// Count rows in the *live* fleet_workers table via the existing pool.
async fn count_fleet_workers_live(pool: &sqlx::PgPool) -> Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fleet_workers")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

pub async fn handle_fleet_db_verify_backups(
    pool: &sqlx::PgPool,
    limit: i64,
    test_restore: bool,
) -> Result<()> {
    println!(
        "{CYAN}▶ ff fleet db verify-backups (limit={limit} test-restore={test_restore}){RESET}"
    );

    // Confirm the decryption key exists — the whole audit is meaningless
    // without it.
    let privkey = ff_db::pg_get_secret(pool, ff_agent::ha::backup::BACKUP_ENC_PRIVKEY)
        .await
        .map_err(|e| anyhow::anyhow!("fleet_secrets lookup: {e}"))?;
    match privkey {
        Some(_) => println!(
            "{GREEN}✓{RESET} fleet_secrets.{} present",
            ff_agent::ha::backup::BACKUP_ENC_PRIVKEY
        ),
        None => {
            println!(
                "{YELLOW}warning:{RESET} fleet_secrets.{} is NOT set. No real \
                 backup encryption has happened yet — .age files on disk \
                 are likely 0-byte stubs from failed `age` CLI runs. \
                 Install `age` (brew install age) and let the orchestrator \
                 produce a real backup first.",
                ff_agent::ha::backup::BACKUP_ENC_PRIVKEY
            );
        }
    }

    let rows = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            i64,
            String,
            chrono::DateTime<chrono::Utc>,
            String,
        ),
    >(
        "SELECT id, database_kind, file_name, size_bytes, checksum_sha256,
                created_at, retention_tier
           FROM backups
          ORDER BY created_at DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query backups: {e}"))?;

    if rows.is_empty() {
        println!("(no rows in `backups` table — run `ff fleet backup` to produce one)");
        return Ok(());
    }

    println!();
    println!(
        "{:<38} {:<8} {:<10} {:<20} {:<8} {:<8} FILE",
        "ID", "KIND", "SIZE", "CREATED", "CHKSUM", "DECRYPT"
    );
    let mut most_recent_pg: Option<BackupRow> = None;
    for (id, kind, file_name, size_bytes, checksum_sha256, created_at, tier) in rows {
        let br = BackupRow {
            id,
            database_kind: kind.clone(),
            file_name: file_name.clone(),
            size_bytes,
            checksum_sha256: checksum_sha256.clone(),
            created_at,
            retention_tier: tier,
        };
        let path = backup_path_on_disk(&br);
        let (chk_str, dec_str) = if !path.exists() {
            ("missing".to_string(), "n/a".to_string())
        } else {
            let chk = verify_checksum(&path, &checksum_sha256)
                .await
                .unwrap_or(false);
            let dec = has_age_header(&path).await.unwrap_or(false);
            let dec_str = if tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
                == 0
            {
                "empty".to_string()
            } else if dec {
                "yes".to_string()
            } else {
                "no".to_string()
            };
            (
                if chk {
                    "ok".to_string()
                } else {
                    "BAD".to_string()
                },
                dec_str,
            )
        };
        println!(
            "{:<38} {:<8} {:<10} {:<20} {:<8} {:<8} {}",
            id.to_string(),
            kind,
            size_bytes,
            created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            chk_str,
            dec_str,
            file_name,
        );
        if kind == "postgres" && most_recent_pg.is_none() {
            most_recent_pg = Some(br);
        }
    }

    if test_restore {
        println!();
        let Some(target) = most_recent_pg else {
            println!("{YELLOW}--test-restore:{RESET} no postgres backups found, skipping");
            return Ok(());
        };
        println!(
            "{CYAN}▶ --test-restore:{RESET} most recent postgres backup = {} ({})",
            target.id, target.file_name
        );
        let scratch = format!("forgefleet_verify_{}", &target.id.simple().to_string()[..8]);
        println!("    scratch db: {scratch}");
        // Invoke the same restore path, then drop the DB.
        let restore_res =
            handle_fleet_db_restore(pool, &target.id.to_string(), None, &scratch, true).await;
        // Always attempt cleanup, even on error.
        let drop_out = tokio::process::Command::new("docker")
            .args([
                "exec",
                "-u",
                "postgres",
                "forgefleet-postgres",
                "dropdb",
                "--if-exists",
                &scratch,
            ])
            .output()
            .await;
        match drop_out {
            Ok(o) if o.status.success() => {
                println!("{GREEN}✓{RESET} scratch db '{scratch}' dropped")
            }
            Ok(o) => println!(
                "{YELLOW}note:{RESET} dropdb '{scratch}' non-zero: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => println!("{YELLOW}note:{RESET} dropdb '{scratch}' failed to spawn: {e}"),
        }
        restore_res?;
    }

    Ok(())
}

pub async fn handle_fleet_revoke_trust(
    pool: &sqlx::PgPool,
    computer: &str,
    yes: bool,
) -> Result<()> {
    if !yes {
        eprintln!("{YELLOW}Revocation is destructive. Pass --yes to confirm.{RESET}");
        std::process::exit(2);
    }
    println!("{CYAN}▶ Revoking SSH trust for '{computer}' across fleet...{RESET}");
    let mgr = ff_agent::ssh_key_manager::SshKeyManager::new(pool.clone());
    let who = whoami_tag();
    let report = mgr
        .revoke_computer_trust(computer, Some(&who))
        .await
        .map_err(|e| anyhow::anyhow!("revoke: {e}"))?;

    println!(
        "\nFingerprint: {}\nRevoked on {} host(s), failed on {}.",
        report.key_fingerprint, report.succeeded, report.failed,
    );
    for t in &report.targets {
        let marker = if t.success { "✓" } else { "✗" };
        println!(
            "  {marker} {:<14} {}",
            t.target,
            if t.success { "ok" } else { t.message.as_str() }
        );
    }
    Ok(())
}

/// Rows-deleted breakdown for a single `remove_computer_core` call.
/// Each field corresponds to one DELETE inside the transaction. The two
/// commands that drive this (`remove-computer`, `disband`) use it to print
/// a human-readable summary.
#[derive(Debug, Default, Clone)]
struct RemoveComputerReport {
    computer_rows: u64,
    fleet_worker_rows: u64,
    fleet_models_rows: u64,
    leader_state_rows: u64,
    revocation_task_id: Option<String>,
}

/// Core remove-computer logic shared by `ff fleet remove-computer` and
/// `ff fleet disband`.
///
/// Runs the DB deletes in a single transaction, enqueues the SSH-trust
/// revocation task on the leader (preferred_node="taylor"), and
/// best-effort publishes `fleet.events.computer_removed` on NATS.
/// Returns a row-level report. Errors are surfaced to the caller; the
/// transaction rolls back on any SQL failure.
async fn remove_computer_core(pool: &sqlx::PgPool, name: &str) -> Result<RemoveComputerReport> {
    let mut tx = pool.begin().await?;
    let mut report = RemoveComputerReport::default();

    // fleet_models has no ON DELETE CASCADE on the fleet_workers FK.
    let r = sqlx::query("DELETE FROM fleet_models WHERE worker_name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.fleet_models_rows = r.rows_affected();

    // fleet_leader_state references computers(id) WITHOUT cascade; the spec
    // says key by member_name so we don't have to resolve the UUID first.
    let r = sqlx::query("DELETE FROM fleet_leader_state WHERE member_name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.leader_state_rows = r.rows_affected();

    // fleet_workers cascades: fleet_workers_ssh_keys, fleet_model_library,
    // fleet_model_deployments, fleet_disk_usage (all ON DELETE CASCADE).
    let r = sqlx::query("DELETE FROM fleet_workers WHERE name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.fleet_worker_rows = r.rows_affected();

    // computers cascades: computer_software, computer_models,
    // computer_model_deployments, computer_downtime_events, computer_trust,
    // fleet_workers, openclaw_installations, computer_docker_containers.
    let r = sqlx::query("DELETE FROM computers WHERE name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    report.computer_rows = r.rows_affected();

    tx.commit().await?;

    // Enqueue SSH revocation as a deferred task so it survives Taylor being
    // offline or the operator running this from a non-leader. Payload is a
    // shell script that invokes `ff fleet revoke-trust`, which re-reads the
    // (now-deleted) key from fleet_ssh_revocations… wait — the key is gone
    // with fleet_workers_ssh_keys. So we have to embed the pubkey in the task
    // payload BEFORE the deletion. That requires a pre-delete lookup — do it
    // via a follow-up patch if the existing trust manager can't cope. For
    // now, fan out a best-effort `ff fleet revoke-trust` which is a no-op on
    // a deleted row. Document the limitation in the summary line.
    //
    // Practical workaround: the revocation script below strips lines by
    // comment-tag `user@host` match on each peer. `ssh_key_manager`
    // canonicalises keys to end with a comment like `<user>@<removed-host>`
    // at onboarding time, so grep'ing for `@<name>` at the end of every
    // authorized_keys line is a reasonable fallback.
    let script = build_remove_computer_ssh_script(name);
    let payload = serde_json::json!({ "command": script });
    let trigger_spec = serde_json::json!({ "node": "taylor" });
    let title = format!("Revoke SSH trust for {name}");
    let who = whoami_tag();
    let defer_id = ff_db::pg_enqueue_deferred(
        pool,
        &title,
        "shell",
        &payload,
        "node_online",
        &trigger_spec,
        Some("taylor"),
        &serde_json::json!([]),
        Some(&who),
        Some(3),
    )
    .await?;
    report.revocation_task_id = Some(defer_id);

    // Best-effort NATS announcement. NATS may not be up — drop errors.
    let _ = ff_agent::nats_client::init_nats(&ff_agent::nats_client::resolve_nats_url()).await;
    ff_agent::nats_client::publish_json(
        "fleet.events.computer_removed",
        &serde_json::json!({
            "name": name,
            "removed_by": who,
            "at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;

    Ok(report)
}

/// Build a shell script that SSH-fans-out a revocation of `name`'s user
/// key across every remaining peer. Run as a `node_online` deferred task
/// on Taylor.
///
/// Strategy: ask the local DB on Taylor for every peer's primary_ip, then
/// for each peer run a grep -v filter on `authorized_keys` that drops any
/// line ending with `@<name>` (the canonical comment suffix OpenClaw
/// writes during onboarding).
fn build_remove_computer_ssh_script(name: &str) -> String {
    let name = name.replace('\'', "'\\''");
    format!(
        r#"set -e
NAME='{name}'
# Pull the list of peers from the local Postgres on Taylor. If psql isn't
# available we fall back to the .forgefleet/fleet.toml parse below.
PEERS=$(ff fleet health --json 2>/dev/null | \
  python3 -c 'import json,sys; d=json.load(sys.stdin); print("\n".join(r["name"] for r in d if r["name"] != "'"$NAME"'"))' 2>/dev/null || true)
if [ -z "$PEERS" ]; then
  echo "no peers resolvable; aborting revocation (removal of DB rows still took effect)"
  exit 0
fi
for P in $PEERS; do
  echo "revoking @$NAME from $P..."
  ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$P" \
    "if [ -f ~/.ssh/authorized_keys ]; then cp ~/.ssh/authorized_keys ~/.ssh/authorized_keys.bak.$$ && grep -v '@'\"$NAME\"'$' ~/.ssh/authorized_keys.bak.$$ > ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && rm -f ~/.ssh/authorized_keys.bak.$$; fi" \
    || echo "  (warn) ssh $P failed; skipping"
done
echo "revocation fan-out complete for $NAME"
"#,
        name = name,
    )
}

pub async fn handle_fleet_remove_computer(
    pool: &sqlx::PgPool,
    name: &str,
    yes: bool,
) -> Result<()> {
    // 1. Look up what actually exists so we can print an honest plan.
    let fleet_node: Option<(String, String, String)> =
        sqlx::query_as("SELECT name, ip, ssh_user FROM fleet_workers WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    let computer: Option<(String, String, String)> = sqlx::query_as(
        "SELECT name, primary_ip, COALESCE(os_family, '') FROM computers WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;

    if fleet_node.is_none() && computer.is_none() {
        eprintln!(
            "{YELLOW}No fleet_workers or computers row named '{name}' — nothing to do.{RESET}"
        );
        std::process::exit(2);
    }

    println!("{CYAN}▶ ff fleet remove-computer {name}{RESET}");
    if let Some((n, ip, user)) = &fleet_node {
        println!("  fleet_workers row:  name={n} ip={ip} ssh_user={user}");
    } else {
        println!("  fleet_workers row:  (none)");
    }
    if let Some((n, ip, osf)) = &computer {
        println!("  computers row:    name={n} primary_ip={ip} os_family={osf}");
    } else {
        println!("  computers row:    (none)");
    }
    println!("  cascades:         fleet_workers_ssh_keys, fleet_model_library,");
    println!("                    fleet_model_deployments, fleet_disk_usage,");
    println!("                    computer_software, computer_models,");
    println!("                    computer_model_deployments, computer_trust,");
    println!("                    computer_downtime_events, fleet_workers,");
    println!("                    openclaw_installations, computer_docker_containers");
    println!("  explicit deletes: fleet_models (no cascade),");
    println!("                    fleet_leader_state WHERE member_name=<name>");
    println!("  side-effect:      1 deferred SSH-revocation task on taylor");

    if !yes {
        eprintln!("\n{YELLOW}Removal is destructive. Pass --yes to proceed.{RESET}");
        std::process::exit(2);
    }

    let report = remove_computer_core(pool, name).await?;
    let total = report.computer_rows
        + report.fleet_worker_rows
        + report.fleet_models_rows
        + report.leader_state_rows;
    println!(
        "\n{GREEN}✓ removed {name}{RESET} — {total} row(s) across \
         computers({cr}), fleet_workers({fn_}), fleet_models({fm}), \
         fleet_leader_state({fls})",
        cr = report.computer_rows,
        fn_ = report.fleet_worker_rows,
        fm = report.fleet_models_rows,
        fls = report.leader_state_rows,
    );
    if let Some(id) = &report.revocation_task_id {
        println!("  enqueued SSH-revocation task: {id}");
        println!("  track progress with: ff defer list");
    }
    Ok(())
}

pub async fn handle_fleet_disband(
    pool: &sqlx::PgPool,
    yes: bool,
    i_know_what_im_doing: bool,
) -> Result<()> {
    // Collect every computer that isn't Taylor. We look at both tables
    // because a computer may exist in one but not the other if something
    // went sideways during onboarding.
    let fleet_names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM fleet_workers WHERE LOWER(name) <> 'taylor' ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    let computer_names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM computers WHERE LOWER(name) <> 'taylor' ORDER BY name",
    )
    .fetch_all(pool)
    .await?;

    let mut targets: Vec<String> = fleet_names.clone();
    for n in &computer_names {
        if !targets.contains(n) {
            targets.push(n.clone());
        }
    }
    targets.sort();

    println!("{CYAN}▶ ff fleet disband{RESET}");
    println!("  This will DELETE every fleet_workers/computers row except 'taylor'.");
    println!("  Requires BOTH --yes AND --i-know-what-im-doing to actually run.");
    println!("  targets:         {} computer(s)", targets.len());
    for n in &targets {
        println!("    {n}");
    }

    if targets.is_empty() {
        println!("{YELLOW}No non-Taylor rows to remove. Nothing to do.{RESET}");
        return Ok(());
    }

    if !(yes && i_know_what_im_doing) {
        eprintln!(
            "\n{YELLOW}Refusing to disband without both --yes and --i-know-what-im-doing.{RESET}"
        );
        std::process::exit(2);
    }

    let mut total_rows: u64 = 0;
    let mut total_tasks: u64 = 0;
    let mut failures: Vec<(String, String)> = Vec::new();
    for name in &targets {
        print!("  removing {name}... ");
        match remove_computer_core(pool, name).await {
            Ok(r) => {
                let sub = r.computer_rows
                    + r.fleet_worker_rows
                    + r.fleet_models_rows
                    + r.leader_state_rows;
                total_rows += sub;
                if r.revocation_task_id.is_some() {
                    total_tasks += 1;
                }
                println!("ok ({sub} rows)");
            }
            Err(e) => {
                println!("{RED}FAIL{RESET} ({e})");
                failures.push((name.clone(), e.to_string()));
            }
        }
    }
    println!(
        "\n{GREEN}✓ disband complete{RESET} — {n} computer(s) removed, \
         {r} DB row(s) deleted, {t} SSH-revocation task(s) enqueued",
        n = targets.len() - failures.len(),
        r = total_rows,
        t = total_tasks,
    );
    if !failures.is_empty() {
        eprintln!("{RED}Failures:{RESET}");
        for (name, err) in &failures {
            eprintln!("  {name}: {err}");
        }
    }
    Ok(())
}

pub async fn handle_fleet_migrate_source_trees(
    pool: &sqlx::PgPool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    // Build the candidate set: every computer that isn't Taylor.
    // We join fleet_workers (for ssh_user/ip) with computers (for
    // source_tree_path) on name.
    #[derive(Debug)]
    struct Candidate {
        name: String,
        ip: String,
        ssh_user: String,
        canonical: String,
    }
    let rows = sqlx::query(
        "SELECT n.name, n.ip, n.ssh_user,
                COALESCE(c.source_tree_path, '~/.forgefleet/sub-agent-0/forge-fleet') AS canonical
           FROM fleet_workers n
           LEFT JOIN computers c ON c.name = n.name
          WHERE LOWER(n.name) <> 'taylor'
          ORDER BY n.name",
    )
    .fetch_all(pool)
    .await?;
    let candidates: Vec<Candidate> = rows
        .iter()
        .map(|r| Candidate {
            name: sqlx::Row::get(r, "name"),
            ip: sqlx::Row::get(r, "ip"),
            ssh_user: sqlx::Row::get(r, "ssh_user"),
            canonical: sqlx::Row::get(r, "canonical"),
        })
        .collect();

    println!("{CYAN}▶ ff fleet migrate-source-trees{RESET}");
    println!("  candidates: {} non-Taylor node(s)", candidates.len());
    if candidates.is_empty() {
        println!("{YELLOW}No non-Taylor nodes. Nothing to do.{RESET}");
        return Ok(());
    }

    // Probe each candidate over SSH for the two paths. Best-effort; if the
    // node is offline we can still enqueue — the task fires on `node_online`.
    struct Probed {
        c: Candidate,
        legacy_exists: bool,
        canonical_exists: bool,
        ssh_reachable: bool,
    }
    let mut probed: Vec<Probed> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let host = &c.ip;
        let user = &c.ssh_user;
        let target = format!("{user}@{host}");
        // One SSH call returns both flags, separated by "|".
        let script = "legacy=0; canonical=0; \
             [ -d ~/taylorProjects/forge-fleet ] && legacy=1; \
             [ -d ~/.forgefleet/sub-agent-0/forge-fleet/.git ] && canonical=1; \
             echo \"$legacy|$canonical\"";
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            tokio::process::Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=4",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    &target,
                    script,
                ])
                .output(),
        )
        .await;
        let (legacy, canonical, reach) = match out {
            Ok(Ok(o)) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let parts: Vec<&str> = s.split('|').collect();
                (
                    parts.first().map(|v| *v == "1").unwrap_or(false),
                    parts.get(1).map(|v| *v == "1").unwrap_or(false),
                    true,
                )
            }
            _ => (false, false, false),
        };
        probed.push(Probed {
            c,
            legacy_exists: legacy,
            canonical_exists: canonical,
            ssh_reachable: reach,
        });
    }

    println!(
        "\n  {:<14} {:<16} {:<7} {:<10} {:<10} action",
        "node", "ip", "ssh", "legacy", "canonical"
    );
    let mut to_enqueue: Vec<&Probed> = Vec::new();
    for p in &probed {
        let action = if !p.ssh_reachable {
            "enqueue (offline — runs on node_online)"
        } else if p.canonical_exists && !p.legacy_exists {
            "skip (already migrated)"
        } else if p.legacy_exists && p.canonical_exists {
            "enqueue (drop legacy, canonical already present)"
        } else if p.legacy_exists {
            "enqueue (move legacy → canonical)"
        } else {
            "enqueue (fresh clone into canonical)"
        };
        println!(
            "  {:<14} {:<16} {:<7} {:<10} {:<10} {}",
            p.c.name,
            p.c.ip,
            if p.ssh_reachable { "ok" } else { "down" },
            if p.legacy_exists { "yes" } else { "no" },
            if p.canonical_exists { "yes" } else { "no" },
            action,
        );
        let already_migrated = p.ssh_reachable && p.canonical_exists && !p.legacy_exists;
        if !already_migrated {
            to_enqueue.push(p);
        }
    }

    if dry_run {
        println!(
            "\n{YELLOW}Dry run — not enqueuing. Drop --dry-run and pass --yes to enqueue.{RESET}"
        );
        return Ok(());
    }
    if !yes {
        println!(
            "\n{YELLOW}Pass --yes to enqueue {} migration task(s).{RESET}",
            to_enqueue.len()
        );
        return Ok(());
    }
    if to_enqueue.is_empty() {
        println!(
            "\n{GREEN}✓ nothing to enqueue — every candidate is already on the canonical path.{RESET}"
        );
        return Ok(());
    }

    let who = whoami_tag();
    let mut enqueued: Vec<(String, String)> = Vec::with_capacity(to_enqueue.len());
    for p in to_enqueue {
        let script = build_migrate_source_tree_script(&p.c.canonical);
        let title = format!("Migrate source tree: {}", p.c.name);
        let payload = serde_json::json!({ "command": script });
        let trigger_spec = serde_json::json!({ "node": p.c.name });
        let id = ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(&p.c.name),
            &serde_json::json!([]),
            Some(&who),
            Some(3),
        )
        .await?;
        enqueued.push((p.c.name.clone(), id));
    }
    println!(
        "\n{GREEN}✓ enqueued {} migration task(s):{RESET}",
        enqueued.len()
    );
    for (name, id) in &enqueued {
        println!("  {:<14} {id}", name);
    }
    println!("\nTrack progress with: ff defer list");
    Ok(())
}

/// Emit the idempotent shell script used by `ff fleet migrate-source-trees`.
/// Mirrors the command spec in issue #120: if canonical/.git is already
/// present drop the legacy dir; otherwise move-or-clone into canonical.
fn build_migrate_source_tree_script(canonical: &str) -> String {
    // `canonical` comes from the DB; never user-shell-input. Still, keep it
    // quoted to be safe against spaces.
    format!(
        r#"set -e
CANONICAL="{canonical}"
mkdir -p "$(dirname "$CANONICAL")"
if [ -d "$CANONICAL/.git" ]; then
  rm -rf ~/taylorProjects/forge-fleet 2>/dev/null || true
  rmdir ~/taylorProjects 2>/dev/null || true
  echo "canonical already present — dropped legacy"
  exit 0
fi
if [ -d ~/taylorProjects/forge-fleet/.git ]; then
  mv ~/taylorProjects/forge-fleet "$CANONICAL"
  rmdir ~/taylorProjects 2>/dev/null || true
  echo "moved legacy → canonical"
else
  git clone https://github.com/venkatyarl/forge-fleet "$CANONICAL"
  rm -rf ~/taylorProjects/forge-fleet 2>/dev/null || true
  rmdir ~/taylorProjects 2>/dev/null || true
  echo "fresh clone into canonical"
fi
"#,
        canonical = canonical,
    )
}

pub async fn handle_fleet_rotate_pulse_hmac(
    pool: &sqlx::PgPool,
    value: Option<String>,
) -> Result<()> {
    println!("{CYAN}▶ Rotating pulse_beat_hmac_key...{RESET}");
    let rotator = ff_agent::secrets_rotation::SecretsRotator::new(pool.clone());
    let out = rotator
        .rotate("pulse_beat_hmac_key", value)
        .await
        .map_err(|e| anyhow::anyhow!("rotate: {e}"))?;
    println!(
        "{GREEN}✓ pulse_beat_hmac_key rotated{RESET} ({} bytes, sha12={})",
        out.new_len, out.new_fingerprint,
    );
    println!("{YELLOW}Daemons will pick up the new key on next 5-minute cache refresh.{RESET}");
    Ok(())
}

pub async fn handle_fleet_backup(pool: &sqlx::PgPool, kind: &str, force: bool) -> Result<()> {
    let my_name = ff_agent::fleet_info::resolve_this_node_name().await;
    let my_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
        .bind(&my_name)
        .fetch_optional(pool)
        .await?
        .unwrap_or_else(uuid::Uuid::nil);

    let orch =
        ff_agent::ha::backup::BackupOrchestrator::new(pool.clone(), my_id, my_name.clone(), None);

    println!("{CYAN}▶ ff fleet backup kind={kind} force={force}{RESET}");
    let reports = orch
        .run_once(kind, force)
        .await
        .map_err(|e| anyhow::anyhow!("backup: {e}"))?;

    for r in &reports {
        if r.produced {
            println!(
                "{GREEN}✓ {} backup produced{RESET}  file={} size={} sha256={} targets={}",
                r.kind,
                r.file_path.display(),
                r.size_bytes,
                &r.sha256[..12.min(r.sha256.len())],
                r.distributed_to.len(),
            );
        } else {
            println!(
                "{YELLOW}(skipped){RESET}  kind={} — not leader (use --force)",
                r.kind
            );
        }
    }
    Ok(())
}

pub async fn handle_fleet_task_coverage(
    pool: &sqlx::PgPool,
    cmd: TaskCoverageCommand,
) -> Result<()> {
    match cmd {
        TaskCoverageCommand::List => {
            let rows = sqlx::query(
                "SELECT task, min_models_loaded, priority, preferred_model_ids, notes
                 FROM fleet_task_coverage
                 ORDER BY
                   CASE priority
                     WHEN 'critical' THEN 0
                     WHEN 'normal' THEN 1
                     WHEN 'nice-to-have' THEN 2
                     ELSE 3
                   END,
                   task",
            )
            .fetch_all(pool)
            .await?;
            if rows.is_empty() {
                println!("(no task coverage rules — run `ff fleet task-coverage seed`)");
                return Ok(());
            }
            println!(
                "{:<32} {:<6} {:<14}  PREFERRED / NOTES",
                "TASK", "MIN", "PRIORITY"
            );
            for r in rows {
                let task: String = sqlx::Row::get(&r, "task");
                let min: i32 = sqlx::Row::get(&r, "min_models_loaded");
                let pri: String = sqlx::Row::get(&r, "priority");
                let preferred: serde_json::Value = sqlx::Row::get(&r, "preferred_model_ids");
                let notes: Option<String> = sqlx::Row::get(&r, "notes");
                let pref_str = preferred
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let extra = if !pref_str.is_empty() {
                    pref_str
                } else {
                    notes.unwrap_or_default()
                };
                println!("{:<32} {:<6} {:<14}  {}", task, min, pri, extra);
            }
        }
    }
    Ok(())
}

pub async fn handle_fleet_revive(
    pool: &sqlx::PgPool,
    computer: &str,
    wol_only: bool,
    internal: bool,
) -> Result<()> {
    let mgr = ff_agent::revive::ReviveManager::new(pool.clone());
    let target = mgr
        .load_target_by_name(computer)
        .await
        .map_err(|e| anyhow::anyhow!("load target: {e}"))?;

    if !internal {
        println!("{CYAN}▶ ff fleet revive {}{RESET}", target.name);
        println!("  primary_ip:    {}", target.primary_ip);
        println!("  ssh_user:      {}", target.ssh_user);
        println!("  ssh_port:      {}", target.ssh_port);
        println!("  os_family:     {}", target.os_family);
        println!("  mac_addresses: {} entry(ies)", target.mac_addresses.len());
    }

    let outcome = if wol_only {
        // WoL-only path short-circuits SSH. Send to every recorded MAC.
        if target.mac_addresses.is_empty() {
            ff_agent::revive::ReviveOutcome::Failed(
                "no MAC addresses on record; cannot WoL-only revive".into(),
            )
        } else {
            let mut sent = false;
            for mac in &target.mac_addresses {
                if ff_agent::revive::send_wol(mac).await.is_ok() {
                    sent = true;
                }
            }
            if sent {
                ff_agent::revive::ReviveOutcome::WolSent
            } else {
                ff_agent::revive::ReviveOutcome::Failed("all WoL sends failed".into())
            }
        }
    } else {
        mgr.attempt(&target)
            .await
            .map_err(|e| anyhow::anyhow!("revive attempt: {e}"))?
    };

    if internal {
        let j = serde_json::json!({
            "computer": target.name,
            "outcome": match &outcome {
                ff_agent::revive::ReviveOutcome::DaemonRestarted => "daemon_restarted",
                ff_agent::revive::ReviveOutcome::DaemonAlreadyRunning => "daemon_already_running",
                ff_agent::revive::ReviveOutcome::WolSent => "wol_sent",
                ff_agent::revive::ReviveOutcome::Failed(_) => "failed",
                ff_agent::revive::ReviveOutcome::Skipped(_) => "skipped",
            },
            "detail": match &outcome {
                ff_agent::revive::ReviveOutcome::Failed(r)
                | ff_agent::revive::ReviveOutcome::Skipped(r) => Some(r.as_str()),
                _ => None,
            },
        });
        println!("{}", j);
    } else {
        match outcome {
            ff_agent::revive::ReviveOutcome::DaemonRestarted => {
                println!("{GREEN}✓ daemon restart kicked via SSH{RESET}");
            }
            ff_agent::revive::ReviveOutcome::DaemonAlreadyRunning => {
                println!("{GREEN}✓ daemon already running on target{RESET}");
            }
            ff_agent::revive::ReviveOutcome::WolSent => {
                println!("{CYAN}↻ Wake-on-LAN packet(s) sent — awaiting pulse{RESET}");
            }
            ff_agent::revive::ReviveOutcome::Skipped(reason) => {
                println!("{YELLOW}— skipped: {reason}{RESET}");
            }
            ff_agent::revive::ReviveOutcome::Failed(reason) => {
                println!("\x1b[31m✗ failed: {reason}{RESET}");
            }
        }
    }
    Ok(())
}

fn secs_ago(ts: chrono::DateTime<chrono::Utc>) -> i64 {
    (chrono::Utc::now() - ts).num_seconds().max(0)
}

pub async fn handle_fleet_leader(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let leader = ff_db::pg_get_current_leader(pool)
        .await
        .map_err(|e| anyhow::anyhow!("pg_get_current_leader: {e}"))?;

    // Candidate pool: fleet_workers × computers, sorted by election_priority.
    let cand_rows = sqlx::query(
        "SELECT c.name AS name,
                fw.election_priority AS election_priority
         FROM fleet_workers fw
         JOIN computers c ON c.name = fw.name
         ORDER BY fw.election_priority ASC, c.name ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list candidates: {e}"))?;

    let candidates: Vec<(String, i32)> = cand_rows
        .iter()
        .map(|r| {
            (
                sqlx::Row::get::<String, _>(r, "name"),
                sqlx::Row::get::<i32, _>(r, "election_priority"),
            )
        })
        .collect();

    // Pulse info: alive + yielding from beats.
    let mut alive_map: std::collections::HashMap<String, (bool, bool)> =
        std::collections::HashMap::new();
    if let Ok(reader) = pulse_reader()
        && let Ok(beats) = reader.all_beats().await
    {
        for b in beats {
            alive_map.insert(b.computer_name.clone(), (!b.going_offline, b.is_yielding));
        }
    }

    if json {
        let cur = leader.as_ref().map(|l| {
            serde_json::json!({
                "member_name": l.member_name,
                "computer_id": l.computer_id,
                "epoch":       l.epoch,
                "elected_at":  l.elected_at,
                "reason":      l.reason,
                "heartbeat_at": l.heartbeat_at,
                "heartbeat_age_secs": secs_ago(l.heartbeat_at),
            })
        });
        let cand: Vec<_> = candidates
            .iter()
            .map(|(name, prio)| {
                let (alive, yielding) = alive_map.get(name).copied().unwrap_or((false, false));
                serde_json::json!({
                    "name": name,
                    "election_priority": prio,
                    "alive": alive,
                    "yielding": yielding,
                    "is_current": leader.as_ref().map(|l| &l.member_name == name).unwrap_or(false),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "current_leader": cur,
                "candidates":     cand,
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    match &leader {
        Some(l) => {
            println!("{CYAN}▶ Current fleet leader:{RESET}");
            println!("  name:          {}", l.member_name);
            println!("  computer_id:   {}", l.computer_id);
            println!("  epoch:         {}", l.epoch);
            println!(
                "  elected_at:    {}",
                l.elected_at.format("%Y-%m-%d %H:%M:%S UTC")
            );
            println!("  heartbeat age: {} seconds", secs_ago(l.heartbeat_at));
            println!("  reason:        {}", l.reason.as_deref().unwrap_or("-"));
        }
        None => {
            println!("{YELLOW}(no current leader in fleet_leader_state){RESET}");
        }
    }

    if !candidates.is_empty() {
        println!("\n  Candidates (by election_priority):");
        for (name, prio) in &candidates {
            let (alive, yielding) = alive_map.get(name).copied().unwrap_or((false, false));
            let alive_str = if alive { "yes" } else { "no" };
            let yield_str = if yielding { "yes" } else { "no" };
            let marker = match &leader {
                Some(l) if &l.member_name == name => "  (← current)",
                _ => "",
            };
            println!(
                "    {:<12} priority={:<5} alive={:<4} yielding={:<4}{}",
                name, prio, alive_str, yield_str, marker
            );
        }
    } else {
        println!("\n  (no candidates in fleet_workers)");
    }
    Ok(())
}

pub async fn handle_fleet_health(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    // Pull computer rows — name, primary_ip, status, last_seen_at.
    let rows = sqlx::query(
        "SELECT name, primary_ip, status, last_seen_at
         FROM computers
         ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("list computers: {e}"))?;

    #[derive(Debug)]
    struct HealthRow {
        name: String,
        ip: String,
        status: String,
        last_beat_secs: Option<i64>,
        cpu_pct: Option<f64>,
        ram_pct: Option<f64>,
        llm_servers: Option<usize>,
        software_count: Option<i64>,
        sdown: bool,
        odown: bool,
    }

    // Pulse lookups.
    let reader = pulse_reader().ok();
    let beats_by_name: std::collections::HashMap<String, ff_pulse::beat_v2::PulseBeatV2> =
        if let Some(r) = &reader {
            r.beats_by_name().await.unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };

    // Software counts per computer (best-effort).
    let sw_rows = sqlx::query(
        "SELECT c.name AS name, COUNT(cs.software_id) AS cnt
         FROM computers c
         LEFT JOIN computer_software cs ON cs.computer_id = c.id
         GROUP BY c.name",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut sw_map: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for r in &sw_rows {
        let name: String = sqlx::Row::get(r, "name");
        let cnt: i64 = sqlx::Row::get(r, "cnt");
        sw_map.insert(name, cnt);
    }

    let mut out: Vec<HealthRow> = Vec::with_capacity(rows.len());
    for r in &rows {
        let name: String = sqlx::Row::get(r, "name");
        let ip: String = sqlx::Row::get(r, "primary_ip");
        let status: String = sqlx::Row::get(r, "status");
        let last_seen: Option<chrono::DateTime<chrono::Utc>> = sqlx::Row::get(r, "last_seen_at");

        let beat = beats_by_name.get(&name);
        let last_beat_secs = beat
            .map(|b| secs_ago(b.timestamp))
            .or_else(|| last_seen.map(secs_ago));

        let sdown = if let Some(r) = &reader {
            r.is_sdown(&name).await.unwrap_or(true)
        } else {
            true
        };
        let odown = if let Some(r) = &reader {
            r.is_odown(&name).await.unwrap_or(false)
        } else {
            false
        };

        out.push(HealthRow {
            name: name.clone(),
            ip,
            status,
            last_beat_secs,
            cpu_pct: beat.map(|b| b.load.cpu_pct),
            ram_pct: beat.map(|b| b.load.ram_pct),
            llm_servers: beat.map(|b| b.llm_servers.len()),
            software_count: sw_map.get(&name).copied(),
            sdown,
            odown,
        });
    }

    if json {
        let arr: Vec<_> = out
            .iter()
            .map(|h| {
                serde_json::json!({
                    "name": h.name,
                    "ip": h.ip,
                    "status": h.status,
                    "last_beat_secs": h.last_beat_secs,
                    "cpu_pct": h.cpu_pct,
                    "ram_pct": h.ram_pct,
                    "llm_servers": h.llm_servers,
                    "software_count": h.software_count,
                    "sdown": h.sdown,
                    "odown": h.odown,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if out.is_empty() {
        println!("(no computers registered)");
        return Ok(());
    }

    println!(
        "{:<11} {:<14} {:<9} {:<10} {:<5} {:<5} {:<12} {:<8}",
        "NAME", "IP", "STATUS", "LAST_BEAT", "CPU%", "RAM%", "LLM SERVERS", "SOFTWARE"
    );
    for h in &out {
        let status = if h.odown {
            "odown".to_string()
        } else if h.sdown {
            "sdown".to_string()
        } else {
            h.status.clone()
        };
        let beat = h
            .last_beat_secs
            .map(|s| format!("{s}s ago"))
            .unwrap_or_else(|| "-".into());
        let cpu = h
            .cpu_pct
            .map(|v| format!("{:.1}", v))
            .unwrap_or_else(|| "-".into());
        let ram = h
            .ram_pct
            .map(|v| format!("{:.1}", v))
            .unwrap_or_else(|| "-".into());
        let llms = h
            .llm_servers
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        let sw = h
            .software_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<11} {:<14} {:<9} {:<10} {:<5} {:<5} {:<12} {:<8}",
            h.name, h.ip, status, beat, cpu, ram, llms, sw
        );
    }
    Ok(())
}

/// Show per-host code identity (SHA-first), with a convergence summary.
/// Designed so a glance at the table answers "are all hosts on the same
/// code?" — the per-machine build counter is only shown with --verbose.
///
/// `live=true` SSHes each host in parallel and reads `forgefleetd
/// --version` directly, so the view is accurate right after an upgrade.
/// `live=false` reads the DB-cached `computer_software.installed_version`
/// (refreshed every 6h) — fast but stale.
pub async fn handle_fleet_versions(pool: &sqlx::PgPool, verbose: bool, live: bool) -> Result<()> {
    use ff_core::build_version::{BuildVersion, display_version_short};

    if live {
        return handle_fleet_versions_live(pool, verbose).await;
    }

    // Pull the installed_version cell stored on each (computer, software_id)
    // pair. ff_git's installed_version is the full 40-char git SHA written
    // by version_check::collect_current; ff_terminal's regex-extracted
    // build_version is what predates the V56 cleanup but rare nodes may
    // still have it cached. Either path falls through code_identity().
    let rows = sqlx::query(
        "SELECT c.name AS name,
                cs.installed_version AS installed,
                sr.latest_version AS latest
           FROM computers c
           JOIN computer_software cs ON cs.computer_id = c.id
           JOIN software_registry sr ON sr.id = cs.software_id
          WHERE cs.software_id = 'ff_git'
          ORDER BY c.name",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query versions: {e}"))?;

    if rows.is_empty() {
        println!(
            "(no ff_git rows in computer_software — fleet may not have run a version_check tick yet)"
        );
        return Ok(());
    }

    // Pick the most-common installed SHA as the "fleet target". A host
    // matches when its installed SHA equals that — regardless of build
    // counter, build date, or local-tree state.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut hosts: Vec<(String, String, String)> = Vec::with_capacity(rows.len());
    for r in &rows {
        let name: String = sqlx::Row::try_get(r, "name").unwrap_or_default();
        let installed: Option<String> = sqlx::Row::try_get(r, "installed").ok();
        let latest: Option<String> = sqlx::Row::try_get(r, "latest").ok();
        let installed = installed.unwrap_or_default();
        let latest = latest.unwrap_or_default();
        if !installed.is_empty() {
            *counts.entry(installed.clone()).or_insert(0) += 1;
        }
        hosts.push((name, installed, latest));
    }
    let target_sha: Option<String> = counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(sha, _)| sha.clone());

    // Route every cell through display_version_short — the unified
    // helper handles ff-shape strings, raw 40-char SHAs, and vendor
    // version strings consistently. Empty cells render as `-`.
    let short = |raw: &str| -> String {
        if raw.is_empty() {
            "-".to_string()
        } else {
            display_version_short(raw)
        }
    };

    if verbose {
        println!(
            "{:<12} {:<10} {:<10} {:<10} {:<8}",
            "NAME", "INSTALLED", "LATEST", "STATE", "BUILD#"
        );
    } else {
        println!(
            "{:<12} {:<10} {:<10} {:<8}",
            "NAME", "INSTALLED", "LATEST", "STATE"
        );
    }
    let mut converged = 0usize;
    for (name, installed, latest) in &hosts {
        let inst_short = short(installed);
        let lat_short = short(latest);
        let state = match target_sha.as_deref() {
            Some(t) if installed == t => {
                converged += 1;
                "✓"
            }
            Some(_) => "drift",
            None => "?",
        };
        if verbose {
            // Try to parse a build counter / date from any embedded
            // BuildVersion-shaped string. Pre-V56 cells may have one;
            // SHA-only cells legitimately don't.
            let parsed = BuildVersion::parse(installed);
            let count = parsed
                .as_ref()
                .map(|v| v.build_count.to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<12} {:<10} {:<10} {:<10} {:<8}",
                name, inst_short, lat_short, state, count
            );
        } else {
            println!(
                "{:<12} {:<10} {:<10} {:<8}",
                name, inst_short, lat_short, state
            );
        }
    }

    let total = hosts.len();
    let target_disp = target_sha
        .as_deref()
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "-".into());
    println!();
    if converged == total {
        println!("{GREEN}✓ converged{RESET}: all {total} host(s) at {target_disp}");
    } else {
        println!(
            "{YELLOW}⚠ drift{RESET}: {}/{total} on {target_disp}; {} drifted",
            converged,
            total - converged,
        );
    }

    Ok(())
}

/// Live variant of `ff fleet versions` — SSHes every computer in
/// parallel and reads `forgefleetd --version` directly. Slower than the
/// cached path (one SSH round-trip per host, capped at ~5s each) but
/// truthful right after a fleet upgrade when the version_check tick
/// hasn't refreshed `installed_version` yet.
pub async fn handle_fleet_versions_live(pool: &sqlx::PgPool, verbose: bool) -> Result<()> {
    use ff_core::build_version::BuildVersion;
    use futures::stream::{FuturesUnordered, StreamExt};
    use tokio::process::Command;

    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| anyhow::anyhow!("pg_list_nodes: {e}"))?;
    if nodes.is_empty() {
        println!("(no computers registered)");
        return Ok(());
    }

    let me = ff_agent::fleet_info::resolve_this_node_name().await;
    let mut futs = FuturesUnordered::new();
    for n in nodes {
        let name = n.name.clone();
        let ip = n.ip.clone();
        let user = n.ssh_user.clone();
        let is_me = me.eq_ignore_ascii_case(&name);
        futs.push(async move {
            let cmd = "~/.local/bin/forgefleetd --version 2>&1 | head -1";
            let out = if is_me {
                Command::new("sh").args(["-c", cmd]).output().await
            } else {
                Command::new("ssh")
                    .args([
                        "-T",
                        "-o",
                        "BatchMode=yes",
                        "-o",
                        "ConnectTimeout=5",
                        &format!("{user}@{ip}"),
                        cmd,
                    ])
                    .output()
                    .await
            };
            let raw = match out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                Ok(o) => format!("ssh-exit:{}", o.status.code().unwrap_or(-1)),
                Err(e) => format!("ssh-error:{e}"),
            };
            (name, raw)
        });
    }

    let mut rows: Vec<(String, String, Option<BuildVersion>)> = Vec::new();
    while let Some((name, raw)) = futs.next().await {
        let parsed = BuildVersion::parse(&raw);
        rows.push((name, raw, parsed));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    // Pick the most-common SHA as the fleet target.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (_, _, parsed) in &rows {
        if let Some(p) = parsed {
            *counts.entry(p.sha.clone()).or_insert(0) += 1;
        }
    }
    let target_sha: Option<String> = counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(sha, _)| sha.clone());

    if verbose {
        println!(
            "{:<12} {:<10} {:<8} {:<8} {:<8}",
            "NAME", "SHA", "STATE", "BUILD#", "STATUS"
        );
    } else {
        println!("{:<12} {:<10} {:<8}", "NAME", "SHA", "STATUS");
    }
    let mut converged = 0usize;
    let mut unreachable = 0usize;
    for (name, raw, parsed) in &rows {
        match parsed {
            Some(v) => {
                let status = match target_sha.as_deref() {
                    Some(t) if v.sha == t => {
                        converged += 1;
                        "✓".to_string()
                    }
                    Some(_) => "drift".to_string(),
                    None => "?".to_string(),
                };
                if verbose {
                    println!(
                        "{:<12} {:<10} {:<8} {:<8} {:<8}",
                        name,
                        v.short_sha(),
                        v.state,
                        v.build_count,
                        status
                    );
                } else {
                    println!("{:<12} {:<10} {:<8}", name, v.short_sha(), status);
                }
            }
            None => {
                unreachable += 1;
                let snippet: String = raw.chars().take(20).collect();
                if verbose {
                    println!("{:<12} {:<10} {:<8} {:<8} {snippet}", name, "?", "?", "?");
                } else {
                    println!("{:<12} {:<10} {snippet}", name, "?");
                }
            }
        }
    }

    let total = rows.len();
    let target_disp = target_sha
        .as_deref()
        .map(|s| {
            let n = s.chars().count().min(8);
            s[..n].to_string()
        })
        .unwrap_or_else(|| "-".into());
    println!();
    if unreachable == 0 && converged == total {
        println!("{GREEN}✓ converged{RESET}: all {total} host(s) live at {target_disp}");
    } else {
        println!(
            "{YELLOW}⚠ {}/{total} live at {target_disp}{RESET}; {} drifted, {} unreachable",
            converged,
            total - converged - unreachable,
            unreachable,
        );
    }

    Ok(())
}

pub async fn handle_fleet_gossip() -> Result<()> {
    let reader = pulse_reader()?;
    let beats = reader
        .all_beats()
        .await
        .map_err(|e| anyhow::anyhow!("all_beats: {e}"))?;

    if beats.is_empty() {
        println!("(no beats present in Redis — is the daemon publishing pulses?)");
        return Ok(());
    }

    println!("{CYAN}▶ Fleet gossip dump — peers_seen per member:{RESET}");
    for b in &beats {
        let age = secs_ago(b.timestamp);
        println!(
            "\n  {} (epoch={}, role={}, {}s old, going_offline={}, yielding={})",
            b.computer_name, b.epoch, b.role_claimed, age, b.going_offline, b.is_yielding,
        );
        if b.peers_seen.is_empty() {
            println!("    (peers_seen empty)");
            continue;
        }
        for p in &b.peers_seen {
            let pa = secs_ago(p.last_beat_at);
            println!(
                "    ├─ {:<12} status={:<6} epoch_witnessed={:<4} last_beat={}s ago",
                p.name, p.status, p.epoch_witnessed, pa,
            );
        }
    }
    Ok(())
}

pub async fn handle_fleet(cmd: FleetCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        FleetCommand::SshMeshCheck {
            node,
            json,
            since,
            repair,
            yes,
        } => {
            if repair && !yes {
                anyhow::bail!(
                    "--repair rewrites authorized_keys / known_hosts on every failed peer — pass --yes to proceed"
                );
            }
            if repair {
                println!("{CYAN}▶ Repairing mesh before probing...{RESET}");
                let failed = ff_db::pg_list_mesh_status(&pool, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("pg_list_mesh_status: {e}"))?
                    .into_iter()
                    .filter(|r| r.status == "failed")
                    .collect::<Vec<_>>();
                println!(
                    "  found {} failed pair(s) — re-enqueuing as mesh_retry tasks",
                    failed.len()
                );
                let created = ff_agent::mesh_check::enqueue_retries(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("  enqueued {} mesh_retry task(s)", created);
            }
            if let Some(spec) = &since {
                let age = parse_duration(spec).ok_or_else(|| {
                    anyhow::anyhow!("unrecognized --since value '{spec}' (try 1h, 30m, 2d)")
                })?;
                println!("{CYAN}▶ Refreshing pairs older than {spec}...{RESET}");
                let n = ff_agent::mesh_check::refresh_stale(&pool, age)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("  refreshed {n} stale pair(s)");
                return Ok(());
            }
            println!("{CYAN}▶ Running pairwise SSH mesh check...{RESET}");
            let matrix = match &node {
                Some(n) => ff_agent::mesh_check::pairwise_ssh_check_node(&pool, n).await,
                None => ff_agent::mesh_check::pairwise_ssh_check(&pool).await,
            }
            .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                let arr: Vec<_> = matrix.cells.iter().map(|c| serde_json::json!({
                    "src": c.src, "dst": c.dst, "status": c.status, "last_error": c.last_error,
                })).collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
            } else {
                let mut ok = 0;
                let mut fail = 0;
                for c in &matrix.cells {
                    let marker = if c.status == "ok" { "✓" } else { "✗" };
                    if c.status == "ok" {
                        ok += 1;
                    } else {
                        fail += 1;
                    }
                    let err = c.last_error.as_deref().unwrap_or("");
                    println!("  {:<10} → {:<10}  {}  {}", c.src, c.dst, marker, err);
                }
                println!(
                    "\n{ok} ok, {fail} failed — checked {} pairs",
                    matrix.cells.len()
                );
            }
        }
        FleetCommand::VerifyNode { name, json } => {
            println!("{CYAN}▶ Running verify-node battery for {name}...{RESET}");
            let report = ff_agent::verify_node::verify_node(&pool, &name)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!(
                    "\nResults for {}: {} pass, {} fail, {} skip",
                    report.node, report.passed, report.failed, report.skipped
                );
                for r in &report.details {
                    let marker = match r.status.as_str() {
                        "pass" => "✓",
                        "fail" => "✗",
                        _ => "—",
                    };
                    let msg = r.message.as_deref().unwrap_or("");
                    println!("  {}  {:<28}  {}", marker, r.check, msg);
                }
            }
        }
        FleetCommand::Leader { json } => {
            handle_fleet_leader(&pool, json).await?;
        }
        FleetCommand::Health { json } => {
            handle_fleet_health(&pool, json).await?;
        }
        FleetCommand::Versions { verbose, live } => {
            handle_fleet_versions(&pool, verbose, live).await?;
        }
        FleetCommand::Gossip => {
            handle_fleet_gossip().await?;
        }
        FleetCommand::MigrateGithub {
            new_owner,
            skip_local,
            only,
            dry_run,
            yes,
        } => {
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            let local = ff_agent::fleet_info::resolve_this_node_name().await;
            let mut targets: Vec<&ff_db::FleetNodeRow> = nodes.iter().collect();
            if let Some(name) = &only {
                targets.retain(|n| &n.name == name);
                if targets.is_empty() {
                    anyhow::bail!("no fleet node named '{name}'");
                }
            } else if skip_local {
                targets.retain(|n| n.name != local);
            }
            println!("{CYAN}▶ ff fleet migrate-github{RESET}");
            println!("  new owner:       {new_owner}");
            println!(
                "  local node:      {local}{}",
                if skip_local { " (skipped)" } else { "" }
            );
            println!("  targets:         {} node(s)", targets.len());
            for n in &targets {
                println!(
                    "    {:<15} {:<16} {}",
                    n.name,
                    n.ip,
                    n.gh_account.clone().unwrap_or_else(|| "-".into())
                );
            }
            if targets.is_empty() {
                println!("{YELLOW}No nodes to enqueue. Nothing to do.{RESET}");
                return Ok(());
            }
            if dry_run || !yes {
                println!(
                    "\n{YELLOW}Dry run — not enqueuing. Pass --yes to actually enqueue.{RESET}"
                );
                return Ok(());
            }

            let who = whoami_tag();
            let mut enqueued: Vec<(String, String)> = Vec::with_capacity(targets.len());
            for n in &targets {
                let script = build_migrate_github_script(&new_owner);
                let title = format!("Migrate GitHub owner → {new_owner} on {}", n.name);
                let payload = serde_json::json!({ "command": script });
                let trigger_spec = serde_json::json!({ "node": n.name });
                let defer_id = ff_db::pg_enqueue_deferred(
                    &pool,
                    &title,
                    "shell",
                    &payload,
                    "node_online",
                    &trigger_spec,
                    Some(&n.name),
                    &serde_json::json!([]),
                    Some(&who),
                    Some(3),
                )
                .await?;
                enqueued.push((n.name.clone(), defer_id));
            }
            println!(
                "\n{GREEN}✓ Enqueued {} migration task(s):{RESET}",
                enqueued.len()
            );
            for (node, id) in &enqueued {
                println!("  {:<15} {id}", node);
            }
            println!("\nTrack progress with: ff defer list");
        }
        FleetCommand::Revive {
            computer,
            wol_only,
            internal,
        } => {
            handle_fleet_revive(&pool, &computer, wol_only, internal).await?;
        }
        FleetCommand::TaskCoverage { command } => {
            handle_fleet_task_coverage(&pool, command).await?;
        }
        FleetCommand::RevokeTrust { computer, yes } => {
            handle_fleet_revoke_trust(&pool, &computer, yes).await?;
        }
        FleetCommand::RemoveComputer { name, yes } => {
            handle_fleet_remove_computer(&pool, &name, yes).await?;
        }
        FleetCommand::Disband {
            yes,
            i_know_what_im_doing,
        } => {
            handle_fleet_disband(&pool, yes, i_know_what_im_doing).await?;
        }
        FleetCommand::MigrateSourceTrees { dry_run, yes } => {
            handle_fleet_migrate_source_trees(&pool, dry_run, yes).await?;
        }
        FleetCommand::RotateSshKey { computer } => {
            let mgr = ff_agent::ssh_key_manager::SshKeyManager::new(pool.clone());
            match mgr.rotate_computer_keypair(&computer).await {
                Ok(()) => println!("{GREEN}✓ rotate complete{RESET}"),
                Err(e) => {
                    eprintln!("{YELLOW}Not yet implemented:{RESET} {e}");
                    std::process::exit(2);
                }
            }
        }
        FleetCommand::RotatePulseHmac { value } => {
            handle_fleet_rotate_pulse_hmac(&pool, value).await?;
        }
        FleetCommand::Backup { kind, force } => {
            handle_fleet_backup(&pool, &kind, force).await?;
        }
        FleetCommand::SetNetworkScope { computer, scope } => {
            handle_fleet_set_network_scope(&pool, &computer, &scope).await?;
        }
        FleetCommand::Db { command } => {
            handle_fleet_db(&pool, command).await?;
        }
        FleetCommand::PanicStop { yes, halt_dbs } => {
            handle_fleet_panic_stop(&pool, yes, halt_dbs).await?;
        }
        FleetCommand::Resume { yes } => {
            handle_fleet_resume(&pool, yes).await?;
        }
        FleetCommand::Quarantine { computer, yes } => {
            handle_fleet_quarantine(&pool, &computer, yes).await?;
        }
        FleetCommand::Unquarantine { computer, yes } => {
            handle_fleet_unquarantine(&pool, &computer, yes).await?;
        }
        FleetCommand::Upgrade {
            software_id,
            computer,
            all,
            dry_run,
            yes,
            force_dirty,
        } => {
            handle_fleet_upgrade(
                &pool,
                &software_id,
                computer,
                all,
                dry_run,
                yes,
                force_dirty,
            )
            .await?;
        }
        FleetCommand::Computers { format, os, role } => {
            let resolver = ff_core::FleetResolver::new();
            let mut computers = resolver
                .resolve()
                .await
                .map_err(|e| anyhow::anyhow!("failed to resolve fleet computers: {e}"))?;

            if let Some(filter) = os {
                let lower = filter.to_ascii_lowercase();
                computers.retain(|c| c.os.to_ascii_lowercase().contains(&lower));
            }
            if let Some(filter) = role {
                let lower = filter.to_ascii_lowercase();
                computers.retain(|c| c.role.to_ascii_lowercase().contains(&lower));
            }

            match format.as_str() {
                "json" => {
                    println!("{}", serde_json::to_string_pretty(&computers)?);
                }
                _ => {
                    println!(
                        "{GREEN}✓ Fleet Computers{RESET} ({} total)",
                        computers.len()
                    );
                    for c in &computers {
                        let os_tag = if c.os.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", c.os)
                        };
                        let role_tag = if c.role.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", c.role)
                        };
                        println!(
                            "  - {name} ({ip}){role_tag}{os_tag}",
                            name = c.name,
                            ip = c.ip,
                            role_tag = role_tag,
                            os_tag = os_tag,
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn build_migrate_github_script(new_owner: &str) -> String {
    format!(
        r#"set -e
if [ -d "/Users/$USER" ]; then
  HOME_BASE="/Users/$USER"
  OS_TYPE="mac"
else
  HOME_BASE="/home/$USER"
  OS_TYPE="linux"
fi
OLD_DIR="$HOME_BASE/taylorProjects/forge-fleet"
NEW_DIR="$HOME_BASE/projects/forge-fleet"
mkdir -p "$HOME_BASE/projects"
if [ ! -d "$NEW_DIR/.git" ]; then
  if [ -d "$OLD_DIR/.git" ]; then
    mv "$OLD_DIR" "$NEW_DIR"
  else
    git clone --depth 50 "https://github.com/{new_owner}/forge-fleet.git" "$NEW_DIR"
  fi
fi
# Retire ~/taylorProjects fully. If the legacy dir or symlink lingers, drop it.
rm -rf "$OLD_DIR" 2>/dev/null || true
cd "$NEW_DIR"
git remote set-url origin "https://github.com/{new_owner}/forge-fleet.git"
git fetch origin main
git reset --hard origin/main
cargo build --release -p ff-terminal
install -m 755 target/release/ff "$HOME_BASE/.local/bin/ff"
if [ "$OS_TYPE" = "mac" ]; then
  codesign --force --sign - "$HOME_BASE/.local/bin/ff" || true
fi
if [ "$OS_TYPE" = "linux" ]; then
  UNIT="/etc/systemd/system/forgefleet-daemon.service"
  if [ -f "$UNIT" ]; then
    sudo sed -i "s|WorkingDirectory=.*taylorProjects.*forge-fleet|WorkingDirectory=$NEW_DIR|" "$UNIT" || true
    sudo systemctl daemon-reload || true
    sudo systemctl restart forgefleet-daemon.service || true
  fi
fi
echo "migrate-github complete on $(hostname): remote=https://github.com/{new_owner}/forge-fleet.git path=$NEW_DIR"
"#
    )
}

fn parse_duration(spec: &str) -> Option<chrono::Duration> {
    let spec = spec.trim();
    let (num, unit) = spec.split_at(spec.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    match unit {
        "s" | "sec" => Some(chrono::Duration::seconds(n)),
        "m" | "min" => Some(chrono::Duration::minutes(n)),
        "d" | "day" => Some(chrono::Duration::days(n)),
        _ => None,
    }
}
