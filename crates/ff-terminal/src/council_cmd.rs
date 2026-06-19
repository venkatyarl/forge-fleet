//! `ff council` — multi-model deliberation (karpathy/llm-council pattern).
//!
//! Formalizes the consensus mechanism run by hand all session: dispatch one
//! question to N council members in PARALLEL, collect their independent answers,
//! and print them side-by-side so a strong "chairman" model can synthesize.
//!
//! v1 = dispatch + collect + side-by-side print (the caller synthesizes).
//! Cross-review/ranking + automated chairman synthesis are v2.
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

    let mut ok = 0usize;
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
                println!("{}", r.stdout.trim());
                ok += 1;
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

    println!(
        "\n{GREEN}✓ {ok}/{} member(s) answered.{RESET} {CYAN}Synthesize the answers above into a \
         single consensus (note agreements, surface dissent) — the chairman is the strong model \
         that convened this council.{RESET}",
        members.len()
    );
    Ok(())
}
