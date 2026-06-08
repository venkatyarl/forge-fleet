//! Fleet conformance — desired-state profiles + a VERIFY GATE that actually
//! runs (not "version string parsed = ok").
//!
//! BUILD #9 conformance + #10 AMD ROCm-bind, increment 1.
//!
//! ## Root gap this closes
//! Detection used to declare a host healthy the moment a `--version` string
//! parsed. Two AMD Strix Halo boxes presented the SAME "green pip but the GPU
//! never binds" symptom with DIFFERENT real causes, both found live:
//!   - **logan**: `torch 2.10.0+cu128` — a CUDA wheel — on an AMD box, so
//!     `torch.cuda.is_available()` is False forever (wrong backend).
//!   - **veronica**: the daemon user is NOT in the `render`/`video` groups, so
//!     it cannot open `/dev/kfd`; `rocminfo` as that user enumerates ZERO gpus
//!     (as root: `gfx1151` fine).
//!
//! A version parse sees neither. This module runs the schema-V120 VERIFY GATES
//! against a host over SSH (read-only) and records a MEASURED `conformant` bool
//! plus the SPECIFIC reason into `conformance_results`.
//!
//! ## Gates (from `conformance_checks.check_kind`)
//! - `amd_arch`   — assert torch carries `+rocm` and NOT `+cu` (the logan case).
//! - `kfd_access` — assert the user is in render+video AND `/dev/kfd` is
//!                  readable (the veronica case).
//! - `gpu_bind`   — assert `+rocm` AND `torch.cuda.is_available()` AND a real
//!                  gfx tensor op succeeds. The real proof of a bound GPU.
//! - `pkg_version`— the legacy "is the version present" check. Kept so the gate
//!                  set is complete; explicitly NOT the whole story.
//!
//! ## Safety (three-mode gate, exactly like the autoscaler)
//! `fleet_secrets.conformance_mode` ∈ {off, dry-run, active}, read EVERY tick:
//! - `off`     (DEFAULT / missing): the tick does NOTHING.
//! - `dry-run`: RECORD per-host conformance, actuate NOTHING.
//! - `active`:  RECORD per-host conformance, actuate NOTHING (increment 1 has
//!              no remediation — the apply-reconciler is a deferred follow-up).
//!
//! So in increment 1 dry-run and active behave identically (record-only). The
//! distinction exists so the follow-up can light up remediation under `active`
//! without re-plumbing the gate.

use sqlx::{PgPool, Row};
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

/// `fleet_secrets` key holding the three-mode gate. Off / missing = no-op.
const CONFORMANCE_MODE_KEY: &str = "conformance_mode";
/// SSH connect timeout for a single read-only verify command.
const SSH_CONNECT_TIMEOUT_SECS: u32 = 10;
/// Captured stdout/stderr is trimmed to this many chars before storage.
const RAW_OUTPUT_CAP: usize = 1200;

/// The gate's three modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceMode {
    Off,
    DryRun,
    Active,
}

impl ConformanceMode {
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("active") => ConformanceMode::Active,
            Some("dry-run") | Some("dry_run") | Some("dryrun") => ConformanceMode::DryRun,
            // off, missing, empty, or any unrecognised value → safe default.
            _ => ConformanceMode::Off,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ConformanceMode::Off => "off",
            ConformanceMode::DryRun => "dry-run",
            ConformanceMode::Active => "active",
        }
    }
}

/// Read the gate from `fleet_secrets`. DEFAULTS TO OFF when the key is missing
/// or unparseable — so shipping this subsystem is harmless until an operator
/// opts in.
async fn read_mode(pg: &PgPool) -> ConformanceMode {
    match ff_db::pg_get_secret(pg, CONFORMANCE_MODE_KEY).await {
        Ok(v) => ConformanceMode::parse(v.as_deref()),
        Err(e) => {
            warn!(error = %e, "conformance: failed to read mode secret; treating as off");
            ConformanceMode::Off
        }
    }
}

/// A computer we will (or did) check, resolved from `computers`.
#[derive(Debug, Clone)]
pub struct ComputerRow {
    pub id: Uuid,
    pub name: String,
    pub primary_ip: String,
    pub ssh_user: String,
    pub ssh_port: i32,
    pub os_family: String,
    pub gpu_kind: Option<String>,
    pub gpu_model: Option<String>,
    pub status: String,
}

/// A profile matched for a host, with its verify gates.
#[derive(Debug, Clone)]
pub struct ProfileWithChecks {
    pub profile_id: Uuid,
    pub profile_key: String,
    pub role: String,
    pub checks: Vec<CheckRow>,
}

#[derive(Debug, Clone)]
pub struct CheckRow {
    pub check_key: String,
    pub check_kind: String,
    pub title: String,
    pub verify_cmd: String,
    pub severity: String,
}

/// The recorded outcome of running one check against one host.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub check_key: String,
    pub check_kind: String,
    pub severity: String,
    pub conformant: bool,
    pub reason: String,
    pub raw_output: String,
}

/// Result of checking a single host against its matched profile.
#[derive(Debug, Clone)]
pub struct HostReport {
    pub computer: String,
    pub profile_key: String,
    pub role: String,
    pub outcomes: Vec<CheckOutcome>,
}

impl HostReport {
    /// A host is conformant for the role iff no `blocker`-severity check failed.
    pub fn conformant(&self) -> bool {
        !self
            .outcomes
            .iter()
            .any(|o| !o.conformant && o.severity == "blocker")
    }

    /// The specific blocker reasons (what a version parse would have missed).
    pub fn blocker_reasons(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .filter(|o| !o.conformant && o.severity == "blocker")
            .map(|o| format!("{}: {}", o.check_key, o.reason))
            .collect()
    }
}

/// Look up a computer by name (case-insensitive).
pub async fn get_computer(pg: &PgPool, name: &str) -> anyhow::Result<Option<ComputerRow>> {
    let row = sqlx::query(
        r#"
        SELECT id, name, primary_ip, ssh_user, ssh_port,
               os_family, gpu_kind, gpu_model, status
        FROM computers
        WHERE lower(name) = lower($1)
        "#,
    )
    .bind(name)
    .fetch_optional(pg)
    .await?;

    Ok(row.map(|r| ComputerRow {
        id: r.get("id"),
        name: r.get("name"),
        primary_ip: r.get("primary_ip"),
        ssh_user: r.get("ssh_user"),
        ssh_port: r.get("ssh_port"),
        os_family: r.get("os_family"),
        gpu_kind: r.get("gpu_kind"),
        gpu_model: r.get("gpu_model"),
        status: r.get("status"),
    }))
}

/// Classify a host into a `hardware_class` used to match a profile.
///
/// Increment-1 scope: we only need the AMD Strix Halo class. The model string
/// (gfx1151 / "Strix" / "Ryzen AI Max") OR `gpu_kind = amd_rocm` identifies it.
/// Everything else is `generic` (no AMD-training profile matches → skipped).
pub fn classify_hardware(c: &ComputerRow) -> String {
    let gpu_kind = c.gpu_kind.as_deref().unwrap_or("").to_ascii_lowercase();
    let gpu_model = c.gpu_model.as_deref().unwrap_or("").to_ascii_lowercase();
    let is_amd = gpu_kind.contains("amd") || gpu_kind.contains("rocm");
    let looks_strix = gpu_model.contains("gfx1151")
        || gpu_model.contains("strix")
        || gpu_model.contains("ryzen ai max")
        || gpu_model.contains("radeon 8");
    if is_amd && (looks_strix || gpu_model.contains("gfx")) {
        return "strix-halo".to_string();
    }
    if is_amd {
        // AMD GPU we can't pin to Strix Halo — still AMD, default to strix-halo
        // for the amd-training role so the arch/kfd/bind gates still run (they
        // are the whole point). A wrong hardware_class only changes which
        // package versions we'd demand, not whether the GPU binds.
        return "strix-halo".to_string();
    }
    "generic".to_string()
}

/// Load the profile (+ its enabled checks) that matches a host's
/// (os_family, hardware_class) for the given `role`. Returns None when no
/// profile is enabled for that key (host is simply not in scope for the role).
pub async fn match_profile(
    pg: &PgPool,
    os_family: &str,
    hardware_class: &str,
    role: &str,
) -> anyhow::Result<Option<ProfileWithChecks>> {
    let prof = sqlx::query(
        r#"
        SELECT id, profile_key, role
        FROM conformance_profiles
        WHERE enabled
          AND os_family = $1
          AND hardware_class = $2
          AND role = $3
        LIMIT 1
        "#,
    )
    .bind(os_family)
    .bind(hardware_class)
    .bind(role)
    .fetch_optional(pg)
    .await?;

    let Some(prof) = prof else {
        return Ok(None);
    };
    let profile_id: Uuid = prof.get("id");
    let profile_key: String = prof.get("profile_key");
    let role: String = prof.get("role");

    let check_rows = sqlx::query(
        r#"
        SELECT check_key, check_kind, title, verify_cmd, severity
        FROM conformance_checks
        WHERE profile_id = $1 AND enabled
        ORDER BY
          CASE check_kind
            WHEN 'amd_arch'   THEN 0
            WHEN 'kfd_access' THEN 1
            WHEN 'gpu_bind'   THEN 2
            ELSE 3
          END,
          check_key
        "#,
    )
    .bind(profile_id)
    .fetch_all(pg)
    .await?;

    let checks = check_rows
        .into_iter()
        .map(|r| CheckRow {
            check_key: r.get("check_key"),
            check_kind: r.get("check_kind"),
            title: r.get("title"),
            verify_cmd: r.get("verify_cmd"),
            severity: r.get("severity"),
        })
        .collect();

    Ok(Some(ProfileWithChecks {
        profile_id,
        profile_key,
        role,
        checks,
    }))
}

/// Run one verify command against a host over SSH (read-only) and classify the
/// outcome. Local execution when the target is this node (avoids SSH-to-self).
async fn run_check(c: &ComputerRow, check: &CheckRow, me: &str) -> CheckOutcome {
    let is_me = me.eq_ignore_ascii_case(&c.name);
    let cmd = check.verify_cmd.clone();

    let output = tokio::task::spawn_blocking({
        let target = format!("{}@{}", c.ssh_user, c.primary_ip);
        let port = c.ssh_port.to_string();
        move || -> std::io::Result<std::process::Output> {
            use std::process::Command;
            if is_me {
                Command::new("sh").args(["-c", &cmd]).output()
            } else {
                Command::new("ssh")
                    .args([
                        "-o",
                        "BatchMode=yes",
                        "-o",
                        &format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"),
                        "-p",
                        &port,
                        &target,
                        &cmd,
                    ])
                    .output()
            }
        }
    })
    .await;

    let (conformant, reason, raw) = match output {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout}{stderr}");
            let ok = out.status.success();
            let reason = classify_reason(&check.check_kind, ok, &stdout, &stderr);
            (ok, reason, trim_raw(&combined))
        }
        Ok(Err(e)) => (
            false,
            format!("verify command could not be executed: {e}"),
            trim_raw(&e.to_string()),
        ),
        Err(e) => (
            false,
            format!("verify task join failed: {e}"),
            String::new(),
        ),
    };

    CheckOutcome {
        check_key: check.check_key.clone(),
        check_kind: check.check_kind.clone(),
        severity: check.severity.clone(),
        conformant,
        reason,
        raw_output: raw,
    }
}

/// Derive a SPECIFIC human+machine readable reason from a check's output. The
/// verify commands print a `NONCONFORMANT: <cause>` line on the failing path;
/// when present we surface that verbatim (it carries the exact cause — the
/// `+cu128 wheel` for logan, the missing group / unreadable kfd for veronica).
fn classify_reason(check_kind: &str, ok: bool, stdout: &str, stderr: &str) -> String {
    if ok {
        // On success surface the positive marker line if the gate printed one.
        let marker = stdout
            .lines()
            .find(|l| l.contains("BIND_OK"))
            .map(str::trim);
        return match (check_kind, marker) {
            ("gpu_bind", Some(m)) => format!("conformant ({m})"),
            _ => "conformant".to_string(),
        };
    }
    // Failing path: prefer the explicit NONCONFORMANT line emitted by the gate.
    let explicit = stdout
        .lines()
        .chain(stderr.lines())
        .find(|l| l.contains("NONCONFORMANT:"))
        .and_then(|l| l.split("NONCONFORMANT:").nth(1))
        .map(|s| s.trim().to_string());
    if let Some(r) = explicit {
        return r;
    }
    // No explicit marker (e.g. a python AssertionError) — surface the last
    // non-empty stderr line, which carries the assert message.
    let assert_line = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string);
    match assert_line {
        Some(l) => l,
        None => format!("{check_kind} check failed (exit non-zero, no diagnostic output)"),
    }
}

fn trim_raw(s: &str) -> String {
    let t = s.trim();
    if t.len() <= RAW_OUTPUT_CAP {
        t.to_string()
    } else {
        t.chars().take(RAW_OUTPUT_CAP).collect::<String>() + "…"
    }
}

/// Persist one host's outcomes into `conformance_results` (latest-wins upsert).
async fn record_outcomes(
    pg: &PgPool,
    computer_id: Uuid,
    profile_id: Uuid,
    checked_by: &str,
    outcomes: &[CheckOutcome],
) -> anyhow::Result<()> {
    for o in outcomes {
        sqlx::query(
            r#"
            INSERT INTO conformance_results
                (computer_id, profile_id, check_key, check_kind, conformant,
                 severity, reason, raw_output, checked_by, checked_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
            ON CONFLICT (computer_id, profile_id, check_key) DO UPDATE SET
                check_kind = EXCLUDED.check_kind,
                conformant = EXCLUDED.conformant,
                severity   = EXCLUDED.severity,
                reason     = EXCLUDED.reason,
                raw_output = EXCLUDED.raw_output,
                checked_by = EXCLUDED.checked_by,
                checked_at = NOW()
            "#,
        )
        .bind(computer_id)
        .bind(profile_id)
        .bind(&o.check_key)
        .bind(&o.check_kind)
        .bind(o.conformant)
        .bind(&o.severity)
        .bind(&o.reason)
        .bind(&o.raw_output)
        .bind(checked_by)
        .execute(pg)
        .await?;
    }
    Ok(())
}

/// Check ONE host against the `role` profile that matches its
/// (os_family, hardware_class). Runs the verify gates over SSH (read-only),
/// records results when `record=true`, and returns the report.
///
/// Returns `Ok(None)` when no profile matches the host (out of scope).
pub async fn check_host(
    pg: &PgPool,
    computer: &ComputerRow,
    role: &str,
    checked_by: &str,
    record: bool,
) -> anyhow::Result<Option<HostReport>> {
    let hw_class = classify_hardware(computer);
    let Some(profile) = match_profile(pg, &computer.os_family, &hw_class, role).await? else {
        return Ok(None);
    };

    let me = ff_agent_self_name().await;
    let mut outcomes = Vec::with_capacity(profile.checks.len());
    for check in &profile.checks {
        outcomes.push(run_check(computer, check, &me).await);
    }

    if record {
        record_outcomes(pg, computer.id, profile.profile_id, checked_by, &outcomes).await?;
    }

    Ok(Some(HostReport {
        computer: computer.name.clone(),
        profile_key: profile.profile_key,
        role: profile.role,
        outcomes,
    }))
}

/// Resolve this worker's name (for ssh-to-self detection / checked_by).
async fn ff_agent_self_name() -> String {
    crate::fleet_info::resolve_this_worker_name().await
}

/// List all online computers that match ANY enabled profile for `role`. Used
/// by the tick and `ff conformance check` (no --host).
pub async fn hosts_in_scope(pg: &PgPool, role: &str) -> anyhow::Result<Vec<ComputerRow>> {
    let rows = sqlx::query(
        r#"
        SELECT id, name, primary_ip, ssh_user, ssh_port,
               os_family, gpu_kind, gpu_model, status
        FROM computers
        WHERE status IN ('online','maintenance')
        ORDER BY primary_ip
        "#,
    )
    .fetch_all(pg)
    .await?;

    let mut out = Vec::new();
    for r in rows {
        let c = ComputerRow {
            id: r.get("id"),
            name: r.get("name"),
            primary_ip: r.get("primary_ip"),
            ssh_user: r.get("ssh_user"),
            ssh_port: r.get("ssh_port"),
            os_family: r.get("os_family"),
            gpu_kind: r.get("gpu_kind"),
            gpu_model: r.get("gpu_model"),
            status: r.get("status"),
        };
        let hw = classify_hardware(&c);
        if match_profile(pg, &c.os_family, &hw, role).await?.is_some() {
            out.push(c);
        }
    }
    Ok(out)
}

/// One conformance pass: for every in-scope host of the `amd-training` role,
/// run the verify gates and record results. `mode` decides side effects:
/// - Off: never called (the tick returns early).
/// - DryRun / Active: RECORD results; actuate nothing (increment 1).
async fn conformance_pass(
    pg: &PgPool,
    mode: ConformanceMode,
    checked_by: &str,
) -> anyhow::Result<usize> {
    let role = "amd-training";
    let hosts = hosts_in_scope(pg, role).await?;
    let record = matches!(mode, ConformanceMode::DryRun | ConformanceMode::Active);
    let mut nonconformant = 0usize;

    for host in &hosts {
        match check_host(pg, host, role, checked_by, record).await {
            Ok(Some(report)) => {
                if !report.conformant() {
                    nonconformant += 1;
                    warn!(
                        host = %report.computer,
                        profile = %report.profile_key,
                        reasons = ?report.blocker_reasons(),
                        "conformance: host NON-CONFORMANT for amd-training"
                    );
                } else {
                    info!(host = %report.computer, profile = %report.profile_key, "conformance: host conformant");
                }
            }
            Ok(None) => {}
            Err(e) => warn!(host = %host.name, error = %e, "conformance: check failed"),
        }
    }
    Ok(nonconformant)
}

/// Leader-gated conformance tick. Mirrors the autoscaler: skip the immediate
/// fire, run only on the elected leader, read the three-mode gate EVERY tick,
/// and no-op entirely when the gate is `off` (default). dry-run/active record
/// per-host conformance; NEITHER remediates in increment 1.
pub fn spawn_conformance_tick(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the immediate fire so pulse/election settle first.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE member_name = $1
                              AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                        )
                        "#,
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }

                    let mode = read_mode(&pg).await;
                    if mode == ConformanceMode::Off {
                        // DEFAULT: the tick does NOTHING. Shipping is harmless.
                        continue;
                    }

                    match conformance_pass(&pg, mode, &worker_name).await {
                        Ok(nonconformant) => info!(
                            mode = mode.as_str(),
                            nonconformant,
                            "conformance pass (record-only; no remediation in increment 1)"
                        ),
                        Err(e) => warn!(error = %e, "conformance tick failed"),
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("conformance tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(gpu_kind: Option<&str>, gpu_model: Option<&str>) -> ComputerRow {
        ComputerRow {
            id: Uuid::nil(),
            name: "x".into(),
            primary_ip: "10.0.0.1".into(),
            ssh_user: "u".into(),
            ssh_port: 22,
            os_family: "linux-ubuntu".into(),
            gpu_kind: gpu_kind.map(str::to_string),
            gpu_model: gpu_model.map(str::to_string),
            status: "online".into(),
        }
    }

    #[test]
    fn mode_parses_safely() {
        assert_eq!(ConformanceMode::parse(None), ConformanceMode::Off);
        assert_eq!(ConformanceMode::parse(Some("")), ConformanceMode::Off);
        assert_eq!(
            ConformanceMode::parse(Some("nonsense")),
            ConformanceMode::Off
        );
        assert_eq!(
            ConformanceMode::parse(Some("dry-run")),
            ConformanceMode::DryRun
        );
        assert_eq!(
            ConformanceMode::parse(Some("dry_run")),
            ConformanceMode::DryRun
        );
        assert_eq!(
            ConformanceMode::parse(Some("ACTIVE")),
            ConformanceMode::Active
        );
    }

    #[test]
    fn strix_halo_classified_from_gpu_kind_or_model() {
        assert_eq!(
            classify_hardware(&comp(Some("amd_rocm"), Some("gfx1151"))),
            "strix-halo"
        );
        assert_eq!(
            classify_hardware(&comp(Some("amd_rocm"), None)),
            "strix-halo"
        );
        assert_eq!(
            classify_hardware(&comp(Some("amd"), Some("Radeon 8060S"))),
            "strix-halo"
        );
        assert_eq!(classify_hardware(&comp(None, None)), "generic");
        assert_eq!(
            classify_hardware(&comp(Some("nvidia_cuda"), Some("RTX 4090"))),
            "generic"
        );
    }

    #[test]
    fn classify_reason_surfaces_explicit_nonconformant_line() {
        // logan case — the +cu wheel.
        let r = classify_reason(
            "amd_arch",
            false,
            "NONCONFORMANT: torch=2.10.0+cu128 is not a +rocm wheel\n",
            "",
        );
        assert_eq!(r, "torch=2.10.0+cu128 is not a +rocm wheel");

        // veronica case — missing group.
        let r = classify_reason(
            "kfd_access",
            false,
            "NONCONFORMANT: user not in group render (groups: u sudo)\n",
            "",
        );
        assert_eq!(r, "user not in group render (groups: u sudo)");
    }

    #[test]
    fn classify_reason_falls_back_to_assert_message() {
        let r = classify_reason(
            "gpu_bind",
            false,
            "",
            "AssertionError: torch.cuda.is_available() is False (GPU never bound)\n",
        );
        assert!(r.contains("torch.cuda.is_available() is False"));
    }

    #[test]
    fn host_report_conformance_ignores_warn_failures() {
        let report = HostReport {
            computer: "logan".into(),
            profile_key: "linux-ubuntu/strix-halo/amd-training".into(),
            role: "amd-training".into(),
            outcomes: vec![
                CheckOutcome {
                    check_key: "amd_arch".into(),
                    check_kind: "amd_arch".into(),
                    severity: "blocker".into(),
                    conformant: false,
                    reason: "torch=2.10.0+cu128 is not a +rocm wheel".into(),
                    raw_output: String::new(),
                },
                CheckOutcome {
                    check_key: "rocm_present".into(),
                    check_kind: "pkg_version".into(),
                    severity: "warn".into(),
                    conformant: false,
                    reason: "no HIP version".into(),
                    raw_output: String::new(),
                },
            ],
        };
        assert!(!report.conformant());
        assert_eq!(report.blocker_reasons().len(), 1);
        assert!(report.blocker_reasons()[0].contains("+cu128"));
    }
}
