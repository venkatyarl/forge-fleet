//! Direct-endpoint council member for local fleet LLMs.
//!
//! [`LocalLlmMember`] implements the base [`CouncilMember`] interface for one
//! EXPLICIT fleet endpoint + model name — the direct-address counterpart to
//! `fleet_oneshot`, which picks a deployment from the live DB router. It POSTs
//! the same OpenAI-shape chat completion (reusing `fleet_oneshot`'s payload
//! helpers rather than forking them), in either BLOCKING or STREAMING (SSE)
//! mode, and formats the result to the council response schema
//! (`answer`/`confidence`/`evidence` — the shape `ff council` members are
//! prompted to return and `ff-terminal::council_cmd` parses).
//!
//! Streaming correctness (the two failure classes this module is built to
//! avoid):
//! 1. **UTF-8 split across chunks** — network chunks can split a multi-byte
//!    character anywhere, so bytes are buffered and only COMPLETE lines
//!    (`\n`-terminated; `0x0A` never occurs inside a multi-byte UTF-8
//!    sequence) are ever decoded. No per-chunk lossy decode.
//! 2. **Final unterminated SSE event** — servers may close the stream without
//!    a trailing blank line (or without a `[DONE]` sentinel); [`SseParser::finish`]
//!    flushes whatever event is still buffered so the tail of the completion
//!    is never dropped.

use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::StreamExt;
use serde_json::{Value, json};

/// Default per-dispatch timeout, matching `fleet_oneshot`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// Base interface every council member implements: a roster label plus one
/// prompt → structured-answer dispatch. `LocalLlmMember` is the fleet-endpoint
/// implementation; vendor-CLI members can adopt the same interface later.
#[async_trait::async_trait]
pub trait CouncilMember: Send + Sync {
    /// Roster label for this member (e.g. `local:qwen3-coder-30b`).
    fn name(&self) -> &str;

    /// Dispatch `prompt` to this member and return its council-schema answer.
    async fn respond(&self, prompt: &str) -> Result<CouncilMemberResponse>;
}

/// One member's answer in the council response schema
/// (`answer`/`confidence`/`evidence`), plus the attribution fields callers
/// need to log the dispatch in `ff_interactions`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CouncilMemberResponse {
    /// The recommendation and reasoning.
    pub answer: String,
    /// Self-reported confidence, clamped to 0.0–1.0.
    pub confidence: f32,
    /// Concrete facts/sources backing the answer.
    pub evidence: Vec<String>,
    /// Base endpoint that served the call (e.g. `http://192.168.5.103:55000`).
    pub endpoint: String,
    /// Model name the request was addressed to.
    pub model: String,
    pub latency_ms: u128,
    /// Prompt/completion tokens from the response `usage` block; `0` when the
    /// server omits it (callers can estimate via `llm_attribution`).
    pub tokens_in: i32,
    pub tokens_out: i32,
}

/// A council member backed by one explicit local LLM endpoint + model name.
#[derive(Debug, Clone)]
pub struct LocalLlmMember {
    name: String,
    endpoint: String,
    model: String,
    stream: bool,
    timeout: Duration,
}

impl LocalLlmMember {
    /// A member addressing `endpoint` (base URL, normalized to
    /// `/v1/chat/completions` on dispatch) as `model`. Blocking by default;
    /// opt into SSE with [`Self::streaming`].
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            name: format!("local:{model}"),
            endpoint: endpoint.into(),
            model,
            stream: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Toggle streaming (SSE) vs blocking dispatch.
    pub fn streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Dispatch `prompt` to the endpoint and format the completion to the
    /// council response schema.
    pub async fn respond(&self, prompt: &str) -> Result<CouncilMemberResponse> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| anyhow!("build http client: {e}"))?;
        let url = ff_core::url::normalize_chat_completions_url(&self.endpoint);
        let body = json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": self.stream,
        });
        let start = std::time::Instant::now();
        let (raw, tokens_in, tokens_out) = if self.stream {
            self.stream_completion(&client, &url, &body).await?
        } else {
            self.blocking_completion(&client, &url, &body).await?
        };
        let text = crate::fleet_oneshot::strip_think_block(&raw);
        if text.trim().is_empty() {
            return Err(anyhow!(
                "{} ({}) returned an empty completion",
                self.endpoint,
                self.model
            ));
        }
        let parsed = parse_council_answer(&text);
        Ok(CouncilMemberResponse {
            answer: parsed.answer,
            confidence: parsed.confidence,
            evidence: parsed.evidence,
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            latency_ms: start.elapsed().as_millis(),
            tokens_in,
            tokens_out,
        })
    }

    /// One non-streaming chat completion, mirroring `fleet_oneshot`'s
    /// dispatch: decode the JSON payload, surface HTTP errors with a truncated
    /// body, and read `usage` tokens.
    async fn blocking_completion(
        &self,
        client: &reqwest::Client,
        url: &str,
        body: &Value,
    ) -> Result<(String, i32, i32)> {
        let resp = client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("POST {url}: {e}"))?;
        let status = resp.status();
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("decode response from {}: {e}", self.endpoint))?;
        if !status.is_success() {
            return Err(anyhow!(
                "{} ({}) returned HTTP {status}: {}",
                self.endpoint,
                self.model,
                payload.to_string().chars().take(400).collect::<String>()
            ));
        }
        let text = crate::fleet_oneshot::extract_completion_text(&payload).ok_or_else(|| {
            anyhow!(
                "{} ({}) returned an empty completion",
                self.endpoint,
                self.model
            )
        })?;
        let (tokens_in, tokens_out) = crate::fleet_oneshot::usage_tokens_i32(&payload);
        Ok((text, tokens_in, tokens_out))
    }

    /// One streaming (SSE) chat completion: accumulate `delta.content` across
    /// events, stop at `[DONE]`, and flush any final unterminated event when
    /// the server closes the stream without one.
    async fn stream_completion(
        &self,
        client: &reqwest::Client,
        url: &str,
        body: &Value,
    ) -> Result<(String, i32, i32)> {
        let resp = client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("POST {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "{} ({}) returned HTTP {status}: {}",
                self.endpoint,
                self.model,
                err_body.chars().take(400).collect::<String>()
            ));
        }
        let mut parser = SseParser::default();
        let mut acc = StreamAccumulator::default();
        let mut chunks = resp.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk
                .map_err(|e| anyhow!("stream from {} ({}): {e}", self.endpoint, self.model))?;
            for payload in parser.push(&chunk) {
                acc.consume(&payload);
            }
            if acc.done {
                break;
            }
        }
        if !acc.done {
            for payload in parser.finish() {
                acc.consume(&payload);
            }
        }
        Ok((acc.text, acc.tokens_in, acc.tokens_out))
    }
}

#[async_trait::async_trait]
impl CouncilMember for LocalLlmMember {
    fn name(&self) -> &str {
        &self.name
    }

    async fn respond(&self, prompt: &str) -> Result<CouncilMemberResponse> {
        self.respond(prompt).await
    }
}

/// Incremental SSE splitter that is safe against arbitrary chunk boundaries.
///
/// Bytes are buffered per LINE and decoded only once the line is complete
/// (`\n`-terminated) — `0x0A` never occurs inside a multi-byte UTF-8 sequence,
/// so a character split across two network chunks is reassembled instead of
/// being lossy-decoded into U+FFFD. A blank line completes an event; `data:`
/// lines accumulate per the SSE spec (multiple lines join with `\n`), and
/// other fields (`event:`, `id:`, `retry:`, `:` comments) are ignored.
#[derive(Debug, Default)]
struct SseParser {
    line_buf: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseParser {
    /// Feed one network chunk; returns the `data` payload of every event
    /// COMPLETED by it.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line_buf);
                self.take_line(&line, &mut events);
            } else {
                self.line_buf.push(byte);
            }
        }
        events
    }

    /// End of stream: flush a trailing unterminated line, then the event it
    /// belongs to — a server that closes without a final blank line must not
    /// lose its last event.
    fn finish(mut self) -> Vec<String> {
        let mut events = Vec::new();
        let line = std::mem::take(&mut self.line_buf);
        if !line.is_empty() {
            self.take_line(&line, &mut events);
        }
        if !self.data_lines.is_empty() {
            events.push(self.data_lines.join("\n"));
        }
        events
    }

    fn take_line(&mut self, mut line: &[u8], events: &mut Vec<String>) {
        // Tolerate CRLF line endings (so a CRLF blank line still delimits).
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            if !self.data_lines.is_empty() {
                events.push(std::mem::take(&mut self.data_lines).join("\n"));
            }
            return;
        }
        let text = String::from_utf8_lossy(line);
        if let Some(data) = text.strip_prefix("data:") {
            self.data_lines
                .push(data.strip_prefix(' ').unwrap_or(data).to_string());
        }
    }
}

/// Folds SSE `data` payloads into the completion text: appends each chunk's
/// `choices[0].delta.content` (or legacy `text`), records the `usage` block
/// when a server includes one (typically the final chunk), and stops at the
/// `[DONE]` sentinel. Malformed keep-alive payloads are skipped, never fatal.
#[derive(Debug, Default)]
struct StreamAccumulator {
    text: String,
    tokens_in: i32,
    tokens_out: i32,
    done: bool,
}

impl StreamAccumulator {
    fn consume(&mut self, payload: &str) {
        let trimmed = payload.trim();
        if trimmed == "[DONE]" {
            self.done = true;
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            return;
        };
        if let Some(delta) = stream_delta_text(&value) {
            self.text.push_str(delta);
        }
        let (tokens_in, tokens_out) = crate::fleet_oneshot::usage_tokens_i32(&value);
        if tokens_in > 0 || tokens_out > 0 {
            self.tokens_in = tokens_in;
            self.tokens_out = tokens_out;
        }
    }
}

/// Pull the text increment out of one streaming chunk, tolerating both the
/// `delta.content` and legacy `text` shapes (the streaming analogue of
/// `fleet_oneshot::extract_completion_text`).
fn stream_delta_text(value: &Value) -> Option<&str> {
    let choice = value.get("choices")?.as_array()?.first()?;
    if let Some(content) = choice
        .get("delta")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str())
    {
        return Some(content);
    }
    choice.get("text").and_then(|t| t.as_str())
}

fn default_confidence() -> f32 {
    0.5
}

/// The council response schema members are prompted to return. The `answer`
/// aliases and defaults match `ff-terminal::council_cmd::MemberAnswer` (which
/// can't be shared here — ff-terminal depends on ff-agent, not vice versa).
#[derive(Debug, Clone, serde::Deserialize)]
struct CouncilAnswer {
    #[serde(alias = "response", alias = "text")]
    answer: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    evidence: Vec<String>,
}

/// Extracts the first balanced `{...}` span so JSON wrapped in prose or a
/// fenced code block still parses.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end >= start).then(|| &s[start..=end])
}

/// Parse a raw completion into the council schema, falling back to the raw
/// text with neutral confidence — a non-compliant model reply should degrade,
/// not fail the member.
fn parse_council_answer(raw: &str) -> CouncilAnswer {
    if let Some(json) = extract_json_object(raw)
        && let Ok(mut parsed) = serde_json::from_str::<CouncilAnswer>(json)
        && !parsed.answer.trim().is_empty()
    {
        parsed.confidence = parsed.confidence.clamp(0.0, 1.0);
        return parsed;
    }
    CouncilAnswer {
        answer: raw.trim().to_string(),
        confidence: default_confidence(),
        evidence: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consume_all(payloads: &[String]) -> StreamAccumulator {
        let mut acc = StreamAccumulator::default();
        for p in payloads {
            acc.consume(p);
        }
        acc
    }

    #[test]
    fn sse_parser_reassembles_utf8_split_across_chunks() {
        // Worst-case chunking: one byte at a time, so every multi-byte
        // character ("héllo", the em dash, "✓") is split mid-sequence.
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"héllo — ✓\"}}]}\n\n";
        let mut parser = SseParser::default();
        let mut payloads = Vec::new();
        for byte in event.as_bytes() {
            payloads.extend(parser.push(std::slice::from_ref(byte)));
        }
        assert_eq!(payloads.len(), 1);
        let acc = consume_all(&payloads);
        assert_eq!(acc.text, "héllo — ✓");
        assert!(!acc.text.contains('\u{FFFD}'));
    }

    #[test]
    fn sse_parser_flushes_final_unterminated_event() {
        // Server closes the stream with no trailing blank line and no [DONE].
        let mut parser = SseParser::default();
        let mut payloads =
            parser.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"head \"}}]}\n\n");
        payloads.extend(parser.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}"));
        assert_eq!(payloads.len(), 1, "unterminated event must not emit early");
        let flushed = parser.finish();
        assert_eq!(flushed.len(), 1);
        payloads.extend(flushed);
        assert_eq!(consume_all(&payloads).text, "head tail");
    }

    #[test]
    fn sse_parser_handles_crlf_and_ignores_non_data_fields() {
        let raw = ": keepalive\r\nevent: message\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\n";
        let mut parser = SseParser::default();
        let payloads = parser.push(raw.as_bytes());
        assert_eq!(payloads.len(), 1);
        assert_eq!(consume_all(&payloads).text, "ok");
    }

    #[test]
    fn sse_parser_joins_multiline_data_per_spec() {
        let mut parser = SseParser::default();
        let payloads = parser.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(payloads, vec!["line1\nline2".to_string()]);
    }

    #[test]
    fn accumulator_stops_at_done_and_reads_usage() {
        let mut parser = SseParser::default();
        let payloads = parser.push(
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n\
              data: [DONE]\n\n",
        );
        let acc = consume_all(&payloads);
        assert!(acc.done);
        assert_eq!(acc.text, "hi");
        assert_eq!((acc.tokens_in, acc.tokens_out), (7, 3));
    }

    #[test]
    fn accumulator_reads_legacy_text_chunks_and_skips_malformed() {
        let mut acc = StreamAccumulator::default();
        acc.consume("{\"choices\":[{\"text\":\"legacy\"}]}");
        acc.consume("not json — a malformed keepalive never fails the stream");
        assert_eq!(acc.text, "legacy");
        assert!(!acc.done);
    }

    #[test]
    fn parse_council_answer_reads_schema_and_clamps_confidence() {
        let raw = "```json\n{\"answer\": \"ship it\", \"confidence\": 4.2, \"evidence\": [\"benchmarked\"]}\n```";
        let parsed = parse_council_answer(raw);
        assert_eq!(parsed.answer, "ship it");
        assert!((parsed.confidence - 1.0).abs() < f32::EPSILON);
        assert_eq!(parsed.evidence, vec!["benchmarked"]);
    }

    #[test]
    fn parse_council_answer_falls_back_to_raw_text() {
        let parsed = parse_council_answer("plain prose answer");
        assert_eq!(parsed.answer, "plain prose answer");
        assert!((parsed.confidence - 0.5).abs() < f32::EPSILON);
        assert!(parsed.evidence.is_empty());
    }

    #[test]
    fn member_builder_sets_name_stream_and_timeout() {
        let member = LocalLlmMember::new("http://192.168.5.103:55000", "qwen3-coder-30b")
            .streaming(true)
            .with_timeout(Duration::from_secs(30));
        assert_eq!(CouncilMember::name(&member), "local:qwen3-coder-30b");
        assert!(member.stream);
        assert_eq!(member.timeout, Duration::from_secs(30));
    }
}
