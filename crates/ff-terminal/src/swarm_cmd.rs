//! `ff swarm` — fleet-wide sub-agent orchestration.
//!
//! Pattern (Kimi K2.6 style, but on YOUR hardware):
//!   1. **Planner** decomposes the goal into N independent sub-tasks
//!      via a single LLM call.
//!   2. **Executor** enqueues one `fleet_tasks` row per sub-task,
//!      pinned to a member with the right capability. Workers on
//!      fleet computers compete for the rows.
//!   3. **Aggregator** polls until all sub-tasks reach a terminal
//!      state, then either prints the raw results or calls a
//!      synthesizer LLM to produce a unified summary.
//!
//! This is the fan-out side. For "outcome-driven multi-step DAG"
//! work (planning, judge, retry) use `ff session` — that's the
//! pre-existing session_runner. Swarm is the simpler shape: one
//! plan, N independent sub-tasks, one synthesis.

use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Subcommand)]
pub enum SwarmCommand {
    /// Run a full swarm: plan → fan out → wait → synthesize. Prints
    /// the final synthesis to stdout. Use `--keep` to leave the
    /// individual sub-task rows visible in `ff tasks list` afterward.
    Run {
        /// What you want the swarm to accomplish. The planner LLM
        /// decomposes this into N independent sub-tasks.
        goal: String,
        /// Number of sub-tasks to plan. Caps the fan-out so a runaway
        /// planner can't queue 500 tasks. Default 8.
        #[arg(long, default_value_t = 8)]
        fanout: usize,
        /// Capability filter passed to each sub-task. Workers without
        /// this capability won't claim the row.
        #[arg(long, default_value = "ff")]
        capability: String,
        /// Computer names that must NOT claim any sub-task,
        /// comma-separated (e.g. "sia,adele,rihanna,beyonce" to keep the
        /// swarm off the DGX pairs, or "taylor" to spare the leader).
        /// Sets fleet_tasks.excludes_computer_ids on every fanned-out
        /// sub-task; unknown names are warned about and skipped, never
        /// silently dropped.
        #[arg(long, default_value = "")]
        exclude: String,
        /// LLM endpoint to use for planning + synthesis. Defaults to
        /// the gateway's local route which picks the cheapest tier.
        #[arg(long, default_value = "http://127.0.0.1:51002/v1/chat/completions")]
        llm: String,
        /// Override the planner model id. If empty, the LLM endpoint's
        /// /v1/models is probed for an id.
        #[arg(long, default_value = "")]
        model: String,
        /// How long (seconds) to wait for sub-tasks before giving up.
        #[arg(long, default_value_t = 1800)]
        timeout_secs: u64,
        /// Write the synthesis to a file in addition to stdout.
        #[arg(long)]
        output: Option<std::path::PathBuf>,
        /// Skip the synthesis pass; just print the raw per-task results.
        #[arg(long, default_value_t = false)]
        no_synthesize: bool,
        /// Don't cancel sub-tasks on timeout — useful for debugging.
        #[arg(long, default_value_t = false)]
        keep: bool,
    },
    /// Run only the planner — print the proposed sub-tasks without
    /// dispatching. Useful for dry-running before paying for fanout.
    Plan {
        goal: String,
        #[arg(long, default_value_t = 8)]
        fanout: usize,
        #[arg(long, default_value = "http://127.0.0.1:51002/v1/chat/completions")]
        llm: String,
        #[arg(long, default_value = "")]
        model: String,
    },
}

pub async fn handle_swarm(cmd: SwarmCommand) -> Result<()> {
    match cmd {
        SwarmCommand::Plan {
            goal,
            fanout,
            llm,
            model,
        } => {
            let plan = plan_subtasks(&goal, fanout, &llm, &model).await?;
            println!("PLAN ({} sub-tasks):", plan.len());
            for (i, t) in plan.iter().enumerate() {
                println!("  {:>2}. {}", i + 1, t);
            }
            Ok(())
        }
        SwarmCommand::Run {
            goal,
            fanout,
            capability,
            exclude,
            llm,
            model,
            timeout_secs,
            output,
            no_synthesize,
            keep: _,
        } => {
            run_swarm(
                &goal,
                fanout,
                &capability,
                &exclude,
                &llm,
                &model,
                timeout_secs,
                output,
                no_synthesize,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_swarm(
    goal: &str,
    fanout: usize,
    capability: &str,
    exclude: &str,
    llm: &str,
    model: &str,
    timeout_secs: u64,
    output: Option<std::path::PathBuf>,
    no_synthesize: bool,
) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow!("connect Postgres: {e}"))?;

    // Resolve --exclude names → computer_ids once; every sub-task carries
    // the same exclusion set. Unknown names are surfaced as a warning and
    // skipped (no silent drop), mirroring `ff tasks add --exclude`.
    let exclude_ids = resolve_exclude_ids(&pool, exclude).await;

    eprintln!("planner: decomposing goal into {fanout} sub-tasks…");
    let subtasks = plan_subtasks(goal, fanout, llm, model).await?;
    if subtasks.is_empty() {
        return Err(anyhow!("planner returned 0 sub-tasks"));
    }
    eprintln!("planner: got {} sub-tasks", subtasks.len());

    let leader: Option<Uuid> =
        sqlx::query_scalar("SELECT computer_id FROM fleet_leader_state LIMIT 1")
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();

    let parent: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (task_type, summary, payload, priority, created_by_computer_id)
        VALUES ('swarm', $1, $2, 70, $3)
        RETURNING id
        "#,
    )
    .bind(format!("swarm: {}", truncate(goal, 120)))
    .bind(serde_json::json!({
        "kind": "swarm",
        "goal": goal,
        "fanout": subtasks.len(),
        "capability": capability,
    }))
    .bind(leader)
    .fetch_one(&pool)
    .await
    .context("insert swarm parent")?;

    eprintln!(
        "executor: enqueuing {} sub-tasks (parent={parent})…",
        subtasks.len()
    );
    let mut child_ids = Vec::with_capacity(subtasks.len());
    for (i, sub) in subtasks.iter().enumerate() {
        let shell_safe = sub.replace('\'', "'\\''");
        let cmd = format!("ff run '{shell_safe}'");
        let id = ff_agent::task_runner::pg_enqueue_shell_task_with_options(
            &pool,
            &format!("swarm[{}/{}]: {}", i + 1, subtasks.len(), truncate(sub, 80)),
            &cmd,
            &[capability.to_string()],
            None, // any member with the capability
            Some(parent),
            70,
            leader,
            false,
            &exclude_ids,
        )
        .await
        .map_err(|e| anyhow!("enqueue sub-task {i}: {e}"))?;
        child_ids.push(id);
    }

    eprintln!(
        "executor: dispatched {} task(s); waiting up to {}s for completion",
        child_ids.len(),
        timeout_secs
    );

    let results = wait_for_children(&pool, &child_ids, timeout_secs).await?;

    let done = results.iter().filter(|r| r.status == "completed").count();
    let failed = results.iter().filter(|r| r.status == "failed").count();
    eprintln!("aggregator: {done} completed, {failed} failed");

    let raw = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "## sub-task {}: {}\n\nstatus: {}\n\n{}\n",
                i + 1,
                truncate(&r.summary, 120),
                r.status,
                r.result_preview.as_deref().unwrap_or("(no output)")
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n\n");

    let final_text = if no_synthesize {
        format!("# Swarm raw results\n\nGoal: {goal}\n\n---\n\n{raw}")
    } else {
        eprintln!("aggregator: synthesizing final result…");
        match synthesize(goal, &raw, llm, model).await {
            Ok(s) => format!(
                "# Swarm synthesis\n\nGoal: {goal}\n\n{s}\n\n## Raw sub-task results\n\n{raw}"
            ),
            Err(e) => {
                eprintln!("warn: synthesis failed ({e}); falling back to raw");
                format!("# Swarm raw results (synthesis failed)\n\nGoal: {goal}\n\n---\n\n{raw}")
            }
        }
    };

    if let Some(path) = &output {
        std::fs::write(path, &final_text).context("write swarm output")?;
        eprintln!("wrote: {}", path.display());
    }
    println!("{final_text}");
    Ok(())
}

#[derive(Debug, Clone)]
struct ChildResult {
    summary: String,
    status: String,
    result_preview: Option<String>,
}

async fn wait_for_children(
    pool: &PgPool,
    ids: &[Uuid],
    timeout_secs: u64,
) -> Result<Vec<ChildResult>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let rows: Vec<(Uuid, String, String, Option<serde_json::Value>)> = sqlx::query_as(
            r#"
            SELECT id, summary, status, result
              FROM fleet_tasks
             WHERE id = ANY($1)
             ORDER BY created_at
            "#,
        )
        .bind(ids)
        .fetch_all(pool)
        .await
        .context("poll child tasks")?;

        let pending = rows
            .iter()
            .filter(|(_, _, s, _)| !matches!(s.as_str(), "completed" | "failed" | "cancelled"))
            .count();
        if pending == 0 {
            return Ok(rows
                .into_iter()
                .map(|(_id, summary, status, result)| {
                    let preview = result.and_then(|v| {
                        v.get("stdout")
                            .and_then(|s| s.as_str())
                            .map(|s| s.chars().take(2000).collect::<String>())
                            .or_else(|| {
                                v.get("output")
                                    .and_then(|s| s.as_str())
                                    .map(|s| s.chars().take(2000).collect::<String>())
                            })
                    });
                    ChildResult {
                        summary,
                        status,
                        result_preview: preview,
                    }
                })
                .collect());
        }
        if std::time::Instant::now() > deadline {
            eprintln!("warn: timeout; {pending} sub-task(s) still pending");
            return Ok(rows
                .into_iter()
                .map(|(_id, summary, status, _result)| ChildResult {
                    summary,
                    status,
                    result_preview: None,
                })
                .collect());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ─── LLM calls ───────────────────────────────────────────────────────────────

async fn plan_subtasks(goal: &str, fanout: usize, llm: &str, model: &str) -> Result<Vec<String>> {
    let prompt = format!(
        "Decompose the following goal into exactly {fanout} independent sub-tasks. \
         Each sub-task must be self-contained — runnable without seeing the others' \
         output. Return ONLY a JSON array of strings, no prose, no markdown. \
         Each string is one full sub-task instruction.\n\nGoal: {goal}"
    );
    let resp = call_llm(llm, model, &prompt, 4096, 0.3).await?;
    let arr = parse_json_array(&resp).ok_or_else(|| {
        anyhow!(
            "planner returned non-JSON-array response: {}",
            truncate(&resp, 400)
        )
    })?;
    Ok(arr.into_iter().take(fanout).collect())
}

async fn synthesize(goal: &str, raw_results: &str, llm: &str, model: &str) -> Result<String> {
    let prompt = format!(
        "You are synthesizing the output of {n} parallel sub-tasks that were \
         dispatched to accomplish a single goal. Produce a single coherent \
         markdown response that addresses the original goal, citing which \
         sub-task contributed which information. Keep it under 1500 words.\n\n\
         Goal: {goal}\n\n\
         Sub-task results:\n{raw_results}",
        n = raw_results.matches("## sub-task").count()
    );
    call_llm(llm, model, &prompt, 8192, 0.5).await
}

async fn call_llm(
    endpoint: &str,
    model: &str,
    prompt: &str,
    max_tokens: usize,
    temperature: f32,
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;

    let model_id = if model.is_empty() {
        match resolve_live_model_id(&client, endpoint).await {
            Some(model_id) => model_id,
            None => probe_model_id(&client, endpoint).await.ok_or_else(|| {
                anyhow!("no healthy chat-capable LLM server in fleet; pass --model or --llm")
            })?,
        }
    } else {
        model.to_string()
    };

    let body = serde_json::json!({
        "model": model_id,
        "messages": [
            {"role": "user", "content": prompt}
        ],
        "max_tokens": max_tokens,
        "temperature": temperature,
    });

    let resp = client.post(endpoint).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "LLM {endpoint} returned {status}: {}",
            truncate(&text, 400)
        ));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse LLM response: {}", truncate(&text, 400)))?;
    let message = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .ok_or_else(|| anyhow!("no choices[0].message in {}", truncate(&text, 400)))?;
    // Some local reasoning-model servers emit the visible answer outside
    // `content`. Try OpenAI-compatible fields before erroring.
    let content = message
        .get("content")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            message
                .get("reasoning_content")
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            message
                .get("reasoning")
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
        })
        .ok_or_else(|| {
            anyhow!(
                "no choices[0].message.content, .reasoning_content, or .reasoning in {}",
                truncate(&text, 400)
            )
        })?;
    Ok(content.to_string())
}

async fn resolve_live_model_id(client: &reqwest::Client, chat_endpoint: &str) -> Option<String> {
    let servers_url = chat_endpoint
        .strip_suffix("/v1/chat/completions")
        .map(|base| format!("{base}/api/llm/servers"))
        .unwrap_or_else(|| chat_endpoint.replace("/v1/chat/completions", "/api/llm/servers"));
    let resp = client
        .get(&servers_url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let servers = v
        .as_array()
        .or_else(|| v.get("data").and_then(|data| data.as_array()))
        .or_else(|| v.get("servers").and_then(|servers| servers.as_array()))?;

    let mut candidates: Vec<(bool, bool, f64, String)> = servers
        .iter()
        .filter_map(|server| {
            if server.get("healthy").and_then(|h| h.as_bool()) != Some(true) {
                return None;
            }
            let status_ok = match server.get("status").and_then(|s| s.as_str()) {
                Some("active") | None => true,
                _ => false,
            };
            if !status_ok {
                return None;
            }
            let model = server.get("model")?.as_str()?.to_string();
            let model_lower = model.to_lowercase();
            let reasoning_only = model_lower.contains("thinking")
                || model_lower.contains("-r1")
                || model_lower.contains("reasoning")
                || model_lower.contains("qwq");
            let preferred = model_lower.contains("instruct")
                || model_lower.contains("coder")
                || model_lower.contains("chat");
            let queue_depth = server
                .get("queue_depth")
                .and_then(|q| q.as_f64())
                .unwrap_or(0.0);
            Some((reasoning_only, preferred, queue_depth, model))
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }
    let has_non_reasoning = candidates
        .iter()
        .any(|(reasoning_only, _, _, _)| !*reasoning_only);
    if has_non_reasoning {
        candidates.retain(|(reasoning_only, _, _, _)| !*reasoning_only);
    }
    candidates.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    candidates.into_iter().next().map(|(_, _, _, model)| model)
}

async fn probe_model_id(client: &reqwest::Client, chat_endpoint: &str) -> Option<String> {
    let models_url = chat_endpoint
        .strip_suffix("/v1/chat/completions")
        .map(|base| format!("{base}/v1/models"))
        .unwrap_or_else(|| chat_endpoint.replace("chat/completions", "models"));
    let resp = client
        .get(&models_url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("data")?
        .as_array()?
        .first()?
        .get("id")?
        .as_str()
        .map(|s| s.to_string())
}

fn parse_json_array(s: &str) -> Option<Vec<String>> {
    // The LLM might wrap the array in ```json...``` or include prose
    // before/after. Pull the first balanced [...] block out.
    let start = s.find('[')?;
    let end = s.rfind(']')?;
    if end <= start {
        return None;
    }
    let slice = &s[start..=end];
    let v: serde_json::Value = serde_json::from_str(slice).ok()?;
    let arr = v.as_array()?;
    Some(
        arr.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

/// Parse a comma-separated `--exclude` worker-name list (e.g.
/// `"sia,adele"`) into the parsed, de-duplicated names, ignoring empty
/// segments and surrounding whitespace. Pure — the DB lookup that turns
/// names into `computer_id`s lives in [`resolve_exclude_ids`]; this is
/// split out so the parsing is unit-testable without a Postgres pool.
fn parse_exclude_names(exclude: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for raw in exclude.split(',') {
        let name = raw.trim();
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Resolve a `--exclude` worker-name list into `computer_id`s. Unknown
/// names are surfaced as a warning and skipped (never silently dropped),
/// so a typo can't quietly fail to exclude the host you meant — same
/// contract as `ff tasks add --exclude`.
async fn resolve_exclude_ids(pool: &PgPool, exclude: &str) -> Vec<Uuid> {
    let names = parse_exclude_names(exclude);
    let mut ids: Vec<Uuid> = Vec::new();
    let mut resolved: Vec<String> = Vec::new();
    for name in &names {
        match sqlx::query_scalar::<_, Uuid>("SELECT id FROM computers WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(id)) => {
                ids.push(id);
                resolved.push(name.clone());
            }
            Ok(None) => {
                eprintln!("warning: --exclude '{name}' matches no computer; skipping")
            }
            Err(e) => {
                eprintln!("warning: resolving --exclude '{name}': {e}; skipping")
            }
        }
    }
    if !ids.is_empty() {
        eprintln!(
            "executor: excluding {} computer(s) from sub-task claims: {}",
            ids.len(),
            resolved.join(", ")
        );
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::parse_exclude_names;

    #[test]
    fn parse_exclude_names_trims_dedups_and_drops_empties() {
        assert_eq!(parse_exclude_names(""), Vec::<String>::new());
        assert_eq!(parse_exclude_names("  "), Vec::<String>::new());
        assert_eq!(
            parse_exclude_names("sia, adele ,rihanna"),
            vec!["sia", "adele", "rihanna"]
        );
        // empty segments from leading/trailing/double commas are ignored
        assert_eq!(parse_exclude_names(",taylor,,"), vec!["taylor"]);
        // duplicates collapse
        assert_eq!(parse_exclude_names("sia,sia,adele"), vec!["sia", "adele"]);
    }
}
