//! WebFetch tool — fetch web pages and convert to text.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct WebFetchTool;

#[async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page and return its content as text. Useful for reading documentation, API responses, or any web content."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "headers": {
                    "type": "object",
                    "description": "Optional HTTP headers as key-value pairs"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let url = match input.get("url").and_then(Value::as_str) {
            Some(u) if !u.trim().is_empty() => u,
            _ => return AgentToolResult::err("Missing or empty 'url' parameter"),
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("ForgeFleet-Agent/0.1")
            .build()
            .unwrap_or_default();

        let mut req = client.get(url);

        // Add custom headers if provided
        if let Some(headers) = input.get("headers").and_then(Value::as_object) {
            for (k, v) in headers {
                if let Some(v_str) = v.as_str() {
                    req = req.header(k.as_str(), v_str);
                }
            }
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                match resp.text().await {
                    Ok(body) => {
                        let text = if content_type.contains("text/html") {
                            strip_html_tags(&body)
                        } else {
                            body
                        };

                        let output = format!(
                            "HTTP {status}\nContent-Type: {content_type}\n\n{}",
                            text.trim()
                        );
                        AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                    }
                    Err(e) => AgentToolResult::err(format!("Failed to read response body: {e}")),
                }
            }
            Err(e) => AgentToolResult::err(format!("Fetch failed: {e}")),
        }
    }
}

/// Basic HTML tag stripping (no external dependency).
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;

    let lower = html.to_ascii_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if !in_tag && starts_with_at(&lower_chars, i, "<script") {
            in_script = true;
            in_tag = true;
        } else if in_script && starts_with_at(&lower_chars, i, "</script") {
            in_script = false;
            in_tag = true;
        } else if !in_tag && starts_with_at(&lower_chars, i, "<style") {
            in_style = true;
            in_tag = true;
        } else if in_style && starts_with_at(&lower_chars, i, "</style") {
            in_style = false;
            in_tag = true;
        } else if chars[i] == '<' {
            in_tag = true;
        } else if chars[i] == '>' {
            in_tag = false;
            i += 1;
            continue;
        } else if !in_tag && !in_script && !in_style {
            result.push(chars[i]);
        }
        i += 1;
    }

    // Collapse whitespace
    let mut collapsed = String::with_capacity(result.len());
    let mut prev_newline = false;
    for line in result.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_newline {
                collapsed.push('\n');
                prev_newline = true;
            }
        } else {
            collapsed.push_str(trimmed);
            collapsed.push('\n');
            prev_newline = false;
        }
    }

    collapsed
}

fn starts_with_at(chars: &[char], pos: usize, needle: &str) -> bool {
    let needle_chars: Vec<char> = needle.chars().collect();
    if pos + needle_chars.len() > chars.len() {
        return false;
    }
    chars[pos..pos + needle_chars.len()] == needle_chars[..]
}
