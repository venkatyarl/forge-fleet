//! `ff ports` subcommand implementations.

use anyhow::Result;

use crate::{CYAN, GREEN, RED, RESET, YELLOW, truncate_for_col};

pub async fn handle_ports_list(
    pool: &sqlx::PgPool,
    kind: Option<String>,
    scope: Option<String>,
    json: bool,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT port, service, kind, description, exposed_on, scope, managed_by, status
         FROM port_registry
         WHERE 1=1",
    );
    let mut idx = 1;
    if kind.is_some() {
        sql.push_str(&format!(" AND kind = ${idx}"));
        idx += 1;
    }
    if scope.is_some() {
        sql.push_str(&format!(" AND scope = ${idx}"));
    }
    sql.push_str(" ORDER BY kind ASC, port ASC");

    let mut q = sqlx::query(&sql);
    if let Some(k) = &kind {
        q = q.bind(k);
    }
    if let Some(s) = &scope {
        q = q.bind(s);
    }

    let rows = q
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list port_registry: {e}"))?;

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "port":        sqlx::Row::get::<i32, _>(r, "port"),
                    "service":     sqlx::Row::get::<String, _>(r, "service"),
                    "kind":        sqlx::Row::get::<String, _>(r, "kind"),
                    "description": sqlx::Row::get::<String, _>(r, "description"),
                    "exposed_on":  sqlx::Row::get::<String, _>(r, "exposed_on"),
                    "scope":       sqlx::Row::get::<String, _>(r, "scope"),
                    "managed_by":  sqlx::Row::get::<Option<String>, _>(r, "managed_by"),
                    "status":      sqlx::Row::get::<String, _>(r, "status"),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    if rows.is_empty() {
        println!("{YELLOW}No rows in port_registry. Run `ff ports seed` first.{RESET}");
        return Ok(());
    }

    println!(
        "{:<6} {:<22} {:<18} {:<26} {:<10} {:<11} DESCRIPTION",
        "PORT", "SERVICE", "KIND", "EXPOSED_ON", "SCOPE", "STATUS",
    );
    println!("  {}", "-".repeat(130));

    let mut last_kind: Option<String> = None;
    for r in &rows {
        let port: i32 = sqlx::Row::get(r, "port");
        let service: String = sqlx::Row::get(r, "service");
        let k: String = sqlx::Row::get(r, "kind");
        let description: String = sqlx::Row::get(r, "description");
        let exposed_on: String = sqlx::Row::get(r, "exposed_on");
        let scp: String = sqlx::Row::get(r, "scope");
        let status: String = sqlx::Row::get(r, "status");

        if last_kind.as_deref() != Some(k.as_str()) {
            println!("\n{CYAN}── {k} ──{RESET}");
            last_kind = Some(k.clone());
        }

        let status_color = match status.as_str() {
            "active" => GREEN,
            "deprecated" => RED,
            "planned" => YELLOW,
            _ => "",
        };

        println!(
            "{:<6} {:<22} {:<18} {:<26} {:<10} {status_color}{:<11}{RESET} {}",
            port,
            truncate_for_col(&service, 22),
            truncate_for_col(&k, 18),
            truncate_for_col(&exposed_on, 26),
            truncate_for_col(&scp, 10),
            status,
            description,
        );
    }
    println!("\n{} port(s) registered.", rows.len());
    Ok(())
}

pub async fn handle_ports_scan(pool: &sqlx::PgPool, computer: &str) -> Result<()> {
    use tokio::process::Command as TokCmd;

    println!("{CYAN}▶ Scanning {computer} for listening ports{RESET}");

    let this_hostname = tokio::process::Command::new("hostname")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    let is_local = this_hostname.starts_with(&computer.to_lowercase())
        || computer.eq_ignore_ascii_case("localhost")
        || computer.eq_ignore_ascii_case("local");

    let probe_cmd = "sh -c 'ss -tlnH 2>/dev/null || lsof -iTCP -sTCP:LISTEN -n -P 2>/dev/null'";

    let output = if is_local {
        TokCmd::new("sh").args(["-c", probe_cmd]).output().await
    } else {
        let row = sqlx::query("SELECT ssh_user, ip FROM fleet_nodes WHERE name = $1 LIMIT 1")
            .bind(computer)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("lookup fleet_nodes: {e}"))?;

        let (ssh_user, ip) = match row {
            Some(r) => (
                sqlx::Row::get::<String, _>(&r, "ssh_user"),
                sqlx::Row::get::<String, _>(&r, "ip"),
            ),
            None => {
                println!("{RED}✗ Unknown computer '{computer}' — not in fleet_nodes.{RESET}");
                return Ok(());
            }
        };

        let dest = format!("{ssh_user}@{ip}");
        TokCmd::new("ssh")
            .args([
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "BatchMode=yes",
                &dest,
                probe_cmd,
            ])
            .output()
            .await
    };

    let probe_stdout = match output {
        Ok(o) if o.status.success() || !o.stdout.is_empty() => {
            String::from_utf8_lossy(&o.stdout).to_string()
        }
        Ok(o) => {
            println!(
                "{RED}✗ probe exited {}:{RESET}\n{}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr),
            );
            return Ok(());
        }
        Err(e) => {
            println!("{RED}✗ probe failed: {e}{RESET}");
            return Ok(());
        }
    };

    let mut listening: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    for line in probe_stdout.lines() {
        for tok in line.split_whitespace() {
            if let Some(colon) = tok.rfind(':') {
                let tail = &tok[colon + 1..];
                let num_end = tail
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(tail.len());
                if num_end == 0 {
                    continue;
                }
                if let Ok(p) = tail[..num_end].parse::<u16>() {
                    listening.insert(p);
                }
            }
        }
    }

    let reg_rows = sqlx::query(
        "SELECT port, service, kind, exposed_on, status FROM port_registry ORDER BY port ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("load port_registry: {e}"))?;

    let mut registered: std::collections::BTreeMap<u16, (String, String, String, String)> =
        std::collections::BTreeMap::new();
    for r in &reg_rows {
        let port: i32 = sqlx::Row::get(r, "port");
        let service: String = sqlx::Row::get(r, "service");
        let kind: String = sqlx::Row::get(r, "kind");
        let exposed_on: String = sqlx::Row::get(r, "exposed_on");
        let status: String = sqlx::Row::get(r, "status");
        registered.insert(port as u16, (service, kind, exposed_on, status));
    }

    println!(
        "\n{CYAN}Listening on {computer}:{RESET} {} port(s)",
        listening.len()
    );
    let mut unexpected: Vec<u16> = Vec::new();
    for p in &listening {
        match registered.get(p) {
            Some((svc, kind, _exposed, status)) => {
                let color = if status == "deprecated" { RED } else { GREEN };
                println!("  {color}✓ {:<6}{RESET} {svc}  ({kind}, {status})", p);
            }
            None => unexpected.push(*p),
        }
    }
    if !unexpected.is_empty() {
        println!("\n{YELLOW}⚠ Unexpected listeners (not in port_registry):{RESET}");
        for p in unexpected {
            println!("  {YELLOW}? {}{RESET}", p);
        }
    }

    let mut missing: Vec<(u16, String, String)> = Vec::new();
    let key = computer.to_ascii_lowercase();
    for (port, (svc, kind, exposed_on, status)) in &registered {
        if status != "active" {
            continue;
        }
        let eo = exposed_on.to_ascii_lowercase();
        let relevant = eo.contains(&key)
            || eo == "all_members"
            || eo == "all_members_with_gguf"
            || eo == "nats_cluster_members"
            || eo == "gpu_members";
        if !relevant {
            continue;
        }
        if !listening.contains(port) {
            missing.push((*port, svc.clone(), kind.clone()));
        }
    }
    if !missing.is_empty() {
        println!("\n{YELLOW}⚠ Expected but not listening:{RESET}");
        for (port, svc, kind) in missing {
            println!("  {YELLOW}∅ {:<6}{RESET} {svc}  ({kind})", port);
        }
    } else {
        println!(
            "\n{GREEN}✓ Every active port_registry entry relevant to {computer} is listening.{RESET}"
        );
    }

    Ok(())
}
