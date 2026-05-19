//! `ff mcp install` — wire the local forgefleet MCP server into each
//! client tool's config so Claude Code / Codex / Kimi / Cursor / Windsurf /
//! Goose all reach for ff's fleet_run / fleet_crew / brain_search by default
//! instead of generic bash / grep / web-fetch.
//!
//! Two layers per client:
//!   1. **MCP server config** — append a `forgefleet` entry to the client's
//!      mcpServers section, pointing at the per-computer federation port
//!      (`http://localhost:50001/mcp` by default).
//!   2. **CLAUDE.md / AGENTS.md instruction** — append a routing rule
//!      ("for fleet/LLM/computer tasks, prefer the forgefleet MCP tools").
//!
//! Idempotent: re-running with the same client+URL is a no-op.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Clone, Subcommand)]
pub enum McpCommand {
    /// Install the forgefleet MCP server into one or more coding-agent
    /// configs (Claude Code, Codex, Kimi, Cursor, Windsurf, Goose).
    Install {
        /// Which client to install for. Pass `all` to install everywhere
        /// we can detect a config file.
        #[arg(long, value_parser = ["all", "claude-code", "codex", "kimi", "cursor", "windsurf", "goose"])]
        r#for: String,
        /// MCP server URL. Defaults to the per-computer federation endpoint
        /// (`http://localhost:50001/mcp`) which every fleet computer hosts.
        #[arg(long, default_value = "http://localhost:50001/mcp")]
        server_url: String,
        /// Skip appending the CLAUDE.md / AGENTS.md routing rule. Useful
        /// for installing the server entry without touching the global
        /// instructions.
        #[arg(long, default_value_t = false)]
        no_instructions: bool,
        /// Show what would change without writing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Print which clients ff would target (based on what configs exist)
    /// without making any change.
    Status,
}

const INSTRUCTION_MARKER: &str = "<!-- ff-mcp-install -->";
const INSTRUCTION_TEXT: &str = r#"<!-- ff-mcp-install -->
## ForgeFleet MCP routing rule

For any task that involves the fleet, the LLM backbone, or the operator's
computers, USE the `forgefleet` MCP tools before reaching for generic
shell / grep / web-fetch primitives:

- **`fleet_run`** — single-turn LLM dispatch through the tiered cascade
  (local 9B → 30B → 70B → cloud). Use for definitions, classifications,
  one-shot rewrites, JSON extraction.
- **`fleet_crew`** — 3-agent pipeline (Context Engineer → Code Writer →
  Code Reviewer). Use for multi-file refactors and edge-case-heavy
  functions.
- **`fleet_status` / `fleet_pulse` / `fleet_worker_detail`** — query live
  fleet state instead of guessing.
- **`brain_search` / `brain_vault_read`** — pull operator memory, notes,
  and architecture decisions before researching elsewhere.
- **`computer_use`** — browser + screenshot operations on a fleet
  computer rather than hosted alternatives.

When the task is well-scoped, dispatching to the local fleet is cheaper
and faster than a cloud call. Only fall back to direct shell or web
when no fleet tool fits.
<!-- /ff-mcp-install -->
"#;

pub async fn handle_mcp(cmd: McpCommand) -> Result<()> {
    match cmd {
        McpCommand::Install {
            r#for: client,
            server_url,
            no_instructions,
            dry_run,
        } => {
            let targets = resolve_targets(&client);
            for target in targets {
                if let Err(e) =
                    install_one(target, &server_url, !no_instructions, dry_run).await
                {
                    eprintln!("  ✗ {target}: {e}");
                }
            }
            Ok(())
        }
        McpCommand::Status => {
            print_status();
            Ok(())
        }
    }
}

fn resolve_targets(arg: &str) -> Vec<&'static str> {
    match arg {
        "all" => vec![
            "claude-code",
            "codex",
            "kimi",
            "cursor",
            "windsurf",
            "goose",
        ],
        single => vec![match single {
            "claude-code" => "claude-code",
            "codex" => "codex",
            "kimi" => "kimi",
            "cursor" => "cursor",
            "windsurf" => "windsurf",
            "goose" => "goose",
            _ => "unknown",
        }],
    }
}

async fn install_one(
    target: &str,
    server_url: &str,
    write_instructions: bool,
    dry_run: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("no home directory")?;

    match target {
        "claude-code" => install_claude_code(&home, server_url, write_instructions, dry_run),
        "codex" => install_codex(&home, server_url, dry_run),
        "kimi" => install_kimi(&home, server_url, dry_run),
        "cursor" => install_cursor(&home, server_url, dry_run),
        "windsurf" => install_windsurf(&home, server_url, dry_run),
        "goose" => install_goose(&home, server_url, dry_run),
        other => bail!("unknown client: {other}"),
    }
}

// ─── Claude Code ─────────────────────────────────────────────────────────────
fn install_claude_code(
    home: &std::path::Path,
    server_url: &str,
    write_instructions: bool,
    dry_run: bool,
) -> Result<()> {
    let settings_path = home.join(".claude").join("settings.json");
    upsert_mcp_server_json(&settings_path, "forgefleet", server_url, dry_run)?;
    println!("  ✓ claude-code: {}", settings_path.display());
    if write_instructions {
        let claude_md = home.join(".claude").join("CLAUDE.md");
        append_instructions_md(&claude_md, dry_run)?;
        println!("    + CLAUDE.md routing rule: {}", claude_md.display());
    }
    Ok(())
}

// ─── Codex CLI ───────────────────────────────────────────────────────────────
fn install_codex(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    let config = home.join(".codex").join("config.toml");
    upsert_codex_mcp(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ codex: {}", config.display());
    Ok(())
}

// ─── Kimi (Moonshot CLI) ─────────────────────────────────────────────────────
fn install_kimi(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    // Kimi Code CLI uses ~/.kimi/config.json with the same mcpServers shape
    // as Claude Code.
    let config = home.join(".kimi").join("config.json");
    upsert_mcp_server_json(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ kimi: {}", config.display());
    Ok(())
}

// ─── Cursor ──────────────────────────────────────────────────────────────────
fn install_cursor(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    let config = home.join(".cursor").join("mcp.json");
    upsert_mcp_server_json(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ cursor: {}", config.display());
    Ok(())
}

// ─── Windsurf ────────────────────────────────────────────────────────────────
fn install_windsurf(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    let config = home.join(".codeium").join("windsurf").join("mcp_config.json");
    upsert_mcp_server_json(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ windsurf: {}", config.display());
    Ok(())
}

// ─── Goose ───────────────────────────────────────────────────────────────────
fn install_goose(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    let config = home.join(".config").join("goose").join("config.yaml");
    upsert_goose_mcp(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ goose: {}", config.display());
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn upsert_mcp_server_json(
    path: &std::path::Path,
    server_name: &str,
    server_url: &str,
    dry_run: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut doc: Value = if path.exists() {
        let s = std::fs::read_to_string(path)?;
        if s.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        json!({})
    };

    let servers = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));

    let entry = json!({
        "type": "http",
        "url": server_url
    });

    if let Some(obj) = servers.as_object_mut() {
        if obj.get(server_name) == Some(&entry) {
            // already correct — no-op
            return Ok(());
        }
        obj.insert(server_name.to_string(), entry);
    }

    if dry_run {
        println!("    (dry-run) would write {}", path.display());
        return Ok(());
    }

    let pretty = serde_json::to_string_pretty(&doc)?;
    std::fs::write(path, pretty)?;
    Ok(())
}

fn upsert_codex_mcp(
    path: &std::path::Path,
    server_name: &str,
    server_url: &str,
    dry_run: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let block = format!(
        "\n[mcp_servers.{server_name}]\ntype = \"http\"\nurl = \"{server_url}\"\n"
    );

    // If the marker is already present and points at the same URL, skip.
    let marker = format!("[mcp_servers.{server_name}]");
    if existing.contains(&marker) && existing.contains(&format!("url = \"{server_url}\"")) {
        return Ok(());
    }

    let new_content = if existing.contains(&marker) {
        // Replace the existing block: crude approach — keep only lines
        // outside this server's section.
        let mut keep: Vec<&str> = Vec::new();
        let mut in_section = false;
        for line in existing.lines() {
            if line.trim_start().starts_with('[') {
                in_section = line.trim() == marker;
                if !in_section {
                    keep.push(line);
                }
                continue;
            }
            if !in_section {
                keep.push(line);
            }
        }
        let mut out = keep.join("\n");
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&block);
        out
    } else {
        let mut out = existing;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&block);
        out
    };

    if dry_run {
        println!("    (dry-run) would write {}", path.display());
        return Ok(());
    }
    std::fs::write(path, new_content)?;
    Ok(())
}

fn upsert_goose_mcp(
    path: &std::path::Path,
    server_name: &str,
    server_url: &str,
    dry_run: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let block = format!(
        "\nextensions:\n  {server_name}:\n    type: http\n    url: {server_url}\n    enabled: true\n"
    );
    let marker = format!("  {server_name}:");
    if existing.contains(&marker) && existing.contains(server_url) {
        return Ok(());
    }
    let new_content = if existing.contains(&marker) {
        // Leave existing untouched if it points at a different URL — operator
        // should reconcile manually. Print a warning instead of clobbering.
        eprintln!("    ! goose already has '{server_name}' configured at a different URL; not overwriting");
        return Ok(());
    } else {
        let mut out = existing;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&block);
        out
    };
    if dry_run {
        println!("    (dry-run) would write {}", path.display());
        return Ok(());
    }
    std::fs::write(path, new_content)?;
    Ok(())
}

fn append_instructions_md(path: &PathBuf, dry_run: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    if existing.contains(INSTRUCTION_MARKER) {
        return Ok(());
    }
    if dry_run {
        println!("    (dry-run) would append routing rule to {}", path.display());
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(INSTRUCTION_TEXT);
    std::fs::write(path, out)?;
    Ok(())
}

fn print_status() {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            eprintln!("no home directory");
            return;
        }
    };
    let candidates: &[(&str, Vec<PathBuf>)] = &[
        ("claude-code", vec![home.join(".claude").join("settings.json")]),
        ("codex", vec![home.join(".codex").join("config.toml")]),
        ("kimi", vec![home.join(".kimi").join("config.json")]),
        ("cursor", vec![home.join(".cursor").join("mcp.json")]),
        ("windsurf", vec![home.join(".codeium").join("windsurf").join("mcp_config.json")]),
        ("goose", vec![home.join(".config").join("goose").join("config.yaml")]),
    ];
    println!("MCP client configs on this computer:");
    for (name, paths) in candidates {
        for path in paths {
            let exists = path.exists();
            let has_ff = if exists {
                std::fs::read_to_string(path)
                    .ok()
                    .map(|s| s.contains("forgefleet"))
                    .unwrap_or(false)
            } else {
                false
            };
            let mark = if !exists {
                "—"
            } else if has_ff {
                "✓ forgefleet installed"
            } else {
                "× forgefleet missing"
            };
            println!("  {:<12} {} {}", name, mark, path.display());
        }
    }
}
