//! WebSearch tool — web search via DuckDuckGo HTML (no API key needed).

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct WebSearchTool;

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str { "WebSearch" }

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

        // Use DuckDuckGo HTML lite (no API key required)
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("ForgeFleet-Agent/0.1")
            .build()
            .unwrap_or_default();

        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding(query)
        );

        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    return AgentToolResult::err(format!("Search returned HTTP {}", resp.status()));
                }

                match resp.text().await {
                    Ok(html) => {
                        let results = parse_ddg_results(&html, max_results);
                        if results.is_empty() {
                            AgentToolResult::ok(format!("No results found for: {query}"))
                        } else {
                            let output = results
                                .iter()
                                .enumerate()
                                .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet))
                                .collect::<Vec<_>>()
                                .join("\n\n");
                            AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                        }
                    }
                    Err(e) => AgentToolResult::err(format!("Failed to read search results: {e}")),
                }
            }
            Err(e) => AgentToolResult::err(format!("Search request failed: {e}")),
        }
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
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
                .map(|t| strip_tags(t))
                .unwrap_or_default();

            let snippet = if let Some(snip_start) = chunk.find("result__snippet") {
                let snip_chunk = &chunk[snip_start..];
                extract_between(snip_chunk, ">", "</")
                    .map(|s| strip_tags(s))
                    .unwrap_or_default()
            } else {
                String::new()
            };

            if !title.is_empty() && !url.is_empty() && url.starts_with("http") {
                results.push(SearchResult { title, url: url.to_string(), snippet });
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
        if ch == '<' { in_tag = true; }
        else if ch == '>' { in_tag = false; }
        else if !in_tag { result.push(ch); }
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
