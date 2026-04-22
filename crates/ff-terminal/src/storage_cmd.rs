//! `ff storage share --from <host> --path <p> --to <peer> --via cx7 --name <n>`
//!
//! Wraps the tonight's manual NFS-export-and-mount dance:
//!   1. Install nfs-kernel-server on <host>.
//!   2. Append an export line + reload.
//!   3. mkdir + mount on <peer>.
//!   4. Record in `shared_volumes`.

use anyhow::{bail, Context, Result};
use sqlx::{PgPool, Row};
use tokio::process::Command;
use uuid::Uuid;

pub async fn handle_storage_share(
    pg: &PgPool,
    from_host: &str,
    export_path: &str,
    to_peer: &str,
    via: &str,
    name: &str,
) -> Result<()> {
    let row_from = sqlx::query("SELECT id, ssh_user, primary_ip FROM computers WHERE name = $1")
        .bind(from_host)
        .fetch_optional(pg)
        .await?
        .with_context(|| format!("host '{}' not found", from_host))?;
    let row_to = sqlx::query("SELECT id, ssh_user, primary_ip FROM computers WHERE name = $1")
        .bind(to_peer)
        .fetch_optional(pg)
        .await?
        .with_context(|| format!("peer '{}' not found", to_peer))?;
    let from_id: Uuid = row_from.try_get("id")?;
    let from_user: String = row_from.try_get("ssh_user")?;
    let from_ip: String = row_from.try_get("primary_ip")?;
    let to_user: String = row_to.try_get("ssh_user")?;
    let to_ip: String = row_to.try_get("primary_ip")?;

    let (host_fabric_ip, _peer_fabric_ip) = if via == "cx7" {
        let pair_name = if from_host < to_peer {
            format!("{}-{}", from_host, to_peer)
        } else {
            format!("{}-{}", to_peer, from_host)
        };
        let pr = sqlx::query(
            "SELECT a_ip, b_ip, computer_a_id FROM fabric_pairs WHERE pair_name = $1",
        )
        .bind(&pair_name)
        .fetch_optional(pg)
        .await?
        .with_context(|| {
            format!(
                "fabric pair '{}' not found; run `ff fabric pair {} {}` first",
                pair_name, from_host, to_peer
            )
        })?;
        let a_ip: String = pr.try_get("a_ip")?;
        let b_ip: String = pr.try_get("b_ip")?;
        let a_id: Uuid = pr.try_get("computer_a_id")?;
        if a_id == from_id {
            (a_ip, b_ip)
        } else {
            (b_ip, a_ip)
        }
    } else {
        (from_ip.clone(), to_ip.clone())
    };

    if host_fabric_ip.is_empty() {
        bail!("fabric pair has no IPs yet; wait for beats to populate");
    }

    println!("[1/4] Installing nfs-kernel-server on {}...", from_host);
    run_ssh(&from_user, &from_ip, "sudo apt-get install -y nfs-kernel-server").await?;

    println!("[2/4] Exporting {} from {}...", export_path, from_host);
    let subnet = host_fabric_ip
        .rsplit_once('.')
        .map(|(p, _)| format!("{}.0/24", p))
        .unwrap_or_else(|| format!("{}/32", host_fabric_ip));
    let export_line = format!(
        "{} {}(ro,sync,no_subtree_check,no_root_squash)",
        export_path, subnet
    );
    let cmd = format!(
        "echo '{}' | sudo tee -a /etc/exports && sudo exportfs -ra && sudo systemctl enable --now nfs-kernel-server",
        export_line
    );
    run_ssh(&from_user, &from_ip, &cmd).await?;

    println!("[3/4] Mounting on {}...", to_peer);
    let mount_cmd = format!(
        "sudo mkdir -p {p} && sudo mount -t nfs -o ro,vers=4,rsize=1048576,wsize=1048576 {ip}:{p} {p}",
        p = export_path,
        ip = host_fabric_ip
    );
    run_ssh(&to_user, &to_ip, &mount_cmd).await?;

    println!("[4/4] Recording shared_volumes entry...");
    sqlx::query(
        "INSERT INTO shared_volumes (name, host_computer_id, export_path, protocol, mounted_on) \
         VALUES ($1, $2, $3, 'nfs4', $4::jsonb) \
         ON CONFLICT (name) DO UPDATE SET export_path = EXCLUDED.export_path",
    )
    .bind(name)
    .bind(from_id)
    .bind(export_path)
    .bind(serde_json::json!([{"computer": to_peer, "mount_path": export_path}]))
    .execute(pg)
    .await?;

    println!(
        "Done: {} is exporting {} to {} via {} fabric",
        from_host, export_path, to_peer, via
    );
    Ok(())
}

async fn run_ssh(user: &str, ip: &str, cmd: &str) -> Result<()> {
    let status = Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ])
        .arg(format!("{}@{}", user, ip))
        .arg(cmd)
        .status()
        .await?;
    if !status.success() {
        bail!("ssh cmd failed on {}@{}", user, ip);
    }
    Ok(())
}
