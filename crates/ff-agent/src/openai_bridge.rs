//! OpenAI bridge — convert between AgentTool types and OpenAI wire format.

use ff_api::tool_calling::{FunctionCall, OpenAiFunction, OpenAiTool, ToolCall, ToolChatMessage};

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

/// Keys various local models use to name the called tool.
const NAME_KEYS: &[&str] = &["name", "tool", "tool_name", "function_name"];
/// Keys various local models use to carry the tool arguments.
const ARG_KEYS: &[&str] = &["arguments", "parameters", "args", "input", "parameter"];

/// Try to parse tool calls from raw text content when the model doesn't use
/// the native (structured `tool_calls` field) tool-calling format.
///
/// Small local models (Qwen/Mistral/Hermes-family 30B's on llama.cpp without
/// `--jinja`, or when the chat template's grammar doesn't fire) routinely emit
/// the call as plain text. Recovering it here turns a silent stall into a real
/// tool execution. Handles, in order of confidence:
/// 1. Tagged: `<tool_call>…</tool_call>`, `<function_call>…</function_call>`,
///    and Mistral's `[TOOL_CALLS][ … ]`.
/// 2. A markdown code fence wrapping the whole payload (```json … ```).
/// 3. Whole-content JSON object — bare `{"name":…,"arguments":…}`, an OpenAI
///    `{"function":{…}}` / `{"type":"function","function":{…}}` wrapper, or a
///    `{"tool_calls":[…]}` envelope. Alternate name/arg keys are accepted.
/// 4. Whole-content JSON array of the above.
/// 5. Embedded: a JSON object buried in surrounding prose ("I'll read it:
///    {…}"). Only accepted when *strongly* tool-call-shaped (BOTH a name key
///    AND an args key present) so incidental JSON in a final answer is not
///    misread as a call.
pub fn parse_text_tool_calls(text: &str) -> Vec<ToolCall> {
    // 1. Tagged forms (most explicit) — return as soon as any match.
    let calls = parse_tagged_tool_calls(text);
    if !calls.is_empty() {
        return calls;
    }

    // Strip an optional markdown code fence wrapping the entire content.
    let trimmed = strip_code_fence(text.trim());

    // 2/3. Whole-content JSON object.
    if trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        let calls = parse_json_value_as_calls(&parsed);
        if !calls.is_empty() {
            return calls;
        }
    }

    // 4. Whole-content JSON array of tool calls.
    if trimmed.starts_with('[')
        && trimmed.ends_with(']')
        && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed)
    {
        let mut calls = Vec::new();
        for item in &arr {
            if let Some(call) = try_parse_single_tool_call(item, calls.len()) {
                calls.push(call);
            }
        }
        if !calls.is_empty() {
            return calls;
        }
    }

    // 5. JSON object(s) embedded in surrounding prose — strict shape only.
    extract_embedded_tool_calls(trimmed)
}

/// Paired open/close tag forms emitted by various chat templates.
const TAG_PAIRS: &[(&str, &str)] = &[
    ("<tool_call>", "</tool_call>"),
    ("<function_call>", "</function_call>"),
];

fn parse_tagged_tool_calls(text: &str) -> Vec<ToolCall> {
    // Mistral: `[TOOL_CALLS]` marker followed by a JSON array (no close tag).
    if let Some(pos) = text.find("[TOOL_CALLS]") {
        let rest = strip_code_fence(text[pos + "[TOOL_CALLS]".len()..].trim());
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(rest) {
            let mut calls = Vec::new();
            for item in &arr {
                if let Some(call) = try_parse_single_tool_call(item, calls.len()) {
                    calls.push(call);
                }
            }
            if !calls.is_empty() {
                return calls;
            }
        } else if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(rest)
            && let Some(call) = try_parse_single_tool_call(&parsed, 0)
        {
            return vec![call];
        }
    }

    let mut calls = Vec::new();
    for (open, close) in TAG_PAIRS {
        let mut search_from = 0;
        while let Some(start) = text[search_from..].find(open) {
            let start = search_from + start + open.len();
            if let Some(end) = text[start..].find(close) {
                let json_str = strip_code_fence(text[start..start + end].trim());
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                    && let Some(call) = try_parse_single_tool_call(&parsed, calls.len())
                {
                    calls.push(call);
                }
                search_from = start + end + close.len();
            } else {
                break;
            }
        }
    }

    calls
}

/// Interpret a parsed JSON value as one or more tool calls. Handles a bare
/// call object, an OpenAI `{"tool_calls":[…]}` envelope, and `{"calls":[…]}`.
fn parse_json_value_as_calls(parsed: &serde_json::Value) -> Vec<ToolCall> {
    for envelope in ["tool_calls", "calls"] {
        if let Some(arr) = parsed.get(envelope).and_then(|v| v.as_array()) {
            let mut calls = Vec::new();
            for item in arr {
                if let Some(call) = try_parse_single_tool_call(item, calls.len()) {
                    calls.push(call);
                }
            }
            return calls;
        }
    }
    try_parse_single_tool_call(parsed, 0)
        .map(|c| vec![c])
        .unwrap_or_default()
}

/// Strip a single surrounding markdown code fence (```lang … ```), returning
/// the inner payload. Returns the input unchanged when not fenced.
fn strip_code_fence(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```")
        && let Some(end) = rest.rfind("```")
    {
        let inner = rest[..end].trim_end();
        // Drop a leading language tag line (e.g. "json", "tool_code").
        return match inner.find('\n') {
            Some(nl) if !inner[..nl].contains(['{', '[', '"']) => inner[nl + 1..].trim(),
            _ => inner.trim(),
        };
    }
    s
}

/// Resolve the tool name from any of the recognized name keys, unwrapping an
/// OpenAI `{"function":{…}}` / `{"type":"function","function":{…}}` wrapper.
fn unwrap_function<'a>(parsed: &'a serde_json::Value) -> &'a serde_json::Value {
    parsed
        .get("function")
        .filter(|f| f.is_object())
        .unwrap_or(parsed)
}

fn find_name(obj: &serde_json::Value) -> Option<String> {
    NAME_KEYS
        .iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn find_args(obj: &serde_json::Value) -> Option<String> {
    ARG_KEYS.iter().find_map(|k| {
        obj.get(*k).map(|v| {
            if v.is_string() {
                v.as_str().unwrap_or("{}").to_string()
            } else {
                v.to_string()
            }
        })
    })
}

fn try_parse_single_tool_call(parsed: &serde_json::Value, index: usize) -> Option<ToolCall> {
    let obj = unwrap_function(parsed);
    let name = find_name(obj)?;
    let arguments = find_args(obj).unwrap_or_else(|| "{}".to_string());

    Some(ToolCall {
        id: format!("call_text_{}", index),
        call_type: "function".to_string(),
        function: FunctionCall { name, arguments },
    })
}

/// Scan prose for balanced `{…}` substrings and parse those that are strongly
/// tool-call-shaped (require BOTH a name key and an args key) to avoid
/// misreading incidental JSON in a model's final answer as a tool call.
fn extract_embedded_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = match_brace(text, i) {
                let slice = &text[i..=end];
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
                    let obj = unwrap_function(&v);
                    if find_name(obj).is_some()
                        && find_args(obj).is_some()
                        && let Some(call) = try_parse_single_tool_call(&v, calls.len())
                    {
                        calls.push(call);
                    }
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    calls
}

/// Find the byte index of the `}` matching the `{` at `open`, respecting
/// nested braces and quoted strings. Returns `None` if unbalanced.
fn match_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (rel, ch) in s[open..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + rel);
                }
            }
            _ => {}
        }
    }
    None
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
    fn parse_embedded_tool_call_in_prose() {
        // The chronic stall case: model emits a prose preamble then the tool
        // JSON in `content` instead of the structured field. Recover it.
        let text = r#"I'll read the file: {"name": "Read", "arguments": {"file_path": "/tmp/x"}}"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Read");
        assert!(calls[0].function.arguments.contains("/tmp/x"));
    }

    #[test]
    fn embedded_requires_name_and_args() {
        // Incidental JSON in a final answer (no args-like key) must NOT be
        // misread as a tool call.
        let text = r#"The result is {"name": "Acme Corp", "founded": 1999}."#;
        assert!(parse_text_tool_calls(text).is_empty());
        let text2 = r#"Here is some data: {"file_path": "/tmp/x", "size": 12}."#;
        assert!(parse_text_tool_calls(text2).is_empty());
    }

    #[test]
    fn parse_fenced_json_tool_call() {
        let text = "Sure, let me do that.\n```json\n{\"name\": \"Bash\", \"arguments\": {\"command\": \"ls\"}}\n```";
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Bash");
        assert!(calls[0].function.arguments.contains("ls"));
    }

    #[test]
    fn parse_fenced_no_lang() {
        let text = "```\n{\"name\": \"Read\", \"arguments\": {\"file_path\": \"/a\"}}\n```";
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Read");
    }

    #[test]
    fn parse_openai_function_wrapper() {
        let text = r#"{"type": "function", "function": {"name": "Read", "arguments": {"file_path": "/a"}}}"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Read");
    }

    #[test]
    fn parse_tool_calls_envelope() {
        let text = r#"{"tool_calls": [{"function": {"name": "Read", "arguments": {"file_path": "/a"}}}, {"name": "Bash", "arguments": {"command": "ls"}}]}"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "Read");
        assert_eq!(calls[1].function.name, "Bash");
    }

    #[test]
    fn parse_alternate_keys() {
        // "tool" name key + "parameters" args key (some Qwen variants).
        let text = r#"{"tool": "Grep", "parameters": {"pattern": "foo"}}"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Grep");
        assert!(calls[0].function.arguments.contains("foo"));
    }

    #[test]
    fn parse_mistral_tool_calls_marker() {
        let text = r#"[TOOL_CALLS][{"name": "Read", "arguments": {"file_path": "/a"}}]"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Read");
    }

    #[test]
    fn parse_function_call_tag() {
        let text =
            r#"<function_call>{"name": "Read", "arguments": {"file_path": "/a"}}</function_call>"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "Read");
    }

    #[test]
    fn brace_in_string_argument_does_not_break_extraction() {
        // A `}` inside a string value must not prematurely close the object.
        let text = r#"Running: {"name": "Bash", "arguments": {"command": "echo ${HOME}"}}"#;
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].function.arguments.contains("${HOME}"));
    }

    #[test]
    fn plain_prose_no_false_positive() {
        let calls = parse_text_tool_calls("I have finished the task. Everything looks good.");
        assert!(calls.is_empty());
    }
}
