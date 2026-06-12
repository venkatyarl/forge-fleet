//! Cortex roadmap #4 — per-community natural-language summaries via a fleet LLM.
//!
//! `detect_communities` (see [`crate::communities`]) clusters the code graph and
//! persists one `brain_communities` row per connected component, keyed by a
//! STABLE `member_hash` and anchored by a `god_node_id` (the highest-degree
//! member). This module fills the reserved `summary` / `summary_model` /
//! `summary_updated_at` columns: for each community it gathers a sample of its
//! member symbols, asks a fleet LLM "what is this cluster responsible for?", and
//! writes the answer back.
//!
//! GraphRAG's re-summarize-only-changed lever is free here: the default run only
//! touches communities whose `summary IS NULL`, and because `member_hash` is
//! stable, a community whose membership didn't change keeps its row + summary
//! across re-detection — so an incremental reindex + detect + summarize only
//! pays for the communities that actually moved. `--all` forces a full refresh.
//!
//! Endpoint selection is DB-first (`ff_db::pg_pick_offload_endpoint` — a warm,
//! tool-capable fleet endpoint) and overridable, so this dogfoods the fleet's
//! own idle LLM capacity rather than a cloud call.

use serde::Serialize;
use sqlx::PgPool;
use std::time::Duration;

/// How a summarize run was configured.
pub struct SummarizeOpts {
    /// Re-summarize every eligible community, not just those with no summary.
    pub all: bool,
    /// Cap communities processed this run (unattended quality is hard — start small).
    pub max: usize,
    /// Skip communities with fewer than this many members (tiny 1–2 node
    /// components aren't worth an LLM call).
    pub min_members: usize,
    /// Override the fleet endpoint (`http://host:port`). Default: DB-routed.
    pub endpoint: Option<String>,
    /// Model id to send with an `--llm` override (ignored when DB-routed —
    /// the route already carries the served model id).
    pub model: Option<String>,
}

/// One community's summary outcome (a small sample is surfaced so a human can
/// eyeball quality after an unattended run).
#[derive(Debug, Clone, Serialize)]
pub struct CommunitySummarySample {
    pub community_id: i32,
    pub god_title: String,
    pub member_count: i32,
    pub summary: String,
}

/// Result of a `summarize_communities` run.
#[derive(Debug, Clone, Serialize)]
pub struct CommunitySummaryStats {
    /// Communities matching the eligibility filter (before the `max` cap).
    pub eligible: usize,
    /// Communities we actually attempted an LLM call for (after the cap).
    pub attempted: usize,
    /// Summaries written.
    pub summarized: usize,
    /// LLM/HTTP failures (left for a later run — never fatal).
    pub failed: usize,
    /// Communities skipped because the LLM returned an empty/garbage summary.
    pub empty: usize,
    /// The endpoint + model that handled the run.
    pub endpoint: String,
    pub model: String,
    /// First few summaries, for quality inspection.
    pub samples: Vec<CommunitySummarySample>,
}

/// Generous per-call ceiling — a small synthesis on a memory-tight host can be slow.
const TIMEOUT_SECS: u64 = 120;
/// Members listed in the prompt (enough to characterize the cluster, bounded so
/// a giant community doesn't blow the context window).
const MAX_PROMPT_MEMBERS: i64 = 40;
/// How many sample summaries to surface in the stats for human inspection.
const MAX_SAMPLES: usize = 5;

/// Run the community-summary pass. `progress(done, total)` is called after each
/// community so the CLI can show a live counter. Never panics; per-community
/// errors are counted, not propagated.
pub async fn summarize_communities<F: Fn(usize, usize)>(
    pool: &PgPool,
    opts: &SummarizeOpts,
    progress: F,
) -> Result<CommunitySummaryStats, String> {
    // ── Resolve the endpoint: explicit override, else a warm tool-capable fleet
    // endpoint (DB-first). Summaries are tiny, so a modest ctx floor is fine.
    let (endpoint, model) = match &opts.endpoint {
        Some(ep) => (
            ep.trim_end_matches('/').to_string(),
            opts.model.clone().unwrap_or_else(|| "default".to_string()),
        ),
        None => {
            let cand = ff_db::pg_pick_offload_endpoint(pool, 4096, None, &[])
                .await
                .map_err(|e| format!("route a summarize endpoint: {e}"))?
                .ok_or_else(|| {
                    "no warm tool-capable fleet endpoint to summarize with — load one \
                     (`ff model load <library_id> --agent`) or pass --llm <url>"
                        .to_string()
                })?;
            let m = cand
                .catalog_id
                .clone()
                .unwrap_or_else(|| cand.catalog_name.clone().unwrap_or_default());
            (cand.endpoint.trim_end_matches('/').to_string(), m)
        }
    };

    // ── Select eligible communities. The god node carries the CURRENT
    // community_id (union-find renumbers each detection run), so join through it
    // to get both the anchor symbol and the id used to fetch members. Biggest
    // communities first = best ROI under the `max` cap.
    let rows: Vec<(i32, i32, i32, String, String)> = sqlx::query_as(
        "SELECT c.id, g.community_id, c.member_count, g.title, g.path
         FROM brain_communities c
         JOIN brain_vault_nodes g ON g.id = c.god_node_id
         WHERE c.member_count >= $1
           AND ($2 OR c.summary IS NULL)
           AND g.community_id IS NOT NULL
           AND g.valid_until IS NULL
         ORDER BY c.member_count DESC
         LIMIT $3",
    )
    .bind(opts.min_members as i32)
    .bind(opts.all)
    .bind(opts.max as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("select eligible communities: {e}"))?;

    let eligible = rows.len();
    let client = reqwest::Client::new();

    // mlx_lm.server validates the `model` field as an HF repo id, so a catalog id
    // ("qwen36-35b-a3b") 401s "Repository Not Found" — it serves the model under
    // its on-disk path instead. Ask the endpoint what it actually serves and pick
    // a usable id (llama.cpp ignores the field, so this is a no-op there). Done
    // ONCE per run since the endpoint is fixed.
    let model = resolve_served_model_id(&client, &endpoint, &model).await;

    let mut stats = CommunitySummaryStats {
        eligible,
        attempted: 0,
        summarized: 0,
        failed: 0,
        empty: 0,
        endpoint: endpoint.clone(),
        model: model.clone(),
        samples: Vec::new(),
    };
    if eligible == 0 {
        return Ok(stats);
    }

    let url = format!("{endpoint}/v1/chat/completions");

    for (i, (comm_id, cid, member_count, god_title, god_path)) in rows.iter().enumerate() {
        stats.attempted += 1;

        // Representative members (most-referenced first), titles + types for the prompt.
        let members: Vec<(String, String)> = sqlx::query_as(
            "SELECT title, COALESCE(node_type, '')
             FROM brain_vault_nodes
             WHERE community_id = $1 AND valid_until IS NULL
             ORDER BY references_ DESC, hits DESC, title ASC
             LIMIT $2",
        )
        .bind(cid)
        .bind(MAX_PROMPT_MEMBERS)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let prompt = build_summary_prompt(god_title, god_path, *member_count, &members);

        match call_summary_llm(&client, &url, &model, &prompt).await {
            Ok(raw) => {
                let summary = clean_summary(&raw);
                if summary.is_empty() {
                    stats.empty += 1;
                } else if let Err(e) = store_summary(pool, *comm_id, &summary, &model).await {
                    tracing::warn!(community = comm_id, error = %e, "store community summary");
                    stats.failed += 1;
                } else {
                    stats.summarized += 1;
                    if stats.samples.len() < MAX_SAMPLES {
                        stats.samples.push(CommunitySummarySample {
                            community_id: *comm_id,
                            god_title: god_title.clone(),
                            member_count: *member_count,
                            summary,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!(community = comm_id, error = %e, "summarize LLM call");
                stats.failed += 1;
            }
        }
        progress(i + 1, eligible);
    }

    Ok(stats)
}

/// Persist a summary on the registry row (stamps model + time).
async fn store_summary(pool: &PgPool, id: i32, summary: &str, model: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE brain_communities
         SET summary = $1, summary_model = $2, summary_updated_at = NOW()
         WHERE id = $3",
    )
    .bind(summary)
    .bind(model)
    .bind(id)
    .execute(pool)
    .await
    .map_err(|e| format!("update summary: {e}"))?;
    Ok(())
}

/// POST one summary prompt to an OpenAI-compatible chat endpoint, return the raw
/// assistant text.
async fn call_summary_llm(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        // Roomy enough that a cooperative think-then-answer endpoint still emits
        // the 1-3 sentence summary after a short reasoning preamble (a tight cap
        // truncates before the answer → empty). The summary itself is ~60 tokens;
        // clean_summary caps the stored length regardless.
        "max_tokens": 512,
        "temperature": 0.2,
        "stream": false,
        // We want the summary, not chain-of-thought (Qwen3-style thinking models
        // otherwise burn the cap on <think>). Harmless on servers that ignore it.
        "chat_template_kwargs": {"enable_thinking": false},
    });

    let resp = client
        .post(url)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
    let payload: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse JSON: {e}"))?;
    let choice = payload
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| "no choices[0] in response".to_string())?;
    let content = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .or_else(|| choice.get("text").and_then(|v| v.as_str()))
        .unwrap_or_default();
    Ok(content.to_string())
}

/// Ask the endpoint what models it serves and choose a `model` id that won't be
/// rejected. Best-effort — on any HTTP/parse failure we keep `fallback` (which is
/// correct for llama.cpp, which ignores the field). Resolves the mlx 401 case
/// (mlx_lm.server validates the OpenAI `model` field as an HF repo id, so the
/// catalog id `qwen36-35b-a3b` 401s "Repository Not Found" — it serves the model
/// under its on-disk path). Shared by `ff cortex summarize`, `ff offload`, and the
/// `fleet_offload` MCP handler.
pub async fn resolve_served_model_id(
    client: &reqwest::Client,
    endpoint: &str,
    fallback: &str,
) -> String {
    let url = format!("{endpoint}/v1/models");
    let served: Vec<String> = match client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(v) => v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
    };
    if served.is_empty() {
        return fallback.to_string();
    }
    pick_served_model_id(&served, fallback)
}

/// Pure: choose which served model id to send. mlx serves the model under its
/// on-disk path, so an exact catalog-id match is rare; prefer (1) an exact match,
/// then (2) a served id that CONTAINS the routed id (the local-path entry whose
/// basename is the catalog id), else (3) keep the routed id. Never blindly picks
/// an unrelated served id — that could make mlx load the wrong model. Unit-tested.
pub fn pick_served_model_id(served: &[String], fallback: &str) -> String {
    // "default"/empty = we don't actually know the model (e.g. --llm with no
    // --model). Prefer the explicitly-loaded local-path entry: mlx_lm.server also
    // lists default-registry models, and serving an unrelated one would silently
    // run the WRONG model — the '/'-prefixed id is the one that was loaded.
    if fallback.is_empty() || fallback == "default" {
        return served
            .iter()
            .find(|s| s.starts_with('/'))
            .or_else(|| served.first())
            .cloned()
            .unwrap_or_else(|| "default".to_string());
    }
    if served.iter().any(|s| s == fallback) {
        return fallback.to_string();
    }
    if let Some(hit) = served.iter().find(|s| s.contains(fallback)) {
        return hit.clone();
    }
    fallback.to_string()
}

/// Build the summary prompt for one community. Pure (unit-tested).
pub fn build_summary_prompt(
    god_title: &str,
    god_path: &str,
    member_count: i32,
    members: &[(String, String)],
) -> String {
    let mut listing = String::new();
    for (title, node_type) in members {
        if title.trim().is_empty() {
            continue;
        }
        let ty = node_type.trim();
        if ty.is_empty() {
            listing.push_str(&format!("- {title}\n"));
        } else {
            listing.push_str(&format!("- {title} [{ty}]\n"));
        }
    }
    format!(
        "You are documenting one cluster of related symbols from a single codebase. \
A community-detection pass over the call/import graph grouped these symbols together. \
Write a concise 1-3 sentence summary of what this cluster is RESPONSIBLE FOR — its \
shared purpose or domain. Use the symbol names as evidence, but synthesize a theme; do \
NOT just list the symbols back. No preamble, no markdown, no \"Summary:\" prefix — output \
only the summary sentences.\n\n\
Anchor symbol (most-connected member): {god_title}\n\
Anchor location: {god_path}\n\
Total members in cluster: {member_count}\n\
Representative members:\n\
{listing}\n\
Summary:"
    )
}

/// Clean a raw LLM reply into a stored summary: strip `<think>` reasoning, code
/// fences, an echoed `Summary:` lead-in, and cap the length. Pure (unit-tested).
pub fn clean_summary(raw: &str) -> String {
    let mut s = strip_think(raw);
    s = strip_code_fence(&s).to_string();
    // Drop an echoed "Summary:" / "**Summary:**" lead-in (case-insensitive).
    let trimmed = s.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["**summary:**", "summary:", "summary -", "summary —"] {
        if lower.starts_with(prefix) {
            s = trimmed[prefix.len()..].trim_start().to_string();
            break;
        }
    }
    let s = s.trim();
    // Cap at a char boundary so a runaway model can't store a wall of text.
    const MAX_LEN: usize = 800;
    if s.chars().count() > MAX_LEN {
        s.chars()
            .take(MAX_LEN)
            .collect::<String>()
            .trim()
            .to_string()
    } else {
        s.to_string()
    }
}

/// Strip `<think>…</think>` reasoning blocks (mirrors the offload-path scrubber).
fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    loop {
        let Some(open) = out.find("<think>") else {
            break;
        };
        match out[open..].find("</think>") {
            Some(rel) => {
                let close = open + rel + "</think>".len();
                out.replace_range(open..close, "");
            }
            None => {
                out.truncate(open);
                break;
            }
        }
    }
    if let Some(i) = out.rfind("</think>") {
        out = out[i + "</think>".len()..].to_string();
    }
    out.trim().to_string()
}

/// Strip a surrounding ``` / ```lang code fence if the model wrapped its reply.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let without_open = t
        .strip_prefix("```markdown")
        .or_else(|| t.strip_prefix("```md"))
        .or_else(|| t.strip_prefix("```text"))
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    without_open.trim().trim_end_matches("```").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_anchor_and_members() {
        let members = vec![
            ("load_model".to_string(), "code:function".to_string()),
            ("unload_model".to_string(), "code:function".to_string()),
            ("blank".to_string(), String::new()),
        ];
        let p = build_summary_prompt(
            "load_model",
            "code://model_runtime.rs#load_model",
            12,
            &members,
        );
        assert!(p.contains("load_model"));
        assert!(p.contains("- unload_model [code:function]"));
        // empty node_type renders without brackets
        assert!(p.contains("- blank\n"));
        assert!(p.contains("Total members in cluster: 12"));
        assert!(p.trim_end().ends_with("Summary:"));
    }

    #[test]
    fn prompt_skips_blank_titles() {
        let members = vec![("".to_string(), "code:function".to_string())];
        let p = build_summary_prompt("anchor", "code://x", 1, &members);
        // a blank-title member produces no bullet line
        assert!(!p.contains("- ["));
        assert!(!p.contains("-  ["));
    }

    #[test]
    fn clean_strips_think_and_prefix() {
        let raw =
            "<think>let me reason about this cluster</think>Summary: Handles model lifecycle.";
        assert_eq!(clean_summary(raw), "Handles model lifecycle.");
    }

    #[test]
    fn clean_strips_code_fence_and_bold_prefix() {
        let raw = "```markdown\n**Summary:** Routes inference across the fleet.\n```";
        assert_eq!(clean_summary(raw), "Routes inference across the fleet.");
    }

    #[test]
    fn clean_handles_unclosed_think() {
        // a thinking model cut off under the token cap — everything is reasoning
        let raw = "<think>still reasoning and ran out of tokens";
        assert_eq!(clean_summary(raw), "");
    }

    #[test]
    fn clean_passes_plain_summary_through() {
        let raw = "This community manages Postgres connection pooling.";
        assert_eq!(clean_summary(raw), raw);
    }

    #[test]
    fn served_id_prefers_local_path_containing_catalog_id() {
        // the real mlx case: catalog id 401s, the on-disk path is the right id.
        let served = vec![
            "mlx-community/Meta-Llama-3.1-8B-Instruct-4bit".to_string(),
            "Qwen/Qwen3-8B".to_string(),
            "/Users/venkat/models/qwen36-35b-a3b".to_string(),
        ];
        assert_eq!(
            pick_served_model_id(&served, "qwen36-35b-a3b"),
            "/Users/venkat/models/qwen36-35b-a3b"
        );
    }

    #[test]
    fn served_id_exact_match_wins() {
        let served = vec![
            "qwen36-35b-a3b".to_string(),
            "/path/qwen36-35b-a3b".to_string(),
        ];
        assert_eq!(
            pick_served_model_id(&served, "qwen36-35b-a3b"),
            "qwen36-35b-a3b"
        );
    }

    #[test]
    fn served_id_keeps_fallback_when_no_match() {
        // llama.cpp ignores the field anyway — never pick an unrelated served id.
        let served = vec!["some-other-model".to_string()];
        assert_eq!(
            pick_served_model_id(&served, "qwen36-35b-a3b"),
            "qwen36-35b-a3b"
        );
    }

    #[test]
    fn served_id_unknown_fallback_prefers_local_path() {
        // --llm with no --model: "default"/"" must pick the loaded local-path
        // model, NOT an unrelated default-registry entry mlx happens to list.
        let served = vec![
            "mlx-community/Meta-Llama-3.1-8B-Instruct-4bit".to_string(),
            "/Users/venkat/models/qwen36-35b-a3b".to_string(),
        ];
        assert_eq!(
            pick_served_model_id(&served, "default"),
            "/Users/venkat/models/qwen36-35b-a3b"
        );
        assert_eq!(
            pick_served_model_id(&served, ""),
            "/Users/venkat/models/qwen36-35b-a3b"
        );
    }

    #[test]
    fn served_id_unknown_fallback_no_path_takes_first() {
        let served = vec!["only-served".to_string()];
        assert_eq!(pick_served_model_id(&served, ""), "only-served");
        assert_eq!(pick_served_model_id(&served, "default"), "only-served");
    }

    #[test]
    fn clean_caps_runaway_length() {
        let raw = "x".repeat(5000);
        let out = clean_summary(&raw);
        assert_eq!(out.chars().count(), 800);
    }
}
