//! `ff offload` — the credit-saver CLI.
//!
//! Direct + measurable counterpart to the `fleet_offload` MCP tool. Picks the
//! best WARM endpoint via `ff_db::pg_pick_offload_endpoint` — capability +
//! kind-aware (a coder for code work), least-loaded-host tiebreak, built on the
//! SAME `pg_route_deployments` scorer the MCP handler and `fleet_route` use so
//! there's no parallel router. Dispatches over the OpenAI-compatible API
//! (thinking disabled so the answer isn't eaten by chain-of-thought) and prints
//! which endpoint/model handled it plus the result. If no warm tool-capable
//! endpoint exists it prints a `do_in_cloud` decision so the caller proceeds.
//!
//! Prefer-warm only — it never cold-loads or waits for a model synchronously
//! (that's orchestrator P3). But on a cold miss it DOES record the unmet demand
//! so the P3 autoscaler warms a matching endpoint for the next call.

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

    // ── Step 1: pick the best WARM endpoint, capability+kind-aware.
    // Prefer a model whose workload matches the task kind (coder for code),
    // fall back to any tool-capable model, then break ties by least-loaded host.
    let candidate = ff_db::pg_pick_offload_endpoint(&pool, min_ctx, kind, &[])
        .await
        .map_err(|e| anyhow::anyhow!("offload router query failed: {e}"))?;

    let candidate = match candidate {
        Some(c) => c,
        None => {
            // ── No warm endpoint → do_in_cloud fallback. First record the UNMET
            // demand so the P3 autoscaler can warm capacity for next time —
            // unmet offload demand (cold → cloud) is exactly what it must see to
            // scale up. Recording only on the warm happy path (below) leaves the
            // autoscaler blind to demand it didn't serve. Distinct `_unmet`
            // source keeps satisfied vs unmet offload demand separable in
            // telemetry; both count toward the demand vector. Fire-and-forget.
            let signaled = ff_db::record_session_work_signal(
                &pool,
                None,
                kind.unwrap_or("general"),
                "offload_unmet",
            )
            .await
            .map_err(|e| tracing::warn!(error = %e, "unmet demand signal write failed (offload)"))
            .is_ok();
            let reason = format!(
                "no warm tool-capable endpoint (require_tool_calling=true, \
                 usable_agent_ctx>={min_ctx}). Do it in cloud — the P3 autoscaler \
                 has been signaled and will warm a matching endpoint if enabled; \
                 retry later to run it locally. Or warm one now with: \
                 ff model load <library_id> --agent"
            );
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "offloaded": false,
                        "decision": "do_in_cloud",
                        "reason": reason,
                        "autoscaler_signaled": signaled,
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

    // ── Orchestrator P2: record the per-session work-kind demand signal
    // (fire-and-forget — a telemetry write must never fail the offload).
    // No session_id at the CLI offload path → falls back to an 'adhoc:offload'
    // bucket inside record_session_work_signal.
    if let Err(e) =
        ff_db::record_session_work_signal(&pool, None, kind.unwrap_or("general"), "offload").await
    {
        tracing::warn!(error = %e, "demand signal write failed (offload)");
    }

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
        // Offload wants the answer, not chain-of-thought. Qwen3-style "thinking"
        // models otherwise burn the token budget on <think> reasoning and can
        // return empty content under a tight cap. Harmless on servers (mlx /
        // some llama.cpp builds) that don't recognize the field.
        "chat_template_kwargs": {"enable_thinking": false},
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
    // Defensively strip any <think>…</think> a thinking model emitted anyway
    // (belt-and-suspenders with chat_template_kwargs.enable_thinking=false).
    let result = strip_think(&extract_completion_text(&payload).unwrap_or_default());

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

/// Strip any `<think>…</think>` blocks a reasoning model emitted, returning the
/// trimmed remainder.
// NOTE: keep in sync with `strip_think_block` in ff-mcp/src/handlers.rs — both
// the CLI and MCP offload paths must scrub reasoning identically.
fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    // 1) Remove well-formed <think>…</think> pairs, left-to-right.
    loop {
        let Some(open) = out.find("<think>") else {
            break;
        };
        match out[open..].find("</think>") {
            Some(rel) => {
                let close = open + rel + "</think>".len();
                out.replace_range(open..close, "");
            }
            // 2) Unclosed opener — a thinking model cut off mid-reasoning under a
            //    token cap. Everything from <think> on is reasoning; drop it.
            None => {
                out.truncate(open);
                break;
            }
        }
    }
    // 3) A lone trailing </think> with no opener (the open tag was consumed by
    //    the chat template): the answer is whatever follows the last </think>.
    if let Some(i) = out.rfind("</think>") {
        out = out[i + "</think>".len()..].to_string();
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::strip_think;

    #[test]
    fn strip_think_handles_all_shapes() {
        // well-formed pair
        assert_eq!(strip_think("<think>reasoning</think>answer"), "answer");
        // lone trailing </think> (open tag consumed by the chat template)
        assert_eq!(
            strip_think("reasoning here</think>the answer"),
            "the answer"
        );
        // unclosed opener — model cut off mid-reasoning under a token cap
        assert_eq!(strip_think("<think>cut off mid thought"), "");
        // unclosed opener after real content
        assert_eq!(
            strip_think("partial answer<think>then cut"),
            "partial answer"
        );
        // no tags at all — passthrough
        assert_eq!(strip_think("plain output"), "plain output");
        // stray leading close before a real block
        assert_eq!(strip_think("</think><think>real</think>tail"), "tail");
        // multiple pairs
        assert_eq!(strip_think("<think>a</think>X<think>b</think>Y"), "XY");
    }
}
