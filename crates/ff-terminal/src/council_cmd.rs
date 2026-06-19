//! `ff council` — multi-model deliberation (karpathy/llm-council pattern).
//!
//! Dispatch one question to N council members in PARALLEL, collect their
//! independent answers, print them side-by-side, then (v2) have a CHAIRMAN model
//! synthesize them into a single consensus — so `ff council` returns a real
//! answer standalone (when a fleet agent / codex / kimi runs it, not just when a
//! strong model like Claude is driving and can synthesize itself).
//!
//! `--no-synthesis` preserves v1 (print + let the caller synthesize).
//! `--local` fleet members + persisted council_sessions are future increments.
//! Design: `.forgefleet/plans/llm-council.md`.

use crate::{CYAN, GREEN, RESET, YELLOW};
use anyhow::Result;
use std::time::Duration;

const MEMBER_PROMPT_PREAMBLE: &str = "You are a COUNCIL MEMBER. Give your own INDEPENDENT, \
    decisive answer to the question below — your honest best judgment, not a hedge. Be concise \
    and specific; lead with the recommendation, then the key reasoning. Question:\n\n";

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
        anyhow::bail!("no council members (pass --members codex,kimi)");
    }
    let timeout = timeout_secs.map(Duration::from_secs);
    let prompt = format!("{MEMBER_PROMPT_PREAMBLE}{question}");

    eprintln!(
        "{CYAN}▶ Convening council: {} member(s) [{}]{RESET}",
        members.len(),
        members.join(", ")
    );

    // Dispatch to every member in parallel. Each member is a vendor CLI
    // (codex/kimi/claude) wielded headlessly via the shared cli_executor — the
    // same path `ff cli` uses, so the I/O is consistent and logged-capable.
    let mut handles = Vec::with_capacity(members.len());
    for member in &members {
        let member = member.clone();
        let prompt = prompt.clone();
        handles.push(tokio::spawn(async move {
            let res =
                ff_agent::cli_executor::execute_cli_in_dir(&member, &prompt, &[], None, timeout)
                    .await;
            (member, res)
        }));
    }

    // Collect answers (member, answer) for the chairman; print each as it lands.
    let mut answers: Vec<(String, String)> = Vec::with_capacity(members.len());
    for handle in handles {
        let (member, res) = match handle.await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("{YELLOW}⚠ a council member task panicked: {e}{RESET}");
                continue;
            }
        };
        println!("\n{CYAN}═══════════ {member} ═══════════{RESET}");
        match res {
            Ok(r) if r.exit_code == 0 && !r.stdout.trim().is_empty() => {
                let answer = r.stdout.trim().to_string();
                println!("{answer}");
                answers.push((member, answer));
            }
            Ok(r) => {
                eprintln!(
                    "{YELLOW}⚠ {member} returned no usable answer (exit {}){RESET}{}",
                    r.exit_code,
                    if r.stderr.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", r.stderr.trim())
                    }
                );
            }
            Err(e) => eprintln!("{YELLOW}⚠ {member} failed: {e}{RESET}"),
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

    // v2: automated chairman synthesis. Nothing to synthesize from 0 answers, and
    // a lone answer IS the consensus — skip a redundant dispatch.
    if ok == 0 {
        anyhow::bail!("no member answered — nothing to synthesize");
    }
    if ok == 1 {
        println!(
            "\n{GREEN}═══════════ CONSENSUS (sole answer) ═══════════{RESET}\n{}",
            answers[0].1
        );
        return Ok(());
    }

    // Pick the chairman: the requested one, else the first member. The chairman
    // sees the question + every member's answer (labeled) and returns one verdict.
    let chair = chairman.unwrap_or_else(|| members[0].clone());
    let mut synth = format!(
        "You are the CHAIRMAN of an LLM council. {ok} members answered the question below \
         independently. Synthesize their answers into ONE decisive consensus: state the \
         recommendation first, note where members AGREE, and explicitly surface any DISSENT \
         (don't average it away). Be concise.\n\n=== QUESTION ===\n{question}\n"
    );
    for (member, answer) in &answers {
        synth.push_str(&format!("\n=== MEMBER {member} ===\n{answer}\n"));
    }

    eprintln!("\n{CYAN}▶ Chairman ({chair}) synthesizing {ok} answers…{RESET}");
    let res = ff_agent::cli_executor::execute_cli_in_dir(&chair, &synth, &[], None, timeout).await;
    match res {
        Ok(r) if r.exit_code == 0 && !r.stdout.trim().is_empty() => {
            println!(
                "\n{GREEN}═══════════ CONSENSUS (chairman: {chair}) ═══════════{RESET}\n{}",
                r.stdout.trim()
            );
        }
        Ok(r) => {
            eprintln!(
                "{YELLOW}⚠ chairman {chair} produced no synthesis (exit {}) — falling back to the \
                 raw answers above.{RESET}{}",
                r.exit_code,
                if r.stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n{}", r.stderr.trim())
                }
            );
        }
        Err(e) => {
            eprintln!(
                "{YELLOW}⚠ chairman {chair} dispatch failed: {e} — falling back to the raw \
                 answers above.{RESET}"
            );
        }
    }
    Ok(())
}
