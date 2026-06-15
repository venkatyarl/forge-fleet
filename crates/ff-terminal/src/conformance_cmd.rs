//! `ff conformance check / profiles / report` — V120 fleet conformance.
//!
//! The VERIFY GATE that proves "version string parsed = ok" is not enough:
//! `check` runs the schema-V120 gates over SSH (read-only) against the
//! amd-training hosts and records a MEASURED conformant bool + the SPECIFIC
//! reason — catching logan's `+cu128` wheel and veronica's render/video / kfd
//! permission gap that a version parse never sees. It NEVER remediates a host.

use anyhow::{Context, Result};
use clap::Subcommand;
use ff_agent::conformance;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use ff_core::config::FleetConfig;

/// `ff conformance <sub>` — desired-state profiles + the VERIFY GATE.
#[derive(Debug, Clone, Subcommand)]
pub enum ConformanceCommand {
    /// Run the V120 verify gates (read-only over SSH) against amd-training
    /// hosts and report a MEASURED conformant bool + the SPECIFIC reason.
    Check {
        /// Limit to a single host (by name); default = all in-scope hosts.
        #[arg(long)]
        host: Option<String>,
        /// Conformance role to check.
        #[arg(long, default_value = "amd-training")]
        role: String,
        /// Persist results into `conformance_results` (latest-wins upsert).
        #[arg(long)]
        record: bool,
        /// Emit JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// List the desired-state profiles + their required packages and gates.
    Profiles,
    /// Show the latest recorded conformance results per host for a role.
    Report {
        /// Conformance role to report on.
        #[arg(long, default_value = "amd-training")]
        role: String,
    },
    /// Plan remediation for non-conformant hosts. DEFAULT = dry-run (prints the
    /// plan, changes nothing). `--apply` runs ONLY the auto-safe actions (the
    /// kfd_access group fix); host-mutating recipes (torch wheel / ROCm install)
    /// are always operator-reviewed and never auto-run.
    Remediate {
        /// Limit to a single host (by name); default = all in-scope hosts.
        #[arg(long)]
        host: Option<String>,
        /// Conformance role to remediate.
        #[arg(long, default_value = "amd-training")]
        role: String,
        /// Actuate the AUTO-SAFE actions (skips Manual ones). Without this flag
        /// nothing runs. Intended for an operator at a terminal — autopilot
        /// loops must never pass it.
        #[arg(long)]
        apply: bool,
        /// Emit JSON instead of the human plan.
        #[arg(long)]
        json: bool,
    },
}

/// Dispatch `ff conformance <sub>`. Opens a short-lived PgPool from
/// `~/.forgefleet/fleet.toml` (same pattern as the other DB-backed CLIs).
pub async fn run(command: ConformanceCommand) -> Result<()> {
    let pg = connect().await?;
    match command {
        ConformanceCommand::Check {
            host,
            role,
            record,
            json,
        } => handle_check(&pg, host.as_deref(), &role, record, json).await,
        ConformanceCommand::Profiles => handle_profiles(&pg).await,
        ConformanceCommand::Report { role } => handle_report(&pg, &role).await,
        ConformanceCommand::Remediate {
            host,
            role,
            apply,
            json,
        } => handle_remediate(&pg, host.as_deref(), &role, apply, json).await,
    }
}

/// Open a short-lived Postgres pool from `~/.forgefleet/fleet.toml`.
async fn connect() -> Result<PgPool> {
    let home = dirs::home_dir().context("no home dir")?;
    let config_path = home.join(".forgefleet/fleet.toml");
    let toml_str = tokio::fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("read {}", config_path.display()))?;
    let config: FleetConfig = toml::from_str(&toml_str).context("parse fleet.toml")?;
    let db_url = config.database.url.trim().to_string();
    if db_url.is_empty() {
        anyhow::bail!("database.url is empty in fleet.toml");
    }
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&db_url)
        .await
        .context("connect postgres")
}

/// `ff conformance check [--host H] [--role R] [--json]`.
pub async fn handle_check(
    pg: &PgPool,
    host: Option<&str>,
    role: &str,
    record: bool,
    json: bool,
) -> Result<()> {
    let checked_by = ff_agent::fleet_info::resolve_this_worker_name().await;

    // Resolve target hosts.
    let hosts = match host {
        Some(name) => {
            let c = conformance::get_computer(pg, name)
                .await?
                .with_context(|| format!("computer '{name}' not found"))?;
            vec![c]
        }
        None => conformance::hosts_in_scope(pg, role).await?,
    };

    if hosts.is_empty() {
        println!("No in-scope hosts for role '{role}'.");
        return Ok(());
    }

    let mut reports = Vec::new();
    for c in &hosts {
        match conformance::check_host(pg, c, role, &checked_by, record).await? {
            Some(r) => reports.push(r),
            None => {
                if host.is_some() {
                    println!(
                        "{} does not match any enabled '{role}' profile (out of scope).",
                        c.name
                    );
                }
            }
        }
    }

    if json {
        print_json(&reports);
        return Ok(());
    }

    for r in &reports {
        let verdict = if r.conformant() {
            "CONFORMANT"
        } else {
            "NON-CONFORMANT"
        };
        println!("\n{}  [{}]  profile={}", r.computer, verdict, r.profile_key);
        for o in &r.outcomes {
            let mark = if o.conformant { "ok  " } else { "FAIL" };
            println!(
                "  [{}] {:<12} ({:<10}) {}",
                mark, o.check_key, o.severity, o.reason
            );
        }
        if !r.conformant() {
            println!("  → blocker reasons:");
            for b in r.blocker_reasons() {
                println!("      - {b}");
            }
        }
    }

    let nonconformant = reports.iter().filter(|r| !r.conformant()).count();
    println!(
        "\n{} host(s) checked for '{role}'; {} NON-CONFORMANT.",
        reports.len(),
        nonconformant
    );
    Ok(())
}

fn print_json(reports: &[conformance::HostReport]) {
    let arr: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            serde_json::json!({
                "computer": r.computer,
                "profile_key": r.profile_key,
                "role": r.role,
                "conformant": r.conformant(),
                "blocker_reasons": r.blocker_reasons(),
                "checks": r.outcomes.iter().map(|o| serde_json::json!({
                    "check_key": o.check_key,
                    "check_kind": o.check_kind,
                    "severity": o.severity,
                    "conformant": o.conformant,
                    "reason": o.reason,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!(arr)).unwrap_or_default()
    );
}

/// `ff conformance profiles` — list desired-state profiles + their gates.
pub async fn handle_profiles(pg: &PgPool) -> Result<()> {
    let profs = sqlx::query(
        r#"
        SELECT id, profile_key, os_family, hardware_class, role, title,
               runtime_env, enabled
        FROM conformance_profiles
        ORDER BY profile_key
        "#,
    )
    .fetch_all(pg)
    .await?;

    if profs.is_empty() {
        println!("No conformance profiles defined.");
        return Ok(());
    }

    for p in &profs {
        let id: uuid::Uuid = p.get("id");
        let key: String = p.get("profile_key");
        let title: String = p.get("title");
        let role: String = p.get("role");
        let enabled: bool = p.get("enabled");
        let runtime_env: serde_json::Value = p.get("runtime_env");
        println!(
            "\n{} {}  (role={}, {})",
            if enabled { "●" } else { "○" },
            key,
            role,
            title
        );
        if runtime_env != serde_json::json!({}) {
            println!("    runtime_env: {runtime_env}");
        }

        let pkgs = sqlx::query(
            r#"
            SELECT software_id, version_constraint, must_contain, must_not_contain, note
            FROM conformance_profile_packages
            WHERE profile_id = $1
            ORDER BY software_id
            "#,
        )
        .bind(id)
        .fetch_all(pg)
        .await?;
        if !pkgs.is_empty() {
            println!("    required packages:");
            for pk in &pkgs {
                let sid: String = pk.get("software_id");
                let vc: Option<String> = pk.get("version_constraint");
                let mc: Option<String> = pk.get("must_contain");
                let mnc: Option<String> = pk.get("must_not_contain");
                let mut constraints = Vec::new();
                if let Some(v) = vc {
                    constraints.push(v);
                }
                if let Some(v) = mc {
                    constraints.push(format!("contains '{v}'"));
                }
                if let Some(v) = mnc {
                    constraints.push(format!("NOT '{v}'"));
                }
                println!(
                    "      - {} {}",
                    sid,
                    if constraints.is_empty() {
                        "(any)".to_string()
                    } else {
                        constraints.join(", ")
                    }
                );
            }
        }

        let checks = sqlx::query(
            r#"
            SELECT check_key, check_kind, severity, title
            FROM conformance_checks
            WHERE profile_id = $1 AND enabled
            ORDER BY check_key
            "#,
        )
        .bind(id)
        .fetch_all(pg)
        .await?;
        if !checks.is_empty() {
            println!("    verify gates:");
            for ch in &checks {
                let ck: String = ch.get("check_key");
                let kind: String = ch.get("check_kind");
                let sev: String = ch.get("severity");
                let t: String = ch.get("title");
                println!("      - [{}] {} ({}) — {}", sev, ck, kind, t);
            }
        }
    }
    Ok(())
}

/// `ff conformance remediate [--host H] [--role R] [--apply] [--json]`.
///
/// Runs a fresh (read-only) conformance check, then maps each non-conformant
/// gate to a remediation action. DEFAULT is a dry-run plan; `--apply` actuates
/// ONLY the auto-safe actions. Manual recipes are always printed, never run.
pub async fn handle_remediate(
    pg: &PgPool,
    host: Option<&str>,
    role: &str,
    apply: bool,
    json: bool,
) -> Result<()> {
    let checked_by = ff_agent::fleet_info::resolve_this_worker_name().await;
    let me = ff_agent::fleet_info::resolve_this_worker_name().await;

    let hosts = match host {
        Some(name) => {
            let c = conformance::get_computer(pg, name)
                .await?
                .with_context(|| format!("computer '{name}' not found"))?;
            vec![c]
        }
        None => conformance::hosts_in_scope(pg, role).await?,
    };

    if hosts.is_empty() {
        println!("No in-scope hosts for role '{role}'.");
        return Ok(());
    }

    // Build a plan per host from a LIVE check (record=false — planning never
    // mutates the recorded conformance state).
    let mut plans = Vec::new();
    for c in &hosts {
        if let Some(report) = conformance::check_host(pg, c, role, &checked_by, false).await? {
            let plan = conformance::plan_remediation(&report, &c.ssh_user);
            plans.push((c.clone(), plan));
        }
    }

    if json {
        print_remediation_json(&plans);
        // JSON mode is report-only; never actuate (autopilot-safe).
        return Ok(());
    }

    let mut total_auto = 0usize;
    let mut total_manual = 0usize;
    for (_c, plan) in &plans {
        if plan.actions.is_empty() {
            println!(
                "\n{}  [CONFORMANT]  profile={}  — nothing to remediate.",
                plan.computer, plan.profile_key
            );
            continue;
        }
        println!(
            "\n{}  [NON-CONFORMANT]  profile={}",
            plan.computer, plan.profile_key
        );
        for a in &plan.actions {
            total_auto += usize::from(a.class == conformance::RemediationClass::AutoSafe);
            total_manual += usize::from(a.class == conformance::RemediationClass::Manual);
            println!(
                "  [{}] {:<12} ({:<8}) {}",
                a.class.as_str(),
                a.check_key,
                a.severity,
                a.reason
            );
            if let Some(cmd) = &a.command {
                println!("        cmd: {cmd}");
            }
            println!("        → {}", a.guidance);
        }
    }

    println!(
        "\nPlan: {total_auto} auto-safe action(s), {total_manual} manual (operator-reviewed) action(s)."
    );

    if !apply {
        if total_auto > 0 {
            println!(
                "Dry-run. Re-run with --apply to actuate the auto-safe action(s); manual ones are never auto-run."
            );
        }
        return Ok(());
    }

    // --apply: actuate ONLY auto-safe actions.
    if total_auto == 0 {
        println!("--apply: no auto-safe actions to run.");
        return Ok(());
    }
    println!("\n--apply: actuating {total_auto} auto-safe action(s)...");
    let mut applied_ok = 0usize;
    for (c, plan) in &plans {
        for a in plan.auto_safe() {
            let Some(cmd) = &a.command else { continue };
            print!("  {} :: {cmd} ... ", c.name);
            let (ok, out) = conformance::apply_remote_command(c, cmd, &me).await;
            if ok {
                applied_ok += 1;
                println!("OK");
            } else {
                println!("FAILED");
                if !out.trim().is_empty() {
                    println!("      {}", out.trim());
                }
            }
        }
    }
    println!(
        "\nApplied {applied_ok}/{total_auto} auto-safe action(s). Restart forgefleetd on changed \
         hosts (new groups apply on process re-exec), then `ff conformance check` to confirm."
    );
    Ok(())
}

fn print_remediation_json(plans: &[(conformance::ComputerRow, conformance::RemediationPlan)]) {
    let arr: Vec<serde_json::Value> = plans
        .iter()
        .map(|(_c, p)| {
            serde_json::json!({
                "computer": p.computer,
                "ssh_user": p.ssh_user,
                "profile_key": p.profile_key,
                "conformant": p.actions.is_empty(),
                "actions": p.actions.iter().map(|a| serde_json::json!({
                    "check_key": a.check_key,
                    "check_kind": a.check_kind,
                    "severity": a.severity,
                    "reason": a.reason,
                    "class": a.class.as_str(),
                    "command": a.command,
                    "guidance": a.guidance,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!(arr)).unwrap_or_default()
    );
}

/// `ff conformance report [--role R]` — latest recorded results per host.
pub async fn handle_report(pg: &PgPool, role: &str) -> Result<()> {
    // conformance_results is keyed UNIQUE(computer_id, profile_id, check_key)
    // and upserted latest-wins, so there is already exactly one row per host
    // per gate — a plain join is correct.
    let rows = sqlx::query(
        r#"
        SELECT c.name AS computer, c.primary_ip AS primary_ip,
               p.profile_key AS profile_key,
               r.check_key, r.check_kind, r.conformant, r.severity,
               r.reason, r.checked_by, r.checked_at
        FROM conformance_results r
        JOIN computers c            ON c.id = r.computer_id
        JOIN conformance_profiles p ON p.id = r.profile_id
        WHERE p.role = $1
        ORDER BY c.primary_ip, r.check_key
        "#,
    )
    .bind(role)
    .fetch_all(pg)
    .await?;

    if rows.is_empty() {
        println!(
            "No recorded conformance results for role '{role}'. Run `ff conformance check` first \
             (or enable the tick via `ff secrets set conformance_mode dry-run`)."
        );
        return Ok(());
    }

    let mut current = String::new();
    let mut host_blockers = 0usize;
    for row in &rows {
        let computer: String = row.get("computer");
        if computer != current {
            if !current.is_empty() {
                println!(
                    "  → {}",
                    if host_blockers == 0 {
                        "CONFORMANT".to_string()
                    } else {
                        format!("NON-CONFORMANT ({host_blockers} blocker failure(s))")
                    }
                );
            }
            current = computer.clone();
            host_blockers = 0;
            let pkey: String = row.get("profile_key");
            println!("\n{computer}  profile={pkey}");
        }
        let ck: String = row.get("check_key");
        let sev: String = row.get("severity");
        let conformant: bool = row.get("conformant");
        let reason: String = row.try_get("reason").unwrap_or_default();
        if !conformant && sev == "blocker" {
            host_blockers += 1;
        }
        let mark = if conformant { "ok  " } else { "FAIL" };
        println!("  [{}] {:<12} ({:<8}) {}", mark, ck, sev, reason);
    }
    if !current.is_empty() {
        println!(
            "  → {}",
            if host_blockers == 0 {
                "CONFORMANT".to_string()
            } else {
                format!("NON-CONFORMANT ({host_blockers} blocker failure(s))")
            }
        );
    }
    Ok(())
}
