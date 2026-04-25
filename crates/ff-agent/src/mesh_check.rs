//! Fleet-wide SSH mesh verification + propagation.
//! See plan: /Users/venkat/.claude/plans/gentle-questing-valley.md §3h.

use std::collections::HashSet;
use std::time::Duration;

use sqlx::PgPool;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct MeshCell {
    pub src: String,
    pub dst: String,
    pub status: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MeshMatrix {
    pub cells: Vec<MeshCell>,
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

pub async fn pairwise_ssh_check(pool: &PgPool) -> Result<MeshMatrix, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    pairwise_ssh_check_inner(pool, &nodes, None).await
}

pub async fn pairwise_ssh_check_node(pool: &PgPool, node: &str) -> Result<MeshMatrix, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    pairwise_ssh_check_inner(pool, &nodes, Some(node)).await
}

async fn pairwise_ssh_check_inner(
    pool: &PgPool,
    nodes: &[ff_db::FleetNodeRow],
    only_node: Option<&str>,
) -> Result<MeshMatrix, String> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let by_name: std::collections::HashMap<String, (String, String)> = nodes
        .iter()
        .map(|n| (n.name.clone(), (n.ssh_user.clone(), n.ip.clone())))
        .collect();

    let mut pairs: Vec<(String, String, String, String, String)> = Vec::new();
    for src in nodes {
        for dst in nodes {
            if src.name == dst.name {
                continue;
            }
            if let Some(n) = only_node {
                if src.name != n && dst.name != n {
                    continue;
                }
            }
            pairs.push((
                src.name.clone(),
                src.ssh_user.clone(),
                src.ip.clone(),
                dst.name.clone(),
                by_name
                    .get(&dst.name)
                    .map(|(u, _)| u.clone())
                    .unwrap_or_default(),
            ));
            let _ = dst;
        }
    }
    let _ = by_name;

    let mut futs = FuturesUnordered::new();
    let mut cells = Vec::with_capacity(pairs.len());
    for (src, src_user, src_ip, dst, dst_user) in pairs {
        let dst_ip = nodes
            .iter()
            .find(|n| n.name == dst)
            .map(|n| n.ip.clone())
            .unwrap_or_default();
        futs.push(probe_pair(src, src_user, src_ip, dst, dst_user, dst_ip));
        if futs.len() >= 8 {
            if let Some(cell) = futs.next().await {
                let _ = ff_db::pg_upsert_mesh_status(
                    pool,
                    &cell.src,
                    &cell.dst,
                    &cell.status,
                    cell.last_error.as_deref(),
                )
                .await;
                cells.push(cell);
            }
        }
    }
    while let Some(cell) = futs.next().await {
        let _ = ff_db::pg_upsert_mesh_status(
            pool,
            &cell.src,
            &cell.dst,
            &cell.status,
            cell.last_error.as_deref(),
        )
        .await;
        cells.push(cell);
    }

    Ok(MeshMatrix {
        cells,
        checked_at: chrono::Utc::now(),
    })
}

async fn probe_pair(
    src: String,
    src_user: String,
    src_ip: String,
    dst: String,
    dst_user: String,
    dst_ip: String,
) -> MeshCell {
    let inner = format!(
        "ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
         {dst_user}@{dst_ip} true"
    );
    let result = timeout(
        Duration::from_secs(12),
        Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &format!("{src_user}@{src_ip}"),
                &inner,
            ])
            .output(),
    )
    .await;

    match result {
        Ok(Ok(out)) if out.status.success() => MeshCell {
            src,
            dst,
            status: "ok".into(),
            last_error: None,
        },
        Ok(Ok(out)) => MeshCell {
            src,
            dst,
            status: "failed".into(),
            last_error: Some(format!(
                "exit {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .chars()
                    .take(120)
                    .collect::<String>()
            )),
        },
        Ok(Err(e)) => MeshCell {
            src,
            dst,
            status: "failed".into(),
            last_error: Some(format!("spawn: {e}")),
        },
        Err(_) => MeshCell {
            src,
            dst,
            status: "failed".into(),
            last_error: Some("timeout".into()),
        },
    }
}

pub async fn mesh_propagate(
    pool: &PgPool,
    params: &serde_json::Value,
) -> Result<(usize, usize), String> {
    let new_node = params
        .get("new_node")
        .and_then(|v| v.as_str())
        .ok_or("missing new_node")?;
    let new_ip = params
        .get("new_node_ip")
        .and_then(|v| v.as_str())
        .ok_or("missing new_node_ip")?;
    let new_user = params
        .get("new_node_ssh_user")
        .and_then(|v| v.as_str())
        .ok_or("missing new_node_ssh_user")?;
    let user_key = params
        .get("user_public_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let host_keys: Vec<String> = params
        .get("host_public_keys")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let known_lines: Vec<String> = host_keys
        .iter()
        .filter(|k| !k.trim().is_empty())
        .map(|k| format!("{new_ip},{new_node} {k}"))
        .collect();

    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    let mut ok = 0usize;
    let mut fail = 0usize;
    for peer in &nodes {
        if peer.name == new_node {
            continue;
        }
        match propagate_to_peer(peer, user_key, &known_lines, new_user, new_ip).await {
            Ok(()) => {
                ok += 1;
                let _ = ff_db::pg_upsert_mesh_status(pool, &peer.name, new_node, "ok", None).await;
                let _ = ff_db::pg_upsert_mesh_status(pool, new_node, &peer.name, "ok", None).await;
            }
            Err(e) => {
                fail += 1;
                let _ =
                    ff_db::pg_upsert_mesh_status(pool, &peer.name, new_node, "failed", Some(&e))
                        .await;
            }
        }
    }
    Ok((ok, fail))
}

async fn propagate_to_peer(
    peer: &ff_db::FleetNodeRow,
    user_key: &str,
    known_lines: &[String],
    new_user: &str,
    new_ip: &str,
) -> Result<(), String> {
    let peer_dest = format!("{}@{}", peer.ssh_user, peer.ip);
    if !user_key.trim().is_empty() {
        let cmd = format!(
            "mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && \
             grep -Fq {quoted} ~/.ssh/authorized_keys || \
             echo {quoted} >> ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys",
            quoted = shell_escape_single(user_key),
        );
        ssh_exec(&peer_dest, &cmd).await?;
    }
    for line in known_lines {
        let cmd = format!(
            "touch ~/.ssh/known_hosts && \
             grep -Fq {quoted} ~/.ssh/known_hosts || \
             echo {quoted} >> ~/.ssh/known_hosts && \
             chmod 644 ~/.ssh/known_hosts",
            quoted = shell_escape_single(line),
        );
        ssh_exec(&peer_dest, &cmd).await?;
    }
    let probe = format!(
        "ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
         {new_user}@{new_ip} true"
    );
    ssh_exec(&peer_dest, &probe).await
}

async fn ssh_exec(dest: &str, cmd: &str) -> Result<(), String> {
    let out = timeout(
        Duration::from_secs(15),
        Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=accept-new",
                dest,
                cmd,
            ])
            .output(),
    )
    .await
    .map_err(|_| format!("ssh to {dest} timed out"))?
    .map_err(|e| format!("ssh spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    Ok(())
}

fn shell_escape_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Re-probe a single (src, dst) pair and upsert the result. Used by the
/// `mesh_retry` deferred task when an auto-retry fires.
pub async fn probe_single_pair(pool: &PgPool, src: &str, dst: &str) -> Result<MeshCell, String> {
    let nodes = ff_db::pg_list_nodes(pool)
        .await
        .map_err(|e| format!("pg_list_nodes: {e}"))?;
    let s = nodes
        .iter()
        .find(|n| n.name == src)
        .ok_or_else(|| format!("src node '{src}' not in fleet_nodes"))?;
    let d = nodes
        .iter()
        .find(|n| n.name == dst)
        .ok_or_else(|| format!("dst node '{dst}' not in fleet_nodes"))?;
    let cell = probe_pair(
        s.name.clone(),
        s.ssh_user.clone(),
        s.ip.clone(),
        d.name.clone(),
        d.ssh_user.clone(),
        d.ip.clone(),
    )
    .await;
    let _ = ff_db::pg_upsert_mesh_status(
        pool,
        &cell.src,
        &cell.dst,
        &cell.status,
        cell.last_error.as_deref(),
    )
    .await;
    Ok(cell)
}

/// For every `fleet_mesh_status` row in status='failed' whose last_checked is
/// older than 10 minutes, enqueue a `mesh_retry` deferred task — de-duplicated
/// against any already-pending retry for the same (src,dst) pair. Capped at
/// 5 attempts per 24h via the deferred_tasks `max_attempts` column.
pub async fn enqueue_retries(pool: &PgPool) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(10);
    let rows = ff_db::pg_list_mesh_status(pool, None)
        .await
        .map_err(|e| format!("pg_list_mesh_status: {e}"))?;
    let stale: Vec<(String, String)> = rows
        .iter()
        .filter(|r| r.status == "failed" && r.last_checked.map(|t| t < cutoff).unwrap_or(true))
        .map(|r| (r.src_node.clone(), r.dst_node.clone()))
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    let existing = ff_db::pg_list_deferred(pool, Some("pending"), 500)
        .await
        .map_err(|e| format!("pg_list_deferred: {e}"))?;
    let mut created = 0;
    for (src, dst) in stale {
        let already = existing.iter().any(|t| {
            t.kind == "mesh_retry"
                && t.payload.get("src").and_then(|v| v.as_str()) == Some(&src)
                && t.payload.get("dst").and_then(|v| v.as_str()) == Some(&dst)
        });
        if already {
            continue;
        }
        let title = format!("Mesh retry {src} → {dst}");
        let payload = serde_json::json!({ "src": src, "dst": dst });
        let trig = serde_json::json!({});
        let caps = serde_json::json!([]);
        if ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "mesh_retry",
            &payload,
            "operator",
            &trig,
            Some("taylor"),
            &caps,
            Some("mesh_auto_retry"),
            Some(5),
        )
        .await
        .is_ok()
        {
            created += 1;
        }
    }
    Ok(created)
}

pub async fn refresh_stale(pool: &PgPool, max_age: chrono::Duration) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - max_age;
    let all = ff_db::pg_list_mesh_status(pool, None)
        .await
        .map_err(|e| format!("pg_list_mesh_status: {e}"))?;
    let stale: HashSet<(String, String)> = all
        .iter()
        .filter(|r| r.last_checked.map(|t| t < cutoff).unwrap_or(true))
        .map(|r| (r.src_node.clone(), r.dst_node.clone()))
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    let _ = pairwise_ssh_check(pool).await?;
    Ok(stale.len())
}
