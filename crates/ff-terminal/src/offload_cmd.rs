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
use ff_agent::fleet_oneshot::{
    EndpointAttestationState, ResolvedFleetTarget, ResolvedTargetProvenance,
    attest_resolved_target, resolve_candidate_target, revalidate_explicit_target,
};
use ff_db::queries::offload_workload_for_kind;
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
    explicit_target: Option<ResolvedFleetTarget>,
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

    // ── Step 1: either revalidate the exact operator-pinned deployment or
    // pick the best WARM endpoint. Explicit requests never enter the automatic
    // fallback path and never relax workload/tool/context constraints.
    let (candidate, unresolved_target) = match explicit_target {
        Some(target) => {
            let workload = offload_workload_for_kind(kind);
            let candidate =
                match revalidate_explicit_target(&pool, &target, workload, true, Some(min_ctx))
                    .await
                {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        let error_text = format!("validate explicit offload target: {error}");
                        record_offload_turn(
                            &pool,
                            prompt,
                            kind,
                            min_ctx,
                            max_tokens,
                            &target,
                            "",
                            0,
                            0,
                            None,
                            "error",
                            Some(&error_text),
                        )
                        .await;
                        return Err(anyhow::anyhow!(error_text));
                    }
                };
            (candidate, target)
        }
        None => {
            // Code-shaped kinds prefer a coder; general kinds use any
            // tool-capable model. The shared selector breaks ties by load.
            let candidate = ff_db::pg_pick_offload_endpoint(&pool, min_ctx, kind, &[])
                .await
                .map_err(|e| anyhow::anyhow!("offload router query failed: {e}"))?;
            let Some(candidate) = candidate else {
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
                .map_err(
                    |e| tracing::warn!(error = %e, "unmet demand signal write failed (offload)"),
                )
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
            };
            let target =
                resolve_candidate_target(&pool, &candidate, ResolvedTargetProvenance::Auto, false)
                    .await
                    .map_err(|error| anyhow::anyhow!("resolve offload target: {error}"))?;
            (candidate, target)
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

    // ── Step 2: resolve and attest the canonical endpoint identity before chat.
    // Reachable identity errors fail closed. Automatic routing retains the
    // shared timeout policy; an explicit pin additionally requires a verified
    // identity and fails if /v1/models times out.
    let client = reqwest::Client::new();
    let target =
        match attest_resolved_target(&client, unresolved_target.clone(), Duration::from_secs(5))
            .await
        {
            Ok(target) => target,
            Err(error) => {
                let error_text = error.to_string();
                record_offload_turn(
                    &pool,
                    prompt,
                    kind,
                    min_ctx,
                    max_tokens,
                    &unresolved_target,
                    "",
                    0,
                    0,
                    None,
                    "error",
                    Some(&error_text),
                )
                .await;
                return Err(anyhow::anyhow!("attest offload target: {error}"));
            }
        };
    if target.provenance.is_pinned() && target.attestation != EndpointAttestationState::Verified {
        let error_text = format!(
            "explicit offload target {} could not be identity-attested ({:?})",
            target.endpoint, target.attestation
        );
        record_offload_turn(
            &pool,
            prompt,
            kind,
            min_ctx,
            max_tokens,
            &target,
            "",
            0,
            0,
            None,
            "error",
            Some(&error_text),
        )
        .await;
        return Err(anyhow::anyhow!(error_text));
    }
    let model = target.inference_model().to_string();
    let url = ff_core::url::normalize_chat_completions_url(&target.endpoint);

    if !json_out {
        eprintln!(
            "{GREEN}● offloading to {}{RESET} \x1b[2m({}, tier {}, ctx {}){RESET}",
            target.endpoint,
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

    let started = Instant::now();
    let resp = match client
        .post(&url)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .json(&body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let error_text = format!("offload dispatch to {} failed: {error}", target.endpoint);
            record_offload_turn(
                &pool,
                prompt,
                kind,
                min_ctx,
                max_tokens,
                &target,
                "",
                0,
                0,
                Some(started.elapsed()),
                "error",
                Some(&error_text),
            )
            .await;
            return Err(anyhow::anyhow!(error_text));
        }
    };

    let latency = started.elapsed();
    let status = resp.status();
    let text = match resp.text().await {
        Ok(text) => text,
        Err(error) => {
            let error_text = format!("read offload response body: {error}");
            record_offload_turn(
                &pool,
                prompt,
                kind,
                min_ctx,
                max_tokens,
                &target,
                "",
                0,
                0,
                Some(latency),
                "error",
                Some(&error_text),
            )
            .await;
            return Err(anyhow::anyhow!(error_text));
        }
    };
    if !status.is_success() {
        let error_text = format!(
            "offload endpoint {} (model {model}) returned HTTP {status}: {text}",
            target.endpoint
        );
        record_offload_turn(
            &pool,
            prompt,
            kind,
            min_ctx,
            max_tokens,
            &target,
            &text,
            0,
            0,
            Some(latency),
            "error",
            Some(&error_text),
        )
        .await;
        eprintln!(
            "{RED}✗ endpoint {} (model {model}) returned HTTP {status}{RESET}",
            target.endpoint
        );
        eprintln!("\x1b[2m{text}\x1b[0m");
        return Err(anyhow::anyhow!(error_text));
    }

    let payload: serde_json::Value = match serde_json::from_str(&text) {
        Ok(payload) => payload,
        Err(error) => {
            let error_text = format!("parse offload response JSON: {error}");
            record_offload_turn(
                &pool,
                prompt,
                kind,
                min_ctx,
                max_tokens,
                &target,
                &text,
                0,
                0,
                Some(latency),
                "error",
                Some(&error_text),
            )
            .await;
            return Err(anyhow::anyhow!(error_text));
        }
    };
    let result = match validated_offload_result(&payload) {
        Ok(result) => result,
        Err(error) => {
            // Do not turn a malformed provider response into a successful empty
            // completion. Keep the raw body out of the operator-facing error;
            // immutable route identity is enough to diagnose the failed turn,
            // while the interaction row retains the canonical route decision.
            let error_text = format!(
                "offload completion rejected: {error}; route worker={} catalog_id={} deployment_id={} provenance={} attestation={:?}",
                target.worker_name,
                target.catalog_id,
                target.deployment_id,
                target.provenance.as_str(),
                target.attestation,
            );
            record_offload_turn(
                &pool,
                prompt,
                kind,
                min_ctx,
                max_tokens,
                &target,
                "",
                0,
                0,
                Some(latency),
                "error",
                Some(&error_text),
            )
            .await;
            return Err(anyhow::anyhow!(error_text));
        }
    };

    // Log the offload turn to ff_interactions (the ff-LLM training corpus). `ff
    // offload` is a core dogfood verb (route code work to a warm fleet coder) and
    // was the last hot dispatch verb not logging its req/resp — after council
    // (#442) and research (#447). Best-effort; never fails the offload.
    let usage = payload.get("usage");
    let usage_tok = |k: &str| -> i32 {
        usage
            .and_then(|u| u.get(k))
            .and_then(|v| v.as_i64())
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0)
    };
    record_offload_turn(
        &pool,
        prompt,
        kind,
        min_ctx,
        max_tokens,
        &target,
        &result,
        usage_tok("prompt_tokens"),
        usage_tok("completion_tokens"),
        Some(latency),
        "success",
        None,
    )
    .await;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "offloaded": true,
                "decision": "offloaded",
                "endpoint": target.endpoint,
                "worker_name": target.worker_name,
                "model": model,
                "route_decision": target.route_decision(),
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
            target.worker_name,
            latency.as_millis()
        );
        println!("{result}");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record_offload_turn(
    pool: &sqlx::PgPool,
    prompt: &str,
    kind: Option<&str>,
    min_ctx: i32,
    max_tokens: u32,
    target: &ResolvedFleetTarget,
    response_text: &str,
    tokens_in: i32,
    tokens_out: i32,
    latency: Option<Duration>,
    outcome: &str,
    error_text: Option<&str>,
) {
    let rec = ff_db::InteractionRecord {
        channel: "offload".to_string(),
        purpose: Some("build".to_string()),
        request_text: prompt.chars().take(16000).collect(),
        request_meta: serde_json::json!({
            "kind": kind,
            "min_ctx": min_ctx,
            "max_tokens": max_tokens,
        }),
        route_decision: target.route_decision(),
        model_versions: serde_json::json!({
            "catalog_id": target.catalog_id,
            "served_model_id": target.served_model_id,
            "served_model_ids": target.served_model_ids,
        }),
        engine: Some(target.engine_label()),
        response_text: response_text.chars().take(16000).collect(),
        tokens_in,
        tokens_out,
        latency_ms: latency.and_then(|value| i32::try_from(value.as_millis()).ok()),
        outcome: outcome.to_string(),
        error_text: error_text.map(str::to_string),
        worker_name: Some(target.worker_name.clone()),
        endpoint: Some(target.endpoint.clone()),
        ..Default::default()
    };
    if let Err(error) = ff_db::pg_record_interaction(pool, &rec).await {
        tracing::warn!(error = %error, "offload: failed to log interaction (non-fatal)");
    }
}

/// Apply the same fail-closed response contract used by MCP and the local
/// executor. This rejects missing/blank/truncated/reasoning-only responses and
/// returns only sanitized public content.
fn validated_offload_result(payload: &serde_json::Value) -> Result<String, String> {
    ff_core::llm_completion_policy::validate_completion_response(payload)
        .map(|completion| completion.content)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::validated_offload_result;
    use serde_json::json;

    #[test]
    fn completion_contract_returns_only_non_empty_public_output() {
        assert_eq!(
            validated_offload_result(&json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"content": "<think>private</think>public answer"}
                }]
            }))
            .unwrap(),
            "public answer",
        );
    }

    #[test]
    fn completion_contract_rejects_empty_and_truncated_output() {
        let empty = json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "  "}}]
        });
        assert!(
            validated_offload_result(&empty)
                .unwrap_err()
                .contains("empty")
        );

        let truncated = json!({
            "choices": [{"finish_reason": "length", "message": {"content": "partial"}}]
        });
        assert!(
            validated_offload_result(&truncated)
                .unwrap_err()
                .contains("truncated")
        );
    }

    #[test]
    fn completion_contract_rejects_missing_and_reasoning_only_output() {
        assert!(
            validated_offload_result(&json!({}))
                .unwrap_err()
                .contains("missing choices")
        );
        let reasoning_only = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": "<think>private only</think>"}
            }]
        });
        assert!(
            validated_offload_result(&reasoning_only)
                .unwrap_err()
                .contains("empty")
        );
    }
}
