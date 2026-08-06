//! `ff converge` — re-apply the onboarding checklist to this machine
//! (idempotent self-heal). Thin renderer over `ff_agent::converge`.

use anyhow::Result;
use ff_agent::converge::{ConvergeStatus, run_converge};

pub async fn handle_converge() -> Result<()> {
    let results = run_converge().await;

    println!("{:<24} {:<10} DETAIL", "ITEM", "STATUS");
    for r in &results {
        let mark = match r.status {
            ConvergeStatus::Ok => "✓",
            ConvergeStatus::Installed => "+",
            ConvergeStatus::Skipped => "-",
            ConvergeStatus::Failed => "✗",
        };
        println!(
            "{:<24} {:<10} {}",
            r.item,
            format!("{mark} {}", r.status.as_str()),
            r.detail
        );
    }

    let count = |s: ConvergeStatus| results.iter().filter(|r| r.status == s).count();
    println!(
        "\n{} ok, {} installed, {} skipped, {} failed",
        count(ConvergeStatus::Ok),
        count(ConvergeStatus::Installed),
        count(ConvergeStatus::Skipped),
        count(ConvergeStatus::Failed),
    );
    Ok(())
}
