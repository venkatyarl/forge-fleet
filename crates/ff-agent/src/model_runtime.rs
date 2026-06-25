//! Model runtime — start/stop local LLM inference servers and track them in Postgres.
//!
//! Supports three runtimes: llama.cpp (llama-server), MLX (mlx_lm.server), vLLM (vllm serve).
//! Processes are spawned detached from the caller. When loaded, a row is upserted into
//! `fleet_model_deployments` so the rest of the fleet can discover the new endpoint.

use std::path::{Path, PathBuf};

static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

/// Options for [`load_model`].
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Library UUID from `fleet_model_library`. Determines which model file to launch.
    pub library_id: String,
    /// Port to bind the inference server on.
    pub port: u16,
    /// Context window size in tokens. Default 65536.
    pub context_size: Option<u32>,
    /// Concurrent parallel request slots (llama.cpp `--parallel`). Default 2.
    /// llama-server splits ctx across slots → per-slot ctx is ctx/parallel.
    /// Defaults give 32K per slot — enough headroom for ff agent dispatch
    /// (system prompt + tools schema + user prompt + reasoning).
    pub parallel: Option<u32>,
    /// Agent-capable serving profile. When `true`, launch the model so a
    /// tool-using agent's full ctx is available on a single slot: forces
    /// `--parallel 1` and raises `--ctx` to at least [`AGENT_MIN_CTX`] (32768)
    /// if the caller asked for less / didn't specify. This is the fix for the
    /// "prompt exceeds context window" overflow that happens when an agent's
    /// tool-schema system prompt is sent to a `--parallel >= 4` endpoint with
    /// only 4-8K per slot. Additive: leave `false` for the existing behaviour.
    pub agent_profile: bool,
    /// Explicit path to a multimodal projector (`mmproj*.gguf`) for vision
    /// models. When `None`, [`load_model`] auto-detects a sibling `mmproj*.gguf`
    /// next to the model file (llama.cpp `--mmproj`). Without a projector,
    /// llama-server rejects image inputs ("image input is not supported").
    pub mmproj_path: Option<String>,
}

/// Minimum per-slot context window for the agent-capable serving profile —
/// enough for the tool-schema system prompt + user prompt + reasoning.
pub const AGENT_MIN_CTX: u32 = 32_768;

/// Serving mode derived from the catalog row's `preferred_workloads`. Drives
/// which llama-server flags get appended on launch — embedders and rerankers
/// share the chat binary but speak different endpoints and tune differently.
///
/// Added 2026-05-18 alongside V91 to support bge-m3 / bge-reranker-v2-m3 /
/// DeepSeek-R1-Distill-Qwen-32B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServingMode {
    /// Default — chat completions on /v1/chat/completions.
    Chat,
    /// /v1/embeddings — requires --embeddings and a pooling strategy on llama.cpp.
    Embedding,
    /// /v1/rerank — requires --reranking on llama.cpp ≥ b3500.
    Reranking,
}

/// Pick a serving mode from a catalog row's `preferred_workloads` JSONB.
/// First matching tag wins; defaults to Chat when no embedding/reranking
/// hint is present (so existing rows behave exactly as before).
///
/// Tolerates singular/plural ("embedding"/"embeddings") and rerank/reranking
/// variants — the V39 seed and V91 use slightly different conventions and we
/// want both to route correctly.
fn serving_mode_from_workloads(workloads: &serde_json::Value) -> ServingMode {
    let Some(arr) = workloads.as_array() else {
        return ServingMode::Chat;
    };
    for v in arr {
        match v.as_str() {
            Some("embedding") | Some("embeddings") => return ServingMode::Embedding,
            Some("rerank") | Some("reranking") => return ServingMode::Reranking,
            _ => {}
        }
    }
    ServingMode::Chat
}

/// Extra llama-server flags for a chat-mode launch.
///
/// Tool-capable models get `--jinja`, which makes llama-server apply the GGUF's
/// own chat template and parse tool calls into the structured `tool_calls`
/// field instead of leaving them as free-form text the agent loop has to
/// recover heuristically (`openai_bridge::parse_text_tool_calls`, #364).
///
/// Current fleet builds already default `--jinja` to enabled, so today this is
/// mostly *pinning*: it makes tool-call parsing deterministic regardless of the
/// per-build/per-version default (which upstream llama.cpp has flipped before)
/// and is a real fix on any older binary that defaults it off. Idempotent where
/// it's already on. Non-tool models get no extra flags — template substitution
/// buys them nothing and a malformed embedded template could refuse to launch.
fn llamacpp_chat_flags(tool_calling: bool) -> Vec<String> {
    let mut flags = Vec::new();
    if tool_calling {
        flags.push("--jinja".to_string());
    }
    flags
}

/// Decide whether a load should use the agent-capable serving profile
/// (`--parallel 1`, ctx >= [`AGENT_MIN_CTX`]). Capable **chat** models default
/// to it so the endpoint is router/autoscaler-eligible (they require
/// `usable_agent_ctx >= 32768`); an explicit `--parallel` opts out for
/// throughput, and an explicit `--agent` always forces it on. Embedders and
/// rerankers are never promoted (single forward pass, no per-slot split).
fn resolve_agent_profile(
    explicit_agent: bool,
    mode: ServingMode,
    tool_calling: bool,
    explicit_parallel: Option<u32>,
) -> bool {
    explicit_agent || (mode == ServingMode::Chat && tool_calling && explicit_parallel.is_none())
}

/// Result of a successful load.
#[derive(Debug, Clone)]
pub struct LoadResult {
    pub deployment_id: String,
    pub pid: u32,
    pub runtime: String,
    pub port: u16,
    pub model_path: String,
}

/// A running inference process detected on this host.
#[derive(Debug, Clone)]
pub struct RunningProcess {
    pub pid: u32,
    pub port: Option<u16>,
    pub model_path: Option<String>,
    pub runtime: String,
    /// Parsed from the launch cmdline (`--parallel` / `-np`, llama.cpp).
    /// `None` when not present (e.g. mlx, or older servers) — the adopter
    /// then leaves parallel_slots NULL rather than guessing.
    pub parallel_slots: Option<i32>,
    /// Parsed from the launch cmdline (`--ctx-size` / `-c` for llama.cpp,
    /// `--max-model-len` for vllm). Lets the reconciler record the real ctx
    /// (and derive usable_agent_ctx) for an adopted out-of-band server.
    pub context_window: Option<i32>,
}

/// PLACEMENT GUARD (V118): can `node` run a model whose runtime is `runtime`?
///
/// The rule mirrors the runtime-choice policy and the autoscaler's
/// `runtime_compatible`, but keyed off the node's own configured `runtime` plus
/// its OS (the authoritative fields on `FleetNodeRow`):
///   • mlx   ⇒ macOS only (Metal). An mlx model on a Linux host is rejected.
///   • vllm  ⇒ the node's runtime must be vllm (CUDA/GB10 box).
///   • llama.cpp / ollama ⇒ runs anywhere; only rejected on a node explicitly
///     configured for a different, incompatible runtime would be wrong, so we
///     allow it broadly (CPU fallback exists everywhere).
/// Returns `Ok(())` if the placement is allowed, else `Err(reason)`.
pub fn check_runtime_placement(node: &ff_db::FleetNodeRow, runtime: &str) -> Result<(), String> {
    let os = node.os.to_ascii_lowercase();
    let is_macos = os.contains("mac") || os.contains("darwin");
    match runtime {
        "mlx" => {
            if is_macos {
                Ok(())
            } else {
                Err(format!("mlx requires macOS, node os='{}'", node.os))
            }
        }
        "vllm" => {
            // vllm needs a CUDA/GB10 box; the node must itself be a vllm node.
            if node.runtime == "vllm" {
                Ok(())
            } else {
                Err(format!(
                    "vllm requires a CUDA/GB10 node (runtime='vllm'), node runtime='{}'",
                    node.runtime
                ))
            }
        }
        // llama.cpp / ollama / unknown: CPU-runnable broadly. The only hard
        // reject is a non-macOS-capable runtime mismatch already covered above.
        _ => Ok(()),
    }
}

/// Spawn an inference server for the given library row, wait for health, and record
/// the deployment row in Postgres.
pub async fn load_model(pool: &sqlx::PgPool, opts: LoadOptions) -> Result<LoadResult, String> {
    // Find the library row.
    let libs = ff_db::pg_list_library(pool, None)
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;
    let lib = libs
        .into_iter()
        .find(|r| r.id == opts.library_id)
        .ok_or_else(|| format!("no library entry with id '{}'", opts.library_id))?;

    let worker_name = crate::fleet_info::resolve_this_worker_name().await;
    if lib.worker_name != worker_name {
        return Err(format!(
            "library row is on '{}', but we're running on '{}'; cross-node load not implemented",
            lib.worker_name, worker_name
        ));
    }

    // PLACEMENT GUARD (V118): reject loading a model this node can't actually
    // run — e.g. an mlx model on a Linux host or a vllm model without CUDA/GB10.
    // Stops the problem upstream instead of letting the runtime spawn and fail
    // cryptically. The library row's own `runtime` is the source of truth.
    if let Some(node) = ff_db::pg_get_node(pool, &worker_name)
        .await
        .map_err(|e| format!("pg_get_node({worker_name}): {e}"))?
        && let Err(reason) = check_runtime_placement(&node, &lib.runtime)
    {
        return Err(format!(
            "placement rejected: cannot load {} ({}) on {}: {reason}",
            lib.catalog_id, lib.runtime, worker_name
        ));
    }

    let port = opts.port;

    // Look up the catalog row so we can pick the right serving mode (chat /
    // embedding / reranking) AND whether the model is tool-calling (which drives
    // the agent-profile default below). Falls back to Chat / non-tool-calling
    // when there's no catalog row or no recognised workload tag — preserves
    // existing behaviour for unknown models.
    let (mode, tool_calling) = match ff_db::pg_get_catalog(pool, &lib.catalog_id)
        .await
        .map_err(|e| format!("pg_get_catalog({}): {e}", lib.catalog_id))?
    {
        Some(cat) => (
            serving_mode_from_workloads(&cat.preferred_workloads),
            cat.tool_calling,
        ),
        None => (ServingMode::Chat, false),
    };

    // Capable chat models default to the agent serving profile so the endpoint
    // is router/autoscaler-eligible (they require usable_agent_ctx >= 32768).
    let agent = resolve_agent_profile(opts.agent_profile, mode, tool_calling, opts.parallel);
    if agent && !opts.agent_profile {
        tracing::info!(
            model = %lib.catalog_id,
            "capable model defaulted to agent serving profile (--parallel 1, ctx >= 32768)"
        );
    }

    // Agent-capable profile forces a single slot and a ctx floor so a
    // tool-schema system prompt can't overflow a split per-slot window. We
    // apply it BEFORE the defaults so a too-small explicit `--ctx` is raised to
    // the floor (the profile is the contract; a too-small ctx would defeat it).
    let (ctx, parallel) = if agent {
        let ctx = opts
            .context_size
            .unwrap_or(AGENT_MIN_CTX)
            .max(AGENT_MIN_CTX);
        (ctx, 1u32)
    } else {
        (
            opts.context_size.unwrap_or(65_536),
            opts.parallel.unwrap_or(2),
        )
    };

    // Build the launch command per runtime.
    let (program, args, runtime_label) = match lib.runtime.as_str() {
        "llama.cpp" => {
            let bin = llama_server_binary();
            // llama-server expects a single .gguf file, not a directory.
            // The library scanner often registers a directory (the model
            // root); resolve to the largest .gguf inside so the spawn
            // command points at real bytes. Discovered 2026-05-16 on
            // sophie: ff was passing `/home/sophie/models/qwen3-coder-30b-a3b`
            // and llama-server bailed with `gguf_init_from_file_ptr: failed
            // to read magic` because that's a directory.
            let model_path = resolve_gguf_for_llamacpp(&lib.file_path)
                .map_err(|e| format!("resolve gguf for {}: {e}", lib.file_path))?;
            let mut args = vec![
                "--model".into(),
                model_path.clone(),
                "--host".into(),
                "0.0.0.0".into(),
                "--port".into(),
                port.to_string(),
                "--ctx-size".into(),
                ctx.to_string(),
                // --mlock pins all weights in RAM so the OS can't evict
                // pages and re-read from disk. Two things this buys us:
                //   1. Steady-state inference latency (no page faults
                //      mid-decode after the page cache evicts under
                //      memory pressure from other workloads).
                //   2. The disk file can be safely deleted after load
                //      because the mmap'd pages stay resident (the
                //      eventual move-semantics policy #133 leans on
                //      this — disk lives on canonical owner only).
                // Cost: real RAM equal to model size (not pageable).
                // Acceptable: every serving host has enough RAM for
                // its loaded models, otherwise --mlock would have
                // failed anyway.
                "--mlock".into(),
            ];
            // Vision projector: explicit opts.mmproj_path wins; otherwise
            // auto-detect a sibling `mmproj*.gguf` next to the model file. With
            // it, llama-server accepts image inputs on /v1/chat/completions;
            // without it, multimodal requests 500 ("image input is not
            // supported"). Harmless for text-only models (none present → skip).
            if let Some(mmproj) = opts
                .mmproj_path
                .clone()
                .or_else(|| find_sibling_mmproj(&model_path))
            {
                tracing::info!(model = %lib.catalog_id, %mmproj, "vision projector enabled (--mmproj)");
                args.push("--mmproj".into());
                args.push(mmproj);
            }
            match mode {
                ServingMode::Chat => {
                    args.push("--parallel".into());
                    args.push(parallel.to_string());
                    // Pin --jinja for tool-capable models so structured
                    // tool-call parsing doesn't depend on the llama.cpp build
                    // default (see llamacpp_chat_flags). Additive: only future
                    // loads pick it up; running deployments are untouched.
                    args.extend(llamacpp_chat_flags(tool_calling));
                }
                ServingMode::Embedding => {
                    // /v1/embeddings on llama.cpp ≥ b3000. BGE models use
                    // [CLS] pooling — pick it explicitly so we don't get
                    // last-token pooling (which is what llama defaults to
                    // for decoder LMs and produces garbage for BERT
                    // encoders).
                    args.push("--embeddings".into());
                    args.push("--pooling".into());
                    args.push("cls".into());
                    // --parallel doesn't apply to embedding mode: each
                    // request is a single forward pass, no KV slots.
                }
                ServingMode::Reranking => {
                    // /v1/rerank on llama.cpp ≥ b3500. Reranker is a
                    // cross-encoder — no pooling flag, no parallel slots.
                    args.push("--reranking".into());
                }
            }
            // On macOS Metal builds this enables full-GPU inference.
            if cfg!(target_os = "macos") {
                args.push("--n-gpu-layers".into());
                args.push("999".into());
            }
            (bin, args, "llama.cpp")
        }
        "mlx" => {
            // mlx_lm.server is chat-only — no /v1/embeddings, no /v1/rerank.
            // Fail loud rather than silently launching a chat server for an
            // embedder.
            if mode != ServingMode::Chat {
                return Err(format!(
                    "mlx runtime does not support {:?} mode (chat only); \
                     use the llama.cpp variant instead",
                    mode
                ));
            }
            // mlx_lm.server expects the MODEL to be either an HF repo id or a local dir
            // with config/weights. We use the local dir.
            let args = vec![
                "--model".into(),
                lib.file_path.clone(),
                "--host".into(),
                "0.0.0.0".into(),
                "--port".into(),
                port.to_string(),
            ];
            ("mlx_lm.server".to_string(), args, "mlx")
        }
        "vllm" => {
            if mode != ServingMode::Chat {
                return Err(format!(
                    "vllm runtime via this launcher does not yet support \
                     {:?} mode; needs --task embedding wiring",
                    mode
                ));
            }
            let args = vec![
                "serve".into(),
                lib.file_path.clone(),
                "--host".into(),
                "0.0.0.0".into(),
                "--port".into(),
                port.to_string(),
                "--max-model-len".into(),
                ctx.to_string(),
            ];
            ("vllm".to_string(), args, "vllm")
        }
        other => return Err(format!("unsupported runtime: {other}")),
    };

    tracing::info!(program, ?args, "spawning inference server");

    // Spawn detached (parent doesn't wait). stdout/stderr to log file if present.
    let log_dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
        .join(".forgefleet/logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("model-{}.log", port));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open log {}: {e}", log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| format!("clone log handle: {e}"))?;

    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args)
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null());

    // On Linux, llama-server links against libraries (libmtmd.so.0 and
    // friends) that live next to the binary in `llama.cpp/build/bin/`.
    // The default ld.so search path doesn't include that dir, and the
    // daemon's process environment doesn't carry whatever LD_LIBRARY_PATH
    // an interactive shell would set — so the spawned server exits
    // immediately with `error while loading shared libraries: libmtmd.so.0`.
    //
    // Discovered 2026-05-18 on veronica: bge-m3 autoload reported success
    // (PID returned, deployment row upserted) but /health was unreachable.
    // model-55001.log was three identical "cannot open shared object file"
    // lines from llama-server's first-attempt loader.
    //
    // Fix: when the program is an absolute path to a llama-server binary
    // inside a llama.cpp build tree, prepend the parent dir to
    // LD_LIBRARY_PATH so co-located .so files resolve. Harmless on macOS
    // (Mach-O uses different linker plumbing) and on system-installed
    // builds where the libs are already on the global loader path.
    if cfg!(target_os = "linux")
        && program.contains("llama-server")
        && let Some(bin_dir) = std::path::Path::new(&program).parent()
    {
        let bin_dir_str = bin_dir.display().to_string();
        let prev = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        let new_val = if prev.is_empty() {
            bin_dir_str.clone()
        } else {
            format!("{bin_dir_str}:{prev}")
        };
        cmd.env("LD_LIBRARY_PATH", new_val);
    }

    // Detach from the parent's session/process-group so the inference
    // server survives `ff model load` exiting (and, on Linux, survives
    // the SSH session ending when we dispatched via ssh+bash).
    //
    // Discovered 2026-05-16 on sophie: ff reported "Loaded — pid X" but
    // the child died seconds later. Cause: systemd-logind tears down the
    // session's process group at logout regardless of `nohup`. setsid()
    // before exec() promotes the child to a new session leader, breaking
    // the parent linkage so the child's lifetime is independent.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // SAFETY: setsid() is a single syscall with well-defined
                // semantics in the post-fork pre-exec window — the only
                // safe Rust we can do here per the pre_exec contract.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    // On Linux, prefer systemd OWNERSHIP over the manual setsid spawn: write
    // the user unit AND start it, so the inference server lives in a persistent
    // `user.slice` cgroup. The manual spawn below survives `ff model load`
    // exiting (setsid breaks the parent linkage), but it does NOT survive the
    // logind SESSION SCOPE being torn down when a *cross-node* dispatch closes
    // its SSH channel — `ff model autoload --node X` runs a one-shot `ff` over
    // ssh, and the moment that ssh command returns, logind kills the session
    // scope's cgroup and the setsid child with it. The server's own log shows
    // it reaching "server is listening" then immediately "cleaning up before
    // exit". The autoscaler's remote-load path uses exactly this, so remote
    // autoloads were silently dying seconds after reporting success.
    //
    // A systemd-started unit runs in user.slice, independent of any session
    // scope, so it survives. `cfg!` (runtime, not `#[cfg]`) keeps this path
    // type-checked on every target while only executing on Linux. Falls back
    // to the manual spawn if systemd isn't usable (no user manager, etc.).
    let mut systemd_pid: Option<u32> = None;
    if cfg!(target_os = "linux") {
        match write_systemd_unit(&program, &args, port, &log_path).await {
            Ok(()) => match start_systemd_unit(port).await {
                Ok(p) => {
                    tracing::info!(
                        port,
                        pid = p,
                        "model_runtime: started via systemd user unit"
                    );
                    systemd_pid = Some(p);
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    port,
                    "model_runtime: systemctl start failed (falling back to manual spawn)"
                ),
            },
            Err(e) => tracing::warn!(
                error = %e,
                port,
                "model_runtime: systemd unit write failed (falling back to manual spawn)"
            ),
        }
    }

    let pid = if let Some(p) = systemd_pid {
        p
    } else {
        let mut child = cmd.spawn().map_err(|e| format!("spawn {program}: {e}"))?;
        let pid = child.id();
        // Reap in background so the child doesn't become a zombie.
        tokio::task::spawn_blocking(move || {
            let _ = child.wait();
        });
        pid
    };

    // Wait for health endpoint to come up (up to 90s).
    let health_ok = wait_for_health(
        runtime_label,
        port,
        std::time::Duration::from_secs(90),
        &SHARED_HTTP,
    )
    .await;

    // Record the parallel slot count so the data plane can compute
    // usable_agent_ctx (= ctx / slots). Only Chat mode splits ctx across
    // `--parallel` slots; embedding/reranking are single forward passes per
    // request (no KV slots) so the full ctx is usable → record 1 slot.
    let recorded_slots: i32 = match mode {
        ServingMode::Chat => parallel as i32,
        ServingMode::Embedding | ServingMode::Reranking => 1,
    };

    // Upsert deployment row.
    let deployment_id = ff_db::pg_upsert_deployment(
        pool,
        &worker_name,
        Some(&lib.id),
        Some(&lib.catalog_id),
        runtime_label,
        port as i32,
        Some(pid as i32),
        if health_ok { "healthy" } else { "starting" },
        Some(ctx as i32),
        Some(recorded_slots),
    )
    .await
    .map_err(|e| format!("pg_upsert_deployment: {e}"))?;

    if !health_ok {
        tracing::warn!(
            pid,
            port,
            "inference server did not become healthy within 90s"
        );
    }

    // V106: mark this library row as hot + bump last_used_at. Single
    // UPDATE per load event — no periodic ticker writes.
    let _ = sqlx::query(
        "UPDATE fleet_model_library SET state = 'hot', last_used_at = NOW() WHERE id = $1",
    )
    .bind(&lib.id)
    .execute(pool)
    .await;

    Ok(LoadResult {
        deployment_id,
        pid,
        runtime: runtime_label.to_string(),
        port,
        model_path: lib.file_path,
    })
}

/// Stop a running inference server tracked under `deployment_id`.
///
/// Identifies the real serving process by the deployment's PORT (live kernel
/// lookup via `ss`/`lsof`) rather than the possibly-stale recorded PID, so it
/// kills the ACTUAL listener even if the server was restarted out-of-band.
/// SIGTERM first (up to 10s), then SIGKILL — to the PID and its process group.
/// On Linux, stops the systemd supervisor first so it doesn't respawn the
/// server. Deletes the deployment row on success.
pub async fn unload_model(pool: &sqlx::PgPool, deployment_id: &str) -> Result<(), String> {
    let worker_name = crate::fleet_info::resolve_this_worker_name().await;
    let deployments = ff_db::pg_list_deployments(pool, Some(&worker_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;
    let dep = deployments
        .into_iter()
        .find(|d| d.id == deployment_id)
        .ok_or_else(|| format!("no deployment '{deployment_id}' on this node"))?;

    // Mark desired_state='retired' BEFORE the kill so a racing reconciler
    // tick doesn't see a missing process for an 'active' row and spawn
    // a replacement we're about to delete. See V90.
    let _ = sqlx::query(
        "UPDATE fleet_model_deployments SET desired_state = 'retired' WHERE id = $1::uuid",
    )
    .bind(deployment_id)
    .execute(pool)
    .await
    .map_err(|e| format!("mark retired: {e}"))?;

    let port = dep.port as u16;
    let recorded_pid = dep.pid.map(|p| p as u32);

    // On Linux, stop+disable the systemd supervisor FIRST. The unit uses
    // `Restart=on-failure`, and a SIGTERM/SIGKILL counts as a non-clean exit,
    // so without this systemd would immediately respawn a fresh llama-server
    // (with a new PID) the moment we kill the listener — defeating the unload.
    #[cfg(target_os = "linux")]
    stop_systemd_unit(port).await;

    // Kill the process that is ACTUALLY listening on this deployment's port,
    // resolved live from the kernel — not the (possibly stale) recorded PID.
    // The recorded PID is passed only as a fallback target. SIGTERM → wait →
    // SIGKILL, against the PID and its process group. Never `pkill -f`.
    let killed = stop_listener_on_port(port, recorded_pid).await;
    if killed.is_empty() {
        tracing::warn!(
            deployment = %deployment_id,
            port,
            "unload: no live listener found on port (already stopped?); clearing DB row"
        );
    }

    // V106: flip the library row back to cold. We capture library_id from
    // the deployment before deleting it so we still know which row to update.
    let lib_id = dep.library_id.clone();
    ff_db::pg_delete_deployment(pool, deployment_id)
        .await
        .map_err(|e| format!("pg_delete_deployment: {e}"))?;
    if let Some(lid) = lib_id {
        let _ = sqlx::query(
            "UPDATE fleet_model_library SET state = 'cold' WHERE id = $1::uuid \
             AND NOT EXISTS ( \
               SELECT 1 FROM fleet_model_deployments dep2 \
                WHERE dep2.library_id = $1::uuid \
                  AND dep2.desired_state = 'active' \
             )",
        )
        .bind(&lid)
        .execute(pool)
        .await;
    }
    Ok(())
}

// ── Memory-aware build support ─────────────────────────────────────────────
//
// Releasing forgefleetd/ff is a heavy release build that OOMs mid-link (the
// `forge-fleet` binary crate) on memory-tight hosts (≤ ~40GB total) when an
// LLM model is resident — observed on sophie (32GB + qwen3-coder-30b) and ace
// (16GB). Before this, those hosts failed the auto-upgrade wave every pass and
// could not self-heal; only a manual unload→build→reload converged them.
//
// The self-built wave now calls `pause_local_models_for_build` before the
// build and `resume_local_models` after, so the pipeline self-heals memory-
// tight hosts. Roomy hosts (> threshold) are a no-op — their models stay up.

/// Hosts with total RAM at or below this many GB pause their resident models
/// for a self-built release build. Splits the fleet cleanly: 16/32GB hosts
/// pause; 64/96/128GB hosts build with models still loaded.
pub const FREE_FOR_BUILD_RAM_GB: f64 = 40.0;

fn paused_models_state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".forgefleet")
        .join("paused_build_models.json")
}

/// Best-effort local total RAM in GB. macOS: `sysctl -n hw.memsize`; Linux:
/// `/proc/meminfo` MemTotal. Returns `None` if undetectable (caller treats
/// that as "roomy" so an unknown host is never needlessly stripped of models).
fn local_total_ram_gb() -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        let bytes: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(bytes / 1e9)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let txt = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in txt.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
                return Some(kb / 1e6);
            }
        }
        None
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PausedModel {
    library_id: String,
    port: u16,
    context_size: Option<u32>,
    /// Snapshotted from the deployment row's `parallel_slots` (V111) so resume
    /// restores the exact slot layout — including an agent-capable deployment
    /// (parallel_slots = 1, ctx >= 32K). Falls back to load_model's default
    /// when the row predates V111 (`None`).
    parallel: Option<u32>,
}

/// Pause local model deployments to free RAM for a release build — only if
/// this host is memory-tight (total RAM ≤ [`FREE_FOR_BUILD_RAM_GB`]).
///
/// Two passes, both keyed on the live process, never on a trusted PID:
///   1. For each DB deployment on this host, snapshot it (if restorable) and
///      kill the process LISTENING on its port (via [`unload_model`]).
///   2. Sweep every remaining live inference server detected by `ps`
///      ([`list_local_processes`]) whose port wasn't already handled, and kill
///      it by port too. This catches the "paused 0" case observed on sia: a
///      real llama-server was alive but the DB had no (or a stale) row for it,
///      so the old DB-only loop freed nothing.
///
/// Snapshots restorable deployments (those with a `library_id`) to a state
/// file for [`resume_local_models`]. No-op on roomy hosts, when RAM is
/// undetectable, or when nothing is running. Returns the number of processes
/// stopped (DB-tracked + swept).
///
/// Never uses `pkill -f` — every kill goes through the port-resolved path.
pub async fn pause_local_models_for_build(pool: &sqlx::PgPool) -> Result<usize, String> {
    let total = local_total_ram_gb().unwrap_or(f64::INFINITY);
    if total > FREE_FOR_BUILD_RAM_GB {
        return Ok(0); // roomy host — build with models loaded
    }
    let worker = crate::fleet_info::resolve_this_worker_name().await;
    let deps = ff_db::pg_list_deployments(pool, Some(&worker))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;

    let mut snapshot: Vec<PausedModel> = Vec::new();
    let mut handled_ports: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut stopped = 0usize;

    // ── Pass 1: DB-tracked deployments ─────────────────────────────────
    for d in &deps {
        if let Some(lib) = d.library_id.clone() {
            snapshot.push(PausedModel {
                library_id: lib,
                port: d.port as u16,
                context_size: d.context_window.map(|c| c as u32),
                parallel: d.parallel_slots.map(|p| p as u32),
            });
        } else {
            tracing::warn!(
                deployment = %d.id,
                "free-for-build: deployment has no library_id; unloading to free RAM but it won't auto-restore"
            );
        }
        handled_ports.insert(d.port as u16);
        let _ = unload_model(pool, &d.id).await; // best-effort; kills by port
        stopped += 1;
    }

    // ── Pass 2: live servers with no (matching) DB row ─────────────────
    // The build needs the RAM regardless of whether ForgeFleet tracks the
    // server. We can't snapshot these for auto-restore (no library_id), but
    // freeing the RAM is the must-have. resume_local_models is a no-op for
    // them — they were untracked to begin with.
    for proc in list_local_processes().await {
        let Some(port) = proc.port else { continue };
        if handled_ports.contains(&port) {
            continue;
        }
        handled_ports.insert(port);
        tracing::warn!(
            pid = proc.pid,
            port,
            runtime = %proc.runtime,
            model = proc.model_path.as_deref().unwrap_or("-"),
            "free-for-build: stopping untracked inference server to free RAM (won't auto-restore)"
        );
        #[cfg(target_os = "linux")]
        stop_systemd_unit(port).await;
        let killed = stop_listener_on_port(port, Some(proc.pid)).await;
        if !killed.is_empty() {
            stopped += 1;
        }
    }

    if stopped == 0 {
        return Ok(0);
    }

    let path = paused_models_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(&snapshot).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write state: {e}"))?;
    tracing::info!(
        stopped,
        restorable = snapshot.len(),
        total_ram_gb = total,
        "free-for-build: stopped inference servers to free RAM for release build"
    );
    Ok(stopped)
}

/// Reload models paused by [`pause_local_models_for_build`]: read the state
/// file, `load_model` each, then remove the file. No-op if no state file
/// exists (roomy host / nothing was paused). Returns the number restored.
pub async fn resume_local_models(pool: &sqlx::PgPool) -> Result<usize, String> {
    let path = paused_models_state_path();
    let Ok(json) = std::fs::read_to_string(&path) else {
        return Ok(0);
    };
    let snapshot: Vec<PausedModel> =
        serde_json::from_str(&json).map_err(|e| format!("parse state: {e}"))?;
    let mut restored = 0usize;
    for m in &snapshot {
        match load_model(
            pool,
            LoadOptions {
                library_id: m.library_id.clone(),
                port: m.port,
                context_size: m.context_size,
                parallel: m.parallel,
                // Exact ctx + parallel are restored from the snapshot above,
                // which already reproduces an agent layout (1 slot, ctx >= 32K)
                // if that's how it was loaded — no need to re-derive the profile.
                agent_profile: false,
                mmproj_path: None, // auto-detect sibling mmproj on restore
            },
        )
        .await
        {
            Ok(_) => restored += 1,
            Err(e) => tracing::warn!(
                library_id = %m.library_id,
                error = %e,
                "resume-from-build: reload failed"
            ),
        }
    }
    let _ = std::fs::remove_file(&path);
    tracing::info!(restored, "resume-from-build: reloaded paused models");
    Ok(restored)
}

/// Scan local processes for running inference servers (llama.cpp/MLX/vLLM).
pub async fn list_local_processes() -> Vec<RunningProcess> {
    let output = tokio::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .await;
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let (pid_str, rest) = match line.split_once(char::is_whitespace) {
            Some(p) => p,
            None => continue,
        };
        let pid: u32 = match pid_str.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let runtime = if rest.contains("llama-server") {
            "llama.cpp"
        } else if rest.contains("mlx_lm.server") || rest.contains("mlx_lm/server") {
            "mlx"
        } else if rest.contains("vllm ") && rest.contains("serve") {
            "vllm"
        } else {
            continue;
        };

        // Parse --port <N>
        let port = parse_flag_value(rest, "--port").and_then(|v| v.parse::<u16>().ok());

        // Parse --model <path>, -m <path> (llama-server short form), or positional
        // after `serve` (vllm).
        let model_path = parse_flag_value(rest, "--model")
            .or_else(|| parse_flag_value(rest, "-m"))
            .or_else(|| {
                if runtime == "vllm" {
                    rest.split_once("serve ")
                        .and_then(|(_, after)| after.split_whitespace().next())
                        .map(String::from)
                } else {
                    None
                }
            });

        // Parse the slot count + ctx so an adopted out-of-band server still
        // gets usable_agent_ctx recorded. llama.cpp: --parallel/-np, --ctx-size/-c.
        // vllm has no slot-splitting (one model len shared) → treat as 1 slot
        // with --max-model-len. mlx serves the full ctx → 1 slot, ctx unknown.
        let parallel_slots = match runtime {
            "llama.cpp" => parse_flag_value(rest, "--parallel")
                .or_else(|| parse_flag_value(rest, "-np"))
                .and_then(|v| v.parse::<i32>().ok()),
            "vllm" | "mlx" => Some(1),
            _ => None,
        };
        let context_window = match runtime {
            "llama.cpp" => parse_flag_value(rest, "--ctx-size")
                .or_else(|| parse_flag_value(rest, "-c"))
                .and_then(|v| v.parse::<i32>().ok()),
            "vllm" => parse_flag_value(rest, "--max-model-len").and_then(|v| v.parse::<i32>().ok()),
            // mlx_lm.server takes no ctx flag (serves the model's full window,
            // RAM-bound). Without a recorded ctx the deployment row keeps
            // usable_agent_ctx NULL forever and the V111 capability router
            // (`usable_agent_ctx >= min_ctx`) is blind to mlx endpoints — which
            // hid taylor's strongest agent server. Honor --max-kv-size when
            // passed, else read the model dir's config.json.
            "mlx" => parse_flag_value(rest, "--max-kv-size")
                .and_then(|v| v.parse::<i32>().ok())
                .or_else(|| model_path.as_deref().and_then(mlx_config_ctx)),
            _ => None,
        };

        found.push(RunningProcess {
            pid,
            port,
            model_path,
            runtime: runtime.to_string(),
            parallel_slots,
            context_window,
        });
    }
    found
}

/// Check the health endpoint for a deployment and update `health_status` in Postgres.
/// Returns Ok(true) if healthy, Ok(false) if reachable but unhealthy, Err if unreachable.
pub async fn health_check_deployment(
    pool: &sqlx::PgPool,
    deployment_id: &str,
) -> Result<bool, String> {
    let worker_name = crate::fleet_info::resolve_this_worker_name().await;
    let deployments = ff_db::pg_list_deployments(pool, Some(&worker_name))
        .await
        .map_err(|e| format!("pg_list_deployments: {e}"))?;
    let dep = deployments
        .into_iter()
        .find(|d| d.id == deployment_id)
        .ok_or_else(|| format!("no deployment '{deployment_id}' on this node"))?;

    let ok = probe_health(
        &dep.runtime,
        dep.port as u16,
        std::time::Duration::from_secs(3),
        &SHARED_HTTP,
    )
    .await;
    let status_new = if ok { "healthy" } else { "unhealthy" };

    // Write status back — use upsert with the existing library/catalog/port to update only status.
    let _ = sqlx::query(
        "UPDATE fleet_model_deployments
            SET health_status = $1, last_health_at = NOW()
          WHERE id = $2::uuid",
    )
    .bind(status_new)
    .bind(&dep.id)
    .execute(pool)
    .await
    .map_err(|e| format!("update deployment: {e}"))?;

    Ok(ok)
}

// ─── helpers ──────────────────────────────────────────────────────────────

/// Resolve `path` to a concrete `.gguf` file suitable for `llama-server --model`.
/// If `path` already points at a `.gguf` file, return it unchanged. If it's a
/// directory, pick the **largest** `.gguf` inside (sharded models put the
/// real weights in the biggest shard; multi-quant directories typically
/// have a single canonical gguf and the biggest is the right pick).
fn resolve_gguf_for_llamacpp(path: &str) -> std::io::Result<String> {
    let p = PathBuf::from(path);
    if p.is_file() && path.ends_with(".gguf") {
        return Ok(path.to_string());
    }
    if !p.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{path} is neither a .gguf file nor a directory"),
        ));
    }
    // Walk the tree (not just the top level) for the largest .gguf. HF- and
    // ollama-style downloads frequently NEST the weights one or more levels
    // deep — e.g. veronica's `qwen3-coder-30b-a3b/qwen3-coder-30b-a3b/*.gguf`
    // (a doubled-name dir), or `snapshots/<hash>/*.gguf`. The library scanner
    // registers the model-root dir as `file_path`, so a single-level scan
    // misses the nested file and load fails `no .gguf files in <dir>` forever
    // (248 autoscaler autoload retries in 12h on veronica alone). Bounded depth
    // + no symlink-following keeps the walk cycle-safe.
    match largest_gguf_under(&p, 8)? {
        Some(ep) => Ok(ep.to_string_lossy().to_string()),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no .gguf files under {path} (searched recursively)"),
        )),
    }
}

/// Recursively find the largest `.gguf` regular file under `dir`, descending at
/// most `depth` more levels. Directory symlinks are NOT followed (cycle-safe);
/// unreadable subdirectories are skipped rather than aborting the whole walk.
fn largest_gguf_under(dir: &Path, depth: u32) -> std::io::Result<Option<PathBuf>> {
    let mut best: Option<(u64, PathBuf)> = None;
    let consider = |size: u64, p: PathBuf, best: &mut Option<(u64, PathBuf)>| {
        if best.as_ref().is_none_or(|(s, _)| size > *s) {
            *best = Some((size, p));
        }
    };
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(ft) = entry.file_type() else { continue };
        let ep = entry.path();
        if ft.is_file() {
            if ep.extension().and_then(|e| e.to_str()) == Some("gguf") {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                consider(size, ep, &mut best);
            }
        } else if ft.is_dir() && depth > 0 {
            // Skip subdirs we can't read instead of failing the whole resolve.
            if let Ok(Some(found)) = largest_gguf_under(&ep, depth - 1) {
                let size = std::fs::metadata(&found).map(|m| m.len()).unwrap_or(0);
                consider(size, found, &mut best);
            }
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// Look for a multimodal projector (`mmproj*.gguf`) alongside a resolved model
/// file. llama.cpp ships the vision projector as a sibling GGUF (e.g.
/// `mmproj-Qwen3VL-30B-A3B-Instruct-F16.gguf` next to the Q4 model). Matching is
/// case-insensitive on the `mmproj` prefix. When more than one is present (F16
/// and Q8 quants both ship), prefer the larger (higher-fidelity F16). Returns
/// `None` for text-only models — the directory simply has no mmproj sibling.
fn find_sibling_mmproj(model_path: &str) -> Option<String> {
    let dir = PathBuf::from(model_path).parent()?.to_path_buf();
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let ep = entry.path();
        let Some(name) = ep.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if !(lower.starts_with("mmproj") && lower.ends_with(".gguf")) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if best.as_ref().is_none_or(|(s, _)| size > *s) {
            best = Some((size, ep));
        }
    }
    best.map(|(_, ep)| ep.to_string_lossy().to_string())
}

fn llama_server_binary() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    // Known install locations across the fleet. The "no-prefix" form
    // (`llama.cpp/build/bin/llama-server` at `$HOME`) is the layout used
    // by sophie and other operators who cloned llama.cpp at the home
    // root rather than under `projects/`. Discovered 2026-05-16 when
    // `ff model load` on sophie failed with "No such file or directory".
    for rel in [
        "llama.cpp/build/bin/llama-server",
        "projects/llama.cpp/build/bin/llama-server",
        ".forgefleet/llama.cpp/build/bin/llama-server",
    ] {
        let candidate = PathBuf::from(&home).join(rel);
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    // Fallback: rely on PATH.
    "llama-server".to_string()
}

async fn wait_for_health(
    runtime: &str,
    port: u16,
    timeout: std::time::Duration,
    client: &reqwest::Client,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if probe_health(runtime, port, std::time::Duration::from_secs(2), client).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
    false
}

/// Public re-export of [`probe_health`] for other modules (e.g. reconciler).
pub async fn probe_health_public(runtime: &str, port: u16, timeout: std::time::Duration) -> bool {
    probe_health(runtime, port, timeout, &SHARED_HTTP).await
}

async fn probe_health(
    runtime: &str,
    port: u16,
    timeout: std::time::Duration,
    client: &reqwest::Client,
) -> bool {
    // llama.cpp and vllm expose /health; MLX uses /v1/models.
    let endpoint = match runtime {
        "mlx" => "/v1/models",
        _ => "/health",
    };
    let url = format!("http://127.0.0.1:{port}{endpoint}");
    client
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Probe a running inference server for its real per-slot context window and
/// slot count, so the reconciler can backfill `usable_agent_ctx` /
/// `context_window` / `parallel_slots` on adopted deployments (servers ff
/// didn't start, whose cmdline lacked explicit `--ctx-size`/`--parallel`).
/// Without this the capability-aware agent router — which filters
/// `usable_agent_ctx IS NOT NULL AND >= N` — is blind to them.
///
/// Returns `(per_slot_ctx, total_slots)` where `per_slot_ctx` is the context a
/// single agent request actually gets (the value the router compares against
/// `min_ctx`). `None` when the runtime doesn't advertise it or the probe fails.
///
/// - llama.cpp `/props`: `default_generation_settings.n_ctx` is already the
///  per-slot context (llama.cpp splits `--ctx-size` across `--parallel`);
///  `total_slots` is the slot count.
/// - vllm `/v1/models`: `max_model_len` is the full per-request context
///  (continuous batching, not slot-split) → `(max_model_len, 1)`.
/// - mlx: no context introspection endpoint → `None`.
pub async fn probe_agent_ctx(runtime: &str, port: u16) -> Option<(i32, i32)> {
    let timeout = std::time::Duration::from_secs(3);
    match runtime {
        "llama.cpp" => {
            let url = format!("http://127.0.0.1:{port}/props");
            let v: serde_json::Value = SHARED_HTTP
                .get(&url)
                .timeout(timeout)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            parse_llama_props_ctx(&v)
        }
        "vllm" => {
            let url = format!("http://127.0.0.1:{port}/v1/models");
            let v: serde_json::Value = SHARED_HTTP
                .get(&url)
                .timeout(timeout)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            parse_vllm_models_ctx(&v)
        }
        _ => None,
    }
}

/// Extract `(per_slot_ctx, total_slots)` from a llama.cpp `/props` body.
/// `default_generation_settings.n_ctx` is the per-slot context; `total_slots`
/// is the parallel-slot count (absent on old builds → assume 1).
fn parse_llama_props_ctx(v: &serde_json::Value) -> Option<(i32, i32)> {
    let per_slot = v
        .get("default_generation_settings")
        .and_then(|s| s.get("n_ctx"))
        .and_then(|n| n.as_i64())
        .filter(|&n| n > 0)? as i32;
    let slots = v
        .get("total_slots")
        .and_then(|n| n.as_i64())
        .filter(|&n| n > 0)
        .unwrap_or(1) as i32;
    Some((per_slot, slots))
}

/// Extract `(max_model_len, 1)` from a vllm `/v1/models` body. vllm uses
/// continuous batching, so the full `max_model_len` is available per request
/// (slot count is effectively 1 for the agent-ctx sizing question).
fn parse_vllm_models_ctx(v: &serde_json::Value) -> Option<(i32, i32)> {
    let max_len = v
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("max_model_len"))
        .and_then(|n| n.as_i64())
        .filter(|&n| n > 0)? as i32;
    Some((max_len, 1))
}

/// Best-effort context window for an mlx_lm.server with no --max-kv-size:
/// read `max_position_embeddings` from the model dir's config.json (multimodal
/// architectures nest it under `text_config`). Capped at 64K — these windows
/// can claim 256K+ which is RAM-unrealistic for fleet Macs, and the capability
/// router only needs to know the endpoint clears its 16K/32K agent bars.
fn mlx_config_ctx(model_path: &str) -> Option<i32> {
    let raw = std::fs::read_to_string(PathBuf::from(model_path).join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let max = v
        .get("max_position_embeddings")
        .or_else(|| {
            v.get("text_config")
                .and_then(|t| t.get("max_position_embeddings"))
        })
        .and_then(|x| x.as_i64())
        .filter(|&n| n > 0)?;
    Some(max.min(65_536) as i32)
}

fn parse_flag_value(cmdline: &str, flag: &str) -> Option<String> {
    let mut parts = cmdline.split_whitespace();
    while let Some(p) = parts.next() {
        if p == flag {
            return parts.next().map(String::from);
        }
        if let Some(rest) = p.strip_prefix(&format!("{flag}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn pid_is_alive(pid: u32) -> bool {
    // `kill -0 <pid>` returns 0 if the process exists and we can signal it.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve the PID(s) of the process(es) currently LISTENING on `port`, by
/// asking the kernel — not by trusting a recorded PID.
///
/// Why: the deployment row's `pid` goes stale whenever the server is
/// restarted out-of-band (systemd `Restart=on-failure` respawns it with a
/// fresh PID, a manual relaunch, an OOM-kill + supervisor restart, etc.).
/// Killing the recorded PID then either no-ops (PID gone) or — worse —
/// kills an unrelated process that has since recycled that PID, while the
/// real llama-server keeps serving. Observed on sia 2026-05: unload killed
/// stale PID 1865734 and reported success, but the live llama-server (a
/// different PID) survived; free-for-build then "paused 0".
///
/// Strategy: prefer `ss -ltnp "sport = :PORT"` (Linux iproute2), parse the
/// `pid=<n>` token. Fall back to `lsof -ti tcp:PORT -sTCP:LISTEN` (macOS and
/// hosts without ss). Returns deduped PIDs of every listener on that port.
///
/// CRITICAL: this is how we avoid `pkill -f <pattern>` self-kills — we only
/// ever act on numeric PIDs the kernel reports as bound to the port.
async fn pids_listening_on_port(port: u16) -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();

    // ── Linux: ss ──────────────────────────────────────────────────────
    // `-l` listening, `-t` tcp, `-n` numeric, `-p` show process. The filter
    // `sport = :PORT` restricts to our port. Output lines carry a
    // `users:(("llama-server",pid=12345,fd=7))` field.
    if let Ok(out) = tokio::process::Command::new("ss")
        .args(["-ltnp", &format!("sport = :{port}")])
        .output()
        .await
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for cap in text.split("pid=").skip(1) {
            let digits: String = cap.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(pid) = digits.parse::<u32>()
                && !pids.contains(&pid)
            {
                pids.push(pid);
            }
        }
    }

    // ── Fallback: lsof (macOS, or Linux without iproute2) ───────────────
    // `-ti` = terse output, PIDs only; restrict to TCP listeners on the port.
    if pids.is_empty()
        && let Ok(out) = tokio::process::Command::new("lsof")
            .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
            .output()
            .await
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Ok(pid) = line.trim().parse::<u32>()
                && !pids.contains(&pid)
            {
                pids.push(pid);
            }
        }
    }

    pids
}

/// Stop whatever is actually LISTENING on `port`: resolve the live PID(s) via
/// [`pids_listening_on_port`], SIGTERM each (and its process group), wait up
/// to 10s, then SIGKILL the stragglers. The `fallback_pid` (the recorded
/// deployment PID) is folded in only as a belt-and-suspenders target so a
/// process that has already stopped listening but is still winding down still
/// gets reaped — it is never the SOLE target.
///
/// Returns the set of PIDs we signalled (for logging). Empty when nothing was
/// found on the port and no fallback was supplied.
///
/// Never uses `pkill -f` — every signal targets a numeric PID resolved from
/// the kernel, so this command can never match and kill itself.
async fn stop_listener_on_port(port: u16, fallback_pid: Option<u32>) -> Vec<u32> {
    let mut targets = pids_listening_on_port(port).await;
    if targets.is_empty() {
        if let Some(fp) = fallback_pid
            && pid_is_alive(fp)
        {
            tracing::warn!(
                port,
                fallback_pid = fp,
                "stop_listener_on_port: nothing listening on port; \
                 falling back to recorded deployment pid"
            );
            targets.push(fp);
        }
        if targets.is_empty() {
            tracing::info!(port, "stop_listener_on_port: no live listener found");
            return targets;
        }
    } else if let Some(fp) = fallback_pid
        && pid_is_alive(fp)
        && !targets.contains(&fp)
    {
        // Recorded pid differs from the live listener — the row was stale.
        // Reap it too (it may be a defunct sibling holding RAM), but the
        // listener PID above is the one that actually matters.
        tracing::warn!(
            port,
            recorded_pid = fp,
            live_pids = ?targets,
            "stop_listener_on_port: recorded deployment pid differs from live listener (stale row)"
        );
        targets.push(fp);
    }

    // SIGTERM each target and its process group. We launch each server as a
    // session leader (setsid in load_model's pre_exec), so its PID == PGID;
    // `kill -- -<pid>` signals the whole group, catching any helper children.
    for &pid in &targets {
        let _ = tokio::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output()
            .await;
        let _ = tokio::process::Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .output()
            .await;
    }

    // Wait up to 10s for graceful exit.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if targets.iter().all(|&p| !pid_is_alive(p)) {
            break;
        }
    }

    // Escalate to SIGKILL on whatever survived.
    for &pid in &targets {
        if pid_is_alive(pid) {
            tracing::warn!(
                pid,
                port,
                "SIGTERM didn't stop process; escalating to SIGKILL"
            );
            let _ = tokio::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .output()
                .await;
            let _ = tokio::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .output()
                .await;
        }
    }

    targets
}

/// Stop and disable the `llama-<port>.service` systemd user unit (if any)
/// before we kill the listener on `port`. Without this, the unit's
/// `Restart=on-failure` would respawn a fresh server the instant our
/// SIGTERM/SIGKILL lands (a signal counts as a non-clean exit), so the
/// "unloaded" model would silently come right back with a new PID.
///
/// Best-effort: `stop` resolves a respawn race; `disable` keeps it from
/// coming back on the next daemon-reload/reboot until the next load
/// rewrites + re-enables the unit. Failures (no systemd, no such unit,
/// no user session) are logged and ignored.
#[cfg(target_os = "linux")]
async fn stop_systemd_unit(port: u16) {
    use tokio::process::Command as TokCmd;
    let unit = format!("llama-{port}.service");
    for verb in ["stop", "disable"] {
        match TokCmd::new("systemctl")
            .args(["--user", verb, &unit])
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                tracing::info!(unit = %unit, verb, "model_runtime: systemd unit handled before kill");
            }
            Ok(out) => {
                // Non-fatal: unit may not exist, or there's no user manager.
                tracing::debug!(
                    unit = %unit,
                    verb,
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "model_runtime: systemctl returned non-zero (continuing)"
                );
            }
            Err(e) => {
                tracing::debug!(unit = %unit, verb, error = %e, "model_runtime: systemctl not available");
            }
        }
    }
}

/// Start (restart) the `llama-<port>.service` systemd user unit and return its
/// `MainPID`. Used on Linux so the inference server is owned by systemd
/// (persistent `user.slice` cgroup) instead of a manual setsid child that
/// logind kills when a cross-node `ff model autoload` SSH session closes.
///
/// `restart` (not `start`) so a stale unit instance is cleanly replaced — the
/// call is idempotent for reloads. Not `#[cfg]`-gated so the `cfg!`-guarded
/// call site type-checks on every target; it is only ever reached on Linux.
async fn start_systemd_unit(port: u16) -> Result<u32, String> {
    use tokio::process::Command as TokCmd;
    let unit = format!("llama-{port}.service");

    let st = TokCmd::new("systemctl")
        .args(["--user", "restart", &unit])
        .output()
        .await
        .map_err(|e| format!("systemctl restart {unit}: {e}"))?;
    if !st.status.success() {
        return Err(format!(
            "systemctl restart {unit} exited {}: {}",
            st.status,
            String::from_utf8_lossy(&st.stderr).trim()
        ));
    }

    // Resolve the actual serving PID. MainPID is "0" until the unit's
    // ExecStart has forked — restart returns once the service is started, so
    // this is populated, but guard against the 0 case anyway.
    let show = TokCmd::new("systemctl")
        .args(["--user", "show", "-p", "MainPID", "--value", &unit])
        .output()
        .await
        .map_err(|e| format!("systemctl show {unit}: {e}"))?;
    let raw = String::from_utf8_lossy(&show.stdout);
    let pid: u32 = raw
        .trim()
        .parse()
        .map_err(|_| format!("MainPID not numeric: {:?}", raw.trim()))?;
    if pid == 0 {
        return Err(format!("{unit} started but MainPID is 0 (not running)"));
    }
    Ok(pid)
}

/// Write a systemd user unit that mirrors the spawn we're about to run, so
/// the OS restarts this llama-server on failure. Best-effort — failures
/// log and return Ok-like so the caller falls through to the manual spawn.
///
/// The unit is named `llama-<port>.service` so each loaded model gets its
/// own supervisor. `Restart=on-failure` (not `always`) so a clean
/// `ff model unload` doesn't trigger a respawn loop. RestartSec=10 gives
/// the OS time to reclaim OOM-killed memory before the relaunch.
///
/// On every `ff model autoload`, the unit is REWRITTEN with the latest
/// args and a `systemctl daemon-reload` is issued, so changes to the
/// catalog (e.g. ctx size, parallel slots) propagate on next load.
///
/// Not `#[cfg]`-gated so the `cfg!(target_os = "linux")`-guarded call site in
/// `load_model` type-checks on every target; it is only ever called on Linux.
async fn write_systemd_unit(
    program: &str,
    args: &[String],
    port: u16,
    log_path: &std::path::Path,
) -> Result<(), String> {
    use tokio::process::Command as TokCmd;

    let home = std::env::var("HOME").map_err(|e| format!("HOME unset: {e}"))?;
    let unit_dir = std::path::PathBuf::from(&home).join(".config/systemd/user");
    tokio::fs::create_dir_all(&unit_dir)
        .await
        .map_err(|e| format!("mkdir {}: {e}", unit_dir.display()))?;

    // Build the ExecStart line. Each arg is space-separated. systemd doesn't
    // do shell expansion on ExecStart, so quoting is only needed for args
    // that contain whitespace — llama-server's args (file paths, host, port,
    // numeric flags) generally don't, but we quote defensively.
    let needs_quote = |s: &str| s.contains(' ') || s.contains('\t');
    let mut exec_start = program.to_string();
    for a in args {
        exec_start.push(' ');
        if needs_quote(a) {
            // systemd uses double-quotes; escape any inner doubles.
            let escaped = a.replace('\\', "\\\\").replace('"', "\\\"");
            exec_start.push('"');
            exec_start.push_str(&escaped);
            exec_start.push('"');
        } else {
            exec_start.push_str(a);
        }
    }

    // LD_LIBRARY_PATH was applied to `cmd.env(...)` above; mirror it on
    // the unit so systemd-spawned restarts find libmtmd.so.0 next to
    // the llama-server binary. We re-derive it from `program`'s parent
    // to stay in sync with the spawn logic.
    let ld_library_path = std::path::Path::new(program)
        .parent()
        .map(|p| p.display().to_string());

    let unit = format!(
        "# Auto-generated by ff_agent::model_runtime on `ff model autoload`.\n\
         # Owned by ForgeFleet — edits here will be overwritten on next load.\n\
         [Unit]\n\
         Description=ForgeFleet-supervised inference server on port {port}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         StartLimitIntervalSec=3600\n\
         StartLimitBurst=20\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         {ld_env}\n\
         [Install]\n\
         WantedBy=default.target\n",
        port = port,
        exec_start = exec_start,
        log = log_path.display(),
        ld_env = ld_library_path
            .map(|p| format!("Environment=LD_LIBRARY_PATH={p}"))
            .unwrap_or_default(),
    );

    let unit_path = unit_dir.join(format!("llama-{port}.service"));
    tokio::fs::write(&unit_path, unit)
        .await
        .map_err(|e| format!("write {}: {e}", unit_path.display()))?;
    tracing::info!(unit = %unit_path.display(), "model_runtime: wrote systemd unit");

    // daemon-reload + enable so the unit is known to systemd and survives
    // reboots. The caller (`load_model`) starts the unit via
    // `start_systemd_unit` right after this returns — systemd owns the
    // process so it survives a cross-node autoload's SSH session closing.
    let dr = TokCmd::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output()
        .await
        .map_err(|e| format!("daemon-reload: {e}"))?;
    if !dr.status.success() {
        return Err(format!(
            "daemon-reload exited {}: {}",
            dr.status,
            String::from_utf8_lossy(&dr.stderr)
        ));
    }

    let en = TokCmd::new("systemctl")
        .args(["--user", "enable", &format!("llama-{port}.service")])
        .output()
        .await
        .map_err(|e| format!("enable: {e}"))?;
    if !en.status.success() {
        // Non-fatal: enable might fail on macOS-style "no system manager"
        // sessions or transient bus issues. The unit file is still written.
        tracing::warn!(
            stderr = %String::from_utf8_lossy(&en.stderr),
            "model_runtime: systemctl enable failed (unit written but won't autostart on reboot)"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_gguf_finds_nested_weights() {
        // veronica's real layout: <root>/<dup-name>/<file>.gguf — the .gguf is
        // two levels below the registered file_path. The pre-fix single-level
        // scan returned "no .gguf files" and the autoscaler retried forever.
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("qwen3-coder-30b-a3b").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        let gguf = nested.join("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf");
        std::fs::write(&gguf, b"GGUF-bytes").unwrap();
        let resolved =
            resolve_gguf_for_llamacpp(tmp.path().to_str().unwrap()).expect("should find nested");
        assert_eq!(resolved, gguf.to_string_lossy());
    }

    #[test]
    fn resolve_gguf_picks_largest_across_subdirs() {
        // When multiple .gguf exist at different depths, the largest wins.
        let tmp = tempfile::tempdir().unwrap();
        let small = tmp.path().join("small.gguf");
        std::fs::write(&small, vec![0u8; 10]).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let big = sub.join("big.gguf");
        std::fs::write(&big, vec![0u8; 5000]).unwrap();
        let resolved = resolve_gguf_for_llamacpp(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(resolved, big.to_string_lossy());
    }

    #[test]
    fn resolve_gguf_direct_file_passthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let gguf = tmp.path().join("model.gguf");
        std::fs::write(&gguf, b"x").unwrap();
        let s = gguf.to_string_lossy().to_string();
        assert_eq!(resolve_gguf_for_llamacpp(&s).unwrap(), s);
    }

    #[test]
    fn resolve_gguf_errors_when_truly_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();
        assert!(resolve_gguf_for_llamacpp(tmp.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn tool_calling_chat_gets_jinja() {
        // Tool-capable chat models must launch with --jinja so the server
        // parses structured tool_calls instead of leaking free-form text.
        assert_eq!(llamacpp_chat_flags(true), vec!["--jinja".to_string()]);
    }

    #[test]
    fn non_tool_chat_gets_no_extra_flags() {
        // Non-tool models get nothing — --jinja buys them nothing and a bad
        // embedded template could refuse to launch.
        assert!(llamacpp_chat_flags(false).is_empty());
    }

    #[test]
    fn parses_llama_props_per_slot_ctx_and_slots() {
        // Shape observed live on veronica's qwen36 llama.cpp /props.
        let v = serde_json::json!({
            "default_generation_settings": { "n_ctx": 4096 },
            "total_slots": 4,
            "model_path": "/x"
        });
        assert_eq!(parse_llama_props_ctx(&v), Some((4096, 4)));
    }

    #[test]
    fn llama_props_missing_total_slots_defaults_to_one() {
        let v = serde_json::json!({ "default_generation_settings": { "n_ctx": 32768 } });
        assert_eq!(parse_llama_props_ctx(&v), Some((32768, 1)));
    }

    #[test]
    fn llama_props_without_n_ctx_is_none() {
        let v = serde_json::json!({ "total_slots": 4 });
        assert_eq!(parse_llama_props_ctx(&v), None);
    }

    #[test]
    fn parses_vllm_max_model_len() {
        let v = serde_json::json!({ "data": [{ "id": "m", "max_model_len": 65536 }] });
        assert_eq!(parse_vllm_models_ctx(&v), Some((65536, 1)));
    }

    #[test]
    fn vllm_without_max_model_len_is_none() {
        let v = serde_json::json!({ "data": [{ "id": "m" }] });
        assert_eq!(parse_vllm_models_ctx(&v), None);
    }

    #[test]
    fn capable_chat_model_defaults_to_agent_profile() {
        // tool-calling chat model, no explicit --parallel → promoted.
        assert!(resolve_agent_profile(false, ServingMode::Chat, true, None));
    }

    #[test]
    fn explicit_parallel_opts_out_of_agent_profile() {
        // Operator pinned --parallel for throughput → not promoted.
        assert!(!resolve_agent_profile(
            false,
            ServingMode::Chat,
            true,
            Some(2)
        ));
    }

    #[test]
    fn non_tool_calling_chat_model_is_not_promoted() {
        // e.g. gemma-4 / a reasoner: chat but not tool-calling → stays default.
        assert!(!resolve_agent_profile(
            false,
            ServingMode::Chat,
            false,
            None
        ));
    }

    #[test]
    fn embedders_and_rerankers_are_never_promoted() {
        // Single forward pass, no per-slot ctx split — even if (wrongly) flagged.
        assert!(!resolve_agent_profile(
            false,
            ServingMode::Embedding,
            true,
            None
        ));
        assert!(!resolve_agent_profile(
            false,
            ServingMode::Reranking,
            true,
            None
        ));
    }

    #[test]
    fn explicit_agent_flag_always_forces_profile() {
        // --agent wins regardless of kind / pinned parallel.
        assert!(resolve_agent_profile(
            true,
            ServingMode::Chat,
            false,
            Some(4)
        ));
        assert!(resolve_agent_profile(
            true,
            ServingMode::Embedding,
            false,
            None
        ));
    }
}
