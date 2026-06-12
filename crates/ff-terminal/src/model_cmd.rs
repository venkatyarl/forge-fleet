use crate::{
    CYAN, GREEN, RED, RESET, YELLOW, expand_tilde, human_bytes, shell_escape_single,
    trunc_for_status, whoami_tag,
};
use anyhow::Result;
use std::path::PathBuf;

/// True when a deployment's per-slot context (`usable_agent_ctx`) meets the
/// agent router floor `min_ctx`. A `None` ctx (unknown / pre-backfill) is NOT
/// agent-ready — the router can't trust an endpoint whose usable slot size it
/// doesn't know. Mirrors the filter in `ff_db::pg_supplied_slots_by_kind`.
fn is_agent_ready(usable_agent_ctx: Option<i32>, min_ctx: i32) -> bool {
    usable_agent_ctx.map(|c| c >= min_ctx).unwrap_or(false)
}

/// Conservative free-RAM floor (GB) below which `ff model reprofile` refuses to
/// relaunch without `--force`. Reprofiling reloads the SAME (already-resident)
/// model, so the only new memory is the larger single-slot KV cache — but a host
/// already at its limit can still OOM when the agent ctx grows from a few K to
/// 32K+. `pg_placement_candidates.free_ram_gb` is `total_ram − resident weights`,
/// so a positive value above this floor leaves headroom for the KV delta.
const REPROFILE_MIN_FREE_RAM_GB: f64 = 4.0;

/// Whether a host has enough conservative free RAM to safely grow a deployment's
/// KV cache during a reprofile. Pure for unit testing. `--force` bypasses this.
fn ram_headroom_ok(free_ram_gb: f64, floor_gb: f64) -> bool {
    free_ram_gb >= floor_gb
}

/// Reprofile a running deployment into the agent-capable serving profile
/// (`--parallel 1 --ctx >= 32768`) so it becomes agent-router-visible. Runs on
/// the host that owns the deployment, SSHing there if it lives elsewhere.
async fn handle_reprofile(
    pool: &sqlx::PgPool,
    id: &str,
    ctx: Option<u32>,
    force: bool,
    json: bool,
) -> Result<()> {
    let min_ctx = ff_agent::model_runtime::AGENT_MIN_CTX as i32;

    // Locate the deployment across the whole fleet so we can route to its owner.
    let all = ff_db::pg_list_deployments(pool, None).await?;
    let dep = all
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| {
            anyhow::anyhow!("no deployment with id '{id}' (see `ff model deployments --show-id`)")
        })?
        .clone();

    let this_node = ff_agent::fleet_info::resolve_this_worker_name().await;
    if !dep.worker_name.eq_ignore_ascii_case(&this_node) {
        // Owner is a different host: SSH `ff model reprofile <id>` over there
        // (resolved from Postgres, never ~/.ssh/config — same pattern as unload).
        let node_row = ff_db::pg_get_node(pool, &dep.worker_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("node '{}' not in fleet_workers", dep.worker_name))?;
        let mut remote_cmd = format!(
            "~/.local/bin/ff model reprofile {}",
            shell_escape_single(id)
        );
        if let Some(c) = ctx {
            remote_cmd.push_str(&format!(" --ctx {c}"));
        }
        if force {
            remote_cmd.push_str(" --force");
        }
        if json {
            remote_cmd.push_str(" --json");
        }
        println!(
            "{CYAN}▶ Reprofiling deployment {id} on {} ({}@{})...{RESET}",
            dep.worker_name, node_row.ssh_user, node_row.ip
        );
        let (code, out, err) =
            ff_agent::model_transfer::ssh_exec(&node_row.ssh_user, &node_row.ip, &remote_cmd)
                .await
                .map_err(|e| anyhow::anyhow!("ssh to {}: {e}", dep.worker_name))?;
        if !out.trim().is_empty() {
            print!("{out}");
        }
        if code != 0 {
            anyhow::bail!(
                "remote reprofile on {} exited {code}: {}",
                dep.worker_name,
                err.trim()
            );
        }
        return Ok(());
    }

    // Local path — this host owns the deployment.
    let catalog_id = dep.catalog_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "deployment {id} has no catalog_id — cannot verify tool-calling; reprofile aborted"
        )
    })?;

    // 1. Must be tool-calling, or the agent router will never pick it regardless
    //    of ctx — reprofiling would just cause a pointless down-window.
    let cat = ff_db::pg_get_catalog(pool, &catalog_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("catalog row '{catalog_id}' not found"))?;
    if !cat.tool_calling {
        anyhow::bail!(
            "model '{catalog_id}' is not tool-calling — the agent router won't use it even at {min_ctx} ctx; reprofile aborted (nothing changed)"
        );
    }

    // 2. Already agent-ready? No-op (don't take an endpoint down for nothing).
    if is_agent_ready(dep.usable_agent_ctx, min_ctx) {
        let msg = format!(
            "deployment {id} ({catalog_id}) on {} is already agent-ready (usable_agent_ctx={} >= {min_ctx}) — nothing to do",
            dep.worker_name,
            dep.usable_agent_ctx.unwrap_or(0),
        );
        if json {
            println!(
                "{}",
                serde_json::json!({"reprofiled": false, "reason": "already_agent_ready",
                    "deployment_id": id, "usable_agent_ctx": dep.usable_agent_ctx})
            );
        } else {
            println!("{GREEN}✓{RESET} {msg}");
        }
        return Ok(());
    }

    // 3. RAM safety: the larger single-slot ctx grows the KV cache. Refuse on a
    //    memory-tight host unless --force.
    let cands = ff_db::pg_placement_candidates(pool).await?;
    let free_ram_gb = cands
        .iter()
        .find(|c| c.worker_name.eq_ignore_ascii_case(&dep.worker_name))
        .map(|c| c.free_ram_gb);
    if let Some(free) = free_ram_gb
        && !force
        && !ram_headroom_ok(free, REPROFILE_MIN_FREE_RAM_GB)
    {
        anyhow::bail!(
            "host {} has only ~{:.1} GB conservative free RAM (floor {:.0} GB); reprofiling grows the KV cache and may OOM. Re-run with --force to override.",
            dep.worker_name,
            free,
            REPROFILE_MIN_FREE_RAM_GB
        );
    }

    // 4. Need a library row to relaunch the same file. Prefer the deployment's
    //    library_id; fall back to this host's library by catalog_id.
    let library_id = match dep.library_id.clone() {
        Some(l) => l,
        None => {
            let libs = ff_db::pg_list_library(pool, Some(&dep.worker_name)).await?;
            libs.iter()
                .find(|r| r.catalog_id == catalog_id)
                .map(|r| r.id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no library row for '{catalog_id}' on {} — cannot reload",
                        dep.worker_name
                    )
                })?
        }
    };

    let target_ctx = ctx.unwrap_or(ff_agent::model_runtime::AGENT_MIN_CTX);
    let port = dep.port as u16;
    let old_ctx = match (dep.usable_agent_ctx, dep.parallel_slots) {
        (Some(u), Some(p)) => format!("{u}x{p}"),
        (Some(u), _) => u.to_string(),
        _ => "?".into(),
    };

    println!(
        "{CYAN}▶ Reprofiling {catalog_id} on {} port {port}: {old_ctx} → agent profile (--parallel 1, ctx >= {}){RESET}",
        dep.worker_name,
        target_ctx.max(ff_agent::model_runtime::AGENT_MIN_CTX),
    );
    println!("  {YELLOW}(brief down-window on port {port} while the server relaunches){RESET}");

    // 5. Unload the current deployment, then reload the SAME model on the SAME
    //    port in the agent profile. load_model health-waits (90s) and records
    //    usable_agent_ctx, so a failed relaunch surfaces as an Err here.
    ff_agent::model_runtime::unload_model(pool, id)
        .await
        .map_err(|e| anyhow::anyhow!("unload of {id} failed (nothing reloaded): {e}"))?;

    let res = ff_agent::model_runtime::load_model(
        pool,
        ff_agent::model_runtime::LoadOptions {
            library_id,
            port,
            context_size: Some(target_ctx),
            parallel: None,
            agent_profile: true,
            mmproj_path: None,
        },
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(
            "RELAUNCH FAILED on port {port} after unload — endpoint is DOWN until the reconciler recovers it: {e}"
        )
    })?;

    // 6. Confirm the new profile is actually agent-ready.
    let now = ff_db::pg_list_deployments(pool, Some(&dep.worker_name)).await?;
    let new = now.iter().find(|d| d.id == res.deployment_id);
    let new_usable = new.and_then(|d| d.usable_agent_ctx);
    let agent_ready = is_agent_ready(new_usable, min_ctx);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "reprofiled": true,
                "old_deployment_id": id,
                "new_deployment_id": res.deployment_id,
                "node": dep.worker_name,
                "port": port,
                "catalog_id": catalog_id,
                "usable_agent_ctx": new_usable,
                "agent_ready": agent_ready,
            })
        );
    } else if agent_ready {
        println!(
            "{GREEN}✓ Reprofiled{RESET} — {catalog_id} on {} port {port} now agent-ready (usable_agent_ctx={}, deployment {})",
            dep.worker_name,
            new_usable.unwrap_or(0),
            res.deployment_id
        );
    } else {
        // Loaded + healthy but the recorded per-slot ctx didn't clear the floor
        // (e.g. reconciler hasn't backfilled yet). Don't claim success.
        println!(
            "{YELLOW}⚠ Relaunched{RESET} {catalog_id} on {} port {port} (deployment {}) but usable_agent_ctx={} is not yet >= {min_ctx}. Re-check with `ff model agent-ready --node {}`.",
            dep.worker_name,
            res.deployment_id,
            new_usable
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".into()),
            dep.worker_name,
        );
    }
    Ok(())
}

pub async fn handle_model(cmd: crate::ModelCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::ModelCommand::ServeTp2 {
            model_id,
            across,
            shared_vault,
            port,
            container_path,
            max_model_len,
            gpu_memory_utilization,
        } => {
            let (a, b) = match across.split_once('+') {
                Some(parts) => parts,
                None => anyhow::bail!("--across requires `<hostA>+<hostB>` (e.g. `sia+adele`)"),
            };
            let path_inside = container_path.unwrap_or_else(|| format!("/models/{}", model_id));
            crate::model_serve_cmd::handle_model_serve_tp2(
                &pool,
                &model_id,
                a,
                b,
                &shared_vault,
                port,
                &path_inside,
                max_model_len,
                gpu_memory_utilization,
            )
            .await?;
        }
        crate::ModelCommand::SyncCatalog => {
            let n = ff_agent::model_catalog::sync_catalog(&pool)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("Synced {n} catalog entries from TOML to Postgres");
        }
        crate::ModelCommand::Search { query } => {
            let rows = ff_db::pg_search_catalog(&pool, &query).await?;
            if rows.is_empty() {
                println!("(no catalog matches for \"{query}\")");
                return Ok(());
            }
            println!(
                "{:<28} {:<10} {:<6} {:<7} NAME",
                "ID", "FAMILY", "TIER", "GATED"
            );
            for r in rows {
                let gated = if r.gated { "yes" } else { "-" };
                println!(
                    "{:<28} {:<10} T{:<5} {:<7} {}",
                    r.id, r.family, r.tier, gated, r.name
                );
            }
        }
        crate::ModelCommand::Catalog => {
            let rows = ff_db::pg_list_catalog(&pool).await?;
            if rows.is_empty() {
                println!("(catalog empty — run `ff model sync-catalog` first)");
                return Ok(());
            }
            println!(
                "{:<28} {:<10} {:<6} {:<7} {:<7} NAME",
                "ID", "FAMILY", "TIER", "PARAMS", "GATED"
            );
            for r in rows {
                let gated = if r.gated { "yes" } else { "-" };
                println!(
                    "{:<28} {:<10} T{:<5} {:<7} {:<7} {}",
                    r.id, r.family, r.tier, r.parameters, gated, r.name
                );
            }
        }
        crate::ModelCommand::Library { node, show_id } => {
            let rows = ff_db::pg_list_library(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(library empty — run `ff model scan` to index your local models dir)");
                return Ok(());
            }
            if show_id {
                println!("{:<38} ", "LIBRARY_ID");
            }
            println!(
                "{:<10} {:<28} {:<10} {:<10} {:<10} PATH",
                "NODE", "CATALOG_ID", "RUNTIME", "QUANT", "SIZE"
            );
            for r in rows {
                let sz = human_bytes(r.size_bytes as u64);
                let quant = r.quant.clone().unwrap_or_else(|| "-".into());
                if show_id {
                    print!("{:<38} ", r.id);
                }
                println!(
                    "{:<10} {:<28} {:<10} {:<10} {:<10} {}",
                    r.worker_name, r.catalog_id, r.runtime, quant, sz, r.file_path
                );
            }
        }
        crate::ModelCommand::Deployments { node, show_id } => {
            let rows = ff_db::pg_list_deployments(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(no deployments recorded)");
                return Ok(());
            }
            // With --show-id, surface DEPLOYMENT_ID (for `ff model unload`),
            // LIBRARY_ID, CTX (for a faithful `ff model load` reload), and
            // AGENT_CTX = usable per-slot ctx × slot count (so you can spot the
            // agent-capable endpoints the router will pick).
            if show_id {
                println!(
                    "{:<38} {:<38} {:<10} {:<28} {:<10} {:<6} {:<7} {:<12} {:<10} STARTED",
                    "DEPLOYMENT_ID",
                    "LIBRARY_ID",
                    "NODE",
                    "CATALOG_ID",
                    "RUNTIME",
                    "PORT",
                    "CTX",
                    "AGENT_CTX",
                    "HEALTH"
                );
                for r in rows {
                    let catalog = r.catalog_id.clone().unwrap_or_else(|| "-".into());
                    let lib = r.library_id.clone().unwrap_or_else(|| "-".into());
                    let ctx = r
                        .context_window
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".into());
                    // e.g. "32768x1" (usable_agent_ctx × parallel_slots).
                    let agent_ctx = match (r.usable_agent_ctx, r.parallel_slots) {
                        (Some(u), Some(p)) => format!("{u}x{p}"),
                        (Some(u), None) => u.to_string(),
                        _ => "-".into(),
                    };
                    println!(
                        "{:<38} {:<38} {:<10} {:<28} {:<10} {:<6} {:<7} {:<12} {:<10} {}",
                        r.id,
                        lib,
                        r.worker_name,
                        catalog,
                        r.runtime,
                        r.port,
                        ctx,
                        agent_ctx,
                        r.health_status,
                        r.started_at.format("%Y-%m-%d %H:%M UTC")
                    );
                }
            } else {
                println!(
                    "{:<10} {:<28} {:<10} {:<6} {:<10} STARTED",
                    "NODE", "CATALOG_ID", "RUNTIME", "PORT", "HEALTH"
                );
                for r in rows {
                    let catalog = r.catalog_id.clone().unwrap_or_else(|| "-".into());
                    println!(
                        "{:<10} {:<28} {:<10} {:<6} {:<10} {}",
                        r.worker_name,
                        catalog,
                        r.runtime,
                        r.port,
                        r.health_status,
                        r.started_at.format("%Y-%m-%d %H:%M UTC")
                    );
                }
            }
        }
        crate::ModelCommand::AgentReady { node, json } => {
            let rows = ff_db::pg_agent_readiness(&pool, node.as_deref()).await?;
            let min_ctx = ff_agent::model_runtime::AGENT_MIN_CTX as i32;
            let leader = ff_db::pg_get_current_leader(&pool)
                .await
                .ok()
                .flatten()
                .map(|l| l.member_name);
            let is_leader = |w: &str| leader.as_deref() == Some(w);

            // Split tool-capable endpoints into agent-capable (per-slot ctx meets
            // the router floor) vs reprofile-candidate (too many slots → too small).
            let (ready, candidates): (
                Vec<&ff_db::AgentReadinessRow>,
                Vec<&ff_db::AgentReadinessRow>,
            ) = rows
                .iter()
                .partition(|r| is_agent_ready(r.usable_agent_ctx, min_ctx));

            let fmt_ctx =
                |r: &ff_db::AgentReadinessRow| match (r.usable_agent_ctx, r.parallel_slots) {
                    (Some(u), Some(p)) => format!("{u}x{p}"),
                    (Some(u), None) => u.to_string(),
                    _ => "-".into(),
                };

            if json {
                let to_obj = |r: &ff_db::AgentReadinessRow| {
                    serde_json::json!({
                        "node": r.worker_name,
                        "catalog_id": r.catalog_id,
                        "port": r.port,
                        "runtime": r.runtime,
                        "context_window": r.context_window,
                        "parallel_slots": r.parallel_slots,
                        "usable_agent_ctx": r.usable_agent_ctx,
                        "kind": if r.is_code { "code" } else { "general" },
                        "is_leader": is_leader(&r.worker_name),
                    })
                };
                let code_ready = ready.iter().filter(|r| r.is_code).count();
                let non_leader = ready.iter().filter(|r| !is_leader(&r.worker_name)).count();
                let out = serde_json::json!({
                    "min_ctx": min_ctx,
                    "leader": leader,
                    "agent_capable": ready.iter().map(|r| to_obj(r)).collect::<Vec<_>>(),
                    "reprofile_candidates": candidates.iter().map(|r| to_obj(r)).collect::<Vec<_>>(),
                    "summary": {
                        "agent_capable": ready.len(),
                        "agent_capable_non_leader": non_leader,
                        "reprofile_candidates": candidates.len(),
                        "code_ready": code_ready,
                        "general_ready": ready.len() - code_ready,
                    }
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }

            println!(
                "{GREEN}AGENT-CAPABLE ENDPOINTS{RESET} (tool_calling + usable_agent_ctx >= {min_ctx})"
            );
            if ready.is_empty() {
                println!("  (none — no endpoint can currently serve an agent/tool task)");
            } else {
                println!(
                    "  {:<10} {:<28} {:<6} {:<10} {:<8} LEADER",
                    "NODE", "CATALOG_ID", "PORT", "AGENT_CTX", "KIND"
                );
                for r in &ready {
                    println!(
                        "  {:<10} {:<28} {:<6} {:<10} {:<8} {}",
                        r.worker_name,
                        r.catalog_id.clone().unwrap_or_else(|| "-".into()),
                        r.port,
                        fmt_ctx(r),
                        if r.is_code { "code" } else { "general" },
                        if is_leader(&r.worker_name) {
                            "leader"
                        } else {
                            ""
                        }
                    );
                }
            }
            println!();
            println!(
                "{YELLOW}REPROFILE CANDIDATES{RESET} (tool-capable but per-slot ctx < {min_ctx})"
            );
            if candidates.is_empty() {
                println!("  (none)");
            } else {
                println!(
                    "  {:<10} {:<28} {:<6} {:<10} {:<8} HINT",
                    "NODE", "CATALOG_ID", "PORT", "AGENT_CTX", "KIND"
                );
                for r in &candidates {
                    println!(
                        "  {:<10} {:<28} {:<6} {:<10} {:<8} relaunch --parallel 1 --ctx {min_ctx}",
                        r.worker_name,
                        r.catalog_id.clone().unwrap_or_else(|| "-".into()),
                        r.port,
                        fmt_ctx(r),
                        if r.is_code { "code" } else { "general" },
                    );
                }
            }
            println!();
            let code_ready = ready.iter().filter(|r| r.is_code).count();
            let non_leader = ready.iter().filter(|r| !is_leader(&r.worker_name)).count();
            println!(
                "{CYAN}SUMMARY{RESET}: {} agent-capable ({} non-leader) | {} reprofile candidate(s) | code: {} ready, general: {} ready",
                ready.len(),
                non_leader,
                candidates.len(),
                code_ready,
                ready.len() - code_ready,
            );
        }
        crate::ModelCommand::FreeForBuild => {
            match ff_agent::model_runtime::pause_local_models_for_build(&pool).await {
                Ok(n) => println!("free-for-build: paused {n} model(s) to free RAM"),
                Err(e) => anyhow::bail!("free-for-build: {e}"),
            }
        }
        crate::ModelCommand::ResumeFromBuild => {
            match ff_agent::model_runtime::resume_local_models(&pool).await {
                Ok(n) => println!("resume-from-build: restored {n} model(s)"),
                Err(e) => anyhow::bail!("resume-from-build: {e}"),
            }
        }
        crate::ModelCommand::Scan { node, models_dir } => {
            // Default: resolve this host's node name from Postgres by IP.
            let worker_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_worker_name().await,
            };
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let default_dir = PathBuf::from(home).join("models");
            let dir = models_dir.unwrap_or(default_dir);

            if !dir.exists() {
                anyhow::bail!("models dir does not exist: {}", dir.display());
            }
            println!("Scanning {} on node {} ...", dir.display(), worker_name);
            let summary =
                ff_agent::model_library_scanner::scan_local_library(&pool, &worker_name, &dir)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            println!("  added:   {}", summary.added);
            println!("  updated: {}", summary.updated);
            println!("  removed: {}", summary.removed);
            println!(
                "  total:   {} across models dir",
                human_bytes(summary.total_bytes)
            );
        }
        crate::ModelCommand::Disk => {
            let rows = ff_db::pg_latest_disk_usage(&pool).await?;
            if rows.is_empty() {
                println!("(no disk usage samples yet — the daemon records these periodically)");
                return Ok(());
            }
            println!(
                "{:<10} {:<24} {:<10} {:<10} {:<10} SAMPLED",
                "NODE", "MODELS_DIR", "FREE", "USED", "MODELS"
            );
            for (node, dir, total, used, free, models_sz, ts) in rows {
                let _ = total;
                println!(
                    "{:<10} {:<24} {:<10} {:<10} {:<10} {}",
                    node,
                    dir,
                    human_bytes(free as u64),
                    human_bytes(used as u64),
                    human_bytes(models_sz as u64),
                    ts.format("%Y-%m-%d %H:%M UTC")
                );
            }
        }
        crate::ModelCommand::Download {
            id,
            runtime,
            node,
            force,
        } => {
            // Resolve target node + node runtime + models_dir.
            let worker_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_worker_name().await,
            };
            let node_row = ff_db::pg_get_node(&pool, &worker_name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("node '{worker_name}' not in fleet_workers"))?;
            let target_runtime = runtime.unwrap_or_else(|| node_row.runtime.clone());
            if target_runtime == "unknown" {
                anyhow::bail!(
                    "node '{worker_name}' has unknown runtime; set with: ff config set fleet.{worker_name}.runtime mlx|llama.cpp|vllm"
                );
            }

            // Lookup catalog entry; pick variant for runtime.
            let catalog = ff_db::pg_get_catalog(&pool, &id).await?.ok_or_else(|| {
                anyhow::anyhow!("no catalog entry with id '{id}' (try `ff model search`)")
            })?;
            let variants = catalog
                .variants
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("catalog variants for '{id}' is not an array"))?;
            let variant = variants
                .iter()
                .find(|v| {
                    v.get("runtime").and_then(|x| x.as_str()) == Some(target_runtime.as_str())
                })
                .ok_or_else(|| {
                    let available: Vec<String> = variants
                        .iter()
                        .filter_map(|v| v.get("runtime").and_then(|x| x.as_str()).map(String::from))
                        .collect();
                    anyhow::anyhow!(
                        "no variant for runtime '{target_runtime}' on '{id}'. available: {}",
                        available.join(", ")
                    )
                })?;

            let hf_repo = variant
                .get("hf_repo")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("variant missing hf_repo"))?;
            let quant = variant
                .get("quant")
                .and_then(|v| v.as_str())
                .map(String::from);
            let size_gb = variant
                .get("size_gb")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);

            // Cross-node downloads are dispatched via the deferred task queue: a
            // defer-worker running on the target node will claim it and run
            // `ff model download <id> --runtime <rt>` locally there.
            let this_node = ff_agent::fleet_info::resolve_this_worker_name().await;
            if worker_name != this_node {
                let escaped_id = shell_escape_single(&id);
                let command = format!(
                    "ff model download {} --runtime {}",
                    escaped_id, target_runtime
                );
                let title = format!(
                    "Download {} ({} variant) on {}",
                    id, target_runtime, worker_name
                );
                let payload = serde_json::json!({ "command": command });
                let trigger_spec = serde_json::json!({});
                let defer_id = ff_db::pg_enqueue_deferred(
                    &pool,
                    &title,
                    "shell",
                    &payload,
                    "now",
                    &trigger_spec,
                    Some(&worker_name),
                    &serde_json::json!([]),
                    Some(&whoami_tag()),
                    Some(3),
                )
                .await?;
                println!(
                    "Enqueued cross-node download as deferred task {defer_id}. It will run on {worker_name} when a defer-worker there claims it."
                );
                println!("Check status with: ff defer list");
                return Ok(());
            }

            // Compute destination dir under models_dir.
            //
            // V139 dir-layout enforcement: new downloads land in
            // <models_dir>/<runtime>/<catalog_id>/ so the runtime is
            // obvious from the path. Old downloads stay where they are;
            // they'll migrate lazily when their deployment restarts and
            // the (deferred) #136b startup-fetch wrapper drops the new
            // copy into the canonical path.
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let models_dir = expand_tilde(&node_row.models_dir, &home);
            let runtime_subdir = match target_runtime.as_str() {
                "llama.cpp" => "llama-cpp",
                "mlx" => "mlx",
                "vllm" => "vllm",
                "ollama" => "ollama",
                other => other,
            };
            let dest = models_dir.join(runtime_subdir).join(&id);

            // PLACEMENT GUARD (V118): before we stream gigabytes onto this node,
            // reject placements that (a) this node can't RUN, or (b) won't FIT.
            // Stops the problem upstream rather than after a long download.
            if let Err(reason) =
                ff_agent::model_runtime::check_runtime_placement(&node_row, &target_runtime)
            {
                anyhow::bail!(
                    "placement rejected: cannot place {id} ({target_runtime}) on {worker_name}: {reason}"
                );
            }
            if size_gb > 0.0 {
                let need_bytes =
                    (size_gb * 1.1 * (1024.0 * 1024.0 * 1024.0)) as u64 + 5 * (1u64 << 30);
                let df_out = std::process::Command::new("df")
                    .arg("-Pk")
                    .arg(&models_dir)
                    .output();
                if let Ok(out) = df_out {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let last = text.lines().last().unwrap_or("").trim();
                    let cols: Vec<&str> = last.split_whitespace().collect();
                    if let Some(free_bytes) = cols
                        .get(3)
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|k| k.saturating_mul(1024))
                    {
                        if free_bytes < need_bytes {
                            anyhow::bail!(
                                "placement rejected: {id} (~{size_gb:.1}GB) won't fit on {worker_name}: need {} but only {} free under {}",
                                human_bytes(need_bytes),
                                human_bytes(free_bytes),
                                models_dir.display(),
                            );
                        }
                    }
                }
            }

            // Ensure runtime parent exists (mkdir -p) before hf_download
            // tries to create the leaf dir. Cheap if already there.
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            // HF token (optional — gated models need it).
            let token = ff_agent::fleet_info::get_hf_token().await;
            if catalog.gated && token.is_none() {
                anyhow::bail!(
                    "model '{id}' is gated on HF; set token first with: ff secrets set huggingface.token <hf_xxx>"
                );
            }

            // Allow patterns: prefer runtime-specific glob to avoid pulling everything.
            // For llama.cpp, also narrow by the variant's quant so we don't pull every
            // quant in the repo (e.g. deepseek-r1-distill-qwen-32b ships 7 quants ≈
            // 140 GB total when only the catalog's Q4_K_M variant is wanted).
            //
            // Tokenizer/config files have no quant suffix, so they stay matched by
            // separate patterns.
            let allow_patterns: Vec<String> = match target_runtime.as_str() {
                "llama.cpp" => {
                    let mut pats = vec!["tokenizer*".into(), "*config*".into()];
                    match quant.as_deref() {
                        Some(q) if !q.is_empty() => {
                            // e.g. "*Q4_K_M*.gguf" — matches both upper- and lower-
                            // case in the glob because we lowercase comparisons in
                            // the matcher? (No — glob_match is case-sensitive.)
                            // Add both common casings to be safe across repos.
                            pats.push(format!("*{q}*.gguf"));
                            let lower = q.to_lowercase();
                            if lower != q {
                                pats.push(format!("*{lower}*.gguf"));
                            }
                        }
                        _ => pats.push("*.gguf".into()),
                    }
                    pats
                }
                "mlx" | "vllm" => vec![
                    "*.safetensors".into(),
                    "*.json".into(),
                    "tokenizer*".into(),
                    "*config*".into(),
                    "README*".into(),
                ],
                other => vec![format!("*.{other}")],
            };
            // No global deny — the quant-narrowed allow above is precise enough.
            // (Previously denied *.f16*/*.bf16* as a blunt cost guard; that bit
            // embedders whose canonical quant *is* F16, like bge-m3-FP16.gguf.)
            let deny_patterns: Vec<String> = vec![];

            let _ = force; // not yet used; resume-by-size is automatic

            // Create job row for tracking.
            let params = serde_json::json!({
                "hf_repo": hf_repo,
                "runtime": target_runtime,
                "quant": quant,
                "dest": dest.to_string_lossy(),
            });
            let job_id =
                ff_db::pg_create_job(&pool, &worker_name, "download", Some(&id), None, &params)
                    .await?;
            ff_db::pg_update_job_progress(
                &pool,
                &job_id,
                Some("running"),
                Some(0.0),
                None,
                None,
                None,
                None,
            )
            .await?;

            println!(
                "{CYAN}▶ Downloading {} ({})\n  source: {}\n  dest:   {}\n  job:    {}{RESET}",
                catalog.name,
                target_runtime,
                hf_repo,
                dest.display(),
                job_id
            );
            if size_gb > 0.0 {
                println!("  estimated size: {size_gb:.1} GB");
            }

            // Run download with progress callback.
            let pool_for_progress = pool.clone();
            let job_id_for_progress = job_id.clone();
            let mut last_pct = -1i32;
            let opts = ff_agent::hf_download::DownloadOptions {
                repo: hf_repo.to_string(),
                revision: None,
                dest_dir: dest.clone(),
                token: token.clone(),
                allow_patterns,
                deny_patterns,
                skip_verify: false,
            };

            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| anyhow::anyhow!("build http client: {e}"))?;
            let result = ff_agent::hf_download::download_repo(&client, opts, move |p| {
                let pct = p.percent as i32;
                if pct != last_pct {
                    last_pct = pct;
                    let bar_w = 30;
                    let filled = (bar_w as f32 * p.percent / 100.0) as usize;
                    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));
                    let done_mb = p.bytes_done / (1u64 << 20);
                    let total_mb = p.bytes_total / (1u64 << 20);
                    eprint!(
                        "\r  [{bar}] {pct:>3}%  {done_mb}/{total_mb} MiB  {}",
                        trunc_for_status(&p.file, 40)
                    );
                    use std::io::Write as _;
                    let _ = std::io::stderr().flush();
                    // Update DB job (fire and forget — best effort)
                    let pool2 = pool_for_progress.clone();
                    let jid = job_id_for_progress.clone();
                    let bd = p.bytes_done as i64;
                    let bt = p.bytes_total as i64;
                    let pp = p.percent;
                    tokio::spawn(async move {
                        let _ = ff_db::pg_update_job_progress(
                            &pool2,
                            &jid,
                            None,
                            Some(pp),
                            Some(bd),
                            Some(bt),
                            None,
                            None,
                        )
                        .await;
                    });
                }
            })
            .await;
            eprintln!(); // newline after progress bar

            match result {
                Ok(files) => {
                    println!("{CYAN}✓ Downloaded {} file(s){RESET}", files.len());
                    let _ = ff_db::pg_update_job_progress(
                        &pool,
                        &job_id,
                        Some("completed"),
                        Some(100.0),
                        None,
                        None,
                        None,
                        None,
                    )
                    .await;
                    // Re-scan node so library reflects the new model.
                    println!("Re-scanning library...");
                    let summary = ff_agent::model_library_scanner::scan_local_library(
                        &pool,
                        &worker_name,
                        &models_dir,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                    println!("  added: {}, updated: {}", summary.added, summary.updated);
                }
                Err(e) => {
                    let _ = ff_db::pg_update_job_progress(
                        &pool,
                        &job_id,
                        Some("failed"),
                        None,
                        None,
                        None,
                        None,
                        Some(&e),
                    )
                    .await;
                    anyhow::bail!("download failed: {e}");
                }
            }
        }
        crate::ModelCommand::DownloadBatch { node, ids } => {
            if ids.is_empty() {
                anyhow::bail!(
                    "no catalog ids provided; usage: ff model download-batch --node <name> <id>..."
                );
            }
            // Resolve target node + its runtime.
            let node_row = ff_db::pg_get_node(&pool, &node)
                .await?
                .ok_or_else(|| anyhow::anyhow!("node '{node}' not in fleet_workers"))?;
            let target_runtime = node_row.runtime.clone();
            if target_runtime == "unknown" {
                anyhow::bail!(
                    "node '{node}' has unknown runtime; set with: ff config set fleet.{node}.runtime mlx|llama.cpp|vllm"
                );
            }

            // Validate every id exists in the catalog BEFORE enqueuing anything.
            for id in &ids {
                if ff_db::pg_get_catalog(&pool, id).await?.is_none() {
                    anyhow::bail!("no catalog entry with id '{id}' (try `ff model search`)");
                }
            }

            let who = whoami_tag();
            let mut enqueued: Vec<(String, String)> = Vec::with_capacity(ids.len());
            for id in &ids {
                let escaped_id = shell_escape_single(id);
                let command = format!(
                    "ff model download {} --runtime {}",
                    escaped_id, target_runtime
                );
                let title = format!("Download {} ({} variant) on {}", id, target_runtime, node);
                let payload = serde_json::json!({ "command": command });
                let trigger_spec = serde_json::json!({});
                let defer_id = ff_db::pg_enqueue_deferred(
                    &pool,
                    &title,
                    "shell",
                    &payload,
                    "now",
                    &trigger_spec,
                    Some(&node),
                    &serde_json::json!([]),
                    Some(&who),
                    Some(3),
                )
                .await?;
                enqueued.push((id.to_string(), defer_id));
            }

            println!(
                "Enqueued {} cross-node downloads on '{}':",
                enqueued.len(),
                node
            );
            for (id, defer_id) in &enqueued {
                println!("  {defer_id}  {id}");
            }
            println!("Check status with: ff defer list");
        }
        crate::ModelCommand::Delete { id, yes } => {
            // Look up library row.
            let all = ff_db::pg_list_library(&pool, None).await?;
            let row = all.iter().find(|r| r.id == id).ok_or_else(|| {
                anyhow::anyhow!("no library entry with id '{id}' (try `ff model library`)")
            })?;

            // Safety: refuse if a deployment references this library row.
            let deployments = ff_db::pg_list_deployments(&pool, Some(&row.worker_name)).await?;
            let in_use = deployments
                .iter()
                .any(|d| d.library_id.as_deref() == Some(&id));
            if in_use {
                anyhow::bail!(
                    "model is currently deployed on {} — unload it first (`ff model unload <deployment_id>`)",
                    row.worker_name
                );
            }

            // Cross-node delete: dispatch to the owning node via the deferred
            // task queue — same pattern as `ff model download`. A defer-worker
            // on the target node claims it and runs the delete locally (where
            // the file actually lives). Bare `ff` is fine in the defer command:
            // the defer-worker runs with a full PATH, unlike a non-login SSH
            // shell. The --yes is implied (operator already confirmed here).
            let this_node = ff_agent::fleet_info::resolve_this_worker_name().await;
            if row.worker_name != this_node {
                if !yes {
                    println!(
                        "This will delete {} ({}) from {} (cross-node). Re-run with --yes to confirm.",
                        row.file_path,
                        human_bytes(row.size_bytes as u64),
                        row.worker_name
                    );
                    return Ok(());
                }
                let escaped_id = shell_escape_single(&id);
                let command = format!("ff model delete {escaped_id} --yes");
                let title = format!("Delete {} on {}", id, row.worker_name);
                let payload = serde_json::json!({ "command": command });
                let defer_id = ff_db::pg_enqueue_deferred(
                    &pool,
                    &title,
                    "shell",
                    &payload,
                    "now",
                    &serde_json::json!({}),
                    Some(&row.worker_name),
                    &serde_json::json!([]),
                    Some(&whoami_tag()),
                    Some(3),
                )
                .await?;
                println!(
                    "Enqueued cross-node delete of {} ({}) on {} as deferred task {defer_id}.",
                    row.file_path,
                    human_bytes(row.size_bytes as u64),
                    row.worker_name
                );
                println!(
                    "It runs when {}'s defer-worker claims it. Check: ff defer list",
                    row.worker_name
                );
                return Ok(());
            }

            if !yes {
                println!(
                    "This will delete {} ({}) from disk. Re-run with --yes to confirm.",
                    row.file_path,
                    human_bytes(row.size_bytes as u64)
                );
                return Ok(());
            }

            let path = std::path::Path::new(&row.file_path);
            let result = if path.is_dir() {
                tokio::fs::remove_dir_all(path).await
            } else {
                tokio::fs::remove_file(path).await
            };
            match result {
                Ok(()) => {
                    let _ = ff_db::pg_delete_library(&pool, &id).await?;
                    println!(
                        "Deleted {} ({}) from {}",
                        row.file_path,
                        human_bytes(row.size_bytes as u64),
                        row.worker_name
                    );
                }
                Err(e) => anyhow::bail!("filesystem remove failed: {e}"),
            }
        }
        crate::ModelCommand::Load {
            id,
            port,
            ctx,
            parallel,
            agent,
            mmproj,
        } => {
            let opts = ff_agent::model_runtime::LoadOptions {
                library_id: id.clone(),
                port,
                context_size: ctx,
                parallel,
                agent_profile: agent,
                mmproj_path: mmproj,
            };
            if agent {
                println!(
                    "{CYAN}▶ Loading library {} on port {port} (agent profile: --parallel 1, ctx >= {})...{RESET}",
                    id,
                    ff_agent::model_runtime::AGENT_MIN_CTX
                );
            } else {
                println!("{CYAN}▶ Loading library {} on port {port}...{RESET}", id);
            }
            match ff_agent::model_runtime::load_model(&pool, opts).await {
                Ok(res) => {
                    println!(
                        "{CYAN}✓ Loaded{RESET} — deployment {} pid {} @ http://127.0.0.1:{}",
                        res.deployment_id, res.pid, res.port
                    );
                }
                Err(e) => anyhow::bail!("load failed: {e}"),
            }
        }
        crate::ModelCommand::Autoload {
            catalog_id,
            ctx,
            node,
            agent,
        } => {
            let worker_name = ff_agent::fleet_info::resolve_this_worker_name().await;

            // Cross-node form: resolve user@ip from Postgres and run
            // `ff model autoload <catalog_id>` on the target over SSH. Built
            // from the DB (never ~/.ssh/config). Used by the P3 autoscaler's
            // remote-load dispatch (and operators).
            if let Some(target) = &node
                && !target.eq_ignore_ascii_case(&worker_name)
            {
                let node_row = ff_db::pg_get_node(&pool, target)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("node '{target}' not in fleet_workers"))?;
                let mut remote_cmd = format!(
                    "~/.local/bin/ff model autoload {}",
                    shell_escape_single(&catalog_id)
                );
                if let Some(c) = ctx {
                    remote_cmd.push_str(&format!(" --ctx {c}"));
                }
                if agent {
                    remote_cmd.push_str(" --agent");
                }
                println!(
                    "{CYAN}▶ Autoloading {catalog_id} on {target} ({}@{})...{RESET}",
                    node_row.ssh_user, node_row.ip
                );
                let (code, out, err) = ff_agent::model_transfer::ssh_exec(
                    &node_row.ssh_user,
                    &node_row.ip,
                    &remote_cmd,
                )
                .await
                .map_err(|e| anyhow::anyhow!("ssh to {target}: {e}"))?;
                if !out.trim().is_empty() {
                    print!("{out}");
                }
                if code != 0 {
                    anyhow::bail!("remote autoload on {target} exited {code}: {}", err.trim());
                }
                return Ok(());
            }

            // 1. Already deployed?
            let deps = ff_db::pg_list_deployments(&pool, Some(&worker_name)).await?;
            if let Some(d) = deps.iter().find(|d| {
                d.catalog_id.as_deref() == Some(&catalog_id) && d.health_status == "healthy"
            }) {
                println!("Already deployed on port {} (deployment {})", d.port, d.id);
                return Ok(());
            }

            // 2. Find library row on this node for this catalog_id.
            let libs = ff_db::pg_list_library(&pool, Some(&worker_name)).await?;
            let lib = libs.iter().find(|r| r.catalog_id == catalog_id)
                .ok_or_else(|| anyhow::anyhow!("model '{catalog_id}' not in library on '{worker_name}'. Download it first: ff model download {catalog_id}"))?;

            // 3. Pick a free port via port_registry — canonical mapping
            //    (55000-55002 llama.cpp/mlx, 51001/51003 vllm, 11434 ollama).
            //    Fall back to legacy 51001..=51020 scan only if the registry
            //    lookup fails (e.g. fresh install where it hasn't seeded yet).
            let port: u16 =
                match ff_agent::ports_registry::pick_llm_port(&pool, &worker_name, &lib.runtime)
                    .await
                {
                    Ok(p) => p as u16,
                    Err(_) => {
                        let used_ports: std::collections::HashSet<i32> =
                            deps.iter().map(|d| d.port).collect();
                        (51001u16..=51020)
                            .find(|p| !used_ports.contains(&(*p as i32)))
                            .ok_or_else(|| {
                                anyhow::anyhow!("no free port in registry or 51001-51020")
                            })?
                    }
                };

            // 4. Load.
            let res = ff_agent::model_runtime::load_model(
                &pool,
                ff_agent::model_runtime::LoadOptions {
                    library_id: lib.id.clone(),
                    port,
                    context_size: ctx,
                    parallel: None,
                    agent_profile: agent,
                    mmproj_path: None, // auto-detect sibling mmproj
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

            println!(
                "Autoloaded {} on port {} (deployment {})",
                catalog_id, res.port, res.deployment_id
            );
        }
        crate::ModelCommand::Unload { id, node } => {
            // Cross-node form: resolve user@ip from Postgres and run
            // `ff model unload <id>` on the target over SSH. We deliberately
            // build the destination from the DB (never ~/.ssh/config).
            if let Some(target) = node {
                let this_node = ff_agent::fleet_info::resolve_this_worker_name().await;
                if !target.eq_ignore_ascii_case(&this_node) {
                    let node_row = ff_db::pg_get_node(&pool, &target)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("node '{target}' not in fleet_workers"))?;
                    // Use the canonical install path: a non-login SSH session
                    // doesn't have ~/.local/bin on PATH, so bare `ff` exits 127.
                    let remote_cmd =
                        format!("~/.local/bin/ff model unload {}", shell_escape_single(&id));
                    println!(
                        "{CYAN}▶ Unloading deployment {id} on {target} ({}@{})...{RESET}",
                        node_row.ssh_user, node_row.ip
                    );
                    let (code, out, err) = ff_agent::model_transfer::ssh_exec(
                        &node_row.ssh_user,
                        &node_row.ip,
                        &remote_cmd,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("ssh to {target}: {e}"))?;
                    if !out.trim().is_empty() {
                        print!("{out}");
                    }
                    if code != 0 {
                        anyhow::bail!("remote unload on {target} exited {code}: {}", err.trim());
                    }
                    return Ok(());
                }
                // node == this host: fall through to the local path.
            }
            match ff_agent::model_runtime::unload_model(&pool, &id).await {
                Ok(()) => println!("Unloaded deployment {id}"),
                Err(e) => anyhow::bail!("unload failed: {e}"),
            }
        }
        crate::ModelCommand::Reprofile {
            id,
            ctx,
            force,
            json,
        } => {
            handle_reprofile(&pool, &id, ctx, force, json).await?;
        }
        crate::ModelCommand::Ps => {
            let procs = ff_agent::model_runtime::list_local_processes().await;
            if procs.is_empty() {
                println!("(no inference servers running)");
                return Ok(());
            }
            println!("{:<8} {:<10} {:<8} MODEL", "PID", "RUNTIME", "PORT");
            for p in procs {
                println!(
                    "{:<8} {:<10} {:<8} {}",
                    p.pid,
                    p.runtime,
                    p.port.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                    p.model_path.clone().unwrap_or_else(|| "-".into())
                );
            }
        }
        crate::ModelCommand::Info { id } => {
            // Try as catalog id first.
            if let Some(c) = ff_db::pg_get_catalog(&pool, &id).await? {
                println!("{CYAN}━ Catalog entry ━{RESET}");
                println!("ID:           {}", c.id);
                println!("Name:         {}", c.name);
                println!("Family:       {}", c.family);
                println!("Parameters:   {}", c.parameters);
                println!("Tier:         T{}", c.tier);
                println!(
                    "Gated:        {}",
                    if c.gated {
                        "yes (HF license required)"
                    } else {
                        "no"
                    }
                );
                println!(
                    "Tool calling: {}",
                    if c.tool_calling { "yes" } else { "no" }
                );
                if let Some(d) = &c.description {
                    println!("Description:  {d}");
                }
                if let Some(arr) = c.preferred_workloads.as_array() {
                    let wl: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    if !wl.is_empty() {
                        println!("Workloads:    {}", wl.join(", "));
                    }
                }
                if let Some(variants) = c.variants.as_array() {
                    println!("\nVariants:");
                    for v in variants {
                        let runtime = v.get("runtime").and_then(|x| x.as_str()).unwrap_or("?");
                        let quant = v.get("quant").and_then(|x| x.as_str()).unwrap_or("-");
                        let repo = v.get("hf_repo").and_then(|x| x.as_str()).unwrap_or("?");
                        let size = v.get("size_gb").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        println!("  - {runtime:<10} quant={quant:<8} {size:>6.1} GB  {repo}");
                    }
                }
                // Where is it on the fleet?
                let lib = ff_db::pg_list_library(&pool, None).await?;
                let copies: Vec<&ff_db::ModelLibraryRow> =
                    lib.iter().filter(|r| r.catalog_id == c.id).collect();
                if !copies.is_empty() {
                    println!("\nOn disk:");
                    for r in &copies {
                        let q = r.quant.clone().unwrap_or_else(|| "-".into());
                        println!(
                            "  - {:<10} ({:<10} {:<6}) {}  [{}]",
                            r.worker_name,
                            r.runtime,
                            q,
                            human_bytes(r.size_bytes as u64),
                            &r.id[..8]
                        );
                    }
                }
                let deps = ff_db::pg_list_deployments(&pool, None).await?;
                let live: Vec<&ff_db::ModelDeploymentRow> = deps
                    .iter()
                    .filter(|d| d.catalog_id.as_deref() == Some(&c.id))
                    .collect();
                if !live.is_empty() {
                    println!("\nDeployments:");
                    for d in &live {
                        println!(
                            "  - {:<10} port {:<5} {:<10} health={}  [{}]",
                            d.worker_name,
                            d.port,
                            d.runtime,
                            d.health_status,
                            &d.id[..8]
                        );
                    }
                }
                return Ok(());
            }
            // Try as library row UUID.
            let all_lib = ff_db::pg_list_library(&pool, None).await?;
            if let Some(r) = all_lib.iter().find(|r| r.id == id) {
                println!("{CYAN}━ Library row ━{RESET}");
                println!("ID:           {}", r.id);
                println!("Node:         {}", r.worker_name);
                println!("Catalog ID:   {}", r.catalog_id);
                println!("Runtime:      {}", r.runtime);
                println!(
                    "Quant:        {}",
                    r.quant.clone().unwrap_or_else(|| "-".into())
                );
                println!("File path:    {}", r.file_path);
                println!("Size:         {}", human_bytes(r.size_bytes as u64));
                if let Some(s) = &r.sha256 {
                    println!("SHA256:       {s}");
                }
                println!(
                    "Downloaded:   {}",
                    r.downloaded_at.format("%Y-%m-%d %H:%M UTC")
                );
                if let Some(t) = r.last_used_at {
                    println!("Last used:    {}", t.format("%Y-%m-%d %H:%M UTC"));
                }
                if let Some(s) = &r.source_url {
                    println!("Source:       {s}");
                }
                return Ok(());
            }
            // Try as deployment UUID.
            let all_dep = ff_db::pg_list_deployments(&pool, None).await?;
            if let Some(d) = all_dep.iter().find(|d| d.id == id) {
                println!("{CYAN}━ Deployment ━{RESET}");
                println!("ID:           {}", d.id);
                println!("Node:         {}", d.worker_name);
                println!(
                    "Catalog ID:   {}",
                    d.catalog_id.clone().unwrap_or_else(|| "-".into())
                );
                println!("Runtime:      {}", d.runtime);
                println!("Port:         {}", d.port);
                println!(
                    "PID:          {}",
                    d.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
                );
                println!("Health:       {}", d.health_status);
                println!(
                    "Started:      {}",
                    d.started_at.format("%Y-%m-%d %H:%M UTC")
                );
                if let Some(t) = d.last_health_at {
                    println!("Last health:  {}", t.format("%Y-%m-%d %H:%M UTC"));
                }
                if let Some(c) = d.context_window {
                    println!("Ctx window:   {c}");
                }
                println!("Tokens used:  {}", d.tokens_used);
                println!("Requests:     {}", d.request_count);
                return Ok(());
            }
            anyhow::bail!("'{id}' is not a known catalog id, library UUID, or deployment UUID");
        }
        crate::ModelCommand::Prune {
            node,
            min_cold_days,
            classified,
        } => {
            let worker_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_worker_name().await,
            };
            let policy = ff_agent::smart_lru::LruPolicy {
                min_cold_days,
                ..Default::default()
            };
            if classified {
                // V118 MOVE-vs-DELETE classified plan (always dry-run).
                let plan =
                    ff_agent::smart_lru::plan_classified_eviction(&pool, &worker_name, &policy)
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;
                if plan.candidates.is_empty() {
                    println!(
                        "Node '{worker_name}' is within quota or has no eligible candidates — no action."
                    );
                    return Ok(());
                }
                println!(
                    "Classified disk-reconcile plan for {worker_name} (would free {}):\n",
                    human_bytes(plan.total_bytes_freed)
                );
                println!(
                    "{:<24} {:<10} {:<8} {:<8} {:<10} REASONS",
                    "CATALOG", "RUNTIME", "SIZE", "ACTION", "TARGET"
                );
                for c in &plan.candidates {
                    println!(
                        "{:<24} {:<10} {:<8} {:<8} {:<10} {}",
                        c.catalog_id,
                        c.runtime,
                        human_bytes(c.size_bytes),
                        c.action.as_str(),
                        c.target_node.as_deref().unwrap_or("-"),
                        c.reasons.join(", ")
                    );
                }
                println!(
                    "\n(dry-run; the leader's disk-reconcile tick actuates this ONLY when fleet_secrets.disk_policy_mode=active)"
                );
                return Ok(());
            }
            let plan = ff_agent::smart_lru::plan_eviction(&pool, &worker_name, &policy)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            if plan.candidates.is_empty() {
                println!("Node '{worker_name}' is within quota — no eviction needed.");
                return Ok(());
            }
            println!(
                "Eviction plan for {worker_name} (would free {}):\n",
                human_bytes(plan.total_bytes_freed)
            );
            println!(
                "{:<38} {:<24} {:<10} {:<10} REASONS",
                "LIBRARY_ID", "CATALOG", "RUNTIME", "SIZE"
            );
            for c in &plan.candidates {
                println!(
                    "{:<38} {:<24} {:<10} {:<10} {}",
                    c.library_id,
                    c.catalog_id,
                    c.runtime,
                    human_bytes(c.size_bytes),
                    c.reasons.join(", ")
                );
            }
            println!("\n(dry-run; use `ff model delete <library-id> --yes` to actually remove)");
        }
        crate::ModelCommand::DiskSample => {
            match ff_agent::disk_sampler::sample_local_disk(&pool).await {
                Ok(s) => {
                    println!("Node:        {}", s.worker_name);
                    println!("Models dir:  {}", s.models_dir.display());
                    println!("Total:       {}", human_bytes(s.total_bytes));
                    println!("Used:        {}", human_bytes(s.used_bytes));
                    println!("Free:        {}", human_bytes(s.free_bytes));
                    println!("Models size: {}", human_bytes(s.models_bytes));
                    println!("Quota:       {}%", s.quota_pct);
                    println!("Over quota:  {}", s.over_quota);
                }
                Err(e) => anyhow::bail!("disk sample failed: {e}"),
            }
        }
        crate::ModelCommand::Ping { id } => {
            match ff_agent::model_runtime::health_check_deployment(&pool, &id).await {
                Ok(true) => println!("{CYAN}✓ healthy{RESET}"),
                Ok(false) => println!("{YELLOW}⚠ unhealthy (reachable but failing){RESET}"),
                Err(e) => anyhow::bail!("health check failed: {e}"),
            }
        }
        crate::ModelCommand::Transfer {
            library_id,
            from,
            to,
        } => {
            let opts = ff_agent::model_transfer::TransferOptions {
                source_node: from.clone(),
                target_node: to.clone(),
                library_id: library_id.clone(),
            };
            match ff_agent::model_transfer::transfer_model(&pool, opts).await {
                Ok(res) => {
                    println!(
                        "{CYAN}✓ transferred{RESET} {} bytes  new library id: {}",
                        res.bytes_transferred, res.target_library_id
                    );
                }
                Err(e) => anyhow::bail!("transfer failed: {e}"),
            }
        }
        crate::ModelCommand::Where { id_or_name } => {
            handle_model_where(&pool, &id_or_name).await?;
        }
        crate::ModelCommand::UpgradeAvailable => {
            handle_model_upgrade_available(&pool).await?;
        }
        crate::ModelCommand::Distribute {
            id_or_catalog,
            to,
            exclude,
            dry_run,
        } => {
            handle_model_distribute(&pool, &id_or_catalog, to.as_deref(), &exclude, dry_run)
                .await?;
        }
        crate::ModelCommand::Convert { library_id, q_bits } => {
            let opts = ff_agent::model_convert::ConvertOptions {
                library_id: library_id.clone(),
                quant_bits: q_bits,
                output_dir: None,
            };
            println!("{CYAN}▶ Converting library {library_id} to MLX ({q_bits}-bit)...{RESET}");
            match ff_agent::model_convert::convert_safetensors_to_mlx(&pool, opts).await {
                Ok(res) => {
                    println!(
                        "{CYAN}✓ converted{RESET} in {}s → {}  (new library id: {})",
                        res.duration_seconds,
                        res.output_path.display(),
                        res.new_library_id,
                    );
                }
                Err(e) => anyhow::bail!("convert failed: {e}"),
            }
        }
        crate::ModelCommand::Jobs { status, limit } => {
            let rows = ff_db::pg_list_jobs(&pool, status.as_deref(), limit).await?;
            if rows.is_empty() {
                println!("(no jobs)");
                return Ok(());
            }
            println!(
                "{:<38} {:<10} {:<12} {:<10} {:<7} TARGET",
                "ID", "NODE", "KIND", "STATUS", "PCT"
            );
            for r in rows {
                let target = r
                    .target_catalog_id
                    .clone()
                    .or(r.target_library_id.clone())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:<10} {:<12} {:<10} {:<6.1}% {}",
                    r.id, r.worker_name, r.kind, r.status, r.progress_pct, target
                );
            }
        }
        crate::ModelCommand::CheckUpstream { json } => {
            println!("{CYAN}▶ Checking HuggingFace for upstream model revisions...{RESET}");
            let checker = ff_agent::model_upstream::ModelUpstreamChecker::new(pool.clone());
            let report = checker
                .check_all()
                .await
                .map_err(|e| anyhow::anyhow!("model upstream check: {e}"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!(
                    "checked={} updated={} unchanged={} skipped={} errors={} flagged={}",
                    report.checked,
                    report.updated,
                    report.unchanged,
                    report.skipped,
                    report.errors.len(),
                    report.computer_rows_flagged,
                );
                if !report.errors.is_empty() {
                    println!("\n{YELLOW}Errors:{RESET}");
                    for (id, err) in &report.errors {
                        println!("  {id}: {err}");
                    }
                }
            }
        }
        crate::ModelCommand::Coverage { json } => {
            let guard = ff_agent::coverage_guard::CoverageGuard::new_dbonly(pool.clone());
            let report = guard
                .check_once()
                .await
                .map_err(|e| anyhow::anyhow!("coverage check: {e}"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!("Tasks required:   {}", report.tasks_required);
                println!("Tasks covered:    {}", report.tasks_covered);
                println!("Gaps:             {}", report.gaps.len());
                println!("Auto-loaded:      {}", report.auto_loaded.len());
                if !report.gaps.is_empty() {
                    println!();
                    println!("{:<32} {:<6} {:<6}  CANDIDATES", "TASK", "MIN", "LOAD");
                    for g in &report.gaps {
                        let cands = if g.candidates.is_empty() {
                            "(none)".to_string()
                        } else {
                            g.candidates
                                .iter()
                                .take(3)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        println!(
                            "{:<32} {:<6} {:<6}  {}",
                            g.task, g.min_required, g.currently_loaded, cands
                        );
                    }
                }
                if !report.auto_loaded.is_empty() {
                    println!();
                    println!(
                        "{GREEN}Enqueued auto-load for:{RESET} {}",
                        report.auto_loaded.join(", ")
                    );
                }
            }
        }
        crate::ModelCommand::Scout { run_now, json } => {
            if run_now {
                println!("{CYAN}▶ Running model scout pass...{RESET}");
                let scout = ff_agent::model_scout::ModelScout::new(pool.clone());
                let report = scout
                    .scout_once()
                    .await
                    .map_err(|e| anyhow::anyhow!("scout: {e}"))?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).unwrap_or_default()
                    );
                } else {
                    println!(
                        "tasks_scanned={} discovered={} added={} filtered={}",
                        report.tasks_scanned,
                        report.discovered,
                        report.added_as_candidates,
                        report.filtered_out,
                    );
                }
            } else {
                let rows = sqlx::query(
                    "SELECT id, display_name, family, license
                     FROM model_catalog
                     WHERE lifecycle_status = 'candidate' AND added_by = 'scout'
                     ORDER BY id
                     LIMIT 100",
                )
                .fetch_all(&pool)
                .await?;
                if json {
                    let arr: Vec<_> = rows
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "id": sqlx::Row::get::<String, _>(r, "id"),
                                "display_name": sqlx::Row::get::<String, _>(r, "display_name"),
                                "family": sqlx::Row::get::<String, _>(r, "family"),
                                "license": sqlx::Row::get::<Option<String>, _>(r, "license"),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
                } else if rows.is_empty() {
                    println!("(no scout candidates — pass --run-now to trigger a pass)");
                } else {
                    println!("{:<40} {:<16} {:<20} NAME", "ID", "FAMILY", "LICENSE");
                    for r in &rows {
                        let id: String = sqlx::Row::get(r, "id");
                        let name: String = sqlx::Row::get(r, "display_name");
                        let fam: String = sqlx::Row::get(r, "family");
                        let lic: Option<String> = sqlx::Row::get(r, "license");
                        println!(
                            "{:<40} {:<16} {:<20} {}",
                            id,
                            fam,
                            lic.unwrap_or_else(|| "-".into()),
                            name
                        );
                    }
                }
            }
        }
        crate::ModelCommand::ReviewCandidates { json } => {
            let rows = sqlx::query(
                "SELECT id, display_name, family, license, added_by, tasks
                 FROM model_catalog
                 WHERE lifecycle_status = 'candidate'
                 ORDER BY added_by, id",
            )
            .fetch_all(&pool)
            .await?;
            if json {
                let arr: Vec<_> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": sqlx::Row::get::<String, _>(r, "id"),
                            "display_name": sqlx::Row::get::<String, _>(r, "display_name"),
                            "family": sqlx::Row::get::<String, _>(r, "family"),
                            "license": sqlx::Row::get::<Option<String>, _>(r, "license"),
                            "added_by": sqlx::Row::get::<Option<String>, _>(r, "added_by"),
                            "tasks": sqlx::Row::get::<serde_json::Value, _>(r, "tasks"),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
            } else if rows.is_empty() {
                println!("(no candidates awaiting review)");
            } else {
                println!(
                    "{:<40} {:<10} {:<16} {:<20} TASKS",
                    "ID", "ADDED_BY", "FAMILY", "LICENSE"
                );
                for r in &rows {
                    let id: String = sqlx::Row::get(r, "id");
                    let fam: String = sqlx::Row::get(r, "family");
                    let lic: Option<String> = sqlx::Row::get(r, "license");
                    let added: Option<String> = sqlx::Row::get(r, "added_by");
                    let tasks: serde_json::Value = sqlx::Row::get(r, "tasks");
                    let tasks_str = tasks
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    println!(
                        "{:<40} {:<10} {:<16} {:<20} {}",
                        id,
                        added.unwrap_or_else(|| "-".into()),
                        fam,
                        lic.unwrap_or_else(|| "-".into()),
                        tasks_str,
                    );
                }
                println!("\nApprove with: ff model approve <id>");
                println!("Reject with:  ff model reject <id>");
            }
        }
        crate::ModelCommand::Approve {
            id,
            skip_benchmark,
            force,
            on_computer,
        } => {
            // 1. Verify the candidate exists and is still in review.
            let row = sqlx::query("SELECT lifecycle_status FROM model_catalog WHERE id = $1")
                .bind(&id)
                .fetch_optional(&pool)
                .await?;
            let Some(row) = row else {
                anyhow::bail!("no catalog row found for id '{id}'");
            };
            let status: String = sqlx::Row::get(&row, "lifecycle_status");
            if status != "candidate" {
                anyhow::bail!(
                    "model '{id}' is in lifecycle_status='{status}' — only 'candidate' rows can be approved"
                );
            }

            let skip = skip_benchmark || force;
            let mut bench_summary: Option<ff_agent::model_benchmark::BenchmarkReport> = None;

            // 2. Benchmark gate (unless skipped).
            if !skip {
                // Open a Pulse reader so we can pick a target and find
                // any healthy loaded endpoint.
                let redis_url = std::env::var("FORGEFLEET_REDIS_URL")
                    .unwrap_or_else(|_| "redis://127.0.0.1:56379".into());
                let pulse = match ff_pulse::reader::PulseReader::new(&redis_url) {
                    Ok(p) => p,
                    Err(e) => {
                        anyhow::bail!(
                            "can't open Pulse at {redis_url}: {e}\n\
                             Either fix Redis connectivity, or re-run with --skip-benchmark."
                        );
                    }
                };

                // Pick the target computer.
                let target = if let Some(c) = on_computer.clone() {
                    c
                } else {
                    match ff_agent::model_benchmark::pick_benchmark_target(&pool, &pulse, &id).await
                    {
                        Ok(Some(n)) => n,
                        Ok(None) => {
                            anyhow::bail!(
                                "no compatible node found to benchmark '{id}' \
                                 (check required_gpu_kind / min_vram_gb / file_size_gb \
                                 vs live Pulse beats). \
                                 Use --on-computer <name> to force one, or \
                                 --skip-benchmark to approve without benchmarking."
                            );
                        }
                        Err(e) => anyhow::bail!("pick_benchmark_target failed: {e}"),
                    }
                };

                println!("{CYAN}→{RESET} Benchmarking '{id}' on '{target}' before promotion…");

                let bencher = ff_agent::model_benchmark::ModelBenchmarker::new(pool.clone(), pulse);
                match bencher.benchmark(&id, &target).await {
                    Ok(report) => {
                        if !report.bench_pass {
                            eprintln!(
                                "{RED}✗ Benchmark failed:{RESET} {}\n  \
                                 tokens/sec: {:.2}\n  \
                                 ttft (ms):  {}\n  \
                                 endpoint:   {}\n\n\
                                 Inspect results with: ff model benchmarks --model {id}\n\
                                 Force anyway with:     ff model approve {id} --skip-benchmark",
                                report.bench_pass_reason,
                                report.tokens_per_sec,
                                report.ttft_ms,
                                report.endpoint,
                            );
                            std::process::exit(1);
                        }
                        bench_summary = Some(report);
                    }
                    Err(ff_agent::model_benchmark::BenchError::NotLoaded(m, c)) => {
                        eprintln!(
                            "{RED}✗ Cannot benchmark:{RESET} model '{m}' is not loaded \
                             on '{c}' (no active+healthy LLM server found in Pulse).\n\n\
                             Either:\n  \
                               • load it first:   ff model load <library_id> --port 51001\n  \
                               • pick a node that has it loaded: --on-computer <name>\n  \
                               • skip the benchmark: --skip-benchmark"
                        );
                        std::process::exit(1);
                    }
                    Err(e) => anyhow::bail!("benchmark error: {e}"),
                }
            }

            // 3. Promote to active (idempotent-safe: we re-check the gate).
            let result = sqlx::query(
                "UPDATE model_catalog
                    SET lifecycle_status = 'active'
                  WHERE id = $1 AND lifecycle_status = 'candidate'",
            )
            .bind(&id)
            .execute(&pool)
            .await?;
            if result.rows_affected() == 0 {
                anyhow::bail!("race: candidate '{id}' was changed by someone else during approval");
            }

            // 4. Report.
            println!("{GREEN}✓{RESET} Promoted '{id}' to lifecycle_status='active'");
            if let Some(r) = bench_summary {
                println!("  benchmark pass:   yes");
                println!("  computer:         {}", r.computer);
                println!("  endpoint:         {}", r.endpoint);
                println!("  tokens/sec:       {:.2}", r.tokens_per_sec);
                println!("  ttft (ms):        {}", r.ttft_ms);
                println!("  prompts:          {}", r.prompt_count);
            } else {
                println!("  benchmark pass:   (skipped)");
            }
        }
        crate::ModelCommand::Reject { id } => {
            let row = sqlx::query(
                "SELECT upstream_id FROM model_catalog
                  WHERE id = $1 AND lifecycle_status = 'candidate'",
            )
            .bind(&id)
            .fetch_optional(&pool)
            .await?;
            let Some(row) = row else {
                anyhow::bail!("no candidate row found for id '{id}'");
            };
            let upstream_id: Option<String> = sqlx::Row::get(&row, "upstream_id");

            let deleted = sqlx::query(
                "DELETE FROM model_catalog
                  WHERE id = $1 AND lifecycle_status = 'candidate'",
            )
            .bind(&id)
            .execute(&pool)
            .await?;
            if deleted.rows_affected() == 0 {
                anyhow::bail!("failed to delete candidate '{id}'");
            }

            if let Some(up) = upstream_id {
                let inserted = sqlx::query(
                    "INSERT INTO model_scout_denylist (model_id, reason, added_by)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (model_id) DO NOTHING",
                )
                .bind(up.to_ascii_lowercase())
                .bind(Some("ff model reject"))
                .bind(whoami_tag())
                .execute(&pool)
                .await?;
                if inserted.rows_affected() == 1 {
                    println!(
                        "{GREEN}✓{RESET} Rejected '{id}' and added upstream_id '{up}' to denylist"
                    );
                } else {
                    println!(
                        "{GREEN}✓{RESET} Rejected '{id}' (upstream '{up}' already in denylist)"
                    );
                }
            } else {
                println!("{GREEN}✓{RESET} Rejected '{id}' (no upstream_id to denylist)");
            }
        }
        crate::ModelCommand::Retire {
            id,
            replace_with,
            reason,
        } => {
            let result = sqlx::query(
                "UPDATE model_catalog
                    SET lifecycle_status   = 'retired',
                        replaced_by        = COALESCE($2, replaced_by),
                        retirement_reason  = $3,
                        retirement_date    = CURRENT_DATE
                  WHERE id = $1",
            )
            .bind(&id)
            .bind(replace_with.as_deref())
            .bind(&reason)
            .execute(&pool)
            .await?;
            if result.rows_affected() == 0 {
                anyhow::bail!("no catalog row for id '{id}'");
            }
            match replace_with {
                Some(rep) => println!("{GREEN}✓{RESET} Retired '{id}' (replaced by '{rep}')"),
                None => println!("{GREEN}✓{RESET} Retired '{id}'"),
            }
        }
        crate::ModelCommand::Benchmark {
            model_id,
            computer,
            json,
        } => {
            let computer = if let Some(c) = computer {
                c
            } else {
                ff_agent::fleet_info::resolve_this_worker_name().await
            };
            match ff_agent::model_benchmark::benchmark_with_defaults(&pool, &model_id, &computer)
                .await
            {
                Ok(report) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&report).unwrap_or_default()
                        );
                    } else {
                        println!("{GREEN}✓ Benchmark complete{RESET}");
                        println!("  model:            {}", report.model_id);
                        println!("  computer:         {}", report.computer);
                        println!("  runtime:          {}", report.runtime);
                        println!("  endpoint:         {}", report.endpoint);
                        println!("  tokens/sec:       {:.2}", report.tokens_per_sec);
                        println!("  ttft (ms):        {}", report.ttft_ms);
                        println!("  prompt eval/sec:  {:.2}", report.prompt_eval_rate);
                        println!("  max ctx tokens:   {}", report.context_tokens_max);
                        println!("  prompt count:     {}", report.prompt_count);
                    }
                }
                Err(e) => {
                    eprintln!("{RED}✗ Benchmark failed: {e}{RESET}");
                    std::process::exit(1);
                }
            }
        }
        crate::ModelCommand::Benchmarks { model } => {
            let target = model.unwrap_or_else(|| {
                eprintln!(
                    "{YELLOW}No --model specified; pass --model <catalog_id> to narrow.{RESET}"
                );
                String::new()
            });
            if target.is_empty() {
                return Ok(());
            }
            match ff_db::pg_get_benchmark_results(&pool, &target).await? {
                Some(v) => {
                    if let Some(obj) = v.as_object() {
                        if obj.is_empty() {
                            println!("(no benchmark runs recorded for '{target}')");
                        } else {
                            println!("{:<48} {:<12} {:<12}", "RUN KEY", "TOKENS/S", "TTFT(ms)");
                            for (key, run) in obj {
                                let tps = run
                                    .get("tokens_per_sec")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let ttft = run.get("ttft_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                                println!("{:<48} {:<12.2} {:<12}", key, tps, ttft);
                            }
                        }
                    } else {
                        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    }
                }
                None => {
                    eprintln!("No catalog row for id '{target}'");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

/// `ff model where <id-or-name>` — show every location of a model across the fleet.
///
/// Accepts:
///   - exact library UUID
///   - exact catalog_id (e.g. "qwen3-next-80b-a3b")
///   - case-insensitive substring (matches catalog_id, name, or partial path)
async fn handle_model_where(pool: &sqlx::PgPool, query: &str) -> anyhow::Result<()> {
    use crate::CYAN;
    use crate::RESET;
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        String,
    )> = sqlx::query_as(
        r#"
        SELECT
            lib.id::text,
            lib.worker_name,
            lib.catalog_id,
            lib.runtime,
            lib.quant,
            lib.size_bytes,
            lib.file_path,
            lib.last_used_at,
            lib.state
        FROM fleet_model_library lib
        WHERE
            lib.id::text = $1
            OR lib.catalog_id = $1
            OR lib.catalog_id ILIKE '%' || $1 || '%'
            OR lib.file_path ILIKE '%' || $1 || '%'
        ORDER BY lib.worker_name, lib.catalog_id
        "#,
    )
    .bind(query)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        println!("(no library rows match '{query}')");
        return Ok(());
    }
    println!(
        "{CYAN}{:<10} {:<28} {:<10} {:<8} {:<5} {:>9}  {}{RESET}",
        "COMPUTER", "MODEL", "RUNTIME", "QUANT", "STATE", "SIZE", "PATH / last_used"
    );
    for (lib_id, worker, catalog, runtime, quant, size, path, last_used, state) in &rows {
        let gb = (*size as f64) / 1024.0 / 1024.0 / 1024.0;
        let size_s = if gb >= 1.0 {
            format!("{:.1} GB", gb)
        } else {
            format!("{} MB", size / 1024 / 1024)
        };
        let used = last_used
            .as_ref()
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<10} {:<28} {:<10} {:<8} {:<5} {:>9}  {}",
            worker,
            truncate_str(catalog, 28),
            runtime,
            quant.as_deref().unwrap_or("-"),
            state,
            size_s,
            path
        );
        println!("           id={}  last_used={}", lib_id, used);
    }
    Ok(())
}

/// `ff model distribute <id-or-catalog>` — auto-pick destination host based on
/// runtime fit + free disk, then transfer.
///
/// Algorithm:
///   1. Resolve query to library row(s). If `id_or_catalog` is a UUID, use it
///      directly. If it's a catalog_id with multiple copies, pick the row whose
///      worker is the most-loaded (we want to move FROM the most-burdened host).
///   2. Read latest fleet_disk_usage per host, filter out the excluded set
///      (defaults: source host + taylor leader).
///   3. Rank candidates by (free_bytes desc, model_count asc) — prefer hosts
///      with most free disk that don't already hold many models.
///   4. Pick top candidate; print plan; transfer (unless --dry-run).
async fn handle_model_distribute(
    pool: &sqlx::PgPool,
    id_or_catalog: &str,
    pinned_to: Option<&str>,
    exclude_csv: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    use crate::{CYAN, RESET, YELLOW};

    // Step 1: resolve library row to move (source). Refuse rows that are
    // currently being served (state='hot') — moving a file out from
    // under a running mmap is asking for trouble. Operator can drop
    // the active deployment first (`ff model unload <dep-id>`) or
    // pass --force (not implemented yet) to override.
    let row: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        String,
    )> = sqlx::query_as(
        r#"
            SELECT lib.id::text, lib.worker_name, lib.catalog_id, lib.runtime,
                   lib.quant, lib.size_bytes, lib.file_path, lib.state
              FROM fleet_model_library lib
             WHERE lib.id::text = $1 OR lib.catalog_id = $1
             ORDER BY lib.size_bytes DESC
             LIMIT 1
            "#,
    )
    .bind(id_or_catalog)
    .fetch_optional(pool)
    .await?;
    let Some((lib_id, source_worker, catalog_id, runtime, _quant, size_bytes, source_path, state)) =
        row
    else {
        anyhow::bail!("no library row found matching '{id_or_catalog}'");
    };
    if state == "hot" {
        anyhow::bail!(
            "library row {lib_id} is state='hot' (actively serving); unload the deployment first or wait for it to retire"
        );
    }

    let source_gb = (size_bytes as f64) / 1024.0 / 1024.0 / 1024.0;
    println!(
        "{CYAN}source{RESET}      {} on {} ({} runtime, {:.1} GB)",
        catalog_id, source_worker, runtime, source_gb
    );

    // Step 2: build exclude set.
    let mut excludes: std::collections::HashSet<String> = exclude_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    excludes.insert(source_worker.clone());

    // Step 3: candidates with free disk.
    //
    // Reserved-host policy (skipped by default, can be overridden with --to):
    //   - Taylor (leader) — daily-use host, never default for cold storage
    //   - DGX hosts (os_family='linux-dgx') — reserved for training
    //
    // Everything else is eligible. Among eligible hosts, just pick by free
    // disk DESC then model_count ASC. No class-based preference.
    let candidates: Vec<(String, i64, i64)> = sqlx::query_as(
        r#"
        WITH free AS (
            SELECT DISTINCT ON (worker_name) worker_name, free_bytes
              FROM fleet_disk_usage
             WHERE worker_name = ANY (
               SELECT name FROM fleet_workers WHERE status = 'online'
             )
             ORDER BY worker_name, sampled_at DESC
        ),
        load AS (
            SELECT worker_name, count(*) AS model_count
              FROM fleet_model_library
             GROUP BY worker_name
        ),
        reserved AS (
            SELECT name AS worker_name
              FROM computers
             WHERE os_family = 'linux-dgx'
                OR name = 'taylor'
        )
        SELECT f.worker_name,
               f.free_bytes,
               COALESCE(l.model_count, 0) AS model_count
          FROM free f
          LEFT JOIN load l ON l.worker_name = f.worker_name
         WHERE f.free_bytes > $1
           AND f.worker_name NOT IN (SELECT worker_name FROM reserved)
         ORDER BY f.free_bytes DESC,
                  COALESCE(l.model_count, 0) ASC
        "#,
    )
    .bind((size_bytes as f64 * 1.5) as i64)
    .fetch_all(pool)
    .await?;

    let mut filtered: Vec<&(String, i64, i64)> = candidates
        .iter()
        .filter(|(name, _, _)| !excludes.contains(name))
        .collect();
    if filtered.is_empty() {
        anyhow::bail!(
            "no candidate host with enough free disk (need {:.1} GB × 1.5; reserved hosts: taylor + DGX; excludes={:?})",
            source_gb,
            excludes
        );
    }

    let pick = if let Some(pinned) = pinned_to {
        filtered
            .iter()
            .find(|(n, _, _)| n == pinned)
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "pinned host '{pinned}' is not in candidate set (online + enough disk + not reserved + not excluded)"
                )
            })?
    } else {
        filtered.remove(0)
    };

    let target_gb = (pick.1 as f64) / 1024.0 / 1024.0 / 1024.0;
    println!(
        "{CYAN}target{RESET}      {} ({:.1} GB free, {} models on disk)",
        pick.0, target_gb, pick.2
    );
    println!(
        "{CYAN}plan{RESET}        rsync {} → {}:~/models/{}/{}",
        source_path, pick.0, runtime, catalog_id
    );

    if dry_run {
        println!("{YELLOW}(dry-run){RESET} no transfer dispatched");
        return Ok(());
    }

    // Step 4: dispatch transfer via existing model_transfer module.
    let opts = ff_agent::model_transfer::TransferOptions {
        source_node: source_worker.clone(),
        target_node: pick.0.clone(),
        library_id: lib_id.clone(),
    };
    println!("{CYAN}▶ transferring...{RESET}");
    match ff_agent::model_transfer::transfer_model(pool, opts).await {
        Ok(res) => {
            println!(
                "{CYAN}✓ done{RESET}      {} bytes  new library_id={}",
                res.bytes_transferred, res.target_library_id
            );
            Ok(())
        }
        Err(e) => anyhow::bail!("transfer failed: {e}"),
    }
}

/// `ff model upgrade-available` — list catalog rows where upstream HF revision
/// has moved past what's on disk. Driven by the daily `ModelUpstreamChecker`
/// tick which writes `model_catalog.upstream_latest_rev` and flips
/// `computer_models.status = 'revision_available'` for stale rows.
async fn handle_model_upgrade_available(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use crate::CYAN;
    use crate::RESET;
    use crate::YELLOW;

    let rows: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        r#"
            SELECT mc.id, mc.upstream_id,
                   mc.upstream_latest_rev,
                   string_agg(DISTINCT cm.status, ',') AS install_statuses,
                   max(mc.upstream_checked_at)         AS last_checked
              FROM model_catalog mc
              LEFT JOIN computer_models cm
                ON cm.model_id = mc.id AND cm.present = true
             WHERE mc.upstream_id IS NOT NULL
               AND mc.upstream_latest_rev IS NOT NULL
               AND EXISTS (
                 SELECT 1 FROM computer_models cm2
                  WHERE cm2.model_id = mc.id
                    AND cm2.status = 'revision_available'
               )
             GROUP BY mc.id, mc.upstream_id, mc.upstream_latest_rev
             ORDER BY last_checked DESC NULLS LAST
            "#,
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        println!("{CYAN}✓ all catalog models match upstream{RESET}");
        return Ok(());
    }

    println!(
        "{CYAN}{:<24} {:<48} {:<14} {}{RESET}",
        "MODEL", "UPSTREAM_REPO", "LATEST_REV", "LAST_CHECKED"
    );
    for (id, upstream_id, rev, _statuses, last_checked) in &rows {
        let rev_short = rev
            .as_deref()
            .map(|s| s.chars().take(10).collect::<String>())
            .unwrap_or_default();
        let checked = last_checked
            .as_ref()
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        println!(
            "{:<24} {:<48} {:<14} {}",
            truncate_str(id, 24),
            truncate_str(upstream_id, 48),
            rev_short,
            checked
        );
    }
    println!();
    println!(
        "{YELLOW}Tip:{RESET} this only shows DETECTED drift. The auto-upgrade verb is task #140 — \
         for now, manually `ff model download <id> --force --node <canonical>` on the canonical \
         owner, then verify + delete old file."
    );
    Ok(())
}

fn truncate_str(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_agent::model_runtime::AGENT_MIN_CTX;

    #[test]
    fn agent_ready_at_or_above_floor() {
        let floor = AGENT_MIN_CTX as i32;
        // logan qwen36 32768x1 — exactly the floor → ready.
        assert!(is_agent_ready(Some(floor), floor));
        // taylor mlx 65536x1 — above floor → ready.
        assert!(is_agent_ready(Some(65536), floor));
    }

    #[test]
    fn reprofile_candidate_below_floor() {
        let floor = AGENT_MIN_CTX as i32;
        // lily/sia 8192x4, veronica 4096x4 — per-slot ctx below floor.
        assert!(!is_agent_ready(Some(8192), floor));
        assert!(!is_agent_ready(Some(4096), floor));
        assert!(!is_agent_ready(Some(floor - 1), floor));
    }

    #[test]
    fn unknown_ctx_is_not_ready() {
        // NULL usable_agent_ctx (pre-backfill / unknown) must not be trusted.
        assert!(!is_agent_ready(None, AGENT_MIN_CTX as i32));
    }

    #[test]
    fn ram_headroom_gates_on_floor() {
        let floor = REPROFILE_MIN_FREE_RAM_GB;
        // Exactly at the floor is OK; below is refused; well above is OK.
        assert!(ram_headroom_ok(floor, floor));
        assert!(ram_headroom_ok(64.0, floor));
        assert!(!ram_headroom_ok(floor - 0.1, floor));
        // A memory-tight host (negative conservative free RAM) is refused.
        assert!(!ram_headroom_ok(-2.0, floor));
    }
}
