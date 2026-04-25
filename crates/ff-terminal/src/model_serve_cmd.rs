//! `ff model serve-tp2 <model> --across <a>+<b> --shared-vault <v> --port P`
//!
//! Wraps the full ray+vllm multi-host launch that was previously done by
//! hand-running ~20 SSH commands. Records launch_recipe in llm_clusters
//! for replay on upgrade/failover.

use anyhow::{Context, Result, bail};
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::process::Command;
use uuid::Uuid;

pub async fn handle_model_serve_tp2(
    pg: &PgPool,
    model_id: &str,
    host_a: &str,
    host_b: &str,
    shared_vault: &str,
    port: u16,
    model_path_inside_container: &str,
    max_model_len: u32,
    gpu_memory_utilization: f32,
) -> Result<()> {
    let row_a = sqlx::query("SELECT id, ssh_user, primary_ip FROM computers WHERE name = $1")
        .bind(host_a)
        .fetch_optional(pg)
        .await?
        .with_context(|| format!("host '{}' not found", host_a))?;
    let row_b = sqlx::query("SELECT id, ssh_user, primary_ip FROM computers WHERE name = $1")
        .bind(host_b)
        .fetch_optional(pg)
        .await?
        .with_context(|| format!("host '{}' not found", host_b))?;
    let a_id: Uuid = row_a.try_get("id")?;
    let a_user: String = row_a.try_get("ssh_user")?;
    let a_ip: String = row_a.try_get("primary_ip")?;
    let b_id: Uuid = row_b.try_get("id")?;
    let b_user: String = row_b.try_get("ssh_user")?;
    let b_ip: String = row_b.try_get("primary_ip")?;

    let pair_name = if host_a < host_b {
        format!("{}-{}", host_a, host_b)
    } else {
        format!("{}-{}", host_b, host_a)
    };
    let pr = sqlx::query(
        "SELECT a_ip, b_ip, computer_a_id, a_iface FROM fabric_pairs WHERE pair_name = $1",
    )
    .bind(&pair_name)
    .fetch_optional(pg)
    .await?
    .with_context(|| format!("fabric pair '{}' not found", pair_name))?;
    let pr_a_ip: String = pr.try_get("a_ip")?;
    let pr_b_ip: String = pr.try_get("b_ip")?;
    let pr_a_id: Uuid = pr.try_get("computer_a_id")?;
    let iface: String = pr.try_get("a_iface")?;
    let (a_fabric, b_fabric) = if pr_a_id == a_id {
        (pr_a_ip, pr_b_ip)
    } else {
        (pr_b_ip, pr_a_ip)
    };

    if a_fabric.is_empty() || b_fabric.is_empty() {
        bail!("fabric pair has no IPs yet");
    }

    let cluster_id = format!("{}-tp2-{}-{}", model_id, host_a, host_b);

    println!(
        "[1/4] Starting ray-head on {} (fabric {})...",
        host_a, a_fabric
    );
    let head_cmd = format!(
        "docker rm -f ff-ray-head 2>/dev/null; docker run -d --name ff-ray-head \
         --network host --ipc=host --gpus all --shm-size=64g \
         -e VLLM_HOST_IP={fab} -e NCCL_SOCKET_IFNAME={ifc} -e GLOO_SOCKET_IFNAME={ifc} \
         -e RAY_memory_monitor_refresh_ms=0 -e RAY_memory_usage_threshold=0.99 \
         -v /home/{u}/models:/models:ro \
         --entrypoint /bin/bash vllm/vllm-openai:latest \
         -c 'pip install \"ray[default]\" --quiet; ray start --head --port=6379 \
             --node-ip-address={fab} --dashboard-host=0.0.0.0 --num-gpus=1; tail -f /dev/null'",
        fab = a_fabric,
        ifc = iface,
        u = a_user
    );
    run_ssh(&a_user, &a_ip, &head_cmd).await?;

    println!(
        "[2/4] Starting ray-worker on {} (fabric {})...",
        host_b, b_fabric
    );
    let worker_cmd = format!(
        "sudo docker rm -f ff-ray-worker 2>/dev/null; sudo docker run -d --name ff-ray-worker \
         --network host --ipc=host --gpus all --shm-size=64g \
         -e VLLM_HOST_IP={fab_b} -e NCCL_SOCKET_IFNAME={ifc} -e GLOO_SOCKET_IFNAME={ifc} \
         -e RAY_memory_monitor_refresh_ms=0 -e RAY_memory_usage_threshold=0.99 \
         -v /home/{u_a}/models:/models:ro \
         --entrypoint /bin/bash vllm/vllm-openai:latest \
         -c 'pip install \"ray[default]\" --quiet; sleep 30; ray start --address={fab_a}:6379 \
             --node-ip-address={fab_b} --num-gpus=1; tail -f /dev/null'",
        fab_a = a_fabric,
        fab_b = b_fabric,
        ifc = iface,
        u_a = a_user
    );
    run_ssh(&b_user, &b_ip, &worker_cmd).await?;

    println!("[3/4] Waiting for ray cluster to form (~90s)...");
    tokio::time::sleep(std::time::Duration::from_secs(90)).await;

    println!("[4/4] Launching vllm serve TP=2 on {}...", host_a);
    let serve_cmd = format!(
        "docker exec -d ff-ray-head bash -c 'vllm serve {path} \
            --tensor-parallel-size 2 --distributed-executor-backend ray \
            --host 0.0.0.0 --port {port} --trust-remote-code \
            --max-model-len {mml} --served-model-name {mid} \
            --gpu-memory-utilization {gmu} --enforce-eager > /tmp/vllm.log 2>&1'",
        path = model_path_inside_container,
        port = port,
        mml = max_model_len,
        mid = model_id,
        gmu = gpu_memory_utilization
    );
    run_ssh(&a_user, &a_ip, &serve_cmd).await?;

    let recipe = json!({
        "head_cmd": head_cmd,
        "worker_cmd": worker_cmd,
        "serve_cmd": serve_cmd,
        "host_a": host_a,
        "host_b": host_b,
        "shared_vault": shared_vault,
        "port": port,
        "max_model_len": max_model_len,
        "gpu_memory_utilization": gpu_memory_utilization,
    });
    sqlx::query(
        "INSERT INTO llm_clusters \
            (id, model_id, runtime, topology, head_computer_id, worker_computer_ids, \
             ray_head_endpoint, api_endpoint, tensor_parallel_size, launch_recipe, status, launched_by) \
         VALUES ($1, $2, 'vllm', 'tp', $3, $4::jsonb, $5, $6, 2, $7::jsonb, 'launching', 'ff model serve-tp2') \
         ON CONFLICT (id) DO UPDATE SET launch_recipe = EXCLUDED.launch_recipe, status = 'launching'",
    )
    .bind(&cluster_id)
    .bind(model_id)
    .bind(a_id)
    .bind(json!([b_id.to_string()]))
    .bind(format!("{}:6379", a_fabric))
    .bind(format!("http://{}:{}", a_ip, port))
    .bind(&recipe)
    .execute(pg)
    .await?;

    println!(
        "Launched cluster {} — API at http://{}:{}/v1/models",
        cluster_id, a_ip, port
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
        bail!("ssh failed on {}@{}", user, ip);
    }
    Ok(())
}
