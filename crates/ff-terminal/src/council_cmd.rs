//! `ff council` — multi-model deliberation (karpathy/llm-council pattern).
//!
//! Dispatch one question to N council members in PARALLEL, collect their
//! independent structured answers (each with a self-reported `confidence` and
//! `evidence`), print them side-by-side, then a CHAIRMAN model synthesizes
//! them into a structured verdict — `{consensus, disagreements,
//! unique_findings, rationale}` — weighing higher-confidence members more
//! heavily and citing which member(s) hold which position instead of
//! blending disagreement away. Every dispatch is logged to `ff_interactions`
//! (audit + training data).
//!
//! A member is either a VENDOR CLI (codex/kimi/claude — via cli_executor), a
//! LOCAL FLEET model (`local` / `local:<model>` — via fleet_oneshot), or an
//! explicit fleet ENDPOINT (`http://host:port[#model]` — via LocalLlmMember,
//! streamed), so one roster can mix cloud + local tiers:
//! `--members codex,local:qwen36-35b,kimi`.
//! `--no-synthesis` preserves the v1 print-only behavior. Design:
//! `.forgefleet/plans/llm-council.md`.

use crate::{CYAN, GREEN, RESET, YELLOW};
use anyhow::Result;
use sqlx::PgPool;
use std::time::Duration;

const MEMBER_PROMPT_PREAMBLE: &str = "Rule 1: The operator (Venkat) is the final authority. \
    When the operator asks for something, your job is to find a way to make it happen — surface \
    risks honestly, propose alternatives if needed, but never silently drop, water down, or \
    route around an operator directive. If truly impossible, say so explicitly with the reason \
    and the closest achievable path. As a council member you may argue tradeoffs, but the \
    operator decision, once stated, is a constraint, not a suggestion: recommendations must \
    include a path that achieves the operator ask.\n\n\
    You are a COUNCIL MEMBER. Give your own INDEPENDENT, \
    decisive answer to the question below — your honest best judgment, not a hedge. Be concise \
    and specific; lead with the recommendation, then the key reasoning. Respond with ONLY a JSON \
    object (no markdown fences, no prose outside it) with exactly these keys: `answer` (string — \
    your recommendation and reasoning), `confidence` (number 0.0-1.0 — your honest confidence in \
    this answer), `evidence` (array of strings — the concrete facts/sources backing it). \
    Question:\n\n";

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

fn default_confidence() -> f32 {
    0.5
}

/// A council member's structured answer: the recommendation plus a
/// self-reported confidence and supporting evidence, so the chairman can weigh
/// contributions instead of blending every answer as equally reliable.
#[derive(Debug, Clone, serde::Deserialize)]
struct MemberAnswer {
    #[serde(alias = "response", alias = "text")]
    answer: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    evidence: Vec<String>,
}

/// Extracts the first balanced `{...}` object from `s`. Members are asked for
/// raw JSON but vendor CLIs and local models often wrap it in prose or a
/// fenced code block anyway — grabbing the outermost braces tolerates that.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end >= start).then(|| &s[start..=end])
}

/// Parses one member's raw response into a structured [`MemberAnswer`],
/// falling back to the raw text with a neutral confidence and no cited
/// evidence when the member didn't return valid JSON — a malformed member
/// reply should never fail the council.
fn parse_member_answer(raw: &str) -> MemberAnswer {
    if let Some(json) = extract_json_object(raw) {
        if let Ok(mut parsed) = serde_json::from_str::<MemberAnswer>(json) {
            if !parsed.answer.trim().is_empty() {
                parsed.confidence = parsed.confidence.clamp(0.0, 1.0);
                return parsed;
            }
        }
    }
    MemberAnswer {
        answer: raw.trim().to_string(),
        confidence: default_confidence(),
        evidence: Vec::new(),
    }
}

/// The chairman's structured verdict: a decisive consensus plus explicit,
/// attributable disagreements and unique findings rather than a blended
/// summary.
#[derive(Debug, Clone, serde::Deserialize)]
struct ChairmanSynthesis {
    consensus: String,
    #[serde(default)]
    disagreements: Vec<String>,
    #[serde(default)]
    unique_findings: Vec<String>,
    #[serde(default)]
    rationale: String,
}

/// Parses the chairman's raw response into a [`ChairmanSynthesis`]. Returns
/// `None` when the chairman didn't produce usable structured JSON, so the
/// caller can fall back to printing the raw text.
fn parse_chairman_synthesis(raw: &str) -> Option<ChairmanSynthesis> {
    let json = extract_json_object(raw)?;
    let parsed: ChairmanSynthesis = serde_json::from_str(json).ok()?;
    (!parsed.consensus.trim().is_empty()).then_some(parsed)
}

/// Builds the chairman's synthesis prompt: members ranked highest-confidence
/// first, each labeled with its confidence and evidence, and an explicit
/// instruction to weigh high-confidence contributions more heavily and name
/// which member(s) hold which position on any disagreement rather than
/// averaging it away. Requires a structured JSON verdict so disagreements and
/// unique findings survive as discrete, attributable items.
fn build_chairman_prompt(question: &str, answers: &[(String, MemberAnswer)]) -> String {
    let mut ranked: Vec<&(String, MemberAnswer)> = answers.iter().collect();
    ranked.sort_by(|a, b| {
        b.1.confidence
            .partial_cmp(&a.1.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut prompt = format!(
        "You are the CHAIRMAN of an LLM council. {} members answered the question below \
         independently, each with a self-reported CONFIDENCE (0.0-1.0) and supporting EVIDENCE \
         below. Weigh higher-confidence contributions more heavily when forming the consensus — \
         do not treat every answer as equally reliable. Do NOT blend disagreements into a mushy \
         average: when members conflict, name which member(s) hold which position. Respond with \
         ONLY a JSON object (no markdown fences, no prose outside it) with exactly these keys: \
         `consensus` (string — the decisive recommendation), `disagreements` (array of strings, \
         each naming the members in conflict and what they disagree on), `unique_findings` \
         (array of strings — points raised by only one member worth preserving), `rationale` \
         (string — why the consensus weighs the evidence the way it does).\n\n\
         === QUESTION ===\n{question}\n",
        ranked.len()
    );
    for (member, ans) in ranked {
        prompt.push_str(&format!(
            "\n=== MEMBER {member} (confidence: {:.2}) ===\n{}\n",
            ans.confidence, ans.answer
        ));
        if ans.evidence.is_empty() {
            prompt.push_str("Evidence: (none cited)\n");
        } else {
            prompt.push_str("Evidence:\n");
            for e in &ans.evidence {
                prompt.push_str(&format!("- {e}\n"));
            }
        }
    }
    prompt
}

/// Greetings and pleasantries that carry no room for members to disagree on.
const TRIVIAL_GREETINGS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "hola",
    "yo",
    "sup",
    "howdy",
    "good morning",
    "good afternoon",
    "good evening",
    "good night",
    "thanks",
    "thank you",
    "ty",
    "bye",
    "goodbye",
    "see ya",
];

/// Well-known constants/facts that don't need multi-model deliberation to answer.
const TRIVIAL_KNOWN_CONSTANTS: &[&str] = &[
    "value of pi",
    "speed of light",
    "boiling point of water",
    "freezing point of water",
    "avogadro's number",
    "avogadro number",
    "gravitational constant",
    "planck's constant",
    "planck constant",
];

/// Heuristically detects trivial prompts (greetings, single-word queries,
/// well-known constants) that don't warrant convening the full council.
/// Used by [`handle_council`] to answer directly from one member instead of
/// dispatching every member + a chairman synthesis, saving compute.
pub fn should_skip_council(prompt: &str) -> bool {
    let normalized = prompt
        .trim()
        .trim_end_matches(['?', '!', '.'])
        .to_lowercase();
    if normalized.is_empty() {
        return true;
    }
    if TRIVIAL_GREETINGS.contains(&normalized.as_str()) {
        return true;
    }
    if normalized.split_whitespace().count() <= 1 {
        return true;
    }
    TRIVIAL_KNOWN_CONSTANTS
        .iter()
        .any(|c| normalized.contains(c))
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

    // Best-effort pool: logs every dispatch to ff_interactions AND lets `local:`
    // members route to a fleet deployment. A missing pool never blocks a council
    // (vendor members still run; local members fail gracefully with a clear msg).
    let pool: Option<PgPool> = ff_agent::fleet_info::get_fleet_pool().await.ok();

    // Trivial prompts (greetings, single-word queries, known constants) don't
    // need N members + a chairman synthesis — answer directly from one member.
    if should_skip_council(&question) {
        let member = chairman.unwrap_or_else(|| members[0].clone());
        eprintln!(
            "{CYAN}▶ Trivial prompt detected — answering directly via {member}, skipping full \
             council{RESET}"
        );
        let raw = dispatch_member(&member, &question, pool.as_ref(), timeout).await;
        log_council(pool.as_ref(), &member, "council_trivial", &question, &raw).await;
        return match raw.answer {
            Some(answer) => {
                println!(
                    "{GREEN}═══════════ DIRECT ANSWER ({member}) ═══════════{RESET}\n{answer}"
                );
                Ok(())
            }
            None => anyhow::bail!(
                "trivial prompt but {member} returned no usable answer{}",
                raw.error.map(|e| format!(": {e}")).unwrap_or_default()
            ),
        };
    }

    let prompt = format!("{MEMBER_PROMPT_PREAMBLE}{question}");

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

    // Collect structured (member, MemberAnswer) pairs for the chairman; print
    // + log each. Members are asked for JSON (answer/confidence/evidence);
    // `parse_member_answer` falls back gracefully when one doesn't comply.
    let mut answers: Vec<(String, MemberAnswer)> = Vec::with_capacity(members.len());
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
                let parsed = parse_member_answer(&answer);
                println!(
                    "{}\n{CYAN}(confidence: {:.2}){RESET}",
                    parsed.answer, parsed.confidence
                );
                answers.push((member, parsed));
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
    // labeled, confidence/evidence-tagged answer and returns one structured
    // verdict, weighing higher-confidence members more heavily instead of
    // blending every answer as equally reliable.
    let chair = chairman.unwrap_or_else(|| members[0].clone());
    let synth = build_chairman_prompt(&question, &answers);

    eprintln!("\n{CYAN}▶ Chairman ({chair}) synthesizing {ok} answers…{RESET}");
    let raw = dispatch_member(&chair, &synth, pool.as_ref(), timeout).await;
    log_council(pool.as_ref(), &chair, "council_chairman", &synth, &raw).await;
    match raw.answer.as_deref().and_then(parse_chairman_synthesis) {
        Some(synthesis) => {
            println!("\n{GREEN}═══════════ CONSENSUS (chairman: {chair}) ═══════════{RESET}");
            println!("{}", synthesis.consensus);
            if !synthesis.disagreements.is_empty() {
                println!("\n{YELLOW}─── Disagreements ───{RESET}");
                for d in &synthesis.disagreements {
                    println!("- {d}");
                }
            }
            if !synthesis.unique_findings.is_empty() {
                println!("\n{CYAN}─── Unique findings ───{RESET}");
                for f in &synthesis.unique_findings {
                    println!("- {f}");
                }
            }
            if !synthesis.rationale.trim().is_empty() {
                println!("\n{CYAN}─── Rationale ───{RESET}\n{}", synthesis.rationale);
            }
        }
        None => match raw.answer {
            Some(unstructured) => println!(
                "\n{YELLOW}⚠ chairman {chair} did not return structured JSON — printing raw \
                 synthesis.{RESET}\n{unstructured}"
            ),
            None => eprintln!(
                "{YELLOW}⚠ chairman {chair} produced no synthesis — falling back to the raw \
                 answers above.{RESET}{}",
                raw.error.map(|e| format!("\n{e}")).unwrap_or_default()
            ),
        },
    }
    Ok(())
}

/// Dispatch one member: an explicit `http(s)://host:port[#model]` fleet
/// endpoint via `LocalLlmMember` (streaming, no DB routing), a
/// `local`/`local:<model>` fleet model via fleet_oneshot, or a vendor CLI via
/// cli_executor. Normalizes all three into a [`MemberRaw`].
async fn dispatch_member(
    member: &str,
    prompt: &str,
    pool: Option<&PgPool>,
    timeout: Option<Duration>,
) -> MemberRaw {
    // Direct-endpoint fleet member: council against one explicit deployment.
    // The response is already parsed to the council schema; re-serialize it so
    // `parse_member_answer` round-trips confidence/evidence unchanged.
    if member.starts_with("http://") || member.starts_with("https://") {
        let (endpoint, model) = match member.split_once('#') {
            Some((endpoint, model)) if !model.is_empty() => (endpoint, model),
            _ => (member, "local"),
        };
        let mut llm =
            ff_agent::local_llm_member::LocalLlmMember::new(endpoint, model).streaming(true);
        if let Some(timeout) = timeout {
            llm = llm.with_timeout(timeout);
        }
        return match llm.respond(prompt).await {
            Ok(r) => MemberRaw {
                answer: Some(
                    serde_json::json!({
                        "answer": r.answer,
                        "confidence": r.confidence,
                        "evidence": r.evidence,
                    })
                    .to_string(),
                ),
                error: None,
                latency_ms: i32::try_from(r.latency_ms).ok(),
                endpoint: Some(format!("{} ({})", r.endpoint, r.model)),
                worker_name: None,
                tokens_in: r.tokens_in,
                tokens_out: r.tokens_out,
                engine: Some(ff_agent::llm_attribution::engine_label(&r.model)),
            },
            Err(e) => MemberRaw::fail(e.to_string()),
        };
    }

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
        purpose: Some("council".to_string()),
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
    use ff_agent::system_prompt::RULE_1_OPERATOR_PRIMACY;

    use super::{
        MEMBER_PROMPT_PREAMBLE, MemberAnswer, build_chairman_prompt, parse_chairman_synthesis,
        parse_member_answer, should_skip_council,
    };

    #[test]
    fn council_charter_leads_with_rule_1_operator_primacy() {
        assert!(MEMBER_PROMPT_PREAMBLE.starts_with(RULE_1_OPERATOR_PRIMACY));
        assert!(
            MEMBER_PROMPT_PREAMBLE
                .contains("operator decision, once stated, is a constraint, not a suggestion")
        );
    }

    #[test]
    fn parse_member_answer_reads_confidence_and_evidence() {
        let raw = r#"{"answer": "use FOR UPDATE SKIP LOCKED", "confidence": 0.9, "evidence": ["avoids lock contention", "already used in the defer queue"]}"#;
        let parsed = parse_member_answer(raw);
        assert_eq!(parsed.answer, "use FOR UPDATE SKIP LOCKED");
        assert!((parsed.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(parsed.evidence.len(), 2);
    }

    #[test]
    fn parse_member_answer_tolerates_markdown_fences() {
        let raw = "Sure, here you go:\n```json\n{\"answer\": \"yes\", \"confidence\": 0.75, \"evidence\": []}\n```";
        let parsed = parse_member_answer(raw);
        assert_eq!(parsed.answer, "yes");
        assert!((parsed.confidence - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_member_answer_clamps_out_of_range_confidence() {
        let raw = r#"{"answer": "x", "confidence": 4.2, "evidence": []}"#;
        assert!((parse_member_answer(raw).confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_member_answer_falls_back_to_raw_text() {
        let raw = "not json at all, just a plain answer";
        let parsed = parse_member_answer(raw);
        assert_eq!(parsed.answer, raw);
        assert!((parsed.confidence - 0.5).abs() < f32::EPSILON);
        assert!(parsed.evidence.is_empty());
    }

    #[test]
    fn parse_chairman_synthesis_reads_all_fields() {
        let raw = r#"{"consensus": "ship it", "disagreements": ["codex vs kimi on rollback plan"], "unique_findings": ["kimi flagged a missing index"], "rationale": "codex had the highest confidence"}"#;
        let synth = parse_chairman_synthesis(raw).expect("should parse");
        assert_eq!(synth.consensus, "ship it");
        assert_eq!(synth.disagreements, vec!["codex vs kimi on rollback plan"]);
        assert_eq!(synth.unique_findings, vec!["kimi flagged a missing index"]);
        assert_eq!(synth.rationale, "codex had the highest confidence");
    }

    #[test]
    fn parse_chairman_synthesis_rejects_empty_consensus() {
        let raw =
            r#"{"consensus": "", "disagreements": [], "unique_findings": [], "rationale": ""}"#;
        assert!(parse_chairman_synthesis(raw).is_none());
    }

    #[test]
    fn parse_chairman_synthesis_rejects_non_json() {
        assert!(parse_chairman_synthesis("just prose, no json here").is_none());
    }

    #[test]
    fn build_chairman_prompt_ranks_by_confidence_descending() {
        let answers = vec![
            (
                "kimi".to_string(),
                MemberAnswer {
                    answer: "low confidence take".to_string(),
                    confidence: 0.2,
                    evidence: vec![],
                },
            ),
            (
                "codex".to_string(),
                MemberAnswer {
                    answer: "high confidence take".to_string(),
                    confidence: 0.95,
                    evidence: vec!["benchmarked it".to_string()],
                },
            ),
        ];
        let prompt = build_chairman_prompt("should we do X?", &answers);
        let codex_pos = prompt.find("MEMBER codex").unwrap();
        let kimi_pos = prompt.find("MEMBER kimi").unwrap();
        assert!(
            codex_pos < kimi_pos,
            "higher-confidence member should be listed first"
        );
        assert!(prompt.to_lowercase().contains("weigh higher-confidence"));
        assert!(prompt.contains("benchmarked it"));
    }

    #[test]
    fn greetings_are_trivial() {
        assert!(should_skip_council("hi"));
        assert!(should_skip_council("Hello!"));
        assert!(should_skip_council("  good morning  "));
        assert!(should_skip_council("thanks"));
    }

    #[test]
    fn single_word_queries_are_trivial() {
        assert!(should_skip_council("recursion"));
        assert!(should_skip_council("kubernetes?"));
    }

    #[test]
    fn known_constants_are_trivial() {
        assert!(should_skip_council("what is the value of pi?"));
        assert!(should_skip_council(
            "What's the speed of light in a vacuum?"
        ));
    }

    #[test]
    fn empty_prompt_is_trivial() {
        assert!(should_skip_council(""));
        assert!(should_skip_council("   "));
    }

    #[test]
    fn substantive_questions_are_not_trivial() {
        assert!(!should_skip_council(
            "Should we migrate the scheduler to use FOR UPDATE SKIP LOCKED?"
        ));
        assert!(!should_skip_council(
            "What's the tradeoff between eager and lazy loading for this cache?"
        ));
    }
}
