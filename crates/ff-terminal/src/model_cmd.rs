use anyhow::Result;
use crate::{expand_tilde, human_bytes, shell_escape_single, trunc_for_status, whoami_tag, CYAN, GREEN, RED, RESET, YELLOW};
use std::path::PathBuf;

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
        crate::ModelCommand::Library { node } => {
            let rows = ff_db::pg_list_library(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(library empty — run `ff model scan` to index your local models dir)");
                return Ok(());
            }
            println!(
                "{:<10} {:<28} {:<10} {:<10} {:<10} PATH",
                "NODE", "CATALOG_ID", "RUNTIME", "QUANT", "SIZE"
            );
            for r in rows {
                let sz = human_bytes(r.size_bytes as u64);
                let quant = r.quant.clone().unwrap_or_else(|| "-".into());
                println!(
                    "{:<10} {:<28} {:<10} {:<10} {:<10} {}",
                    r.node_name, r.catalog_id, r.runtime, quant, sz, r.file_path
                );
            }
        }
        crate::ModelCommand::Deployments { node } => {
            let rows = ff_db::pg_list_deployments(&pool, node.as_deref()).await?;
            if rows.is_empty() {
                println!("(no deployments recorded)");
                return Ok(());
            }
            println!(
                "{:<10} {:<28} {:<10} {:<6} {:<10} STARTED",
                "NODE", "CATALOG_ID", "RUNTIME", "PORT", "HEALTH"
            );
            for r in rows {
                let catalog = r.catalog_id.clone().unwrap_or_else(|| "-".into());
                println!(
                    "{:<10} {:<28} {:<10} {:<6} {:<10} {}",
                    r.node_name,
                    catalog,
                    r.runtime,
                    r.port,
                    r.health_status,
                    r.started_at.format("%Y-%m-%d %H:%M UTC")
                );
            }
        }
        crate::ModelCommand::Scan { node, models_dir } => {
            // Default: resolve this host's node name from Postgres by IP.
            let node_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_node_name().await,
            };
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let default_dir = PathBuf::from(home).join("models");
            let dir = models_dir.unwrap_or(default_dir);

            if !dir.exists() {
                anyhow::bail!("models dir does not exist: {}", dir.display());
            }
            println!("Scanning {} on node {} ...", dir.display(), node_name);
            let summary =
                ff_agent::model_library_scanner::scan_local_library(&pool, &node_name, &dir)
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
            let node_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_node_name().await,
            };
            let node_row = ff_db::pg_get_node(&pool, &node_name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("node '{node_name}' not in fleet_nodes"))?;
            let target_runtime = runtime.unwrap_or_else(|| node_row.runtime.clone());
            if target_runtime == "unknown" {
                anyhow::bail!(
                    "node '{node_name}' has unknown runtime; set with: ff config set fleet.{node_name}.runtime mlx|llama.cpp|vllm"
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
            let this_node = ff_agent::fleet_info::resolve_this_node_name().await;
            if node_name != this_node {
                let escaped_id = shell_escape_single(&id);
                let command = format!(
                    "ff model download {} --runtime {}",
                    escaped_id, target_runtime
                );
                let title = format!(
                    "Download {} ({} variant) on {}",
                    id, target_runtime, node_name
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
                    Some(&node_name),
                    &serde_json::json!([]),
                    Some(&whoami_tag()),
                    Some(3),
                )
                .await?;
                println!(
                    "Enqueued cross-node download as deferred task {defer_id}. It will run on {node_name} when a defer-worker there claims it."
                );
                println!("Check status with: ff defer list");
                return Ok(());
            }

            // Compute destination dir under models_dir.
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let models_dir = expand_tilde(&node_row.models_dir, &home);
            let dest = models_dir.join(&id);

            // HF token (optional — gated models need it).
            let token = ff_agent::fleet_info::get_hf_token().await;
            if catalog.gated && token.is_none() {
                anyhow::bail!(
                    "model '{id}' is gated on HF; set token first with: ff secrets set huggingface.token <hf_xxx>"
                );
            }

            // Allow patterns: prefer runtime-specific glob to avoid pulling everything.
            let allow_patterns: Vec<String> = match target_runtime.as_str() {
                "llama.cpp" => vec!["*.gguf".into(), "tokenizer*".into(), "*config*".into()],
                "mlx" | "vllm" => vec![
                    "*.safetensors".into(),
                    "*.json".into(),
                    "tokenizer*".into(),
                    "*config*".into(),
                    "README*".into(),
                ],
                other => vec![format!("*.{other}")],
            };
            let deny_patterns: Vec<String> = vec!["*.f16*".into(), "*.bf16*".into()];

            let _ = force; // not yet used; resume-by-size is automatic

            // Create job row for tracking.
            let params = serde_json::json!({
                "hf_repo": hf_repo,
                "runtime": target_runtime,
                "quant": quant,
                "dest": dest.to_string_lossy(),
            });
            let job_id =
                ff_db::pg_create_job(&pool, &node_name, "download", Some(&id), None, &params)
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

            let result = ff_agent::hf_download::download_repo(opts, move |p| {
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
                        &node_name,
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
                .ok_or_else(|| anyhow::anyhow!("node '{node}' not in fleet_nodes"))?;
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
            let deployments = ff_db::pg_list_deployments(&pool, Some(&row.node_name)).await?;
            let in_use = deployments
                .iter()
                .any(|d| d.library_id.as_deref() == Some(&id));
            if in_use {
                anyhow::bail!(
                    "model is currently deployed on {} — unload it first (`ff model unload <deployment_id>`)",
                    row.node_name
                );
            }

            // Cross-node delete not yet wired — only this host.
            let this_node = ff_agent::fleet_info::resolve_this_node_name().await;
            if row.node_name != this_node {
                anyhow::bail!(
                    "cross-node delete not yet implemented. run on '{}' instead.",
                    row.node_name
                );
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
                        row.node_name
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
        } => {
            let opts = ff_agent::model_runtime::LoadOptions {
                library_id: id.clone(),
                port,
                context_size: ctx,
                parallel,
            };
            println!("{CYAN}▶ Loading library {} on port {port}...{RESET}", id);
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
        crate::ModelCommand::Autoload { catalog_id, ctx } => {
            let node_name = ff_agent::fleet_info::resolve_this_node_name().await;

            // 1. Already deployed?
            let deps = ff_db::pg_list_deployments(&pool, Some(&node_name)).await?;
            if let Some(d) = deps.iter().find(|d| {
                d.catalog_id.as_deref() == Some(&catalog_id) && d.health_status == "healthy"
            }) {
                println!("Already deployed on port {} (deployment {})", d.port, d.id);
                return Ok(());
            }

            // 2. Find library row on this node for this catalog_id.
            let libs = ff_db::pg_list_library(&pool, Some(&node_name)).await?;
            let lib = libs.iter().find(|r| r.catalog_id == catalog_id)
                .ok_or_else(|| anyhow::anyhow!("model '{catalog_id}' not in library on '{node_name}'. Download it first: ff model download {catalog_id}"))?;

            // 3. Pick a free port via port_registry — canonical mapping
            //    (55000-55002 llama.cpp/mlx, 51001/51003 vllm, 11434 ollama).
            //    Fall back to legacy 51001..=51020 scan only if the registry
            //    lookup fails (e.g. fresh install where it hasn't seeded yet).
            let port: u16 = match ff_agent::ports_registry::pick_llm_port(
                &pool,
                &node_name,
                &lib.runtime,
            )
            .await
            {
                Ok(p) => p as u16,
                Err(_) => {
                    let used_ports: std::collections::HashSet<i32> =
                        deps.iter().map(|d| d.port).collect();
                    (51001u16..=51020)
                        .find(|p| !used_ports.contains(&(*p as i32)))
                        .ok_or_else(|| anyhow::anyhow!("no free port in registry or 51001-51020"))?
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
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

            println!(
                "Autoloaded {} on port {} (deployment {})",
                catalog_id, res.port, res.deployment_id
            );
        }
        crate::ModelCommand::Unload { id } => {
            match ff_agent::model_runtime::unload_model(&pool, &id).await {
                Ok(()) => println!("Unloaded deployment {id}"),
                Err(e) => anyhow::bail!("unload failed: {e}"),
            }
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
                            r.node_name,
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
                            d.node_name,
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
                println!("Node:         {}", r.node_name);
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
                println!("Node:         {}", d.node_name);
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
        } => {
            let node_name = match node {
                Some(n) => n,
                None => ff_agent::fleet_info::resolve_this_node_name().await,
            };
            let policy = ff_agent::smart_lru::LruPolicy {
                min_cold_days,
                ..Default::default()
            };
            let plan = ff_agent::smart_lru::plan_eviction(&pool, &node_name, &policy)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            if plan.candidates.is_empty() {
                println!("Node '{node_name}' is within quota — no eviction needed.");
                return Ok(());
            }
            println!(
                "Eviction plan for {node_name} (would free {}):\n",
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
        crate::ModelCommand::DiskSample => match ff_agent::disk_sampler::sample_local_disk(&pool).await {
            Ok(s) => {
                println!("Node:        {}", s.node_name);
                println!("Models dir:  {}", s.models_dir.display());
                println!("Total:       {}", human_bytes(s.total_bytes));
                println!("Used:        {}", human_bytes(s.used_bytes));
                println!("Free:        {}", human_bytes(s.free_bytes));
                println!("Models size: {}", human_bytes(s.models_bytes));
                println!("Quota:       {}%", s.quota_pct);
                println!("Over quota:  {}", s.over_quota);
            }
            Err(e) => anyhow::bail!("disk sample failed: {e}"),
        },
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
                    r.id, r.node_name, r.kind, r.status, r.progress_pct, target
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
                    .unwrap_or_else(|_| "redis://127.0.0.1:6380".into());
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
                ff_agent::fleet_info::resolve_this_node_name().await
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
