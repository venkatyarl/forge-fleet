//! WebSearch tool — web search via DuckDuckGo HTML (no API key needed).

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct WebSearchTool {
    client: reqwest::Client,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("ForgeFleet-Agent/0.1")
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns search results with titles, URLs, and snippets."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum number of results (default 8)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let query = match input.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q,
            _ => return AgentToolResult::err("Missing or empty 'query' parameter"),
        };

        let max_results = input
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(8) as usize;

        match fetch_search_results(&self.client, query, max_results).await {
            Ok(results) if results.is_empty() => {
                AgentToolResult::ok(format!("No results found for: {query}"))
            }
            Ok(results) => AgentToolResult::ok(truncate_output(
                &format_results(&results),
                MAX_TOOL_RESULT_CHARS,
            )),
            Err(e) => AgentToolResult::err(e),
        }
    }
}

/// Run a DuckDuckGo HTML search and return parsed results (title/url/snippet).
/// Used both by the [`WebSearchTool`] agent tool and by callers that want web
/// grounding WITHOUT a full agent loop (e.g. research sub-agents that run as
/// plain chat completions — see `research.rs`).
pub async fn fetch_search_results(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(query));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Search request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Search returned HTTP {}", resp.status()));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read search results: {e}"))?;
    Ok(parse_ddg_results(&html, max_results))
}

/// Format parsed results as a numbered `N. title\n   url\n   snippet` block —
/// the same shape the agent tool returns to the model.
pub fn format_results(results: &[SearchResult]) -> String {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Best-effort web grounding for a non-tool caller: search and return a
/// formatted results block, or `None` if the search fails or finds nothing.
/// Callers fall back to ungrounded behavior on `None` — never an error path.
pub async fn fetch_web_context(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Option<String> {
    match fetch_search_results(client, query, max_results).await {
        Ok(results) if !results.is_empty() => Some(format_results(&results)),
        _ => None,
    }
}

pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn parse_ddg_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // Parse DuckDuckGo HTML lite results
    // Results are in <a class="result__a" href="...">title</a>
    // Snippets in <a class="result__snippet" ...>text</a>
    for chunk in html.split("class=\"result__a\"") {
        if results.len() >= max {
            break;
        }
        if chunk.contains("href=\"") {
            let url = extract_between(chunk, "href=\"", "\"").unwrap_or_default();
            let title = extract_between(chunk, ">", "</a>")
                .map(strip_tags)
                .unwrap_or_default();

            let snippet = if let Some(snip_start) = chunk.find("result__snippet") {
                let snip_chunk = &chunk[snip_start..];
                extract_between(snip_chunk, ">", "</")
                    .map(strip_tags)
                    .unwrap_or_default()
            } else {
                String::new()
            };

            if !title.is_empty() && !url.is_empty() && url.starts_with("http") {
                results.push(SearchResult {
                    title,
                    url: url.to_string(),
                    snippet,
                });
            }
        }
    }

    results
}

fn extract_between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let s = text.find(start)? + start.len();
    let e = text[s..].find(end)? + s;
    Some(&text[s..e])
}

fn strip_tags(text: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(ch);
        }
    }
    result.trim().to_string()
}

fn urlencoding(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_results_numbers_and_lays_out_each_hit() {
        let results = vec![
            SearchResult {
                title: "MLX 0.30 release".into(),
                url: "https://example.com/mlx".into(),
                snippet: "Adds FP8 kernels.".into(),
            },
            SearchResult {
                title: "vLLM on GB10".into(),
                url: "https://example.com/vllm".into(),
                snippet: "Blackwell support.".into(),
            },
        ];
        let out = format_results(&results);
        assert_eq!(
            out,
            "1. MLX 0.30 release\n   https://example.com/mlx\n   Adds FP8 kernels.\n\n\
             2. vLLM on GB10\n   https://example.com/vllm\n   Blackwell support."
        );
    }

    #[test]
    fn format_results_empty_is_empty() {
        assert_eq!(format_results(&[]), "");
    }

    #[test]
    fn parse_ddg_extracts_title_url_snippet_and_honors_max() {
        let html = r#"
            <a class="result__a" href="https://a.test/one">First &amp; Best</a>
            <a class="result__snippet">Snippet one.</a>
            <a class="result__a" href="https://b.test/two">Second</a>
            <a class="result__snippet">Snippet two.</a>
            <a class="result__a" href="https://c.test/three">Third</a>
        "#;
        let all = parse_ddg_results(html, 8);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].url, "https://a.test/one");
        assert_eq!(all[0].snippet, "Snippet one.");
        assert_eq!(all[1].url, "https://b.test/two");
        // max caps the result count.
        assert_eq!(parse_ddg_results(html, 2).len(), 2);
    }

    #[test]
    fn parse_ddg_skips_non_http_and_empty() {
        // A relative/`javascript:` href must not pollute results.
        let html = r#"<a class="result__a" href="/ddg-internal">Ad</a>"#;
        assert!(parse_ddg_results(html, 8).is_empty());
    }
}
