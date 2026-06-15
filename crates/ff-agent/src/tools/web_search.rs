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
///
/// DuckDuckGo answers a burst of concurrent requests with HTTP 202 (no body) —
/// its anomaly/rate-limit throttle. `ff research --parallel N` fires N searches
/// at once and routinely trips it, dropping those sub-agents to ungrounded
/// model-memory. Retry a 202 (and 429) a few times with growing backoff before
/// giving up; other non-success statuses fail fast.
pub async fn fetch_search_results(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(query));
    let mut attempt = 0u32;
    loop {
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Search request failed: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            let html = resp
                .text()
                .await
                .map_err(|e| format!("Failed to read search results: {e}"))?;
            return Ok(parse_ddg_results(&html, max_results));
        }
        // 202 (anomaly throttle) / 429 (rate limit) are transient — back off and
        // retry. Anything else (4xx/5xx) is unlikely to fix itself; fail fast.
        let retriable = matches!(status.as_u16(), 202 | 429);
        match retry_backoff(attempt).filter(|_| retriable) {
            Some(delay) => {
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            None => return Err(format!("Search returned HTTP {status}")),
        }
    }
}

/// Backoff before retry `attempt` (0-indexed) of a throttled DuckDuckGo search,
/// or `None` once the retry budget is exhausted. Three retries with growing
/// delay (≈400ms / 900ms / 1600ms) — enough to outlast a brief concurrent burst
/// without blowing past the client's 15s timeout.
fn retry_backoff(attempt: u32) -> Option<std::time::Duration> {
    match attempt {
        0 => Some(std::time::Duration::from_millis(400)),
        1 => Some(std::time::Duration::from_millis(900)),
        2 => Some(std::time::Duration::from_millis(1600)),
        _ => None,
    }
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
/// formatted results block, or `None` if every backend fails or finds nothing.
/// Callers fall back to ungrounded behavior on `None` — never an error path.
///
/// Backends are tried in order of reliability:
///   1. **SearXNG** (`searxng_base`, if configured) — a self-hosted metasearch
///      instance on a fleet node, queried via its key-free JSON API. This is the
///      durable fix for the DuckDuckGo block (below): a fleet-native backend with
///      no external scraping dependency. Off by default; the caller passes the
///      base URL from the `searxng.url` fleet secret.
///   2. **DuckDuckGo** HTML scrape — works when DDG isn't throttling.
///   3. **Wikipedia** `list=search` — narrow coverage, but never IP-blocked.
///
/// DuckDuckGo intermittently hard-blocks a scraping IP with an HTTP-202 CAPTCHA
/// challenge ("select all squares containing a duck") that no retry can clear —
/// when the leader has been searching heavily, *every* DDG request comes back
/// 202 with zero results. Rather than silently drop the whole research run to
/// ungrounded model memory, the SearXNG primary (when configured) and the
/// Wikipedia fallback (reliable 200s, real citable URLs) keep factual
/// sub-questions grounded.
pub async fn fetch_web_context(
    client: &reqwest::Client,
    searxng_base: Option<&str>,
    query: &str,
    max_results: usize,
) -> Option<String> {
    // Primary (when configured): self-hosted SearXNG metasearch on the fleet.
    if let Some(base) = searxng_base.map(str::trim).filter(|b| !b.is_empty()) {
        if let Ok(results) = fetch_searxng_results(client, base, query, max_results).await {
            if !results.is_empty() {
                return Some(format_results(&results));
            }
        }
    }
    // Secondary: general-web results via DuckDuckGo.
    if let Ok(results) = fetch_search_results(client, query, max_results).await {
        if !results.is_empty() {
            return Some(format_results(&results));
        }
    }
    // Fallback: Wikipedia (no API key, not IP-blocked). Narrower coverage than a
    // general web search, but real sources beat hallucinated ones.
    match fetch_wikipedia_results(client, query, max_results).await {
        Ok(results) if !results.is_empty() => Some(format_results(&results)),
        _ => None,
    }
}

/// Query a self-hosted [SearXNG](https://docs.searxng.org/) metasearch instance
/// via its JSON API (`{base}/search?q=…&format=json`) and parse the results.
///
/// SearXNG aggregates many upstream engines behind one key-free endpoint we run
/// on a fleet node, so it sidesteps the per-IP scraping blocks that wall direct
/// DuckDuckGo access — fitting the "fleet replaces cloud subscriptions" model.
/// The instance must enable the `json` output format in its `settings.yml`
/// (`search.formats: [html, json]`), otherwise it returns HTTP 403 for
/// `format=json` and this backend yields no results (caller falls back).
pub async fn fetch_searxng_results(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/search?q={}&format=json", urlencoding(query));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("SearXNG request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("SearXNG returned HTTP {}", resp.status()));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read SearXNG results: {e}"))?;
    Ok(parse_searxng_results(&body, max_results))
}

/// Parse a SearXNG `format=json` response into [`SearchResult`]s. The JSON shape
/// is `{ "results": [ { "url", "title", "content" }, … ] }`; SearXNG returns
/// plain text (no HTML) in `content`, so no tag-stripping is needed. Non-http
/// and title-less entries are skipped.
fn parse_searxng_results(body: &str, max: usize) -> Vec<SearchResult> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(hits) = json.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    hits.iter()
        .filter_map(|hit| {
            let url = hit.get("url").and_then(Value::as_str)?;
            let title = hit.get("title").and_then(Value::as_str).unwrap_or_default();
            if title.is_empty() || !url.starts_with("http") {
                return None;
            }
            let snippet = hit
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            Some(SearchResult {
                title: title.to_string(),
                url: url.to_string(),
                snippet,
            })
        })
        .take(max)
        .collect()
}

/// Key-free fallback search via the Wikipedia `list=search` API. Returns real
/// article titles, URLs, and snippets — used by [`fetch_web_context`] when the
/// primary (DuckDuckGo) backend is blocked or empty.
///
/// Research grounding passes a verbose natural-language sub-question (DuckDuckGo
/// handles those fine). Wikipedia's search is keyword-based, though, and a full
/// sentence routinely matches zero articles — so if the verbatim query comes up
/// empty, retry once with a stop-word-stripped keyword query before giving up.
pub async fn fetch_wikipedia_results(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let first = wikipedia_search(client, query, max_results).await?;
    if !first.is_empty() {
        return Ok(first);
    }
    let keywords = wikipedia_keywords(query);
    if keywords.is_empty() || keywords == query {
        return Ok(first);
    }
    wikipedia_search(client, &keywords, max_results).await
}

/// One Wikipedia `list=search` request for `query`.
async fn wikipedia_search(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&srlimit={}&format=json",
        urlencoding(query),
        max_results.clamp(1, 20),
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Wikipedia request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Wikipedia returned HTTP {}", resp.status()));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read Wikipedia results: {e}"))?;
    Ok(parse_wikipedia_results(&body, max_results))
}

/// Reduce a verbose sub-question to keyword search terms for Wikipedia: drop
/// question/stop words, keep proper nouns and content words (≥4 chars, or any
/// token with an interior capital/digit like `PagedAttention`/`GPT4`), cap to 6
/// terms in original order. Returns an empty string if nothing survives.
fn wikipedia_keywords(query: &str) -> String {
    const STOP: &[&str] = &[
        "what",
        "which",
        "who",
        "whom",
        "whose",
        "when",
        "where",
        "why",
        "how",
        "is",
        "are",
        "was",
        "were",
        "the",
        "a",
        "an",
        "and",
        "or",
        "of",
        "to",
        "in",
        "for",
        "on",
        "with",
        "that",
        "this",
        "it",
        "its",
        "their",
        "by",
        "from",
        "as",
        "at",
        "be",
        "can",
        "will",
        "does",
        "do",
        "did",
        "into",
        "about",
        "explain",
        "describe",
        "detail",
        "list",
        "give",
        "tell",
        "between",
        "recent",
        "recently",
        "latest",
        "current",
        "currently",
    ];
    let mut out: Vec<&str> = Vec::new();
    for raw in query.split(|c: char| !c.is_alphanumeric() && c != '-') {
        if out.len() >= 6 {
            break;
        }
        let word = raw.trim_matches('-');
        if word.is_empty() {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if STOP.contains(&lower.as_str()) {
            continue;
        }
        // Keep distinctive tokens: long-ish words, or anything with an interior
        // capital/digit (proper nouns, product names, version strings).
        let has_interior_signal = word
            .chars()
            .skip(1)
            .any(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if word.chars().count() >= 4 || has_interior_signal {
            out.push(word);
        }
    }
    out.join(" ")
}

/// Parse a Wikipedia `list=search` JSON response into [`SearchResult`]s. The
/// API's `snippet` field is HTML (`<span class="searchmatch">…` plus entities),
/// so strip tags and decode the handful of entities Wikipedia emits.
fn parse_wikipedia_results(body: &str, max: usize) -> Vec<SearchResult> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(hits) = json
        .get("query")
        .and_then(|q| q.get("search"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    hits.iter()
        .take(max)
        .filter_map(|hit| {
            let title = hit.get("title").and_then(Value::as_str)?;
            if title.is_empty() {
                return None;
            }
            let snippet = hit
                .get("snippet")
                .and_then(Value::as_str)
                .map(|s| decode_entities(&strip_tags(s)))
                .unwrap_or_default();
            Some(SearchResult {
                url: format!("https://en.wikipedia.org/wiki/{}", title.replace(' ', "_")),
                title: title.to_string(),
                snippet,
            })
        })
        .collect()
}

/// Decode the small set of HTML entities Wikipedia snippets contain. Not a
/// general-purpose decoder — just enough to render snippets as plain text.
fn decode_entities(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
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

            if let (false, Some(clean)) = (title.is_empty(), clean_ddg_url(url)) {
                results.push(SearchResult {
                    title,
                    url: clean,
                    snippet,
                });
            }
        }
    }

    results
}

/// Resolve a raw DuckDuckGo result href into a real external URL, or `None` if
/// it is a DuckDuckGo self/ad/navigation link. When DDG is throttling it serves
/// a degraded page whose only links point back to `duckduckgo.com`; counting
/// those as results would mask the failure and skip the Wikipedia fallback. DDG
/// also sometimes wraps real results in a `/l/?uddg=<percent-encoded>` redirect
/// — unwrap those so genuine results survive.
fn clean_ddg_url(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    // Protocol-relative `//host/…` → assume https so host checks work.
    let normalized = match raw.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => raw.to_string(),
    };
    // Redirect wrapper: pull the real target out of the `uddg=` parameter.
    if normalized.contains("duckduckgo.com/l/") {
        let target = normalized
            .find("uddg=")
            .map(|i| &normalized[i + "uddg=".len()..])
            .map(|v| v.split('&').next().unwrap_or(v))
            .map(percent_decode);
        return target.filter(|t| t.starts_with("http"));
    }
    // Any other duckduckgo.com link is navigation/ads, never a result.
    if normalized.contains("duckduckgo.com") {
        return None;
    }
    normalized.starts_with("http").then_some(normalized)
}

/// Minimal `%XX`/`+` percent-decoder for unwrapping DDG redirect targets.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
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
    fn retry_backoff_grows_then_exhausts() {
        // Three retries with growing delay, then None (budget exhausted).
        assert_eq!(
            retry_backoff(0),
            Some(std::time::Duration::from_millis(400))
        );
        assert_eq!(
            retry_backoff(1),
            Some(std::time::Duration::from_millis(900))
        );
        assert_eq!(
            retry_backoff(2),
            Some(std::time::Duration::from_millis(1600))
        );
        assert_eq!(retry_backoff(3), None);
        assert_eq!(retry_backoff(99), None);
    }

    #[test]
    fn parse_wikipedia_extracts_title_url_snippet_strips_html_and_honors_max() {
        let body = r#"{"query":{"search":[
            {"title":"VLLM","snippet":"&quot;<span class=\"searchmatch\">vLLM</span>&quot; is an engine &amp; library."},
            {"title":"Large language model","snippet":"open-source <span class=\"searchmatch\">LLM</span> serving"},
            {"title":"Third","snippet":"x"}
        ]}}"#;
        let all = parse_wikipedia_results(body, 8);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].title, "VLLM");
        assert_eq!(all[0].url, "https://en.wikipedia.org/wiki/VLLM");
        // tags stripped, entities decoded.
        assert_eq!(all[0].snippet, "\"vLLM\" is an engine & library.");
        // spaces in titles become underscores in the URL.
        assert_eq!(
            all[1].url,
            "https://en.wikipedia.org/wiki/Large_language_model"
        );
        // max caps the result count.
        assert_eq!(parse_wikipedia_results(body, 2).len(), 2);
    }

    #[test]
    fn wikipedia_keywords_strips_stopwords_keeps_proper_nouns_and_caps() {
        // Question/stop words dropped; PagedAttention kept (interior capital),
        // short stop words like "is/the/and/for" gone.
        assert_eq!(
            wikipedia_keywords(
                "What is the vLLM inference engine and what is PagedAttention used for"
            ),
            "vLLM inference engine PagedAttention used"
        );
        // Caps at 6 terms.
        let many = wikipedia_keywords("alpha bravo charlie delta echo foxtrot golf hotel");
        assert_eq!(many.split(' ').count(), 6);
        // A token with an interior digit survives even if short (GB10, GPT4).
        assert_eq!(wikipedia_keywords("the GB10 GPU"), "GB10 GPU");
        // All-stopword input reduces to empty.
        assert_eq!(wikipedia_keywords("what is the"), "");
    }

    #[test]
    fn parse_wikipedia_handles_garbage_and_missing_fields() {
        assert!(parse_wikipedia_results("not json", 8).is_empty());
        assert!(parse_wikipedia_results(r#"{"query":{}}"#, 8).is_empty());
        // A hit missing its title is skipped, not panicked on.
        assert!(parse_wikipedia_results(r#"{"query":{"search":[{"snippet":"x"}]}}"#, 8).is_empty());
    }

    #[test]
    fn parse_searxng_extracts_url_title_content_and_honors_max() {
        let body = r#"{"results":[
            {"url":"https://docs.vllm.ai/page","title":"vLLM docs","content":"PagedAttention engine."},
            {"url":"https://ml-explore.github.io/mlx","title":"MLX","content":"Array framework for Apple silicon."},
            {"url":"https://example.com/three","title":"Third","content":"x"}
        ]}"#;
        let all = parse_searxng_results(body, 8);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].url, "https://docs.vllm.ai/page");
        assert_eq!(all[0].title, "vLLM docs");
        assert_eq!(all[0].snippet, "PagedAttention engine.");
        // max caps the result count.
        assert_eq!(parse_searxng_results(body, 2).len(), 2);
    }

    #[test]
    fn parse_searxng_handles_garbage_missing_fields_and_non_http() {
        // Not JSON / no results array → empty, no panic.
        assert!(parse_searxng_results("not json", 8).is_empty());
        assert!(parse_searxng_results(r#"{"foo":1}"#, 8).is_empty());
        // A hit missing its url is skipped; a non-http url (ftp/relative) too.
        let body = r#"{"results":[
            {"title":"no url","content":"x"},
            {"url":"ftp://nope","title":"bad scheme","content":"x"},
            {"url":"https://ok.test","title":"","content":"empty title dropped"},
            {"url":"https://keep.test","title":"Keep","content":"kept"}
        ]}"#;
        let all = parse_searxng_results(body, 8);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].url, "https://keep.test");
    }

    #[test]
    fn parse_ddg_skips_non_http_and_empty() {
        // A relative/`javascript:` href must not pollute results.
        let html = r#"<a class="result__a" href="/ddg-internal">Ad</a>"#;
        assert!(parse_ddg_results(html, 8).is_empty());
    }

    #[test]
    fn clean_ddg_url_drops_self_links_unwraps_redirects_keeps_external() {
        // Real external link passes through.
        assert_eq!(
            clean_ddg_url("https://docs.ray.io/en/latest/"),
            Some("https://docs.ray.io/en/latest/".to_string())
        );
        // DDG `/l/?uddg=` redirect is unwrapped + percent-decoded.
        assert_eq!(
            clean_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.vllm.ai%2Fpage&rut=x"),
            Some("https://docs.vllm.ai/page".to_string())
        );
        // A bare duckduckgo.com nav/homepage link (degraded-page junk) is dropped.
        assert_eq!(clean_ddg_url("https://duckduckgo.com/about"), None);
        assert_eq!(clean_ddg_url("//duckduckgo.com/settings"), None);
        // Relative / empty hrefs are dropped.
        assert_eq!(clean_ddg_url("/ddg-internal"), None);
        assert_eq!(clean_ddg_url(""), None);
    }

    #[test]
    fn parse_ddg_drops_degraded_page_with_only_self_links() {
        // A throttled DDG page whose only result link is duckduckgo.com must
        // parse to EMPTY so the caller falls back to another backend.
        let html = r#"<a class="result__a" href="https://duckduckgo.com/?q=x">DuckDuckGo</a>"#;
        assert!(parse_ddg_results(html, 8).is_empty());
    }
}
