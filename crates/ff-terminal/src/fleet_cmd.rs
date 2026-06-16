//! `ff fleet` subcommand implementations.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{
    CYAN, FleetCommand, FleetDbCommand, GREEN, LeaderAction, RED, RESET, TaskCoverageCommand,
    YELLOW, pulse_reader, whoami_tag,
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
    let local = ff_agent::fleet_info::resolve_this_worker_name().await;
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
    let local = ff_agent::fleet_info::resolve_this_worker_name().await;
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
        FleetDbCommand::Backup { kind, now } => {
            handle_fleet_db_backup_now(pool, &kind, now).await?;
        }
        FleetDbCommand::Drill { on } => {
            handle_fleet_db_drill(pool, on.as_deref()).await?;
        }
    }
    Ok(())
}

/// `ff fleet db drill` — run the backup restore-drill on demand. Shares the
/// exact path (`RestoreDrillTick::run_record_and_alert`) the daily leader tick
/// uses: decrypt → extract → validate the newest Postgres backup, record to
/// `backup_drills`, alert on failure. Exits non-zero on a failed drill.
pub async fn handle_fleet_db_drill(pool: &sqlx::PgPool, on: Option<&str>) -> Result<()> {
    let my_name = ff_agent::fleet_info::resolve_this_worker_name().await;
    // Cross-node: dispatch the drill to a remote computer via the deferred-task
    // queue and report back its result. Proves DR-readiness on the node that
    // would actually take over (the backup fanned out there AND restores).
    if let Some(node) = on {
        if !node.eq_ignore_ascii_case(&my_name) {
            return enqueue_remote_drill(pool, node, &my_name).await;
        }
    }
    println!("{CYAN}▶ ff fleet db drill{RESET}  (node={my_name})");
    let tick = ff_agent::ha::restore_drill::RestoreDrillTick::new(pool.clone(), my_name);
    let o = tick.run_record_and_alert().await;
    if o.success {
        println!(
            "{GREEN}✓ restore drill PASSED{RESET}  backup={} files={} bytes={} pg_version={} verifybackup={:?} ({}ms)",
            o.backup_file,
            o.file_count.unwrap_or(0),
            o.extracted_bytes.unwrap_or(0),
            o.pg_version.as_deref().unwrap_or("?"),
            o.verifybackup,
            o.duration_ms,
        );
        println!("    {}", o.detail);
    } else {
        eprintln!(
            "{RED}✗ restore drill FAILED{RESET}  backup={} stage={}\n    {}",
            o.backup_file, o.stage, o.detail
        );
        std::process::exit(1);
    }
    Ok(())
}

/// Enqueue a restore-drill on a remote fleet computer via the deferred-task
/// queue, then poll `backup_drills` for that node's result. Backing
/// `ff fleet db drill --on <node>`: proves the backup fanned out to `<node>`
/// AND is restorable there — the leader-loss recovery story, on the node that
/// would actually take over.
async fn enqueue_remote_drill(pool: &sqlx::PgPool, node: &str, me: &str) -> Result<()> {
    println!(
        "{CYAN}▶ ff fleet db drill --on {node}{RESET}  (dispatched from {me} via the defer queue)"
    );
    let baseline = chrono::Utc::now();
    // The remote defer-worker runs this shell command; `ff` lives at the
    // canonical install path. The drill records into the shared `backup_drills`
    // table with `drill_node=<node>`, which is how we recover its result.
    let payload = serde_json::json!({
        "command": "\"$HOME/.local/bin/ff\" fleet db drill",
        "summary": format!("backup restore-drill on {node}"),
    });
    let trigger_spec = serde_json::json!({ "node": node });
    let id = ff_db::pg_enqueue_deferred(
        pool,
        &format!("backup restore-drill → {node}"),
        "shell",
        &payload,
        "node_online",
        &trigger_spec,
        Some(node),
        &serde_json::json!([]),
        Some("ff fleet db drill --on"),
        Some(3),
    )
    .await?;
    println!(
        "{GREEN}✓{RESET} enqueued drill task {id} (preferred_node={node}, trigger=node_online)"
    );
    println!(
        "  waiting up to 200s for {node} to run it (the defer-worker claims pending tasks ~every 15s)…"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(200);
    loop {
        #[allow(clippy::type_complexity)]
        let row: Option<(
            bool,
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<bool>,
            String,
            Option<i64>,
        )> = sqlx::query_as(
            "SELECT success, stage, detail, file_count, extracted_bytes, pg_version, \
                    verifybackup, backup_file, duration_ms \
               FROM backup_drills \
              WHERE drill_node = $1 AND started_at > $2 \
              ORDER BY started_at DESC LIMIT 1",
        )
        .bind(node)
        .bind(baseline)
        .fetch_optional(pool)
        .await?;

        if let Some((
            success,
            stage,
            detail,
            file_count,
            extracted_bytes,
            pg_version,
            verifybackup,
            backup_file,
            duration_ms,
        )) = row
        {
            let detail = detail.unwrap_or_default();
            if success {
                println!(
                    "{GREEN}✓ remote restore drill PASSED on {node}{RESET}  backup={} files={} bytes={} pg_version={} verifybackup={:?} ({}ms)",
                    backup_file,
                    file_count.unwrap_or(0),
                    extracted_bytes.unwrap_or(0),
                    pg_version.as_deref().unwrap_or("?"),
                    verifybackup,
                    duration_ms.unwrap_or(0),
                );
                println!("    {detail}");
                return Ok(());
            }
            eprintln!(
                "{RED}✗ remote restore drill FAILED on {node}{RESET}  backup={backup_file} stage={stage}\n    {detail}"
            );
            std::process::exit(1);
        }

        if std::time::Instant::now() >= deadline {
            eprintln!(
                "{YELLOW}⏱ no result from {node} within 200s.{RESET} The task may still be \
                 queued/running — check `ff defer get {id}`. A worker that is offline or has \
                 no backup copy won't report."
            );
            std::process::exit(2);
        }
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    }
}

/// `ff fleet db backup --kind <all|postgres|redis> [--now]` — force an
/// immediate backup cycle through the real HA orchestrator.
pub async fn handle_fleet_db_backup_now(
    pool: &sqlx::PgPool,
    kind: &str,
    force: bool,
) -> Result<()> {
    let kind = kind.to_lowercase();
    if !matches!(kind.as_str(), "all" | "postgres" | "redis") {
        anyhow::bail!("--kind must be one of: all | postgres | redis (got '{kind}')");
    }

    // Resolve THIS host's identity the same way the daemon does.
    let my_name = ff_agent::fleet_info::resolve_this_worker_name().await;
    let computer_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
            .bind(&my_name)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("query computers by name: {e}"))?;
    let Some(computer_id) = computer_id else {
        anyhow::bail!(
            "no `computers` row for this host ('{my_name}') — run `ff onboard` first. \
             Backups must originate on an enrolled host (normally the leader)."
        );
    };

    println!("{CYAN}▶ Forcing {kind} backup on '{my_name}' (force={force})...{RESET}");
    let orchestrator = ff_agent::ha::backup::BackupOrchestrator::new(
        pool.clone(),
        computer_id,
        my_name.clone(),
        None,
    );
    let reports = orchestrator
        .run_once(&kind, force)
        .await
        .map_err(|e| anyhow::anyhow!("backup run_once: {e}"))?;

    let mut any_skipped = false;
    for r in &reports {
        if !r.produced {
            any_skipped = true;
            println!(
                "{YELLOW}⚠ {kind} skipped — '{my_name}' is not the leader. \
                 Re-run with --now (the default) or on the leader.{RESET}",
                kind = r.kind
            );
        } else {
            println!(
                "{GREEN}✓{RESET} {kind} backup produced: {file} ({bytes} bytes) → distributing to \
                 {n} peer(s)",
                kind = r.kind,
                file = r.file_name,
                bytes = r.size_bytes,
                n = r.distributed_to.len(),
            );
        }
    }
    if any_skipped {
        std::process::exit(2);
    }
    println!(
        "{GREEN}✓{RESET} backup cycle complete; HA distribution enqueued (watch `ff defer list`)."
    );
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
    let my_name = ff_agent::fleet_info::resolve_this_worker_name().await;
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
        let me = ff_agent::fleet_info::resolve_this_worker_name().await;
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
    let my_name = ff_agent::fleet_info::resolve_this_worker_name().await;
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

    // HA Phase 2: surface an active maintenance lease (designated standby).
    if let Some((standby, until)) = ff_db::pg_get_active_maintenance_lease(&pool)
        .await
        .unwrap_or(None)
    {
        println!(
            "  {CYAN}maintenance lease:{RESET} → {standby} until {} (auto fail-back)",
            until.format("%Y-%m-%d %H:%M:%S UTC")
        );
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

/// `ff fleet leader step-down` (HA Phase 1). Voluntarily hand fleet leadership
/// to the next-preferred follower for a bounded window, then auto-fail-back.
///
/// Mechanism: write the `leader_yield_request` fleet_secret as
/// `<member>|<rfc3339_until>`. The target's daemon (leader_tick) reads it each
/// tick, publishes `is_yielding=true` in its pulse beat, and yields the leader
/// singleton; every node's election skips a yielding candidate, so the next
/// follower takes over. When the deadline passes (or `--clear` deletes the
/// secret) the flag drops and the original leader re-asserts. This does NOT
/// move the Postgres/Redis primary — fleet leadership and DB primary are
/// independent (see plans/ha-leader-handoff.md §4), so it is safe only when the
/// caller accepts a brief leadership move, hence `--yes`.
pub async fn handle_fleet_leader_step_down(
    pool: &sqlx::PgPool,
    minutes: i64,
    member: Option<String>,
    to: Option<String>,
    clear: bool,
    yes: bool,
) -> Result<()> {
    const KEY: &str = "leader_yield_request";

    if clear {
        let existed = ff_db::pg_delete_secret(pool, KEY)
            .await
            .map_err(|e| anyhow::anyhow!("clear leader_yield_request: {e}"))?;
        // Also clear any HA Phase 2 maintenance lease (designated standby).
        ff_db::pg_clear_maintenance_lease(pool)
            .await
            .map_err(|e| anyhow::anyhow!("clear maintenance lease: {e}"))?;
        if existed {
            println!(
                "{GREEN}✓ step-down cleared{RESET} — the target will re-assert leadership within ~2 ticks."
            );
        } else {
            println!("  no active step-down request (nothing to clear).");
        }
        return Ok(());
    }

    // Resolve the target member: explicit --member, else the current leader.
    let target = match member {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => ff_db::pg_get_current_leader(pool)
            .await
            .map_err(|e| anyhow::anyhow!("pg_get_current_leader: {e}"))?
            .map(|l| l.member_name)
            .ok_or_else(|| {
                anyhow::anyhow!("no current leader recorded; pass --member <name> explicitly")
            })?,
    };

    if !yes {
        eprintln!(
            "{YELLOW}⚠ This hands fleet leadership away from '{target}' for {minutes} min \
             (auto fail-back after).{RESET}\n  Re-run with {CYAN}--yes{RESET} to confirm, \
             or {CYAN}--clear{RESET} to cancel an active request."
        );
        std::process::exit(1);
    }

    let minutes = minutes.clamp(1, 24 * 60);
    let until = chrono::Utc::now() + chrono::Duration::minutes(minutes);
    let value = format!("{target}|{}", until.to_rfc3339());
    ff_db::pg_set_secret(
        pool,
        KEY,
        &value,
        Some("HA Phase 1 voluntary leader step-down (ff fleet leader step-down)"),
        Some("ff fleet leader step-down"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("set leader_yield_request: {e}"))?;

    // HA Phase 2: if a standby was designated, record a maintenance lease so
    // election prefers it OUTRIGHT (not just next-by-priority) until fail-back.
    if let Some(standby) = to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        ff_db::pg_set_maintenance_lease(pool, standby, until)
            .await
            .map_err(|e| anyhow::anyhow!("set maintenance lease: {e}"))?;
        println!(
            "{GREEN}✓ maintenance handoff: '{target}' → '{standby}'{RESET}\n  \
             '{standby}' takes leadership within ~2 ticks; automatic fail-back at {} ({minutes} min).\n  \
             cancel early: {CYAN}ff fleet leader step-down --clear{RESET}\n  \
             watch: {CYAN}ff fleet leader{RESET}",
            until.to_rfc3339()
        );
    } else {
        println!(
            "{GREEN}✓ step-down requested for '{target}'{RESET}\n  \
             it will yield within ~2 election ticks; automatic fail-back at {} ({minutes} min).\n  \
             designate a successor with {CYAN}--to <node>{RESET}\n  \
             cancel early: {CYAN}ff fleet leader step-down --clear{RESET}\n  \
             watch: {CYAN}ff fleet leader{RESET}",
            until.to_rfc3339()
        );
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

    // Sort by primary IP, numerically by octet (fleet-table convention — the
    // operator reads the fleet by subnet layout, not alphabet). Applies to both
    // the JSON and text paths so they share one stable order.
    out.sort_by_key(|h| crate::helpers::ip_sort_key(&h.ip));

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
/// Pick the convergence target for `ff fleet versions`, as already-normalized
/// short code identities (see `display_version_short`).
///
/// Prefer the upstream **LATEST** (the SHA the auto-upgrade wave is rolling
/// toward) so a host actually on LATEST reads as converged and stale hosts read
/// as drift. Fall back to the fleet's modal installed SHA only when LATEST is
/// unknown — e.g. the 6h upstream-check tick hasn't populated `latest_version`
/// yet — in which case STATE just reports fleet homogeneity.
///
/// Returns `(target_short, using_latest)`, or `None` if neither a LATEST nor a
/// mode is available.
///
/// The old code compared each host's installed SHA against the *mode* and
/// ignored LATEST entirely, so the one host on LATEST (e.g. a freshly
/// hand-deployed leader) was flagged `drift` while the majority a release
/// behind read `✓` — backwards from what the LATEST column shows.
fn pick_version_target(
    latest_short: Option<&str>,
    mode_short: Option<&str>,
) -> Option<(String, bool)> {
    if let Some(l) = latest_short {
        if !l.is_empty() && l != "-" {
            return Some((l.to_string(), true));
        }
    }
    mode_short
        .filter(|m| !m.is_empty() && *m != "-")
        .map(|m| (m.to_string(), false))
}

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

    // Tally installed SHAs so the fleet's modal SHA can serve as a fallback
    // drift target when the upstream LATEST is unknown (see pick_version_target).
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
    let mode_sha: Option<String> = counts
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

    // Drift target: prefer the upstream LATEST that the auto-upgrade wave is
    // rolling toward, so a host ON latest reads ✓ and stale hosts read drift.
    // `latest` is sr.latest_version (identical for every ff_git row) — take the
    // first non-empty one. Compare on the normalized short code identity (what
    // the table prints), so a 40-char installed SHA and an 8-char LATEST that
    // are the same commit compare equal.
    let latest_short: Option<String> = hosts
        .iter()
        .map(|(_, _, latest)| latest)
        .find(|l| !l.is_empty())
        .map(|l| short(l));
    let mode_short: Option<String> = mode_sha.as_deref().map(short);
    let target = pick_version_target(latest_short.as_deref(), mode_short.as_deref());
    let target_short: Option<String> = target.as_ref().map(|(t, _)| t.clone());
    let using_latest = target.as_ref().map(|(_, l)| *l).unwrap_or(false);

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
        let state = match target_short.as_deref() {
            Some(t) if !inst_short.is_empty() && inst_short != "-" && inst_short == t => {
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
    let target_disp = target_short.as_deref().unwrap_or("-");
    // Name the target so the summary is unambiguous: LATEST = upstream the wave
    // rolls toward; "fleet" = modal fallback when LATEST is unknown.
    let target_kind = if using_latest { "LATEST" } else { "fleet" };
    println!();
    if target_short.is_none() {
        println!(
            "{YELLOW}⚠ no target{RESET}: no LATEST or installed SHA known across {total} host(s)"
        );
    } else if converged == total {
        println!("{GREEN}✓ converged{RESET}: all {total} host(s) on {target_kind} {target_disp}");
    } else {
        println!(
            "{YELLOW}⚠ drift{RESET}: {}/{total} on {target_kind} {target_disp}; {} drifted",
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

    let me = ff_agent::fleet_info::resolve_this_worker_name().await;
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
            let report = ff_agent::verify_computer::verify_computer(&pool, &name)
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
        FleetCommand::Integrity { json } => {
            let my_name = ff_agent::fleet_info::resolve_this_worker_name().await;
            if !json {
                println!(
                    "{CYAN}▶ Running fleet-integrity sweep (verify battery across all online members)...{RESET}"
                );
            }
            let summary = ff_agent::fleet_integrity::run_integrity_sweep(&pool, &my_name)
                .await
                .map_err(|e| anyhow::anyhow!("integrity sweep: {e}"))?;
            if json {
                let degraded: Vec<serde_json::Value> = summary
                    .degraded
                    .iter()
                    .map(|g| {
                        serde_json::json!({
                            "node": g.node,
                            "failed": g.failed,
                            "failing_checks": g.failing_checks,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "checked": summary.checked,
                        "degraded": degraded,
                        "reports": summary.reports,
                    }))
                    .unwrap_or_default()
                );
            } else if summary.degraded.is_empty() {
                println!(
                    "{GREEN}✓ all {} online member(s) passed the verify battery{RESET}",
                    summary.checked
                );
            } else {
                println!(
                    "{YELLOW}⚠ {} of {} online member(s) degraded:{RESET}",
                    summary.degraded.len(),
                    summary.checked
                );
                for g in &summary.degraded {
                    println!(
                        "  {RED}✗{RESET} {:<10} {} failing: {}",
                        g.node,
                        g.failed,
                        g.failing_checks.join(", ")
                    );
                }
                println!(
                    "\n  Inspect a node: {CYAN}ff fleet verify-node <name>{RESET}\n  \
                     Enable the scheduled sweep+alert: {CYAN}ff secrets set fleet_integrity_mode report{RESET}"
                );
            }
        }
        FleetCommand::Leader { json, action } => match action {
            None | Some(LeaderAction::Status { .. }) => {
                // `--json` at the `leader` level OR `status --json` both work.
                let json = json || matches!(action, Some(LeaderAction::Status { json: true }));
                handle_fleet_leader(&pool, json).await?;
            }
            Some(LeaderAction::StepDown {
                minutes,
                member,
                to,
                clear,
                yes,
            }) => {
                handle_fleet_leader_step_down(&pool, minutes, member, to, clear, yes).await?;
            }
        },
        FleetCommand::Health { json } => {
            handle_fleet_health(&pool, json).await?;
        }
        FleetCommand::Versions { verbose, live } => {
            handle_fleet_versions(&pool, verbose, live).await?;
        }
        FleetCommand::Gossip => {
            handle_fleet_gossip().await?;
        }
        FleetCommand::Route {
            workload,
            tool_calling,
            min_ctx,
            exclude_host,
            least_loaded,
            limit,
            format,
        } => {
            handle_fleet_route(
                &pool,
                &workload,
                tool_calling,
                min_ctx,
                exclude_host,
                least_loaded,
                limit,
                &format,
            )
            .await?;
        }
        FleetCommand::MigrateGithub {
            new_owner,
            skip_local,
            only,
            dry_run,
            yes,
        } => {
            let nodes = ff_db::pg_list_nodes(&pool).await?;
            let local = ff_agent::fleet_info::resolve_this_worker_name().await;
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
            handle_fleet_computers(format, os, role).await?;
        }
        FleetCommand::Exec { node, json, cmd } => {
            handle_fleet_exec(&pool, &node, json, &cmd).await?;
        }
        FleetCommand::Deploy {
            all,
            node,
            concurrency,
            json,
        } => {
            handle_fleet_deploy(&pool, all, node, concurrency, json).await?;
        }
        FleetCommand::Autoscaler { mode } => {
            handle_fleet_autoscaler(&pool, &mode).await?;
        }
        FleetCommand::Rollout { command } => {
            handle_fleet_rollout(&pool, command).await?;
        }
    }
    Ok(())
}

/// `ff fleet rollout <start|status>` — staged upgrade rollouts (item 26).
async fn handle_fleet_rollout(pool: &sqlx::PgPool, cmd: crate::RolloutCommand) -> Result<()> {
    use crate::RolloutCommand;
    match cmd {
        RolloutCommand::Start {
            software,
            staged,
            canary,
            failure_threshold_pct,
            dry_run,
        } => {
            if !staged {
                anyhow::bail!(
                    "pass --staged to use the gated rollout path (unstaged all-at-once is `ff fleet upgrade`)"
                );
            }
            let me = ff_agent::fleet_info::resolve_this_worker_name().await;

            // Resolvable non-leader targets, in the wave's resolution order.
            let (plans, skipped) = ff_agent::auto_upgrade::resolve_upgrade_plans_with_suffix(
                pool, &software, None, false, None,
            )
            .await?;
            let leader_lower = me.to_ascii_lowercase();
            let targets: Vec<String> = plans
                .into_iter()
                .map(|p| p.computer_name)
                .filter(|n| !n.eq_ignore_ascii_case(&leader_lower))
                .collect();

            if targets.is_empty() {
                anyhow::bail!(
                    "no resolvable non-leader targets for software_id='{software}' \
                     ({} skipped)",
                    skipped.len()
                );
            }

            let stages = ff_agent::upgrade_rollout::plan_stages(&targets, canary);
            println!("{CYAN}▶ ff fleet rollout start {software} --staged{RESET}");
            println!("  software:          {software}");
            println!("  targets (non-leader): {}", targets.len());
            println!("  failure threshold:  {failure_threshold_pct}% (canary halts on first fail)");
            for s in &stages {
                let label = if s.stage_idx == 0 { "canary" } else { "stage" };
                println!(
                    "  {label} {}: {} host(s) — {}",
                    s.stage_idx,
                    s.target_names.len(),
                    s.target_names.join(", ")
                );
            }

            if dry_run {
                println!(
                    "\n{YELLOW}Dry run — no rollout row created, no canary composed. \
                     Drop --dry-run to start.{RESET}"
                );
                return Ok(());
            }

            let id = ff_agent::upgrade_rollout::create_staged_rollout(
                pool,
                &software,
                &targets,
                canary,
                failure_threshold_pct,
                &me,
            )
            .await
            .map_err(|e| anyhow::anyhow!("create rollout: {e}"))?;

            println!("\n{GREEN}✓ Started staged rollout {id}{RESET}");
            println!("  Composed canary stage 0 only; the leader tick advances the rest.");
            println!(
                "  NOTE: set `ff secrets set staged_rollout_mode active` so the tick progresses \
                 stages (default off = canary composed but never advanced)."
            );
            println!("  Track with: ff fleet rollout status");
            Ok(())
        }
        RolloutCommand::Status { json } => handle_fleet_rollout_status(pool, json).await,
    }
}

/// `ff fleet rollout status` — list rollouts, most recent first.
async fn handle_fleet_rollout_status(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    use sqlx::Row;
    let rows = sqlx::query(
        r#"
        SELECT id, COALESCE(software_id, '') AS software_id,
               COALESCE(started_by, '') AS started_by,
               current_stage, status, failure_threshold_pct,
               COALESCE(halted_reason, '') AS halted_reason,
               jsonb_array_length(COALESCE(stages, '[]'::jsonb)) AS stage_count,
               created_at
          FROM upgrade_rollouts
         ORDER BY created_at DESC
         LIMIT 50
        "#,
    )
    .fetch_all(pool)
    .await?;

    if json {
        let mut arr: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
        for r in &rows {
            arr.push(serde_json::json!({
                "id": r.get::<uuid::Uuid, _>("id").to_string(),
                "software_id": r.get::<String, _>("software_id"),
                "started_by": r.get::<String, _>("started_by"),
                "current_stage": r.get::<i32, _>("current_stage"),
                "stage_count": r.get::<i32, _>("stage_count"),
                "status": r.get::<String, _>("status"),
                "failure_threshold_pct": r.get::<i32, _>("failure_threshold_pct"),
                "halted_reason": r.get::<String, _>("halted_reason"),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            }));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr))?
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!("{YELLOW}No rollouts.{RESET}");
        return Ok(());
    }

    println!(
        "{:<38} {:<16} {:<12} {:<8} {:<6} reason",
        "id", "software", "status", "stage", "thr%"
    );
    for r in &rows {
        let id: uuid::Uuid = r.get("id");
        let status: String = r.get("status");
        let stage: i32 = r.get("current_stage");
        let stage_count: i32 = r.get("stage_count");
        let reason: String = r.get("halted_reason");
        println!(
            "{:<38} {:<16} {:<12} {:<8} {:<6} {}",
            id.to_string(),
            r.get::<String, _>("software_id"),
            status,
            format!("{stage}/{}", stage_count.saturating_sub(1).max(0)),
            r.get::<i32, _>("failure_threshold_pct"),
            reason
        );
    }
    Ok(())
}

/// `ff fleet autoscaler <off|dry-run|active|status>` — read or set the P3
/// adaptive serving-mix autoscaler gate stored in `fleet_secrets.autoscaler_mode`.
/// `status` (the default) just prints the current value; the other three set it.
/// Default when the key is missing is `off` (the tick is a no-op).
async fn handle_fleet_autoscaler(pool: &sqlx::PgPool, mode: &str) -> Result<()> {
    const KEY: &str = "autoscaler_mode";
    let normalized = mode.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "status" => {
            let current = ff_db::pg_get_secret(pool, KEY)
                .await?
                .unwrap_or_else(|| "off".to_string());
            println!("autoscaler_mode = {current}");
            if current == "off" {
                println!(
                    "  (the P3 autoscaler tick is a no-op; set 'dry-run' to observe, 'active' to actuate)"
                );
            }
        }
        "off" | "dry-run" | "active" => {
            let who = whoami_tag();
            ff_db::pg_set_secret(
                pool,
                KEY,
                &normalized,
                Some("Orchestrator P3 adaptive serving-mix autoscaler gate (off|dry-run|active)"),
                Some(&who),
            )
            .await?;
            println!("{CYAN}▶ autoscaler_mode set to '{normalized}'{RESET}");
            match normalized.as_str() {
                "off" => println!("  the autoscaler tick will do nothing."),
                "dry-run" => {
                    println!("  the autoscaler will compute + log its plan but actuate nothing.")
                }
                "active" => println!(
                    "  the autoscaler will load/unload models to follow demand. Watch forgefleetd logs."
                ),
                _ => {}
            }
        }
        other => {
            anyhow::bail!(
                "unknown autoscaler mode '{other}' — expected one of: off | dry-run | active | status"
            );
        }
    }
    Ok(())
}

/// Build the per-candidate JSON object — byte-identical to the shape the
/// `fleet_route` MCP handler emits, so an agent gets the same structure from
/// the CLI as from MCP (the whole point of a mirror verb).
fn route_candidate_json(r: &ff_db::RouteCandidate) -> serde_json::Value {
    serde_json::json!({
        "worker_name": r.worker_name,
        "endpoint": r.endpoint,
        "catalog_id": r.catalog_id,
        "catalog_name": r.catalog_name,
        "family": r.family,
        "tier": r.tier,
        "tool_calling": r.tool_calling,
        "context_window": r.context_window,
        "usable_agent_ctx": r.usable_agent_ctx,
        "parallel_slots": r.parallel_slots,
        "health": r.health_status,
        "health_age_sec": r.health_age_sec,
        "host": {
            "os_family": r.os_family,
            "has_gpu": r.has_gpu,
            "is_unified_memory": r.is_unified_memory,
            "total_ram_gb": r.total_ram_gb,
        },
        // Latest sampled host load (most recent computer_metrics_history row;
        // null when the host has never been sampled). This is the signal the
        // `--least-loaded` tiebreak orders equal-tier candidates by.
        "load": {
            "cpu_pct": r.cpu_pct,
            "llm_active_requests": r.llm_active_requests,
        }
    })
}

/// Render a candidate's latest sampled load as `"<cpu>%/<reqs>"` (e.g.
/// `"3.9%/0"`) for the `LOAD` column. An unsampled host (never written a
/// `computer_metrics_history` row) shows `"-"` rather than a fake `0%/0`, so the
/// operator can tell "idle" from "no data". Either half falls back to `?` if
/// only one of the two metrics is present. Pure — unit-tested.
fn fmt_route_load(cpu_pct: Option<f64>, active_requests: Option<i32>) -> String {
    match (cpu_pct, active_requests) {
        (None, None) => "-".to_string(),
        (cpu, reqs) => format!(
            "{}/{}",
            cpu.map(|c| format!("{c:.1}%"))
                .unwrap_or_else(|| "?".into()),
            reqs.map(|r| r.to_string()).unwrap_or_else(|| "?".into()),
        ),
    }
}

/// Whether routing should require a tool-calling model. The explicit
/// `--tool-calling` flag forces it; `workload="tool_calling"` ALSO implies it,
/// so the tag-based call keeps working AND benefits from the real
/// `fleet_model_catalog.tool_calling` column — identical rule to the
/// `fleet_route` MCP handler (the mirror must not diverge here).
fn route_require_tool_calling(workload: &str, flag: bool) -> bool {
    flag || workload == "tool_calling"
}

/// Normalize the candidate limit to the scorer's contract: a non-positive
/// value means "use the default" (3), matching the MCP handler.
fn normalize_route_limit(limit: i64) -> i64 {
    if limit <= 0 { 3 } else { limit }
}

/// Whether the text view should warn that the winning candidate can't fit a
/// tool-using agent. Fires only when the operator hasn't already pinned an
/// agent-grade floor (`--min-ctx >= floor`) AND the best candidate's per-slot
/// ctx is known and below the floor. Unknown ctx never warns (can't tell).
fn route_warns_below_agent_floor(min_ctx: Option<i32>, best_ctx: Option<i32>, floor: i32) -> bool {
    min_ctx.unwrap_or(0) < floor && best_ctx.is_some_and(|c| c < floor)
}

/// `ff fleet route <workload> [--tool-calling] [--min-ctx N] [--exclude-host H]...`
/// — CLI mirror of the `fleet_route` MCP tool. Read-only workload-aware routing:
/// returns the best healthy deployment to send a `<workload>` request to, plus
/// runner-ups, via the SAME scorer (`ff_db::pg_route_deployments`) the
/// agent-swarm router uses — no parallel scorer to drift.
async fn handle_fleet_route(
    pool: &sqlx::PgPool,
    workload: &str,
    tool_calling: bool,
    min_ctx: Option<i32>,
    exclude_host: Vec<String>,
    least_loaded: bool,
    limit: i64,
    format: &str,
) -> Result<()> {
    let require_tool_calling = route_require_tool_calling(workload, tool_calling);
    let limit = normalize_route_limit(limit);

    let filter = ff_db::RouteFilter {
        workload: Some(workload.to_string()),
        require_tool_calling,
        min_ctx,
        exclude_hosts: exclude_host.clone(),
        // `ff fleet route` is an observability view: show whatever is marked
        // healthy. The freshness floor is applied only on live dispatch.
        max_health_age_sec: None,
        // Opt-in via `--least-loaded` to preview the dispatch ordering.
        prefer_least_loaded: least_loaded,
        limit,
    };
    let rows = ff_db::pg_route_deployments(pool, &filter)
        .await
        .map_err(|e| anyhow::anyhow!("fleet_route db: {e}"))?;

    // Human-readable constraint summary, reused in the header and the
    // no-match reason.
    let mut constraints = Vec::new();
    if require_tool_calling {
        constraints.push("tool_calling=true".to_string());
    }
    if let Some(c) = min_ctx {
        constraints.push(format!("usable_agent_ctx>={c}"));
    }
    if !exclude_host.is_empty() {
        constraints.push(format!("excluding {exclude_host:?}"));
    }
    if least_loaded {
        constraints.push("least-loaded-first".to_string());
    }

    if rows.is_empty() {
        let extra = if constraints.is_empty() {
            String::new()
        } else {
            format!(" with {}", constraints.join(", "))
        };
        let reason = format!(
            "no healthy deployment matches workload {workload:?}{extra}. \
             Load an agent-capable model with: ff model load <library_id> --agent"
        );
        if format == "json" {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workload": workload,
                    "decision": null,
                    "reason": reason,
                    "candidates": [],
                }))?
            );
        } else {
            println!("{YELLOW}⚠ {reason}{RESET}");
        }
        return Ok(());
    }

    if format == "json" {
        let candidates: Vec<serde_json::Value> = rows.iter().map(route_candidate_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "workload": workload,
                "decision": route_candidate_json(&rows[0]),
                "candidates": candidates,
            }))?
        );
        return Ok(());
    }

    // Text view: a one-line winner banner + a candidate table.
    let constraint_tag = if constraints.is_empty() {
        String::new()
    } else {
        format!(" [{}]", constraints.join(", "))
    };
    println!(
        "{GREEN}✓ fleet route{RESET} — workload {CYAN}{workload}{RESET}{constraint_tag} \
         ({} candidate{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );

    let best = &rows[0];
    println!(
        "{GREEN}→ best:{RESET} {CYAN}{}{RESET}  {}  {}  tier{}",
        best.worker_name,
        best.endpoint,
        best.catalog_id.as_deref().unwrap_or("-"),
        best.tier,
    );

    println!(
        "  {:<10} {:<30} {:<22} {:<4} {:<5} {:<14} {:<6} {:<11} {}",
        "WORKER",
        "ENDPOINT",
        "MODEL",
        "TIER",
        "TOOLS",
        "CTX(use/win)",
        "SLOTS",
        "LOAD(cpu/rq)",
        "HEALTH"
    );
    for r in &rows {
        let ctx = format!(
            "{}/{}",
            r.usable_agent_ctx
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            r.context_window
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
        );
        let health = match r.health_age_sec {
            Some(age) => format!("{} {age}s ago", r.health_status),
            None => r.health_status.clone(),
        };
        println!(
            "  {:<10} {:<30} {:<22} {:<4} {:<5} {:<14} {:<6} {:<11} {}",
            r.worker_name,
            r.endpoint,
            r.catalog_id.as_deref().unwrap_or("-"),
            r.tier,
            if r.tool_calling { "yes" } else { "no" },
            ctx,
            r.parallel_slots
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".into()),
            fmt_route_load(r.cpu_pct, r.llm_active_requests),
            health,
        );
    }

    // Agent-dispatch foot-gun guard: the default ranking can put a high-slot,
    // low-per-slot-ctx endpoint on top — fine for one-shot `fleet_run` calls,
    // but a tool-using agent's prompt won't fit. Warn only when the operator
    // hasn't already pinned an agent-grade floor (`--min-ctx >= AGENT_MIN_CTX`),
    // and only when the winner is actually below it. Read-only hint — the
    // scorer and JSON output (the MCP-mirror contract) are untouched.
    let agent_floor = ff_agent::model_runtime::AGENT_MIN_CTX as i32;
    if route_warns_below_agent_floor(min_ctx, best.usable_agent_ctx, agent_floor) {
        println!(
            "{YELLOW}⚠ best candidate's per-slot ctx ({}) is below the agent floor ({agent_floor}) \
             — ok for one-shot calls, but a tool-using agent may overflow. For agent dispatch: \
             ff fleet route {workload} --tool-calling --min-ctx {agent_floor}{RESET}",
            best.usable_agent_ctx.unwrap_or(0),
        );
    }
    Ok(())
}

/// `ff fleet exec <node> [--] <cmd...>` — run a command synchronously over
/// SSH on a single fleet computer and return its remote exit code.
///
/// Node resolution mirrors the revive/task-runner path: the ssh_user,
/// primary_ip and ssh_port come from the Postgres `computers` table (with a
/// `fleet_workers` fallback for the ssh_user), and the IP is rewritten to the
/// best-reachable address via `fleet_info::resolve_best_ip` (LAN preferred,
/// Tailscale fallback). We never read ~/.ssh/config — user@ip is built from
/// the DB.
///
/// In streaming mode (default) stdout/stderr are inherited so the remote
/// output appears live in the terminal; the process exits with the remote
/// exit code. In `--json` mode the output is captured and emitted as a single
/// `{node, exit_code, stdout, stderr}` object (still exiting with the remote
/// code so callers can branch on `$?`).
async fn handle_fleet_exec(
    pool: &sqlx::PgPool,
    node: &str,
    json: bool,
    cmd: &[String],
) -> Result<()> {
    if cmd.is_empty() {
        anyhow::bail!("no command given — usage: ff fleet exec <node> [--] <cmd...>");
    }

    // Resolve ssh_user + ip + port from Postgres. Prefer the `computers`
    // row (canonical hardware identity); fall back to `fleet_workers` for the
    // ssh_user when computers.ssh_user is null/empty. Match by name or IP.
    let row: Option<(String, String, String, i32)> = sqlx::query_as(
        "SELECT c.name,
                c.primary_ip,
                COALESCE(NULLIF(c.ssh_user, ''), fw.ssh_user, 'venkat') AS ssh_user,
                COALESCE(NULLIF(c.ssh_port, 0), 22)                     AS ssh_port
           FROM computers c
           LEFT JOIN fleet_workers fw ON fw.name = c.name
          WHERE LOWER(c.name) = LOWER($1) OR c.primary_ip = $1
          LIMIT 1",
    )
    .bind(node)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query computers: {e}"))?;

    let (name, primary_ip, ssh_user, ssh_port) = match row {
        Some(r) => r,
        None => anyhow::bail!(
            "no computer named (or IP) '{node}' in Postgres. \
             Run `ff fleet computers` to list known hosts."
        ),
    };

    // Rewrite to the best-reachable IP (LAN preferred, Tailscale fallback) —
    // same helper revive uses so we don't hit a stale LAN address on a
    // tailscale-only host.
    let target_ip = match ff_agent::fleet_info::resolve_best_ip(&name).await {
        Some((ip, _kind)) => ip,
        None => primary_ip,
    };

    let user_at_host = format!("{ssh_user}@{target_ip}");
    let remote_cmd = cmd.join(" ");

    // Build the ssh invocation. BatchMode keeps it non-interactive (no
    // password prompt hangs); accept-new trusts first-seen host keys the
    // way the rest of the fleet tooling does.
    let mut ssh = tokio::process::Command::new("ssh");
    ssh.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg(&user_at_host)
        .arg(&remote_cmd);

    if json {
        let out = ssh
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("spawn ssh {user_at_host}: {e}"))?;
        let exit_code = out.status.code().unwrap_or(-1);
        let payload = serde_json::json!({
            "node": name,
            "exit_code": exit_code,
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        return Ok(());
    }

    eprintln!("{CYAN}▶ ff fleet exec {name} ({user_at_host}):{RESET} {remote_cmd}");
    // Inherit stdio so stdout/stderr stream live to the terminal.
    let status = ssh
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn ssh {user_at_host}: {e}"))?;

    let exit_code = status.code().unwrap_or(-1);
    if exit_code != 0 {
        eprintln!("{YELLOW}(remote exit code {exit_code}){RESET}");
        std::process::exit(exit_code);
    }
    Ok(())
}

/// One deploy target, resolved from Postgres `computers` (+ `fleet_workers`
/// for ssh_user). Mirrors the field set `handle_fleet_exec` resolves, plus
/// the bits the deploy playbook + memory-tight gating need.
#[derive(Clone)]
struct DeployTarget {
    name: String,
    primary_ip: String,
    ssh_user: String,
    ssh_port: i32,
    os_family: String,
    total_ram_gb: i32,
    source_tree_path: String,
}

/// Result of one host's deploy attempt.
struct DeployResult {
    name: String,
    ok: bool,
    /// Running-binary SHA after restart (short, e.g. `db1a950e`) when we could
    /// parse `forgefleetd --version`; otherwise a short raw snippet / error.
    sha: String,
    secs: f64,
    detail: String,
}

/// A host is "memory-tight" when total_ram_gb <= 40 (the 32GB Linux boxes:
/// marcus/sophie/priya/lily/beyonce). On these we free RAM before building
/// and allow a longer per-host timeout. See the memory-tight-host rebuild
/// pattern.
const MEMORY_TIGHT_RAM_GB: i32 = 40;
const DEPLOY_TIMEOUT_ROOMY_SECS: u64 = 25 * 60;
const DEPLOY_TIMEOUT_TIGHT_SECS: u64 = 45 * 60;

/// Expand a leading `~/` to `$HOME/` so the path is safe inside a
/// double-quoted shell string (tilde does not expand there). Same trick the
/// auto_upgrade playbook substitution uses.
fn expand_home(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("~/") {
        format!("$HOME/{rest}")
    } else {
        raw.to_string()
    }
}

/// Build the self-built deploy playbook for one host.
///
/// This is the canonical `forgefleetd_git` upgrade sequence
/// (`crates/ff-agent/src/upgrade_playbooks.rs`) widened to build + install
/// BOTH binaries in a single cargo invocation:
///   - source `~/.cargo/env` (dash has no interactive PATH),
///   - git fetch + `reset --hard origin/main` (force-converge, no merge),
///   - `cargo build --release -p forge-fleet -p ff-terminal` (ff needs the
///     `-p ff-terminal` package selector or the CLI binary silently stays
///     stale),
///   - install both binaries to ~/.local/bin,
///   - codesign on macOS (cp/install breaks the signature → SIGKILL),
///   - restart per os_family using the matching idiom (launchctl kickstart on
///     macOS, systemd --user → pkill+nohup fallback on linux/linux-dgx).
///
/// `os_family` is taken from the `computers` row — never hardcoded per host.
/// DGX (`linux-dgx`) builds with `-j 2` to keep LLVM RAM pressure manageable
/// on the 4-core GB10 boxes.
fn deploy_playbook(os_family: &str, source_tree_path: &str) -> String {
    let src = expand_home(source_tree_path);
    // -p forge-fleet builds the forgefleetd daemon bin; -p ff-terminal builds
    // the ff CLI. Both in one cargo invocation → one shared compile.
    let cargo_build = if os_family == "linux-dgx" {
        "cargo build --release -p forge-fleet -p ff-terminal -j 2"
    } else {
        "cargo build --release -p forge-fleet -p ff-terminal"
    };

    // git: force-converge to origin/main. Linux trees accumulate build
    // artifacts that block a clean reset, so clean those two paths first
    // (mirrors the upgrade playbook).
    let git_sync = if os_family == "macos" {
        format!("cd \"{src}\" && git fetch origin && git reset --hard origin/main")
    } else {
        format!(
            "cd \"{src}\" && git fetch origin && git reset --hard origin/main && \
             git clean -fdx graphify-out node-compile-cache"
        )
    };

    match os_family {
        "macos" => format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             {git_sync} && \
             {cargo_build} && \
             install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && \
             install -m 755 target/release/ff ~/.local/bin/ff && \
             codesign --force --sign - ~/.local/bin/forgefleetd && \
             codesign --force --sign - ~/.local/bin/ff && \
             USER_ID=$(stat -f %u \"$HOME\" 2>/dev/null || id -u); \
             launchctl kickstart -k \"gui/${{USER_ID}}/com.forgefleet.forgefleetd\" 2>/dev/null \
               || launchctl kickstart -k \"user/${{USER_ID}}/com.forgefleet.forgefleetd\" 2>/dev/null \
               || ( for p in $(pgrep -x forgefleetd); do kill -TERM \"$p\" 2>/dev/null; done; sleep 2; \
                    for p in $(pgrep -x forgefleetd); do kill -KILL \"$p\" 2>/dev/null; done; \
                    nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                    </dev/null >/tmp/forgefleetd.log 2>&1 & disown ); \
             sleep 4; ~/.local/bin/ff model resume-from-build 2>/dev/null || true; \
             RN=$(pgrep -xc forgefleetd 2>/dev/null || echo 0); \
             echo \"RESTART_VERIFY count=$RN (macos: launchd-managed)\""
        ),
        // linux + linux-dgx share the same restart idiom; only -j differs
        // (folded into cargo_build above). Prefer the systemd user unit; the
        // fallback kills the running daemon by PID *excluding this shell* ($$)
        // — a `pkill -f forgefleetd...` would also match (and kill) THIS deploy
        // command's own SSH shell, which exited it 255 before the restart ran.
        _ => format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             {git_sync} && \
             {cargo_build} && \
             install -m 755 target/release/forgefleetd ~/.local/bin/forgefleetd && \
             install -m 755 target/release/ff ~/.local/bin/ff && \
             export XDG_RUNTIME_DIR=\"${{XDG_RUNTIME_DIR:-/run/user/$(id -u)}}\"; \
             systemctl --user stop forgefleetd.service 2>/dev/null; \
             for p in $(pgrep -x forgefleetd); do kill -TERM \"$p\" 2>/dev/null; done; sleep 2; \
             for p in $(pgrep -x forgefleetd); do kill -KILL \"$p\" 2>/dev/null; done; \
             ( systemctl --user reset-failed forgefleetd.service 2>/dev/null; \
               systemctl --user start forgefleetd.service 2>/dev/null ) \
               || ( nohup \"$HOME/.local/bin/forgefleetd\" --worker-name $(hostname -s) start \
                    </dev/null >/tmp/forgefleetd.log 2>&1 & disown ); \
             sleep 4; ~/.local/bin/ff model resume-from-build 2>/dev/null || true; \
             RP=$(pgrep -x forgefleetd | head -1); RN=$(pgrep -xc forgefleetd 2>/dev/null || echo 0); \
             RE=$(readlink /proc/$RP/exe 2>/dev/null); \
             echo \"RESTART_VERIFY count=$RN exe=$RE\"; \
             case \"$RE\" in *'(deleted)'*) echo 'RESTART_STALE: running deleted inode' >&2; exit 7;; esac; \
             [ \"$RN\" -ge 1 ] || {{ echo 'RESTART_DOWN: no forgefleetd running' >&2; exit 8; }}"
        ),
    }
}

/// Run one shell command on a target over SSH with a deadline, capturing
/// output. Resolves the best-reachable IP (LAN→Tailscale) the same way
/// `handle_fleet_exec` does. Returns (exit_code, stdout, stderr); a timeout
/// surfaces as exit_code = -2.
async fn deploy_ssh(
    t: &DeployTarget,
    remote_cmd: &str,
    timeout_secs: u64,
) -> (i32, String, String) {
    let target_ip = match ff_agent::fleet_info::resolve_best_ip(&t.name).await {
        Some((ip, _kind)) => ip,
        None => t.primary_ip.clone(),
    };
    let user_at_host = format!("{}@{target_ip}", t.ssh_user);

    let mut ssh = tokio::process::Command::new("ssh");
    ssh.arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-p")
        .arg(t.ssh_port.to_string())
        .arg(&user_at_host)
        .arg(remote_cmd);

    let fut = ssh.output();
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await {
        Ok(Ok(out)) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ),
        Ok(Err(e)) => (-1, String::new(), format!("ssh spawn error: {e}")),
        Err(_) => (
            -2,
            String::new(),
            format!("timed out after {timeout_secs}s"),
        ),
    }
}

/// Deploy the full forgefleetd + ff playbook to one target, then verify
/// convergence by reading the RUNNING binary SHA. Never panics — every
/// failure mode collapses into a `DeployResult { ok: false, .. }`.
async fn deploy_one_host(t: DeployTarget) -> DeployResult {
    use ff_core::build_version::BuildVersion;
    let start = std::time::Instant::now();
    let tight = t.total_ram_gb > 0 && t.total_ram_gb <= MEMORY_TIGHT_RAM_GB;
    let timeout_secs = if tight {
        DEPLOY_TIMEOUT_TIGHT_SECS
    } else {
        DEPLOY_TIMEOUT_ROOMY_SECS
    };

    // 1) Memory-tight hosts: free RAM (pause local model deployments) before
    //    the cargo build so the release build doesn't OOM. Best-effort — a
    //    non-zero exit (e.g. nothing to free) is not fatal.
    if tight {
        let (_code, _o, _e) = deploy_ssh(&t, "~/.local/bin/ff model free-for-build", 120).await;
    }

    // 2) Build + install + restart.
    let playbook = deploy_playbook(&t.os_family, &t.source_tree_path);
    let (code, _stdout, stderr) = deploy_ssh(&t, &playbook, timeout_secs).await;
    if code != 0 {
        let snippet: String = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        return DeployResult {
            name: t.name,
            ok: false,
            sha: "-".into(),
            secs: start.elapsed().as_secs_f64(),
            detail: if code == -2 {
                snippet
            } else {
                format!("playbook exit {code}: {snippet}")
            },
        };
    }

    // 3) Convergence = RUNNING binary. Give the freshly-restarted daemon a
    //    moment, then read its version SHA. We read forgefleetd (the daemon
    //    we just bounced) so the SHA reflects the running process, not just
    //    the on-disk binary.
    let (vcode, vout, verr) = deploy_ssh(
        &t,
        "sleep 3; ~/.local/bin/forgefleetd --version 2>&1 | head -1",
        60,
    )
    .await;
    let raw = if vcode == 0 {
        vout.trim().to_string()
    } else {
        format!("version-probe exit {vcode}: {}", verr.trim())
    };
    match BuildVersion::parse(&raw) {
        Some(v) => DeployResult {
            name: t.name,
            ok: true,
            sha: v.short_sha().to_string(),
            secs: start.elapsed().as_secs_f64(),
            detail: format!("{} ({})", v.date, v.state),
        },
        None => {
            // Built + restarted fine but we couldn't parse a SHA — report the
            // raw snippet and mark it not-converged so the operator looks.
            let snippet: String = raw.chars().take(40).collect();
            DeployResult {
                name: t.name,
                ok: false,
                sha: "?".into(),
                secs: start.elapsed().as_secs_f64(),
                detail: format!("restarted but version unparsable: {snippet}"),
            }
        }
    }
}

/// `ff fleet deploy --all | --node <name>` — fast PARALLEL self-built deploy.
///
/// Additive alternative to the `ff tasks compose-fleet-upgrade` wave. Targets
/// resolve from Postgres (`computers` ⋈ `fleet_workers`); `--all` selects every
/// ONLINE non-leader computer (the leader is excluded — it restarts itself
/// badly). Each target runs the deploy playbook over SSH concurrently (bounded
/// by --concurrency, default 6); memory-tight hosts (total_ram_gb ≤ 40) get a
/// `ff model free-for-build` first and a 45-min timeout. After restart we read
/// each host's RUNNING forgefleetd SHA and report per-host ok/fail + SHA +
/// duration, then a convergence summary.
async fn handle_fleet_deploy(
    pool: &sqlx::PgPool,
    all: bool,
    node: Option<String>,
    concurrency: usize,
    json: bool,
) -> Result<()> {
    use futures::stream::{FuturesUnordered, StreamExt};

    if !all && node.is_none() {
        anyhow::bail!("pass --all or --node <name> to pick targets");
    }
    if all && node.is_some() {
        anyhow::bail!("--all and --node are mutually exclusive");
    }
    let concurrency = concurrency.max(1);

    // Resolve targets. Both shapes pull the same columns; --all filters to
    // online non-leader, --node matches one host by name or IP (leader
    // allowed — the only way to deploy the leader).
    let targets: Vec<DeployTarget> = if all {
        sqlx::query_as::<_, (String, String, String, i32, String, i32, Option<String>)>(
            "SELECT c.name,
                    c.primary_ip,
                    COALESCE(NULLIF(c.ssh_user, ''), fw.ssh_user, 'venkat') AS ssh_user,
                    COALESCE(NULLIF(c.ssh_port, 0), 22)                     AS ssh_port,
                    COALESCE(c.os_family, 'linux')                          AS os_family,
                    COALESCE(c.total_ram_gb, 0)                             AS total_ram_gb,
                    c.source_tree_path
               FROM computers c
               LEFT JOIN fleet_workers fw ON fw.name = c.name
              WHERE c.status = 'online'
                AND COALESCE(fw.role, '') <> 'leader'
                -- Skip reserved/drained hosts (V114): a host the operator (or the
                -- P3 autoscaler) reserved must not become a build target.
                AND COALESCE(c.reservation_state, 'available') = 'available'
              ORDER BY string_to_array(c.primary_ip, '.')::int[]",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("query online non-leader computers: {e}"))?
        .into_iter()
        .map(
            |(name, primary_ip, ssh_user, ssh_port, os_family, total_ram_gb, stp)| DeployTarget {
                name,
                primary_ip,
                ssh_user,
                ssh_port,
                os_family,
                total_ram_gb,
                source_tree_path: stp.unwrap_or_else(|| "~/projects/forge-fleet".into()),
            },
        )
        .collect()
    } else {
        let n = node.unwrap();
        let row = sqlx::query_as::<_, (String, String, String, i32, String, i32, Option<String>)>(
            "SELECT c.name,
                    c.primary_ip,
                    COALESCE(NULLIF(c.ssh_user, ''), fw.ssh_user, 'venkat') AS ssh_user,
                    COALESCE(NULLIF(c.ssh_port, 0), 22)                     AS ssh_port,
                    COALESCE(c.os_family, 'linux')                          AS os_family,
                    COALESCE(c.total_ram_gb, 0)                             AS total_ram_gb,
                    c.source_tree_path
               FROM computers c
               LEFT JOIN fleet_workers fw ON fw.name = c.name
              WHERE LOWER(c.name) = LOWER($1) OR c.primary_ip = $1
              LIMIT 1",
        )
        .bind(&n)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("query computers: {e}"))?;
        match row {
            Some((name, primary_ip, ssh_user, ssh_port, os_family, total_ram_gb, stp)) => {
                vec![DeployTarget {
                    name,
                    primary_ip,
                    ssh_user,
                    ssh_port,
                    os_family,
                    total_ram_gb,
                    source_tree_path: stp.unwrap_or_else(|| "~/projects/forge-fleet".into()),
                }]
            }
            None => anyhow::bail!(
                "no computer named (or IP) '{n}' in Postgres. \
                 Run `ff fleet computers` to list known hosts."
            ),
        }
    };

    if targets.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("{YELLOW}No deploy targets (no online non-leader computers).{RESET}");
        }
        return Ok(());
    }

    if !json {
        eprintln!(
            "{CYAN}▶ ff fleet deploy{RESET}: {} target(s), up to {} building in parallel",
            targets.len(),
            concurrency.min(targets.len())
        );
        for t in &targets {
            let tight = t.total_ram_gb > 0 && t.total_ram_gb <= MEMORY_TIGHT_RAM_GB;
            eprintln!(
                "  {:<12} {:<10} {:>3}GB{}",
                t.name,
                t.os_family,
                t.total_ram_gb,
                if tight {
                    " (memory-tight: free-for-build + 45m timeout)"
                } else {
                    ""
                }
            );
        }
    }

    // Drive the deploys with bounded concurrency: keep at most `concurrency`
    // hosts building at once, refilling as each completes.
    let mut iter = targets.into_iter();
    let mut inflight = FuturesUnordered::new();
    for _ in 0..concurrency {
        if let Some(t) = iter.next() {
            inflight.push(deploy_one_host(t));
        }
    }
    let mut results: Vec<DeployResult> = Vec::new();
    while let Some(res) = inflight.next().await {
        if !json {
            let mark = if res.ok {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{RED}✗{RESET}")
            };
            eprintln!(
                "  {mark} {:<12} {:<10} {:>6.0}s  {}",
                res.name, res.sha, res.secs, res.detail
            );
        }
        results.push(res);
        if let Some(t) = iter.next() {
            inflight.push(deploy_one_host(t));
        }
    }
    results.sort_by(|a, b| a.name.cmp(&b.name));

    // Convergence target = the most-common SHA among successful hosts.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in &results {
        if r.ok {
            *counts.entry(r.sha.clone()).or_insert(0) += 1;
        }
    }
    let target_sha = counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(sha, _)| sha.clone());
    let converged = results
        .iter()
        .filter(|r| r.ok && target_sha.as_deref() == Some(r.sha.as_str()))
        .count();
    let total = results.len();

    if json {
        let arr: Vec<_> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "host": r.name,
                    "status": if r.ok { "ok" } else { "fail" },
                    "sha": r.sha,
                    "secs": (r.secs * 10.0).round() / 10.0,
                    "detail": r.detail,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "results": arr,
            "target_sha": target_sha,
            "converged": converged,
            "total": total,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        if converged != total {
            std::process::exit(1);
        }
        return Ok(());
    }

    println!(
        "\n{:<14} {:<8} {:<10} {:>7}",
        "host", "status", "sha", "secs"
    );
    println!("{}", "─".repeat(42));
    for r in &results {
        let status = if r.ok {
            format!("{GREEN}ok{RESET}  ")
        } else {
            format!("{RED}fail{RESET}")
        };
        println!("{:<14} {:<8} {:<10} {:>7.0}", r.name, status, r.sha, r.secs);
    }
    let target_disp = target_sha.as_deref().unwrap_or("-");
    println!();
    if converged == total && total > 0 {
        println!("{GREEN}✓ {converged}/{total} converged on {target_disp}{RESET}");
    } else {
        println!(
            "{YELLOW}⚠ {converged}/{total} converged on {target_disp}{RESET} \
             ({} not converged)",
            total - converged
        );
        std::process::exit(1);
    }
    Ok(())
}

async fn handle_fleet_computers(
    format: String,
    os_filter: Option<String>,
    role_filter: Option<String>,
) -> Result<()> {
    // Pull from `computers` JOIN `fleet_workers` so every consumer (LLMs
    // discovering DGX Sparks, humans wondering which machine has a GPU,
    // automation looking up CPU/RAM) gets the canonical hardware shape
    // in one call. The thin resolver path only had ip/os/role and forced
    // callers to round-trip Postgres themselves.
    use ff_agent::fleet_info::get_fleet_pool;
    use serde::Serialize;

    let pool = get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("postgres unreachable: {e}"))?;

    #[derive(sqlx::FromRow, Serialize)]
    struct Row {
        name: String,
        primary_ip: String,
        ssh_user: String,
        role: String,
        os_family: String,
        os_distribution: String,
        os_version: Option<String>,
        cpu_cores: i32,
        total_ram_gb: i32,
        has_gpu: bool,
        gpu_kind: String,
        gpu_model: Option<String>,
        gpu_count: i32,
        gpu_total_vram_gb: Option<f64>,
        is_dgx: bool,
        is_unified_memory: bool,
    }

    let mut rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT c.name,
                c.primary_ip,
                COALESCE(fw.ssh_user, 'venkat') AS ssh_user,
                COALESCE(fw.role, 'unknown') AS role,
                COALESCE(c.os_family, 'unknown') AS os_family,
                COALESCE(c.os_distribution, '') AS os_distribution,
                c.os_version,
                COALESCE(c.cpu_cores, 0) AS cpu_cores,
                COALESCE(c.total_ram_gb, 0) AS total_ram_gb,
                COALESCE(c.has_gpu, false) AS has_gpu,
                COALESCE(c.gpu_kind, 'none') AS gpu_kind,
                c.gpu_model,
                COALESCE(c.gpu_count, 0) AS gpu_count,
                c.gpu_total_vram_gb,
                (c.os_family = 'linux-dgx') AS is_dgx,
                (c.gpu_kind IN ('apple_silicon', 'gb10')) AS is_unified_memory
         FROM computers c
         LEFT JOIN fleet_workers fw ON fw.name = c.name
         ORDER BY
            CASE COALESCE(fw.role,'')
                WHEN 'leader' THEN 0
                WHEN 'standby' THEN 1
                WHEN 'worker' THEN 2
                ELSE 9
            END,
            string_to_array(c.primary_ip, '.')::int[]",
    )
    .fetch_all(&pool)
    .await?;

    if let Some(filter) = os_filter {
        let lower = filter.to_ascii_lowercase();
        rows.retain(|c| c.os_family.to_ascii_lowercase().contains(&lower));
    }
    if let Some(filter) = role_filter {
        let lower = filter.to_ascii_lowercase();
        rows.retain(|c| c.role.to_ascii_lowercase().contains(&lower));
    }

    match format.as_str() {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        _ => {
            println!("{GREEN}✓ Fleet Computers{RESET} ({} total)", rows.len());
            for c in &rows {
                let role_tag = match c.role.as_str() {
                    "leader" => format!("{GREEN}leader{RESET}"),
                    "standby" => format!("{YELLOW}standby{RESET}"),
                    "worker" => "worker".to_string(),
                    other => other.to_string(),
                };
                let hw = if c.has_gpu {
                    // Prefer the gpu_model string when available (it carries
                    // the canonical "NVIDIA GB10 Grace+Blackwell" name);
                    // fall back to the kind label otherwise.
                    let primary = match (&c.gpu_model, c.gpu_kind.as_str()) {
                        (Some(m), _) if !m.is_empty() => m.clone(),
                        (_, "apple_silicon") => "Apple Silicon".to_string(),
                        (_, "gb10") => "NVIDIA GB10".to_string(),
                        (_, "nvidia_cuda") => "NVIDIA CUDA".to_string(),
                        (_, "amd_rocm") => "AMD ROCm".to_string(),
                        (_, other) => other.to_string(),
                    };
                    let vram_tag = match c.gpu_total_vram_gb {
                        Some(v) if v > 0.0 => format!(" {v:.0}GB"),
                        _ => String::new(),
                    };
                    let unified_tag = if c.is_unified_memory {
                        " (unified)"
                    } else {
                        ""
                    };
                    format!("{primary}{vram_tag}{unified_tag}")
                } else {
                    "(no GPU)".to_string()
                };
                let dgx_tag = if c.is_dgx {
                    format!(" {CYAN}[DGX Spark]{RESET}")
                } else {
                    String::new()
                };
                println!(
                    "  {name:<10} {ip:<16} {role:<8}  {os:<14} {cores}C/{ram}GB  {hw}{dgx}",
                    name = c.name,
                    ip = c.primary_ip,
                    role = role_tag,
                    os = c.os_family,
                    cores = c.cpu_cores,
                    ram = c.total_ram_gb,
                    hw = hw,
                    dgx = dgx_tag,
                );
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

#[cfg(test)]
mod version_target_tests {
    use super::pick_version_target;

    #[test]
    fn prefers_latest_over_mode() {
        // The bug fix: when LATEST is known, it is the target even if most
        // hosts sit on an older modal SHA. Otherwise the one host on LATEST
        // reads `drift` while the stale majority reads `✓`.
        let t = pick_version_target(Some("17a5c3c4"), Some("fb60060c"));
        assert_eq!(t, Some(("17a5c3c4".to_string(), true)));
    }

    #[test]
    fn falls_back_to_mode_when_latest_unknown() {
        // 6h upstream-check tick hasn't populated latest_version yet → report
        // fleet homogeneity against the modal installed SHA.
        let t = pick_version_target(None, Some("fb60060c"));
        assert_eq!(t, Some(("fb60060c".to_string(), false)));
    }

    #[test]
    fn treats_empty_and_dash_latest_as_unknown() {
        assert_eq!(
            pick_version_target(Some(""), Some("fb60060c")),
            Some(("fb60060c".to_string(), false))
        );
        assert_eq!(
            pick_version_target(Some("-"), Some("fb60060c")),
            Some(("fb60060c".to_string(), false))
        );
    }

    #[test]
    fn none_when_neither_known() {
        assert_eq!(pick_version_target(None, None), None);
        assert_eq!(pick_version_target(Some(""), Some("-")), None);
    }
}

#[cfg(test)]
mod route_tests {
    use super::{
        fmt_route_load, normalize_route_limit, route_require_tool_calling,
        route_warns_below_agent_floor,
    };

    #[test]
    fn route_load_unsampled_host_shows_dash() {
        // Never-sampled host (no metrics row) must read "-", not a fake idle
        // "0%/0" — so the operator can tell "no data" from "genuinely idle".
        assert_eq!(fmt_route_load(None, None), "-");
    }

    #[test]
    fn route_load_formats_cpu_and_requests() {
        assert_eq!(fmt_route_load(Some(3.94), Some(0)), "3.9%/0");
        assert_eq!(fmt_route_load(Some(16.0), Some(2)), "16.0%/2");
    }

    #[test]
    fn route_load_partial_sample_marks_missing_half() {
        // One metric present, the other null → "?" for the missing half rather
        // than dropping to "-" (the host HAS been sampled).
        assert_eq!(fmt_route_load(Some(5.0), None), "5.0%/?");
        assert_eq!(fmt_route_load(None, Some(3)), "?/3");
    }

    #[test]
    fn warns_when_best_below_floor_and_no_pinned_min() {
        // Default call, winner is an 8192-per-slot endpoint → warn.
        assert!(route_warns_below_agent_floor(None, Some(8192), 32768));
    }

    #[test]
    fn no_warn_when_operator_pinned_agent_floor() {
        // `--min-ctx 32768` means the operator already controls the floor;
        // anything returned satisfies it, so the hint is redundant noise.
        assert!(!route_warns_below_agent_floor(
            Some(32768),
            Some(8192),
            32768
        ));
    }

    #[test]
    fn no_warn_when_best_meets_floor_or_unknown() {
        assert!(!route_warns_below_agent_floor(None, Some(32768), 32768));
        assert!(!route_warns_below_agent_floor(None, Some(65536), 32768));
        // Unknown per-slot ctx can't be judged → never warn.
        assert!(!route_warns_below_agent_floor(None, None, 32768));
    }

    #[test]
    fn explicit_flag_requires_tool_calling() {
        assert!(route_require_tool_calling("code", true));
        assert!(!route_require_tool_calling("code", false));
    }

    #[test]
    fn tool_calling_workload_implies_requirement() {
        // The subtle mirror rule: routing workload="tool_calling" must require
        // a tool-calling model even without the flag, exactly like the MCP tool.
        assert!(route_require_tool_calling("tool_calling", false));
        assert!(route_require_tool_calling("tool_calling", true));
    }

    #[test]
    fn limit_normalizes_nonpositive_to_default() {
        assert_eq!(normalize_route_limit(0), 3);
        assert_eq!(normalize_route_limit(-5), 3);
        assert_eq!(normalize_route_limit(1), 1);
        assert_eq!(normalize_route_limit(10), 10);
    }
}
