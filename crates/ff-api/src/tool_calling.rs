//! OpenAI-compatible tool-calling types for function calling with local LLMs.
//!
//! These types extend the base ChatCompletion types in `types.rs` with explicit
//! tool/function-calling fields used by the agent loop to communicate with
//! OpenAI-compatible LLM endpoints (Ollama, llama.cpp, vLLM, etc.).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Tool definitions sent in the request
// ---------------------------------------------------------------------------

/// An OpenAI tool definition sent in `ChatCompletionRequest.tools[]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OpenAiFunction,
}

/// Function metadata within an OpenAI tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiFunction {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the function parameters.
    pub parameters: Value,
}

// ---------------------------------------------------------------------------
// Tool calls returned by the model
// ---------------------------------------------------------------------------

/// A tool call returned by the model in the assistant message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// The function name and arguments within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded string of the function arguments.
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// Extended message type with tool-calling fields
// ---------------------------------------------------------------------------

/// Chat message that supports the full OpenAI tool-calling protocol.
///
/// - `role: "assistant"` messages may include `tool_calls`.
/// - `role: "tool"` messages must include `tool_call_id`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ToolChatMessage {
    /// Create a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(Value::String(content.into())),
            ..Default::default()
        }
    }

    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(Value::String(content.into())),
            ..Default::default()
        }
    }

    /// Create an assistant message with text content.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(Value::String(content.into())),
            ..Default::default()
        }
    }

    /// Create an assistant message that contains tool calls (no text content).
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(tool_calls),
            ..Default::default()
        }
    }

    /// Create a tool-result message.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(Value::String(content.into())),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    /// Extract text content from this message, if present.
    pub fn text_content(&self) -> Option<&str> {
        self.content.as_ref().and_then(Value::as_str)
    }

    /// Returns true if this message contains tool calls.
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Request / Response types with tool-calling support
// ---------------------------------------------------------------------------

/// Extended ChatCompletionRequest with `tools` and `tool_choice` fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ToolChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

/// A response choice that may include tool calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChatChoice {
    pub index: usize,
    #[serde(default)]
    pub message: Option<ToolChatMessage>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Extended ChatCompletionResponse with tool-calling support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ToolChatChoice>,
    #[serde(default)]
    pub usage: Option<super::types::Usage>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_tool_chat_message() {
        let msg = ToolChatMessage::user("hello");
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ToolChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, "user");
        assert_eq!(parsed.text_content(), Some("hello"));
    }

    #[test]
    fn parse_tool_calls_response() {
        let json = json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "qwen2.5-coder-32b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc123",
                        "type": "function",
                        "function": {
                            "name": "Bash",
                            "arguments": "{\"command\":\"echo hello\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30 }
        });

        let resp: ToolChatCompletionResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        let msg = resp.choices[0].message.as_ref().unwrap();
        assert!(msg.has_tool_calls());
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].function.name, "Bash");
    }

    #[test]
    fn serialize_request_with_tools() {
        let req = ToolChatCompletionRequest {
            model: "qwen2.5-coder-32b".into(),
            messages: vec![
                ToolChatMessage::system("You are a coding agent."),
                ToolChatMessage::user("List files"),
            ],
            tools: Some(vec![OpenAiTool {
                tool_type: "function".into(),
                function: OpenAiFunction {
                    name: "Bash".into(),
                    description: "Run a shell command".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" }
                        },
                        "required": ["command"]
                    }),
                },
            }]),
            tool_choice: Some(json!("auto")),
            temperature: Some(0.3),
            max_tokens: Some(4096),
            stream: Some(false),
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["tools"][0]["function"]["name"], "Bash");
        assert_eq!(json["stream"], false);
    }
}
