//! `ff offload` — the credit-saver CLI.
//!
//! Direct + measurable counterpart to the `fleet_offload` MCP tool. Picks the
//! best WARM tool-capable deployment via the V111 capability router
//! (`ff_db::pg_pick_agent_endpoint` — the SAME scored selector the MCP handler
//! and `fleet_route` use, so there's no parallel router), dispatches the task
//! to it over the OpenAI-compatible API, and prints which endpoint/model
//! handled it plus the result. If no warm tool-capable endpoint exists it
//! prints a `do_in_cloud` decision so the caller does the work itself.
//!
//! v1 = prefer-warm only. No cold-load / load-time logic (that's v2).

use crate::{CYAN, GREEN, RED, RESET, YELLOW};
use anyhow::Result;
use std::time::{Duration, Instant};

const DEFAULT_MAX_TOKENS: u32 = 4096;
const MIN_MAX_TOKENS: u32 = 256;
const MAX_MAX_TOKENS: u32 = 8192;
/// Generous ceiling — local models on memory-tight hosts can be slow on bulk
/// codegen. Mirrors the MCP handler + GatewayLlmExec per-call timeout.
const TIMEOUT_SECS: u64 = 600;

pub async fn handle_offload(
    prompt: &str,
    output: &str,
    kind: Option<&str>,
    est_output_tokens: Option<u32>,
    min_ctx: i32,
) -> Result<()> {
    let json_out = output.eq_ignore_ascii_case("json");
    let min_ctx = min_ctx.max(1);
    let max_tokens = est_output_tokens
        .map(|v| v.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS))
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;

    if !json_out {
        eprintln!(
            "{CYAN}▶ ff offload{RESET}  \x1b[2mmin_ctx={min_ctx} max_tokens={max_tokens}{}{RESET}",
            kind.map(|k| format!(" kind={k}")).unwrap_or_default()
        );
    }

    // ── Step 1: find a WARM tool-capable endpoint (V111 capability router).
    let candidate = ff_db::pg_pick_agent_endpoint(&pool, min_ctx, &[])
        .await
        .map_err(|e| anyhow::anyhow!("offload router query failed: {e}"))?;

    let candidate = match candidate {
        Some(c) => c,
        None => {
            // ── No warm endpoint → do_in_cloud fallback.
            let reason = format!(
                "no warm tool-capable endpoint (require_tool_calling=true, \
                 usable_agent_ctx>={min_ctx}). Do it in cloud; or warm one with: \
                 ff model load <library_id> --agent"
            );
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "offloaded": false,
                        "decision": "do_in_cloud",
                        "reason": reason,
                        "kind": kind,
                        "min_ctx": min_ctx,
                    }))?
                );
            } else {
                eprintln!("{YELLOW}● decision: do_in_cloud{RESET}");
                eprintln!("\x1b[2m  {reason}{RESET}");
            }
            return Ok(());
        }
    };

    // ── Step 2: dispatch to the warm local endpoint over the OpenAI API.
    let model = candidate
        .catalog_id
        .clone()
        .unwrap_or_else(|| candidate.catalog_name.clone().unwrap_or_default());
    let url = format!(
        "{}/v1/chat/completions",
        candidate.endpoint.trim_end_matches('/')
    );

    if !json_out {
        eprintln!(
            "{GREEN}● offloading to {}{RESET} \x1b[2m({}, tier {}, ctx {}){RESET}",
            candidate.endpoint,
            model,
            candidate.tier,
            candidate
                .usable_agent_ctx
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into())
        );
    }

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "stream": false,
    });

    let client = reqwest::Client::new();
    let started = Instant::now();
    let resp = client
        .post(&url)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("offload dispatch to {} failed: {e}", candidate.endpoint))?;

    let latency = started.elapsed();
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("read offload response body: {e}"))?;
    if !status.is_success() {
        eprintln!(
            "{RED}✗ endpoint {} (model {model}) returned HTTP {status}{RESET}",
            candidate.endpoint
        );
        eprintln!("\x1b[2m{text}\x1b[0m");
        std::process::exit(1);
    }

    let payload: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse offload response JSON: {e}"))?;
    let result = extract_completion_text(&payload).unwrap_or_default();

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "offloaded": true,
                "decision": "offloaded",
                "endpoint": candidate.endpoint,
                "worker_name": candidate.worker_name,
                "model": model,
                "tier": candidate.tier,
                "usable_agent_ctx": candidate.usable_agent_ctx,
                "kind": kind,
                "latency_ms": latency.as_millis(),
                "result": result,
            }))?
        );
    } else {
        eprintln!(
            "\x1b[2m  handled by {} in {} ms — review before using{RESET}\n",
            candidate.worker_name,
            latency.as_millis()
        );
        println!("{result}");
    }

    Ok(())
}

/// Pull the assistant text out of an OpenAI-compatible chat completion. Mirrors
/// the ff-mcp `extract_completion_text` helper (kept local to avoid a crate
/// dependency just for one parser).
fn extract_completion_text(payload: &serde_json::Value) -> Option<String> {
    let choice = payload.get("choices")?.as_array()?.first()?;
    if let Some(s) = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
    {
        return Some(s.to_string());
    }
    choice
        .get("text")
        .and_then(|v| v.as_str())
        .map(String::from)
}
