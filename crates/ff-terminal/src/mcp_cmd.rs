//! `ff mcp install` — wire the local forgefleet MCP server into each
//! client tool's config so Claude Code / Codex / Gemini / Kimi / Cursor / Windsurf /
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
    /// configs (Claude Code, Claude Desktop, Codex, Gemini, Kimi, Cursor, Windsurf,
    /// Goose, Grok).
    Install {
        /// Which client to install for. Pass `all` to install everywhere
        /// we can detect a config file.
        #[arg(long, value_parser = ["all", "claude-code", "claude-desktop", "codex", "gemini", "kimi", "kimi-desktop", "cursor", "windsurf", "goose", "grok"])]
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
    Status {
        /// Emit one JSON object per client config
        /// (client/config_path/exists/forgefleet_installed/state) instead of
        /// the human table, so an agent can consume the install map structurally.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

const INSTRUCTION_MARKER: &str = "<!-- ff-mcp-install -->";
const INSTRUCTION_TEXT: &str = r#"<!-- ff-mcp-install -->
## ForgeFleet (ff) — how to work with the fleet

You have the `forgefleet` MCP tools wired in. ForgeFleet is a distributed
multi-LLM platform (many computers, local + cloud models). Use these tools and
the `ff` CLI before generic shell / grep / web primitives.

### Discovery-FIRST — search before you build (hard rule)
Before writing any new table/module/feature, inventory what ALREADY exists — the
#1 waste is rebuilding a capability the fleet already has. Order:
1. **Cortex / code graph** — `cortex_find` / `cortex_search` ("what
   handles X?") to find the owning crate/module. Faster + cheaper than grep.
2. **`ff db query "<read-only SQL>"`** — confirm the LIVE Postgres schema; source
   `CREATE TABLE` strings can drift from the live DB. Never extend a table you
   haven't confirmed live.
3. **`brain_search`** for prior decisions; grep/read files LAST.
Then reuse/extend what exists instead of forking.

### Dogfood ff (and it logs training data)
Prefer routing work THROUGH ff over raw cloud calls — it surfaces ff's bugs and
every call is logged to `ff_interactions` (the training corpus for ff's own LLM):
- **`fleet_run`** — single-turn LLM dispatch (tiered local → cloud): definitions,
  classifications, one-shot rewrites, JSON extraction.
- **`fleet_crew`** — Code Writer → Reviewer pipeline for multi-file refactors.
- **`ff offload` / `ff research`** — dispatch coding/research to fleet models.

### Memory + state — don't keep it only in your head
- **`memory_*`** (the Scratchpad): bounded, self-curating working memory with
  fixed blocks (task/decisions/findings/state/scratch) and layered scope. Read it
  at the start of work; record decisions/findings as you go. **Pass `cwd` (your
  absolute working directory) on every `memory_*` call** — the server derives a
  stable project id from it so this repo's memory is SHARED with the other CLIs
  (Claude Code / Codex / Kimi) working in the same repo. Omit `cwd` only for
  throwaway session-local notes.
- **`brain_search` / `brain_vault_read`** — operator memory, notes, architecture.
- **`fleet_status` / `fleet_pulse` / `fleet_worker_detail`** — live fleet state.
- **`computer_use`** — browser/screenshot on a fleet computer.

When a task is well-scoped, dispatching to the local fleet is cheaper than a cloud
call. Only fall back to direct shell/web when no fleet tool fits.
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
                if let Err(e) = install_one(target, &server_url, !no_instructions, dry_run).await {
                    eprintln!("  ✗ {target}: {e}");
                }
            }
            Ok(())
        }
        McpCommand::Status { json } => {
            print_status(json);
            Ok(())
        }
    }
}

fn resolve_targets(arg: &str) -> Vec<&'static str> {
    match arg {
        "all" => vec![
            "claude-code",
            "claude-desktop",
            "codex",
            "gemini",
            "kimi",
            "kimi-desktop",
            "cursor",
            "windsurf",
            "goose",
            "grok",
        ],
        single => vec![match single {
            "claude-code" => "claude-code",
            "claude-desktop" => "claude-desktop",
            "codex" => "codex",
            "gemini" => "gemini",
            "kimi" => "kimi",
            "kimi-desktop" => "kimi-desktop",
            "cursor" => "cursor",
            "windsurf" => "windsurf",
            "goose" => "goose",
            "grok" => "grok",
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
        "claude-desktop" => install_claude_desktop(&home, server_url, dry_run),
        "codex" => install_codex(&home, server_url, write_instructions, dry_run),
        "gemini" => install_gemini(&home, server_url, write_instructions, dry_run),
        "kimi" => install_kimi(&home, server_url, write_instructions, dry_run),
        "kimi-desktop" => install_kimi_desktop(&home, server_url, dry_run),
        "cursor" => install_cursor(&home, server_url, dry_run),
        "windsurf" => install_windsurf(&home, server_url, dry_run),
        "goose" => install_goose(&home, server_url, dry_run),
        "grok" => install_grok(&home, server_url, dry_run),
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
    upsert_resilient_mcp_server_json(&settings_path, "forgefleet", server_url, dry_run)?;
    println!("  ✓ claude-code: {}", settings_path.display());
    // Claude Code versions differ on where user-scope mcpServers are honored:
    // newer builds read ~/.claude.json, older ones ~/.claude/settings.json —
    // and a write to the wrong one is SILENTLY ignored (found live on vinny
    // 2026-08-06: ff reported "installed" while the session saw no tools).
    // Write both; the upsert is idempotent.
    let user_scope = home.join(".claude.json");
    upsert_mcp_server_json(&user_scope, "forgefleet", server_url, dry_run)?;
    println!("  ✓ claude-code: {} (user scope)", user_scope.display());
    if write_instructions {
        let claude_md = home.join(".claude").join("CLAUDE.md");
        append_instructions_md(&claude_md, dry_run)?;
        println!("    + CLAUDE.md routing rule: {}", claude_md.display());
    }
    Ok(())
}

// ─── Claude Desktop ──────────────────────────────────────────────────────────
/// OS-specific config path for the Claude Desktop app. macOS keeps it under
/// `~/Library/Application Support/Claude/`; Linux (and the Flatpak/AppImage
/// builds) use `~/.config/Claude/`. Same `mcpServers` JSON shape as Claude Code.
fn claude_desktop_config_path(home: &std::path::Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("Claude")
            .join("claude_desktop_config.json")
    } else {
        home.join(".config")
            .join("Claude")
            .join("claude_desktop_config.json")
    }
}

fn install_claude_desktop(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    let config = claude_desktop_config_path(home);
    // The Claude DESKTOP app (unlike the Claude Code CLI) does NOT accept
    // `{"type":"http","url":...}` MCP entries in its config file — it silently
    // skips them ("The following entries … are not valid MCP server
    // configurations and were skipped: forgefleet"). Desktop only launches
    // stdio servers, so bridge the remote HTTP endpoint through `npx
    // mcp-remote`, which Desktop CAN spawn. (Claude Code keeps the http form.)
    upsert_mcp_server_stdio_bridge(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ claude-desktop: {}", config.display());
    Ok(())
}

// ─── Codex CLI ───────────────────────────────────────────────────────────────
fn install_codex(
    home: &std::path::Path,
    server_url: &str,
    write_instructions: bool,
    dry_run: bool,
) -> Result<()> {
    let config = home.join(".codex").join("config.toml");
    upsert_codex_mcp(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ codex: {}", config.display());
    if write_instructions {
        // Codex reads global instructions from ~/.codex/AGENTS.md.
        let agents_md = home.join(".codex").join("AGENTS.md");
        append_instructions_md(&agents_md, dry_run)?;
        println!("    + ff routing rule: {}", agents_md.display());
    }
    Ok(())
}

// ─── Gemini CLI ─────────────────────────────────────────────────────────────
fn install_gemini(
    home: &std::path::Path,
    server_url: &str,
    write_instructions: bool,
    dry_run: bool,
) -> Result<()> {
    let config = home.join(".gemini").join("settings.json");
    let entry = if matches!(
        server_url,
        "http://localhost:50001/mcp" | "http://127.0.0.1:50001/mcp"
    ) {
        json!({ "command": "forgefleetd", "args": ["mcp", "--stdio"] })
    } else {
        json!({ "httpUrl": server_url })
    };
    upsert_mcp_entry(&config, "forgefleet", entry, dry_run)?;
    println!("  ✓ gemini: {}", config.display());
    if write_instructions {
        let gemini_md = home.join(".gemini").join("GEMINI.md");
        append_instructions_md(&gemini_md, dry_run)?;
        println!("    + ff routing rule: {}", gemini_md.display());
    }
    Ok(())
}

// ─── Kimi (Moonshot CLI) ─────────────────────────────────────────────────────
fn install_kimi(
    home: &std::path::Path,
    server_url: &str,
    write_instructions: bool,
    dry_run: bool,
) -> Result<()> {
    // Kimi Code CLI ≥0.29 reads user-level MCP servers from
    // ~/.kimi-code/mcp.json (the legacy ~/.kimi/config.json location is no
    // longer loaded by current releases).
    let config = home.join(".kimi-code").join("mcp.json");
    upsert_resilient_mcp_server_json(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ kimi: {}", config.display());
    // Some kimi builds/versions read ~/.kimi/mcp.json instead — a write to
    // the wrong one is silently ignored (found live on vinny 2026-08-06).
    // Upsert the fallback too; idempotent.
    let legacy = home.join(".kimi").join("mcp.json");
    upsert_mcp_server_json(&legacy, "forgefleet", server_url, dry_run)?;
    println!("  ✓ kimi: {} (fallback path)", legacy.display());
    if write_instructions {
        // Kimi reads agent instructions from ~/.kimi/AGENTS.md (the cross-tool
        // AGENTS.md convention).
        let agents_md = home.join(".kimi").join("AGENTS.md");
        append_instructions_md(&agents_md, dry_run)?;
        println!("    + ff routing rule: {}", agents_md.display());
    }
    Ok(())
}

// ─── Kimi Desktop (Kimi Work / Vivace app) ───────────────────────────────────
/// Config path for the Kimi DESKTOP app, which bundles a kimi-code runtime
/// with its own `mcpServers` file (distinct from the standalone Kimi CLI's
/// `~/.kimi-code/mcp.json`). macOS keeps the app data under
/// `~/Library/Application Support/kimi-desktop/`; the runtime's MCP config is
/// `daimon-share/daimon/runtime/kimi-code/home/mcp.json`. Same `mcpServers`
/// JSON shape as the CLI.
fn kimi_desktop_config_path(home: &std::path::Path) -> PathBuf {
    let rel = "daimon-share/daimon/runtime/kimi-code/home/mcp.json";
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("kimi-desktop")
            .join(rel)
    } else {
        // Best-effort Linux location (Electron apps use ~/.config/<app>).
        home.join(".config").join("kimi-desktop").join(rel)
    }
}

fn install_kimi_desktop(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    let config = kimi_desktop_config_path(home);
    upsert_mcp_server_json(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ kimi-desktop: {}", config.display());
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
    let config = home
        .join(".codeium")
        .join("windsurf")
        .join("mcp_config.json");
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

// ─── Grok CLI (xAI) ──────────────────────────────────────────────────────────
fn install_grok(home: &std::path::Path, server_url: &str, dry_run: bool) -> Result<()> {
    // grok-cli reads MCP servers from ~/.grok/mcp-config.json using the same
    // `mcpServers` shape as Claude Code / Cursor.
    let config = home.join(".grok").join("mcp-config.json");
    upsert_mcp_server_json(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ grok: {}", config.display());
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn upsert_mcp_server_json(
    path: &std::path::Path,
    server_name: &str,
    server_url: &str,
    dry_run: bool,
) -> Result<()> {
    // Claude Code / Cursor / Kimi / Windsurf / Grok accept a native remote
    // (`type:"http"`) MCP entry.
    let entry = json!({ "type": "http", "url": server_url });
    upsert_mcp_entry(path, server_name, entry, dry_run)
}

/// A local stdio server is available as soon as the installed binary can be
/// spawned. Unlike a one-shot remote HTTP enumeration it survives an MCP HTTP
/// daemon or Postgres outage at agent startup; DB-backed calls degrade with a
/// normal tool error while schema-independent tools remain usable.
fn upsert_resilient_mcp_server_json(
    path: &std::path::Path,
    server_name: &str,
    server_url: &str,
    dry_run: bool,
) -> Result<()> {
    if matches!(
        server_url,
        "http://localhost:50001/mcp" | "http://127.0.0.1:50001/mcp"
    ) {
        upsert_mcp_entry(
            path,
            server_name,
            json!({ "command": "forgefleetd", "args": ["mcp", "--stdio"] }),
            dry_run,
        )
    } else {
        upsert_mcp_server_json(path, server_name, server_url, dry_run)
    }
}

/// Like [`upsert_mcp_server_json`] but writes a STDIO entry that bridges the
/// remote HTTP MCP endpoint through `npx mcp-remote`. Required by the Claude
/// Desktop app, whose config loader only launches stdio (`command`) servers and
/// silently skips `type:"http"` entries.
fn upsert_mcp_server_stdio_bridge(
    path: &std::path::Path,
    server_name: &str,
    server_url: &str,
    dry_run: bool,
) -> Result<()> {
    // GUI apps launch with a stripped PATH (no /opt/homebrew/bin on macOS),
    // so a bare `npx` fails to resolve AND npx's own `#!/usr/bin/env node`
    // shebang can't find node — the server shows "disconnected" either way
    // (vinny, Claude Desktop, 2026-08-06). Resolve the absolute npx path AND
    // pin an explicit env.PATH covering the Node bin dir.
    let npx = resolve_gui_binary("npx");
    let entry = json!({
        "command": npx,
        "args": ["-y", "mcp-remote", server_url, "--transport", "http-only"],
        "env": { "PATH": gui_path() },
    });
    upsert_mcp_entry(path, server_name, entry, dry_run)
}

/// PATH string for GUI-launched processes: the well-known binary dirs GUI
/// apps miss (homebrew, local) plus the system defaults.
fn gui_path() -> String {
    "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string()
}

/// Resolve a binary for GUI-launched contexts (desktop apps get a minimal
/// PATH): prefer the well-known absolute locations, fall back to PATH lookup,
/// and only then to the bare name.
fn resolve_gui_binary(name: &str) -> String {
    let well_known = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/opt/bin",
    ];
    for dir in well_known {
        let candidate = format!("{dir}/{name}");
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = format!("{dir}/{name}");
            if std::path::Path::new(&candidate).exists() {
                return candidate;
            }
        }
    }
    name.to_string()
}

/// Insert/replace `mcpServers.<server_name>` with `entry` in a JSON config,
/// preserving every other key in the file. Idempotent.
fn upsert_mcp_entry(
    path: &std::path::Path,
    server_name: &str,
    entry: Value,
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

    if !matches!(
        server_url,
        "http://localhost:50001/mcp" | "http://127.0.0.1:50001/mcp"
    ) {
        // Remote Codex configs retain the native HTTP transport. The local
        // default is the resilient DB-independent stdio bootstrap below.
        let block =
            format!("\n[mcp_servers.{server_name}]\ntype = \"http\"\nurl = \"{server_url}\"\n");
        return replace_codex_section(path, &existing, server_name, &block, dry_run);
    }
    let block = format!(
        "\n[mcp_servers.{server_name}]\ncommand = \"forgefleetd\"\nargs = [\"mcp\", \"--stdio\"]\nstartup_timeout_sec = 30\ntool_timeout_sec = 120\n"
    );

    // If the marker is already present and points at the same URL, skip.
    let marker = format!("[mcp_servers.{server_name}]");
    if existing.contains(&marker)
        && existing.contains("command = \"forgefleetd\"")
        && existing.contains("args = [\"mcp\", \"--stdio\"]")
    {
        return Ok(());
    }

    replace_codex_section(path, &existing, server_name, &block, dry_run)
}

fn replace_codex_section(
    path: &std::path::Path,
    existing: &str,
    server_name: &str,
    block: &str,
    dry_run: bool,
) -> Result<()> {
    let marker = format!("[mcp_servers.{server_name}]");
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
        let mut out = existing.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(block);
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
        eprintln!(
            "    ! goose already has '{server_name}' configured at a different URL; not overwriting"
        );
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
        println!(
            "    (dry-run) would append routing rule to {}",
            path.display()
        );
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

/// Classify a single client config into a stable machine-readable state string,
/// derived purely from whether the config file exists and whether it already
/// names the forgefleet server. Pure so it's unit-testable without a real FS.
fn classify_state(exists: bool, has_ff: bool) -> &'static str {
    if !exists {
        "absent"
    } else if has_ff {
        "installed"
    } else {
        "not_installed"
    }
}

/// Human marker for the text table, derived from the same state. Kept separate
/// so the JSON path carries the stable `state` token and the text path keeps
/// its existing glyphs byte-for-byte.
fn text_mark(state: &str) -> &'static str {
    match state {
        "absent" => "—",
        "installed" => "✓ forgefleet installed",
        _ => "× forgefleet missing",
    }
}

fn status_candidates(home: &std::path::Path) -> Vec<(&'static str, Vec<PathBuf>)> {
    // Each client maps to its candidate config paths in preference order.
    // Install writes ALL candidates (a write to a path the client ignores is
    // silently dropped — vinny 2026-08-06), so status must accept a match on
    // ANY candidate and report the path that matched.
    let mut candidates: Vec<(&'static str, Vec<PathBuf>)> = vec![
        (
            "claude-code",
            vec![
                home.join(".claude").join("settings.json"),
                home.join(".claude.json"),
            ],
        ),
        ("claude-desktop", vec![claude_desktop_config_path(home)]),
        ("codex", vec![home.join(".codex").join("config.toml")]),
        ("gemini", vec![home.join(".gemini").join("settings.json")]),
        (
            "kimi",
            vec![
                home.join(".kimi-code").join("mcp.json"),
                home.join(".kimi").join("mcp.json"),
            ],
        ),
        ("kimi-desktop", vec![kimi_desktop_config_path(home)]),
        ("cursor", vec![home.join(".cursor").join("mcp.json")]),
        (
            "windsurf",
            vec![home
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json")],
        ),
        (
            "goose",
            vec![home.join(".config").join("goose").join("config.yaml")],
        ),
        ("grok", vec![home.join(".grok").join("mcp-config.json")]),
    ];
    let legacy_kimi = home.join(".kimi").join("config.json");
    if legacy_kimi.exists() {
        candidates.insert(5, ("kimi-legacy", vec![legacy_kimi]));
    }
    candidates
}

fn config_has_forgefleet(path: &std::path::Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        return serde_json::from_str::<Value>(&contents)
            .ok()
            .and_then(|doc| {
                doc.get("mcpServers")?
                    .as_object()?
                    .get("forgefleet")
                    .cloned()
            })
            .is_some_and(|server| server.is_object());
    }
    contents.contains("forgefleet")
}

fn status_rows(home: &std::path::Path) -> Vec<Value> {
    status_candidates(home)
        .into_iter()
        .map(|(name, paths)| {
            // A client counts as installed if ANY candidate path carries the
            // forgefleet entry; report the path that matched (else primary).
            let matched = paths.iter().find(|p| p.exists() && config_has_forgefleet(p));
            let (path, has_ff) = match matched {
                Some(p) => (p.clone(), true),
                None => (paths[0].clone(), false),
            };
            let exists = path.exists();
            let transport = has_ff.then(|| config_forgefleet_transport(&path)).flatten();
>>>>>>> 4c3c75d9 (feat: desktop apps download-to-Downloads + MCP dual-path status + canonical che            json!({
                "client": name,
                "config_path": path.display().to_string(),
                "exists": exists,
                "forgefleet_installed": has_ff,
                "state": classify_state(exists, has_ff),
            })
        })
        .collect()
}

fn render_status(home: &std::path::Path, as_json: bool) -> String {
    let rows = status_rows(home);
    if as_json {
        return serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string());
    }

    let mut output = String::from("MCP client configs on this computer:\n");
    for row in rows {
        let name = row["client"].as_str().unwrap_or_default();
        let state = row["state"].as_str().unwrap_or_default();
        let path = row["config_path"].as_str().unwrap_or_default();
        output.push_str(&format!("  {name:<12} {} {path}\n", text_mark(state)));
    }
    output
}

fn print_status(as_json: bool) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            eprintln!("no home directory");
            return;
        }
    };
    println!("{}", render_status(&home, as_json).trim_end());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_state_covers_all_three() {
        // config file absent → absent regardless of has_ff
        assert_eq!(classify_state(false, false), "absent");
        assert_eq!(classify_state(false, true), "absent");
        // exists + names forgefleet → installed
        assert_eq!(classify_state(true, true), "installed");
        // exists but no forgefleet entry → not_installed
        assert_eq!(classify_state(true, false), "not_installed");
    }

    #[test]
    fn text_mark_matches_state_glyphs() {
        // pins the exact table glyphs so the text path stays byte-for-byte
        assert_eq!(text_mark("absent"), "—");
        assert_eq!(text_mark("installed"), "✓ forgefleet installed");
        assert_eq!(text_mark("not_installed"), "× forgefleet missing");
    }

    #[test]
    fn kimi_status_uses_authoritative_json_and_reports_installed() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".kimi-code").join("mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"{"mcpServers":{"forgefleet":{"command":"forgefleetd"}}}"#,
        )
        .unwrap();

        let row = status_rows(temp.path())
            .into_iter()
            .find(|row| row["client"] == "kimi")
            .unwrap();
        assert_eq!(row["config_path"], config.display().to_string());
        assert_eq!(row["exists"], true);
        assert_eq!(row["forgefleet_installed"], true);
        assert_eq!(row["state"], "installed");
    }

    #[test]
    fn kimi_status_reports_missing_and_malformed_json_truthfully() {
        let temp = tempfile::tempdir().unwrap();
        let missing = status_rows(temp.path())
            .into_iter()
            .find(|row| row["client"] == "kimi")
            .unwrap();
        assert_eq!(missing["exists"], false);
        assert_eq!(missing["forgefleet_installed"], false);
        assert_eq!(missing["state"], "absent");

        let config = temp.path().join(".kimi-code").join("mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, r#"{"mcpServers":{"forgefleet": broken}"#).unwrap();
        let malformed = status_rows(temp.path())
            .into_iter()
            .find(|row| row["client"] == "kimi")
            .unwrap();
        assert_eq!(malformed["exists"], true);
        assert_eq!(malformed["forgefleet_installed"], false);
        assert_eq!(malformed["state"], "not_installed");
    }

    #[test]
    fn kimi_status_requires_forgefleet_server_object() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".kimi-code").join("mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();

        for (value, expected_state) in [
            (serde_json::json!(null), "not_installed"),
            (serde_json::json!("forgefleet"), "not_installed"),
            (serde_json::json!([]), "not_installed"),
            (serde_json::json!({}), "installed"),
        ] {
            std::fs::write(
                &config,
                serde_json::json!({"mcpServers": {"forgefleet": value}}).to_string(),
            )
            .unwrap();
            let row = status_rows(temp.path())
                .into_iter()
                .find(|row| row["client"] == "kimi")
                .unwrap();
            assert_eq!(row["state"], expected_state, "server value: {value}");
        }
    }

    #[test]
    fn kimi_json_and_text_status_share_truth_and_legacy_is_conditional() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".kimi-code").join("mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, r#"{"mcpServers":{"other":{}}}"#).unwrap();

        let json_output: Value = serde_json::from_str(&render_status(temp.path(), true)).unwrap();
        let kimi = json_output
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["client"] == "kimi")
            .unwrap();
        assert_eq!(kimi["state"], "not_installed");
        let text_output = render_status(temp.path(), false);
        assert!(text_output.contains("kimi         × forgefleet missing"));
        assert!(!text_output.contains("kimi-legacy"));

        let legacy = temp.path().join(".kimi").join("config.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"mcpServers":{"forgefleet":{}}}"#).unwrap();
        let text_output = render_status(temp.path(), false);
        assert!(text_output.contains("kimi-legacy  ✓ forgefleet installed"));
    }

    #[test]
    fn all_targets_include_gemini() {
        assert!(resolve_targets("all").contains(&"gemini"));
    }

    #[test]
    fn gemini_install_preserves_settings_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".gemini").join("settings.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"{"theme":"dark","mcpServers":{"other":{"command":"other"}}}"#,
        )
        .unwrap();

        install_gemini(temp.path(), "https://fleet.example/mcp", false, false).unwrap();
        let first = std::fs::read_to_string(&config).unwrap();
        let doc: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(doc["theme"], "dark");
        assert_eq!(doc["mcpServers"]["other"]["command"], "other");
        assert_eq!(
            doc["mcpServers"]["forgefleet"]["httpUrl"],
            "https://fleet.example/mcp"
        );

        install_gemini(temp.path(), "https://fleet.example/mcp", false, false).unwrap();
        assert_eq!(std::fs::read_to_string(config).unwrap(), first);
    }
}
