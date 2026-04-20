//! Vision-LLM analyzer for fetched social posts.
//!
//! Sends each image / extracted frame to an OpenAI-compatible
//! `/v1/chat/completions` endpoint that speaks the multimodal message
//! format (content = array of parts, image parts as data-URIs), then
//! rolls per-frame JSON responses up into a single [`Analysis`].

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::fetcher::FetchedPost;

/// Top-level rolled-up analysis returned to the orchestrator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Analysis {
    pub summary: String,
    pub detected_urls: Vec<String>,
    pub detected_tools: Vec<String>,
    pub entities: Vec<String>,
    pub ocr_combined: String,
    /// Per-frame raw responses for debugging.
    pub per_frame: Vec<FrameAnalysis>,
}

/// Parsed JSON from a single vision call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameAnalysis {
    pub description: String,
    pub ocr_text: String,
    pub urls: Vec<String>,
    pub tools_mentioned: Vec<String>,
    pub code_snippet: Option<String>,
    /// Raw LLM text if we couldn't parse JSON, else empty.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub raw_fallback: String,
}

const PROMPT: &str = "Describe this image. Extract any visible text (OCR), any URLs, any app/tool names mentioned, and any code shown. Reply JSON only: {\"description\":\"...\",\"ocr_text\":\"...\",\"urls\":[],\"tools_mentioned\":[],\"code_snippet\":null}";

const STRICT_PROMPT: &str = "Reply with ONLY a JSON object — no prose, no code fences, no preamble. Keys: description (string), ocr_text (string), urls (array of strings), tools_mentioned (array of strings), code_snippet (string or null). Describe the image; extract visible text, URLs, tool names, and any code.";

/// Run the vision-LLM over every image/frame in `post` and roll up the
/// results.
pub async fn analyze(
    post: &FetchedPost,
    llm_endpoint: &str,
    model_id: &str,
) -> Result<Analysis> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("build reqwest client")?;

    let mut per_frame: Vec<FrameAnalysis> = Vec::new();
    for item in &post.media_items {
        if item.kind != "image" && item.kind != "frame" {
            continue;
        }
        let bytes = match tokio::fs::read(&item.local_path).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %item.local_path, error = %e, "read media for analysis");
                continue;
            }
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let data_uri = format!("data:{};base64,{}", item.mime, b64);

        let analysis = match call_llm(&client, llm_endpoint, model_id, &data_uri, PROMPT).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "initial vision call failed, retrying with stricter prompt");
                match call_llm(&client, llm_endpoint, model_id, &data_uri, STRICT_PROMPT).await {
                    Ok(a) => a,
                    Err(e2) => {
                        tracing::warn!(error = %e2, "stricter vision call also failed; degrading");
                        FrameAnalysis {
                            raw_fallback: format!("vision call failed: {e2}"),
                            ..Default::default()
                        }
                    }
                }
            }
        };
        per_frame.push(analysis);
    }

    Ok(rollup(per_frame, post))
}

/// Single vision call → parsed [`FrameAnalysis`].
async fn call_llm(
    client: &reqwest::Client,
    endpoint: &str,
    model_id: &str,
    data_uri: &str,
    prompt: &str,
) -> Result<FrameAnalysis> {
    let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
    let body = json!({
        "model": model_id,
        "temperature": 0.0,
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                { "type": "image_url", "image_url": { "url": data_uri } }
            ]
        }]
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("vision LLM POST")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("vision LLM {status}: {text}"));
    }
    let envelope: serde_json::Value = resp.json().await.context("parse LLM envelope")?;
    let content = envelope
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("no choices[0].message.content in LLM response"))?;

    // Strip common code-fence wrappers before JSON parse.
    let trimmed = strip_code_fence(content);
    match serde_json::from_str::<FrameAnalysis>(trimmed) {
        Ok(mut a) => {
            a.raw_fallback.clear();
            Ok(a)
        }
        Err(_) => Err(anyhow!("non-JSON vision response: {content}")),
    }
}

fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let without_fence = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```JSON"))
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    without_fence.trim().trim_end_matches("```").trim()
}

/// Merge per-frame results into a single [`Analysis`]. Dedupes URLs /
/// tools / entities; concatenates OCR text; picks the longest per-frame
/// description as the summary seed (TODO: replace with an LLM summary
/// pass once we have a text-only endpoint wired in).
fn rollup(per_frame: Vec<FrameAnalysis>, post: &FetchedPost) -> Analysis {
    let mut urls = std::collections::BTreeSet::<String>::new();
    let mut tools = std::collections::BTreeSet::<String>::new();
    let mut entities = std::collections::BTreeSet::<String>::new();
    let mut ocr_parts: Vec<String> = Vec::new();
    let mut longest_desc = String::new();

    for fa in &per_frame {
        for u in &fa.urls {
            let u = u.trim();
            if !u.is_empty() {
                urls.insert(u.to_string());
            }
        }
        for t in &fa.tools_mentioned {
            let t = t.trim();
            if !t.is_empty() {
                tools.insert(t.to_string());
                entities.insert(t.to_string());
            }
        }
        if !fa.ocr_text.trim().is_empty() {
            ocr_parts.push(fa.ocr_text.trim().to_string());
        }
        if fa.description.len() > longest_desc.len() {
            longest_desc = fa.description.clone();
        }
    }

    // Fold caption text into OCR so downstream search picks it up.
    if let Some(caption) = &post.caption {
        if !caption.trim().is_empty() {
            ocr_parts.push(caption.trim().to_string());
        }
    }

    let summary = if longest_desc.is_empty() {
        post.caption.clone().unwrap_or_default()
    } else {
        longest_desc
    };

    Analysis {
        summary,
        detected_urls: urls.into_iter().collect(),
        detected_tools: tools.into_iter().collect(),
        entities: entities.into_iter().collect(),
        ocr_combined: ocr_parts.join("\n---\n"),
        per_frame,
    }
}
