//! Extended research tools — competitor analysis, trend analysis, market research.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::web_search::WebSearchTool;
use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct CompetitorAnalysisTool;
#[async_trait]
impl AgentTool for CompetitorAnalysisTool {
    fn name(&self) -> &str {
        "CompetitorAnalysis"
    }
    fn description(&self) -> &str {
        "Research competitors: fetch their website, analyze features, pricing, tech stack, and market position."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "competitor":{"type":"string","description":"Company name or website URL"},
            "aspects":{"type":"array","items":{"type":"string","enum":["features","pricing","tech_stack","team","funding","social"]},"description":"What to analyze"}
        },"required":["competitor"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let competitor = input
            .get("competitor")
            .and_then(Value::as_str)
            .unwrap_or("");
        if competitor.is_empty() {
            return AgentToolResult::err("'competitor' required");
        }

        let search = WebSearchTool;
        let search_result = search.execute(json!({"query": format!("{competitor} product features pricing"), "max_results": 5}), ctx).await;

        AgentToolResult::ok(format!(
            "Competitor Analysis: {competitor}\n\n## Search Results\n{}\n\n## Next Steps\n\
            Use WebFetch to visit the competitor's website for detailed analysis.\n\
            Check these aspects:\n\
            - Features: What do they offer?\n\
            - Pricing: What are their plans?\n\
            - Tech stack: Check with BuiltWith or Wappalyzer\n\
            - Social: Check Twitter, LinkedIn follower counts\n\
            - Reviews: Search '{competitor} reviews' for user sentiment",
            search_result.content
        ))
    }
}

pub struct TrendAnalysisTool;
#[async_trait]
impl AgentTool for TrendAnalysisTool {
    fn name(&self) -> &str {
        "TrendAnalysis"
    }
    fn description(&self) -> &str {
        "Analyze trends: search HackerNews, Reddit, GitHub trending, Product Hunt for what's popular in a topic area."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "topic":{"type":"string","description":"Topic to analyze trends for"},
            "sources":{"type":"array","items":{"type":"string","enum":["hackernews","reddit","github","producthunt","web"]},"description":"Sources to check"}
        },"required":["topic"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let topic = input.get("topic").and_then(Value::as_str).unwrap_or("");
        if topic.is_empty() {
            return AgentToolResult::err("'topic' required");
        }

        let mut results = Vec::new();

        // HackerNews search
        let hn_url = format!(
            "https://hn.algolia.com/api/v1/search?query={}&tags=story&hitsPerPage=5",
            urlenc(topic)
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        if let Ok(resp) = client.get(&hn_url).send().await {
            if let Ok(data) = resp.json::<Value>().await {
                if let Some(hits) = data.get("hits").and_then(Value::as_array) {
                    results.push("## HackerNews".into());
                    for hit in hits.iter().take(5) {
                        let title = hit.get("title").and_then(Value::as_str).unwrap_or("?");
                        let points = hit.get("points").and_then(Value::as_u64).unwrap_or(0);
                        let comments = hit.get("num_comments").and_then(Value::as_u64).unwrap_or(0);
                        results.push(format!("  - {title} ({points} pts, {comments} comments)"));
                    }
                }
            }
        }

        // GitHub trending (via search API)
        let gh_url = format!(
            "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page=5",
            urlenc(topic)
        );
        if let Ok(resp) = client
            .get(&gh_url)
            .header("User-Agent", "ForgeFleet")
            .send()
            .await
        {
            if let Ok(data) = resp.json::<Value>().await {
                if let Some(items) = data.get("items").and_then(Value::as_array) {
                    results.push("## GitHub Trending".into());
                    for item in items.iter().take(5) {
                        let name = item.get("full_name").and_then(Value::as_str).unwrap_or("?");
                        let stars = item
                            .get("stargazers_count")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let desc = item
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let desc_preview: String = desc.chars().take(60).collect();
                        results.push(format!("  - {name} (⭐{stars}) — {desc_preview}"));
                    }
                }
            }
        }

        // Web search for broader trends
        let search = WebSearchTool;
        let web_result = search
            .execute(
                json!({"query": format!("{topic} trends 2026"), "max_results": 3}),
                ctx,
            )
            .await;
        results.push(format!("## Web\n{}", web_result.content));

        AgentToolResult::ok(format!(
            "Trend Analysis: {topic}\n\n{}",
            results.join("\n\n")
        ))
    }
}

pub struct MarketResearchTool;
#[async_trait]
impl AgentTool for MarketResearchTool {
    fn name(&self) -> &str {
        "MarketResearch"
    }
    fn description(&self) -> &str {
        "Research market size, demographics, industry analysis, and market trends for a given market or industry."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "market":{"type":"string","description":"Market or industry to research"},
            "aspects":{"type":"array","items":{"type":"string","enum":["size","growth","players","demographics","trends"]},"description":"What to research"}
        },"required":["market"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let market = input.get("market").and_then(Value::as_str).unwrap_or("");
        if market.is_empty() {
            return AgentToolResult::err("'market' required");
        }

        let search = WebSearchTool;
        let result = search
            .execute(
                json!({"query": format!("{market} market size growth 2026 TAM"), "max_results": 5}),
                ctx,
            )
            .await;

        AgentToolResult::ok(format!(
            "Market Research: {market}\n\n{}\n\n## Analysis Framework\n\
            Use the search results above to identify:\n\
            - TAM (Total Addressable Market)\n\
            - Growth rate (CAGR)\n\
            - Key players and market share\n\
            - Target demographics\n\
            - Emerging trends and disruptions\n\
            - Regulatory considerations",
            result.content
        ))
    }
}

fn urlenc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}
