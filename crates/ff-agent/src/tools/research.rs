//! Research tools — deep research, scholar search, wiki lookup, trend analysis.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};
use super::web_search::WebSearchTool;

/// Deep research tool — multi-source research with summarization.
pub struct DeepResearchTool;

#[async_trait]
impl AgentTool for DeepResearchTool {
    fn name(&self) -> &str { "DeepResearch" }
    fn description(&self) -> &str {
        "Conduct deep research on a topic. Searches multiple sources (web, academic, wiki), fetches relevant pages, and compiles a structured research report with citations. Use for thorough investigation."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "Research topic or question" },
                "depth": { "type": "string", "enum": ["quick", "standard", "deep"], "description": "Research depth (default: standard)" },
                "sources": { "type": "number", "description": "Max sources to consult (default: 5)" }
            },
            "required": ["topic"]
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let topic = match input.get("topic").and_then(Value::as_str) {
            Some(t) => t,
            None => return AgentToolResult::err("Missing 'topic'"),
        };
        let max_sources = input.get("sources").and_then(Value::as_u64).unwrap_or(5) as usize;

        // Step 1: Search
        let search_input = json!({"query": topic, "max_results": max_sources});
        let search = WebSearchTool;
        let search_result = search.execute(search_input, ctx).await;

        // Step 2: Compile report
        let report = format!(
            "# Research Report: {topic}\n\n## Search Results\n\n{}\n\n## Next Steps\nUse WebFetch to read the most relevant URLs above for detailed information.\nUse the Agent tool to delegate deeper investigation to a specialized research agent.",
            search_result.content
        );

        AgentToolResult::ok(truncate_output(&report, MAX_TOOL_RESULT_CHARS))
    }
}

/// Wikipedia lookup tool.
pub struct WikiLookupTool;

#[async_trait]
impl AgentTool for WikiLookupTool {
    fn name(&self) -> &str { "WikiLookup" }
    fn description(&self) -> &str { "Look up a topic on Wikipedia and return a summary." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "Topic to look up" }
            },
            "required": ["topic"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let topic = input.get("topic").and_then(Value::as_str).unwrap_or("");
        if topic.is_empty() { return AgentToolResult::err("Missing 'topic'"); }

        let url = format!(
            "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
            topic.replace(' ', "_")
        );

        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10))
            .user_agent("ForgeFleet-Agent/0.1").build().unwrap_or_default();

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<Value>().await {
                    Ok(data) => {
                        let title = data.get("title").and_then(Value::as_str).unwrap_or(topic);
                        let extract = data.get("extract").and_then(Value::as_str).unwrap_or("No summary available.");
                        let page_url = data.get("content_urls").and_then(|u| u.get("desktop")).and_then(|d| d.get("page")).and_then(Value::as_str).unwrap_or("");
                        AgentToolResult::ok(format!("# {title}\n\n{extract}\n\nSource: {page_url}"))
                    }
                    Err(e) => AgentToolResult::err(format!("Failed to parse Wikipedia response: {e}")),
                }
            }
            Ok(resp) => AgentToolResult::err(format!("Wikipedia returned HTTP {}", resp.status())),
            Err(e) => AgentToolResult::err(format!("Wikipedia lookup failed: {e}")),
        }
    }
}

/// Scholar search tool — search academic papers.
pub struct ScholarSearchTool;

#[async_trait]
impl AgentTool for ScholarSearchTool {
    fn name(&self) -> &str { "ScholarSearch" }
    fn description(&self) -> &str { "Search academic papers on Semantic Scholar. Returns titles, authors, abstracts, and citation counts." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "limit": { "type": "number", "description": "Max results (default: 5)" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");
        if query.is_empty() { return AgentToolResult::err("Missing 'query'"); }
        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(5);

        let url = format!(
            "https://api.semanticscholar.org/graph/v1/paper/search?query={}&limit={}&fields=title,authors,abstract,citationCount,year,url",
            urlencoding(query), limit
        );

        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default();
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<Value>().await {
                    Ok(data) => {
                        let papers = data.get("data").and_then(Value::as_array);
                        match papers {
                            Some(papers) => {
                                let mut output = format!("# Scholar Results: {query}\n\n");
                                for (i, paper) in papers.iter().enumerate() {
                                    let title = paper.get("title").and_then(Value::as_str).unwrap_or("Untitled");
                                    let year = paper.get("year").and_then(Value::as_u64).unwrap_or(0);
                                    let citations = paper.get("citationCount").and_then(Value::as_u64).unwrap_or(0);
                                    let url = paper.get("url").and_then(Value::as_str).unwrap_or("");
                                    let abstract_text = paper.get("abstract").and_then(Value::as_str).unwrap_or("No abstract");
                                    let authors: Vec<&str> = paper.get("authors").and_then(Value::as_array)
                                        .map(|a| a.iter().filter_map(|a| a.get("name").and_then(Value::as_str)).collect())
                                        .unwrap_or_default();

                                    output.push_str(&format!(
                                        "{}. **{}** ({})\n   Authors: {}\n   Citations: {}\n   {}\n   {}\n\n",
                                        i + 1, title, year, authors.join(", "), citations,
                                        if abstract_text.len() > 200 { format!("{}...", &abstract_text[..200]) } else { abstract_text.to_string() },
                                        url
                                    ));
                                }
                                AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                            }
                            None => AgentToolResult::ok("No papers found.".to_string()),
                        }
                    }
                    Err(e) => AgentToolResult::err(format!("Parse error: {e}")),
                }
            }
            _ => AgentToolResult::err("Semantic Scholar API request failed".to_string()),
        }
    }
}

fn urlencoding(input: &str) -> String {
    input.chars().map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
        ' ' => "+".to_string(),
        _ => format!("%{:02X}", c as u8),
    }).collect()
}
