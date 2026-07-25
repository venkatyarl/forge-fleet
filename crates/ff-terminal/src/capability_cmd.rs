use anyhow::Result;

use crate::{CYAN, GREEN, RESET, YELLOW};

pub async fn handle_capability(need: &str, kind: Option<&str>, text: bool) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(anyhow::Error::msg)?;
    let result = match kind {
        Some(kind) => {
            let kind = ff_brain::CapabilityKind::parse(kind).map_err(anyhow::Error::msg)?;
            ff_brain::capability_check(&pool, need, kind).await
        }
        None => ff_brain::capability_check_all(&pool, need).await,
    }
    .map_err(anyhow::Error::msg)?;

    if !text {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    if let Some(best) = &result.best {
        println!(
            "{GREEN}✓ capability found{RESET}: {CYAN}{}{RESET} ({}, {}, {:?})",
            best.name,
            best.kind,
            if best.is_local { "local" } else { "remote" },
            best.est_cost_class
        );
        println!("  invoke: {}", best.invoke_hint);
        if result.matches.len() > 1 {
            println!("  {} other match(es)", result.matches.len() - 1);
        }
    } else {
        println!("{YELLOW}⚠ no capability match for {need:?}{RESET}");
    }
    Ok(())
}
