//! HttpRequest tool — generic HTTP client for API calls.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct HttpRequestTool;

#[async_trait]
impl AgentTool for HttpRequestTool {
    fn name(&self) -> &str { "HttpRequest" }

    fn description(&self) -> &str {
        "Make HTTP requests (GET, POST, PUT, DELETE, PATCH). Use for API calls, webhooks, and web services."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to request" },
                "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"], "description": "HTTP method (default GET)" },
                "headers": { "type": "object", "description": "HTTP headers as key-value pairs" },
                "body": { "type": "string", "description": "Request body (for POST/PUT/PATCH)" },
                "json_body": { "type": "object", "description": "JSON request body (auto-sets Content-Type)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let url = match input.get("url").and_then(Value::as_str) {
            Some(u) if !u.is_empty() => u,
            _ => return AgentToolResult::err("Missing 'url'"),
        };

        let method = input.get("method").and_then(Value::as_str).unwrap_or("GET").to_uppercase();
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap_or_default();

        let mut req = match method.as_str() {
            "GET" => client.get(url),
            "POST" => client.post(url),
            "PUT" => client.put(url),
            "DELETE" => client.delete(url),
            "PATCH" => client.patch(url),
            _ => return AgentToolResult::err(format!("Unknown method: {method}")),
        };

        if let Some(headers) = input.get("headers").and_then(Value::as_object) {
            for (k, v) in headers {
                if let Some(v_str) = v.as_str() { req = req.header(k.as_str(), v_str); }
            }
        }

        if let Some(json_body) = input.get("json_body") {
            req = req.json(json_body);
        } else if let Some(body) = input.get("body").and_then(Value::as_str) {
            req = req.body(body.to_string());
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let headers: Vec<String> = resp.headers().iter()
                    .take(10)
                    .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("")))
                    .collect();
                let body = resp.text().await.unwrap_or_default();

                let output = format!("HTTP {status}\n{}\n\n{}", headers.join("\n"), body);
                if status.is_success() {
                    AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                } else {
                    AgentToolResult::err(truncate_output(&output, MAX_TOOL_RESULT_CHARS))
                }
            }
            Err(e) => AgentToolResult::err(format!("Request failed: {e}")),
        }
    }
}
