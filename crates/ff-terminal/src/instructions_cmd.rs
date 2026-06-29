//! `ff instructions sync` — materialize the ForgeFleet methodology block into
//! this node's global TUI configs (~/.claude/CLAUDE.md, ~/.codex/AGENTS.md,
//! ~/.kimi/AGENTS.md) plus the shared ~/.forgefleet/instructions/ff-methodology.md.
//!
//! Roadmap D / the council's recommended fallback to #658's MCP-instructions
//! primary path. Idempotent; forgefleetd also runs this on boot so every node
//! self-syncs without an SSH fan-out.

const GREEN: &str = "\x1b[32m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Handle `ff instructions sync`.
pub async fn handle_sync() -> anyhow::Result<()> {
    let written = ff_agent::instructions_sync::sync_local()
        .map_err(|e| anyhow::anyhow!("sync failed: {e}"))?;
    for p in &written {
        println!("{GREEN}✓{RESET} {p}");
    }
    println!(
        "{DIM}synced the methodology block into {} file(s) on this node{RESET}",
        written.len()
    );
    println!(
        "{DIM}(every node also self-syncs on forgefleetd boot — run `ff fleet deploy --all` to push fleet-wide){RESET}"
    );
    Ok(())
}
