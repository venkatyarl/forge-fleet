//! `ff council` — multi-model deliberation (karpathy/llm-council pattern).
//!
//! Dispatch one question to N council members in PARALLEL, collect their
//! independent answers, print them side-by-side, then a CHAIRMAN model
//! synthesizes them into a single consensus. Every dispatch is logged to
//! `ff_interactions` (audit + training data).
//!
//! A member is either a VENDOR CLI (codex/kimi/claude — via cli_executor) or a
//! LOCAL FLEET model (`local` / `local:<model>` — via fleet_oneshot), so one
//! roster can mix cloud + local tiers: `--members codex,local:qwen36-35b,kimi`.
//! `--no-synthesis` preserves the v1 print-only behavior. Design:
//! `.forgefleet/plans/llm-council.md`.

use crate::{CYAN, GREEN, RESET, YELLOW};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::Duration;

const MEMBER_PROMPT_PREAMBLE: &str = "You are a COUNCIL MEMBER. Give your own INDEPENDENT, \
    decisive answer to the question below — your honest best judgment, not a hedge. Be concise \
    and specific; lead with the recommendation, then the key reasoning.\n\n\
    Return ONLY a JSON object with this exact schema so the chairman can weigh your input:\n\
    {\"answer\": <string>, \"confidence\": <float 0.0-1.0>, \"evidence\": [<string>, …], \
    \"citations\": [<string>, …]}\n\
    `confidence` is your self-assessed certainty in [0.0, 1.0]; `evidence` lists the facts \
    backing the answer; `citations` lists sources (URLs, files, refs). Question:\n\n";

/// Default self-assessed certainty when a member omits `confidence` or returns
/// prose instead of the schema — neutral, so it neither dominates nor is ignored.
fn default_confidence() -> f64 {
    0.5
}

/// Structured council response schema shared by ALL member types (vendor CLI and
/// local fleet models). Members are asked to return this JSON; the chairman uses
/// `confidence`/`evidence`/`citations` to weigh each input instead of treating
/// every answer as equally certain. Serde `default`s make every field but
/// `answer` optional so a schema-ignoring member still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilResponse {
    /// The member's actual recommendation/answer text.
    pub answer: String,
    /// Self-assessed certainty in `[0.0, 1.0]`.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// Facts/reasoning the member offers in support of `answer`.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Sources backing the answer (URLs, file paths, references).
    #[serde(default)]
    pub citations: Vec<String>,
}

impl CouncilResponse {
    /// Coerce a parsed response into a safe, weighable shape: clamp `confidence`
    /// to `[0.0, 1.0]` (mapping non-finite values to the neutral default) and
    /// drop blank evidence/citation entries.
    fn normalize(&mut self) {
        if !self.confidence.is_finite() {
            self.confidence = default_confidence();
        }
        self.confidence = self.confidence.clamp(0.0, 1.0);
        self.evidence.retain(|s| !s.trim().is_empty());
        self.citations.retain(|s| !s.trim().is_empty());
    }

    /// Serialization validation: reject a response that can't be safely persisted
    /// or weighed (empty answer, or a `confidence` that is non-finite or outside
    /// `[0.0, 1.0]`). Callers fall back to plain-text handling on `Err`.
    fn validate(&self) -> Result<()> {
        if self.answer.trim().is_empty() {
            anyhow::bail!("council response has an empty answer");
        }
        if !self.confidence.is_finite() {
            anyhow::bail!(
                "confidence must be a finite number, got {}",
                self.confidence
            );
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            anyhow::bail!("confidence {} out of range [0.0, 1.0]", self.confidence);
        }
        Ok(())
    }
}

/// Parse a member's raw output into the shared [`CouncilResponse`] schema. Tries
/// the structured JSON first (members are prompted for it); on any miss — no JSON,
/// malformed JSON, or a response that fails [`CouncilResponse::validate`] — falls
/// back to treating the whole text as the `answer` with a neutral confidence, so
/// EVERY member type yields a uniform, chairman-weighable response.
fn parse_council_response(text: &str) -> CouncilResponse {
    if let Some(json) = extract_json_object(text) {
        if let Ok(mut resp) = serde_json::from_str::<CouncilResponse>(&json) {
            resp.normalize();
            if resp.validate().is_ok() {
                return resp;
            }
        }
    }
    CouncilResponse {
        answer: text.trim().to_string(),
        confidence: default_confidence(),
        evidence: Vec::new(),
        citations: Vec::new(),
    }
}

/// Extract the first balanced-looking JSON object from free text (a member may
/// wrap the schema in prose or a ```json fence). Spans the first `{` to the last
/// `}`; `serde_json` rejects it if that span isn't valid JSON.
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| text[start..=end].to_string())
}

/// Normalized result of one member dispatch (vendor CLI or local fleet model),
/// holding everything the council needs to print AND to log to `ff_interactions`.
struct MemberRaw {
    /// `Some(text)` when the member produced a usable answer.
    answer: Option<String>,
    /// Human-facing failure reason when `answer` is `None`.
    error: Option<String>,
    latency_ms: Option<i32>,
    /// What served the call: `ff council/<member>` for a vendor CLI, or the real
    /// `http://host:port (model)` for a local fleet member.
    endpoint: Option<String>,
    /// The fleet computer that answered (local members only).
    worker_name: Option<String>,
    /// Prompt/completion tokens for this dispatch. Populated for local fleet
    /// members (the endpoint returns a `usage` block) and for vendor CLIs
    /// that echo usage in their output; `0` when nothing was reported.
    tokens_in: i32,
    tokens_out: i32,
    /// Canonical engine that answered (`local:<catalog_id>` for fleet models);
    /// `None` falls back to the member name (the vendor CLI case).
    engine: Option<String>,
}

impl MemberRaw {
    fn fail(msg: impl Into<String>) -> Self {
        Self {
            answer: None,
            error: Some(msg.into()),
            latency_ms: None,
            endpoint: None,
            worker_name: None,
            tokens_in: 0,
            tokens_out: 0,
            engine: None,
        }
    }
}

pub async fn handle_council(
    question: String,
    members_csv: String,
    timeout_secs: Option<u64>,
    chairman: Option<String>,
    no_synthesis: bool,
) -> Result<()> {
    let members: Vec<String> = members_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if members.is_empty() {
        anyhow::bail!("no council members (pass --members codex,kimi,local:<model>)");
    }
    let timeout = timeout_secs.map(Duration::from_secs);
    let prompt = format!("{MEMBER_PROMPT_PREAMBLE}{question}");

    // Best-effort pool: logs every dispatch to ff_interactions AND lets `local:`
    // members route to a fleet deployment. A missing pool never blocks a council
    // (vendor members still run; local members fail gracefully with a clear msg).
    let pool: Option<PgPool> = ff_agent::fleet_info::get_fleet_pool().await.ok();

    eprintln!(
        "{CYAN}▶ Convening council: {} member(s) [{}]{RESET}",
        members.len(),
        members.join(", ")
    );

    // Dispatch every member in parallel (vendor CLI or local fleet model).
    let mut handles = Vec::with_capacity(members.len());
    for member in &members {
        let member = member.clone();
        let prompt = prompt.clone();
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let raw = dispatch_member(&member, &prompt, pool.as_ref(), timeout).await;
            (member, raw)
        }));
    }

    // Collect answers for the chairman; print + log each. Every member type's
    // raw output is normalized into the shared CouncilResponse schema so the
    // chairman can weigh confidence/evidence/citations uniformly.
    let mut answers: Vec<(String, CouncilResponse)> = Vec::with_capacity(members.len());
    for handle in handles {
        let (member, raw) = match handle.await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("{YELLOW}⚠ a council member task panicked: {e}{RESET}");
                continue;
            }
        };
        log_council(pool.as_ref(), &member, "council_member", &prompt, &raw).await;
        println!("\n{CYAN}═══════════ {member} ═══════════{RESET}");
        match raw.answer {
            Some(answer) => {
                let resp = parse_council_response(&answer);
                println!("{}", resp.answer);
                eprintln!(
                    "{CYAN}  ↳ confidence {:.2}, {} evidence, {} citation(s){RESET}",
                    resp.confidence,
                    resp.evidence.len(),
                    resp.citations.len()
                );
                answers.push((member, resp));
            }
            None => eprintln!(
                "{YELLOW}⚠ {member} returned no usable answer{RESET}{}",
                raw.error.map(|e| format!("\n{e}")).unwrap_or_default()
            ),
        }
    }

    let ok = answers.len();
    eprintln!(
        "\n{GREEN}✓ {ok}/{} member(s) answered.{RESET}",
        members.len()
    );

    // v1 behavior: print + let the caller synthesize.
    if no_synthesis {
        println!(
            "\n{CYAN}Synthesize the answers above into a single consensus (note agreements, \
             surface dissent) — the chairman is the strong model that convened this council.{RESET}"
        );
        return Ok(());
    }

    // Automated chairman synthesis. Nothing to synthesize from 0 answers, and a
    // lone answer IS the consensus — skip a redundant dispatch.
    if ok == 0 {
        anyhow::bail!("no member answered — nothing to synthesize");
    }
    if ok == 1 {
        println!(
            "\n{GREEN}═══════════ CONSENSUS (sole answer) ═══════════{RESET}\n{}",
            answers[0].1.answer
        );
        return Ok(());
    }

    // Pick the chairman: the requested one, else the first member (vendor or
    // local — dispatch_member handles both). It sees the question + every
    // labeled answer and returns one verdict.
    let chair = chairman.unwrap_or_else(|| members[0].clone());
    let mut synth = format!(
        "You are the CHAIRMAN of an LLM council. {ok} members answered the question below \
         independently. Each member reports a `confidence` in [0.0, 1.0] plus supporting \
         `evidence` and `citations` — WEIGH higher-confidence, better-evidenced answers more \
         heavily. Synthesize into ONE decisive consensus: state the recommendation first, note \
         where members AGREE, and explicitly surface any DISSENT (don't average it away). Be \
         concise.\n\n=== QUESTION ===\n{question}\n"
    );
    for (member, resp) in &answers {
        synth.push_str(&format!(
            "\n=== MEMBER {member} (confidence {:.2}) ===\n{}\n",
            resp.confidence, resp.answer
        ));
        if !resp.evidence.is_empty() {
            synth.push_str(&format!("evidence: {}\n", resp.evidence.join("; ")));
        }
        if !resp.citations.is_empty() {
            synth.push_str(&format!("citations: {}\n", resp.citations.join("; ")));
        }
    }

    eprintln!("\n{CYAN}▶ Chairman ({chair}) synthesizing {ok} answers…{RESET}");
    let raw = dispatch_member(&chair, &synth, pool.as_ref(), timeout).await;
    log_council(pool.as_ref(), &chair, "council_chairman", &synth, &raw).await;
    match raw.answer {
        Some(consensus) => println!(
            "\n{GREEN}═══════════ CONSENSUS (chairman: {chair}) ═══════════{RESET}\n{consensus}"
        ),
        None => eprintln!(
            "{YELLOW}⚠ chairman {chair} produced no synthesis — falling back to the raw answers \
             above.{RESET}{}",
            raw.error.map(|e| format!("\n{e}")).unwrap_or_default()
        ),
    }
    Ok(())
}

/// Dispatch one member: a `local`/`local:<model>` fleet model via fleet_oneshot,
/// or a vendor CLI via cli_executor. Normalizes both into a [`MemberRaw`].
async fn dispatch_member(
    member: &str,
    prompt: &str,
    pool: Option<&PgPool>,
    timeout: Option<Duration>,
) -> MemberRaw {
    // Local fleet member: `local` (any healthy model) or `local:<model>` (biased).
    if member == "local" || member.starts_with("local:") {
        let model_hint = member.strip_prefix("local:").filter(|s| !s.is_empty());
        let Some(pool) = pool else {
            return MemberRaw::fail(
                "local council member needs the fleet DB (pool unavailable) — skipping",
            );
        };
        return match ff_agent::fleet_oneshot::fleet_oneshot(pool, prompt, model_hint, timeout).await
        {
            Ok(o) => MemberRaw {
                answer: Some(o.text),
                error: None,
                latency_ms: i32::try_from(o.latency_ms).ok(),
                endpoint: Some(format!("{} ({})", o.endpoint, o.model)),
                worker_name: Some(o.worker_name),
                tokens_in: o.tokens_in,
                tokens_out: o.tokens_out,
                engine: Some(ff_agent::llm_attribution::engine_label(&o.model)),
            },
            Err(e) => MemberRaw::fail(e.to_string()),
        };
    }

    // Vendor CLI member.
    match ff_agent::cli_executor::execute_cli_in_dir(member, prompt, &[], None, timeout).await {
        Ok(r) if r.exit_code == 0 && !r.stdout.trim().is_empty() => {
            // Vendor CLIs sometimes echo usage (JSON keys or a "tokens used"
            // line) on stdout/stderr — scrape what's there.
            let (tokens_in, tokens_out) = ff_agent::llm_attribution::parse_cli_token_counts(
                &format!("{}\n{}", r.stdout, r.stderr),
            );
            MemberRaw {
                answer: Some(r.stdout.trim().to_string()),
                error: None,
                latency_ms: i32::try_from(r.duration_ms).ok(),
                endpoint: Some(format!("ff council/{member}")),
                worker_name: None,
                tokens_in,
                tokens_out,
                engine: None,
            }
        }
        Ok(r) => MemberRaw {
            answer: None,
            error: Some(format!(
                "exit {}{}",
                r.exit_code,
                if r.stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", r.stderr.trim())
                }
            )),
            latency_ms: i32::try_from(r.duration_ms).ok(),
            endpoint: Some(format!("ff council/{member}")),
            worker_name: None,
            tokens_in: 0,
            tokens_out: 0,
            engine: None,
        },
        Err(e) => MemberRaw::fail(e.to_string()),
    }
}

/// Record one council dispatch (a member answer or the chairman synthesis) in
/// `ff_interactions`. Best-effort: a log failure never affects the council.
/// `channel` distinguishes `council_member` from `council_chairman`.
async fn log_council(
    pool: Option<&PgPool>,
    member: &str,
    channel: &str,
    request: &str,
    raw: &MemberRaw,
) {
    let Some(pool) = pool else { return };
    let (response_text, outcome, error_text) = match &raw.answer {
        Some(a) => (
            a.chars().take(16000).collect::<String>(),
            "success".to_string(),
            None,
        ),
        None => (
            String::new(),
            "error".to_string(),
            raw.error
                .as_ref()
                .map(|e| e.chars().take(2000).collect::<String>()),
        ),
    };
    // Canonical engine: the fleet model that actually answered when known
    // (local members), else the member's vendor CLI name (claude/codex/kimi).
    // Estimate missing token counts (chars/4, flagged) on successful answers
    // and price cloud engines from the config rates table — local is $0.
    let engine = raw
        .engine
        .clone()
        .unwrap_or_else(|| ff_agent::llm_attribution::engine_label(member));
    let (tokens_in, tokens_out, tokens_estimated) = if raw.answer.is_some() {
        ff_agent::llm_attribution::tokens_or_estimate(
            raw.tokens_in,
            raw.tokens_out,
            request,
            raw.answer.as_deref().unwrap_or_default(),
        )
    } else {
        (raw.tokens_in, raw.tokens_out, false)
    };
    let cost_usd = ff_agent::llm_attribution::cost_usd(&engine, tokens_in, tokens_out);
    let rec = ff_db::InteractionRecord {
        channel: channel.to_string(),
        request_text: request.chars().take(16000).collect(),
        request_meta: serde_json::json!({ "tokens_estimated": tokens_estimated }),
        engine: Some(engine),
        response_text,
        latency_ms: raw.latency_ms,
        tokens_in,
        tokens_out,
        cost_usd,
        outcome,
        error_text,
        endpoint: raw
            .endpoint
            .clone()
            .or_else(|| Some(format!("ff council/{member}"))),
        worker_name: raw.worker_name.clone(),
        ..Default::default()
    };
    if let Err(e) = ff_db::pg_record_interaction(pool, &rec).await {
        eprintln!("{YELLOW}⚠ council: failed to log interaction (non-fatal): {e}{RESET}");
    }
}

#[cfg(test)]
mod tests {
    use super::{CouncilResponse, parse_council_response};

    #[test]
    fn parses_full_structured_response() {
        let text = r#"Sure — here is my verdict:
        {"answer": "Use Postgres", "confidence": 0.9,
         "evidence": ["ACID guarantees", "mature ecosystem"],
         "citations": ["https://postgresql.org/docs"]}"#;
        let r = parse_council_response(text);
        assert_eq!(r.answer, "Use Postgres");
        assert!((r.confidence - 0.9).abs() < 1e-9);
        assert_eq!(r.evidence, vec!["ACID guarantees", "mature ecosystem"]);
        assert_eq!(r.citations, vec!["https://postgresql.org/docs"]);
        assert!(r.validate().is_ok());
    }

    #[test]
    fn missing_optional_fields_get_defaults() {
        // A member that returns only `answer` still parses; confidence defaults
        // to the neutral value and the lists default to empty.
        let r = parse_council_response(r#"{"answer": "Ship it"}"#);
        assert_eq!(r.answer, "Ship it");
        assert!((r.confidence - 0.5).abs() < 1e-9);
        assert!(r.evidence.is_empty());
        assert!(r.citations.is_empty());
    }

    #[test]
    fn plain_prose_falls_back_to_answer() {
        // No JSON at all: the whole text becomes the answer with neutral confidence.
        let r = parse_council_response("Just go with option B, it's simpler.");
        assert_eq!(r.answer, "Just go with option B, it's simpler.");
        assert!((r.confidence - 0.5).abs() < 1e-9);
        assert!(r.evidence.is_empty() && r.citations.is_empty());
    }

    #[test]
    fn out_of_range_confidence_is_clamped_and_blanks_dropped() {
        let r = parse_council_response(
            r#"{"answer": "x", "confidence": 5.0, "evidence": ["real", "  "], "citations": [""]}"#,
        );
        assert!((r.confidence - 1.0).abs() < 1e-9);
        assert_eq!(r.evidence, vec!["real"]);
        assert!(r.citations.is_empty());
        assert!(r.validate().is_ok());
    }

    #[test]
    fn non_finite_confidence_normalizes_to_default() {
        // NaN/Inf can't be weighed — normalize maps them back to the neutral default.
        let mut r = CouncilResponse {
            answer: "x".into(),
            confidence: f64::NAN,
            evidence: vec![],
            citations: vec![],
        };
        r.normalize();
        assert!((r.confidence - 0.5).abs() < 1e-9);
        assert!(r.validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_fields() {
        let bad_conf = CouncilResponse {
            answer: "x".into(),
            confidence: 2.0,
            evidence: vec![],
            citations: vec![],
        };
        assert!(bad_conf.validate().is_err());
        let empty_answer = CouncilResponse {
            answer: "   ".into(),
            confidence: 0.5,
            evidence: vec![],
            citations: vec![],
        };
        assert!(empty_answer.validate().is_err());
    }

    #[test]
    fn schema_serde_round_trips() {
        let r = CouncilResponse {
            answer: "a".into(),
            confidence: 0.7,
            evidence: vec!["e".into()],
            citations: vec!["c".into()],
        };
        let s = serde_json::to_string(&r).expect("serialize");
        let back: CouncilResponse = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.answer, "a");
        assert!((back.confidence - 0.7).abs() < 1e-9);
        assert_eq!(back.evidence, vec!["e".to_string()]);
        assert_eq!(back.citations, vec!["c".to_string()]);
    }
}
