//! SSE streaming — parse Server-Sent Events from llama.cpp/vLLM streaming responses.
//!
//! When stream:true is set, the LLM returns text/event-stream with delta chunks.
//! This module parses SSE format and emits token-by-token events.

use ff_api::tool_calling::{FunctionCall, ToolCall, ToolChatMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A parsed SSE event from a streaming chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub id: String,
    pub model: String,
    pub delta: StreamDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    pub function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamFunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Accumulator that reconstructs a full message from streaming deltas.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    pub id: String,
    pub model: String,
    pub content: String,
    pub tool_calls: Vec<AccumulatedToolCall>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct AccumulatedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a parsed SSE chunk into the accumulator.
    pub fn feed(&mut self, chunk: &StreamChunk) {
        if !chunk.id.is_empty() {
            self.id = chunk.id.clone();
        }
        if !chunk.model.is_empty() {
            self.model = chunk.model.clone();
        }
        if chunk.finish_reason.is_some() {
            self.finish_reason = chunk.finish_reason.clone();
        }

        if let Some(content) = &chunk.delta.content {
            self.content.push_str(content);
        }

        if let Some(tool_calls) = &chunk.delta.tool_calls {
            for tc in tool_calls {
                // Ensure we have enough slots
                while self.tool_calls.len() <= tc.index {
                    self.tool_calls.push(AccumulatedToolCall::default());
                }

                let slot = &mut self.tool_calls[tc.index];
                if let Some(id) = &tc.id {
                    slot.id = id.clone();
                }
                if let Some(func) = &tc.function {
                    if let Some(name) = &func.name {
                        slot.name = name.clone();
                    }
                    if let Some(args) = &func.arguments {
                        slot.arguments.push_str(args);
                    }
                }
            }
        }
    }

    /// Finalize into a ToolChatMessage.
    pub fn into_message(self) -> ToolChatMessage {
        if !self.tool_calls.is_empty() {
            let calls: Vec<ToolCall> = self
                .tool_calls
                .into_iter()
                .filter(|tc| !tc.name.is_empty())
                .map(|tc| ToolCall {
                    id: if tc.id.is_empty() {
                        format!("call_{}", uuid::Uuid::new_v4().as_simple())
                    } else {
                        tc.id
                    },
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: tc.name,
                        arguments: tc.arguments,
                    },
                })
                .collect();

            if !calls.is_empty() {
                return ToolChatMessage::assistant_tool_calls(calls);
            }
        }

        ToolChatMessage::assistant(&self.content)
    }

    /// Check if the accumulator has tool calls.
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.iter().any(|tc| !tc.name.is_empty())
    }
}

/// Parse a single SSE line into a StreamChunk.
/// SSE format: `data: {"id":"...","choices":[{"delta":{"content":"token"}}]}`
pub fn parse_sse_line(line: &str) -> Option<StreamChunk> {
    let data = line.strip_prefix("data: ")?;
    if data.trim() == "[DONE]" {
        return None;
    }

    let json: Value = serde_json::from_str(data).ok()?;
    let choices = json.get("choices")?.as_array()?;
    let choice = choices.first()?;

    let delta = choice.get("delta")?;

    Some(StreamChunk {
        id: json
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model: json
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        delta: StreamDelta {
            role: delta.get("role").and_then(Value::as_str).map(String::from),
            content: delta
                .get("content")
                .and_then(Value::as_str)
                .map(String::from),
            tool_calls: delta
                .get("tool_calls")
                .and_then(|tc| serde_json::from_value(tc.clone()).ok()),
        },
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// Parse a full SSE response body (multiple lines) into chunks.
pub fn parse_sse_body(body: &str) -> Vec<StreamChunk> {
    body.lines()
        .filter(|line| line.starts_with("data: "))
        .filter_map(|line| parse_sse_line(line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_delta() {
        let line = r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk = parse_sse_line(line).unwrap();
        assert_eq!(chunk.delta.content, Some("Hello".into()));
    }

    #[test]
    fn parse_done_marker() {
        assert!(parse_sse_line("data: [DONE]").is_none());
    }

    #[test]
    fn accumulate_content() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&StreamChunk {
            id: "1".into(),
            model: "test".into(),
            delta: StreamDelta {
                role: None,
                content: Some("Hel".into()),
                tool_calls: None,
            },
            finish_reason: None,
        });
        acc.feed(&StreamChunk {
            id: "1".into(),
            model: "test".into(),
            delta: StreamDelta {
                role: None,
                content: Some("lo!".into()),
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
        });

        assert_eq!(acc.content, "Hello!");
        assert_eq!(acc.finish_reason, Some("stop".into()));

        let msg = acc.into_message();
        assert_eq!(msg.text_content(), Some("Hello!"));
    }

    #[test]
    fn accumulate_tool_calls() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&StreamChunk {
            id: "1".into(),
            model: "test".into(),
            delta: StreamDelta {
                role: None,
                content: None,
                tool_calls: Some(vec![StreamToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    call_type: Some("function".into()),
                    function: Some(StreamFunctionDelta {
                        name: Some("Bash".into()),
                        arguments: Some("{\"com".into()),
                    }),
                }]),
            },
            finish_reason: None,
        });
        acc.feed(&StreamChunk {
            id: "1".into(),
            model: "test".into(),
            delta: StreamDelta {
                role: None,
                content: None,
                tool_calls: Some(vec![StreamToolCallDelta {
                    index: 0,
                    id: None,
                    call_type: None,
                    function: Some(StreamFunctionDelta {
                        name: None,
                        arguments: Some("mand\":\"ls\"}".into()),
                    }),
                }]),
            },
            finish_reason: Some("tool_calls".into()),
        });

        assert!(acc.has_tool_calls());
        assert_eq!(acc.tool_calls[0].name, "Bash");
        assert_eq!(acc.tool_calls[0].arguments, "{\"command\":\"ls\"}");
    }
}
