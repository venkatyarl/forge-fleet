//! OpenAI bridge — convert between AgentTool types and OpenAI wire format.

use ff_api::tool_calling::{
    FunctionCall, OpenAiFunction, OpenAiTool, ToolCall, ToolChatMessage,
};

use crate::tools::AgentTool;

/// Convert an AgentTool into an OpenAI tool definition for the request payload.
pub fn tool_to_openai(tool: &dyn AgentTool) -> OpenAiTool {
    OpenAiTool {
        tool_type: "function".to_string(),
        function: OpenAiFunction {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters_schema(),
        },
    }
}

/// Convert all tools to OpenAI format (Box version).
pub fn tools_to_openai(tools: &[Box<dyn AgentTool>]) -> Vec<OpenAiTool> {
    tools.iter().map(|t| tool_to_openai(t.as_ref())).collect()
}

/// Convert all tools to OpenAI format (Arc version).
pub fn tools_to_openai_arc(tools: &[std::sync::Arc<dyn AgentTool>]) -> Vec<OpenAiTool> {
    tools.iter().map(|t| tool_to_openai(t.as_ref())).collect()
}

/// Build a tool-result message in OpenAI format.
pub fn tool_result_message(tool_call_id: &str, content: &str, _is_error: bool) -> ToolChatMessage {
    ToolChatMessage::tool_result(tool_call_id, content)
}

/// Build an assistant message that contains tool calls (for reconstructing history).
pub fn assistant_tool_calls_message(tool_calls: &[ToolCall]) -> ToolChatMessage {
    ToolChatMessage::assistant_tool_calls(tool_calls.to_vec())
}

/// Extract tool calls from an assistant message, handling various LLM response formats.
///
/// Some models use `finish_reason: "tool_calls"`, others use `"stop"` but still
/// include tool_calls in the message. We check the message itself as the primary signal.
pub fn extract_tool_calls(message: &ToolChatMessage) -> Vec<ToolCall> {
    message.tool_calls.clone().unwrap_or_default()
}

/// Try to parse tool calls from raw text content when the model doesn't use
/// the native tool-calling format.
///
/// Handles multiple fallback formats:
/// 1. `<tool_call>{"name":"...", "arguments":{...}}</tool_call>` tags
/// 2. Raw JSON object `{"name":"...", "arguments":{...}}` as the entire content
///    (common with llama.cpp servers without --jinja flag)
pub fn parse_text_tool_calls(text: &str) -> Vec<ToolCall> {
    // First try: <tool_call> tags
    let mut calls = parse_tagged_tool_calls(text);
    if !calls.is_empty() {
        return calls;
    }

    // Second try: raw JSON object with "name" and "arguments" fields
    // This handles the case where llama.cpp returns the tool call as plain text content
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(call) = try_parse_single_tool_call(&parsed, calls.len()) {
                calls.push(call);
                return calls;
            }
        }
    }

    // Third try: JSON array of tool calls
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
            for item in &arr {
                if let Some(call) = try_parse_single_tool_call(item, calls.len()) {
                    calls.push(call);
                }
            }
        }
    }

    calls
}

fn parse_tagged_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut search_from = 0;

    while let Some(start) = text[search_from..].find("<tool_call>") {
        let start = search_from + start + "<tool_call>".len();
        if let Some(end) = text[start..].find("</tool_call>") {
            let json_str = text[start..start + end].trim();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(call) = try_parse_single_tool_call(&parsed, calls.len()) {
                    calls.push(call);
                }
            }
            search_from = start + end + "</tool_call>".len();
        } else {
            break;
        }
    }

    calls
}

fn try_parse_single_tool_call(parsed: &serde_json::Value, index: usize) -> Option<ToolCall> {
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if name.is_empty() {
        return None;
    }

    let arguments = parsed
        .get("arguments")
        .map(|v| {
            if v.is_string() {
                v.as_str().unwrap_or("{}").to_string()
            } else {
                v.to_string()
            }
        })
        .unwrap_or_else(|| "{}".to_string());

    Some(ToolCall {
        id: format!("call_text_{}", index),
        call_type: "function".to_string(),
        function: FunctionCall { name, arguments },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_tool_calls_single() {
        let text = r#"I'll read the file.
<tool_call>{"name": "Read", "arguments": {"file_path": "/tmp/test.rs"}}</tool_call>
"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Read");
    }

    #[test]
    fn parse_text_tool_calls_multiple() {
        let text = r#"Let me check both files.
<tool_call>{"name": "Read", "arguments": {"file_path": "/a.rs"}}</tool_call>
<tool_call>{"name": "Read", "arguments": {"file_path": "/b.rs"}}</tool_call>
"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn parse_text_tool_calls_empty() {
        let calls = parse_text_tool_calls("No tool calls here.");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_raw_json_tool_call() {
        // This is what llama.cpp returns without --jinja
        let text = r#"{"name": "Read", "arguments": {"file_path": "/etc/hostname"}}"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Read");
        assert!(calls[0].function.arguments.contains("/etc/hostname"));
    }

    #[test]
    fn parse_raw_json_with_surrounding_text() {
        // If there's text around the JSON, don't parse it as raw JSON
        let text = r#"I'll read the file: {"name": "Read", "arguments": {"file_path": "/tmp/x"}}"#;
        let calls = parse_text_tool_calls(text);
        // This should NOT match because the text doesn't start with {
        assert!(calls.is_empty());
    }
}
