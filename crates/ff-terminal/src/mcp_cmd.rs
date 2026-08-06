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
    // Claude Code supports native HTTP MCP servers.  Keep the client pointed
    // at the independently supervised loopback service so a forgefleetd
    // restart does not permanently close a session-owned stdio pipe.
    upsert_mcp_server_json(&settings_path, "forgefleet", server_url, dry_run)?;
    println!("  ✓ claude-code: {}", settings_path.display());
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
    // Gemini's native HTTP transport works for loopback as well as remote
    // URLs.  A local stdio child couples MCP lifetime to one CLI session.
    let entry = json!({ "httpUrl": server_url });
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
    // Kimi Code supports native HTTP MCP entries.  Use the supervised service
    // instead of a per-session stdio child so daemon restarts are reconnectable.
    upsert_mcp_server_json(&config, "forgefleet", server_url, dry_run)?;
    println!("  ✓ kimi: {}", config.display());
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
    let entry = json!({
        "command": "npx",
        "args": ["-y", "mcp-remote", server_url, "--transport", "http-only"],
    });
    upsert_mcp_entry(path, server_name, entry, dry_run)
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
        // default now uses the same native transport via the independently
        // supervised loopback MCP service below.
        let block =
            format!("\n[mcp_servers.{server_name}]\ntype = \"http\"\nurl = \"{server_url}\"\n");
        return replace_codex_section(path, &existing, server_name, &block, dry_run);
    }
    let block = format!(
        "\n[mcp_servers.{server_name}]\nurl = \"{server_url}\"\nstartup_timeout_sec = 30\ntool_timeout_sec = 120\n"
    );

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
        out.push_str(block);
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

fn status_candidates(home: &std::path::Path) -> Vec<(&'static str, PathBuf)> {
    let mut candidates = vec![
        ("claude-code", home.join(".claude").join("settings.json")),
        ("claude-desktop", claude_desktop_config_path(home)),
        ("codex", home.join(".codex").join("config.toml")),
        ("gemini", home.join(".gemini").join("settings.json")),
        ("kimi", home.join(".kimi-code").join("mcp.json")),
        ("kimi-desktop", kimi_desktop_config_path(home)),
        ("cursor", home.join(".cursor").join("mcp.json")),
        (
            "windsurf",
            home.join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
        ),
        (
            "goose",
            home.join(".config").join("goose").join("config.yaml"),
        ),
        ("grok", home.join(".grok").join("mcp-config.json")),
    ];
    let legacy_kimi = home.join(".kimi").join("config.json");
    if legacy_kimi.exists() {
        candidates.insert(5, ("kimi-legacy", legacy_kimi));
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

/// Report the configured transport, not merely whether a server entry exists.
/// A session-owned stdio child is materially different from the supervised
/// loopback HTTP service: once its pipe closes, an already-open client cannot
/// reconnect it.  Keep this parser deliberately read-only and tolerant of the
/// small JSON/TOML/YAML shapes written by the installers above.
fn config_forgefleet_transport(path: &std::path::Path) -> Option<&'static str> {
    let contents = std::fs::read_to_string(path).ok()?;
    if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        let doc: Value = serde_json::from_str(&contents).ok()?;
        let server = doc.get("mcpServers")?.as_object()?.get("forgefleet")?;
        if server.get("type").and_then(Value::as_str) == Some("http")
            || server.get("url").and_then(Value::as_str).is_some()
            || server.get("httpUrl").and_then(Value::as_str).is_some()
        {
            return Some("http");
        }
        if server.get("command").and_then(Value::as_str) == Some("npx")
            && server
                .get("args")
                .and_then(Value::as_array)
                .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some("mcp-remote")))
        {
            return Some("http_bridge");
        }
        if server.get("command").and_then(Value::as_str).is_some() {
            return Some("stdio");
        }
        return Some("unknown");
    }

    let section = if let Some((_, tail)) = contents.split_once("[mcp_servers.forgefleet]") {
        tail.split_once("\n[")
            .map(|(current, _)| current)
            .unwrap_or(tail)
    } else {
        // Goose uses YAML.  Its installer writes only native HTTP entries.
        if contents.contains("  forgefleet:") && contents.contains("    type: http") {
            return Some("http");
        }
        return None;
    };
    if section
        .lines()
        .any(|line| line.trim_start().starts_with("url ="))
    {
        Some("http")
    } else if section
        .lines()
        .any(|line| line.trim_start().starts_with("command ="))
    {
        Some("stdio")
    } else {
        Some("unknown")
    }
}

fn status_rows(home: &std::path::Path) -> Vec<Value> {
    status_candidates(home)
        .into_iter()
        .map(|(name, path)| {
            let exists = path.exists();
            let has_ff = exists && config_has_forgefleet(&path);
            let transport = has_ff.then(|| config_forgefleet_transport(&path)).flatten();
            json!({
                "client": name,
                "config_path": path.display().to_string(),
                "exists": exists,
                "forgefleet_installed": has_ff,
                "transport": transport,
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
        let mark = match (state, row["transport"].as_str()) {
            ("installed", Some("http")) => "✓ forgefleet installed (http)",
            ("installed", Some("http_bridge")) => "✓ forgefleet installed (http bridge)",
            ("installed", Some("stdio")) => "⚠ forgefleet installed (stdio; session-coupled)",
            _ => text_mark(state),
        };
        output.push_str(&format!("  {name:<12} {mark} {path}\n"));
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
        assert_eq!(row["transport"], "stdio");
        assert_eq!(row["state"], "installed");
        assert!(render_status(temp.path(), false).contains("stdio; session-coupled"));
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
    fn codex_localhost_replaces_legacy_stdio_with_native_http() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "model = \"gpt-5\"\n\n[mcp_servers.forgefleet]\ncommand = \"forgefleetd\"\nargs = [\"mcp\", \"--stdio\"]\nstartup_timeout_sec = 30\ntool_timeout_sec = 120\n",
        )
        .unwrap();

        upsert_codex_mcp(&config, "forgefleet", "http://127.0.0.1:50001/mcp", false).unwrap();

        let updated = std::fs::read_to_string(config).unwrap();
        assert!(updated.contains("model = \"gpt-5\""));
        assert!(updated.contains("url = \"http://127.0.0.1:50001/mcp\""));
        assert!(!updated.contains("command = \"forgefleetd\""));
        assert!(!updated.contains("--stdio"));
    }

    #[test]
    fn codex_local_http_install_is_byte_idempotent_and_preserves_other_sections() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "model = \"gpt-5\"\n\n[mcp_servers.other]\nurl = \"https://other.example/mcp\"\n",
        )
        .unwrap();

        upsert_codex_mcp(&config, "forgefleet", "http://localhost:50001/mcp", false).unwrap();
        let first = std::fs::read_to_string(&config).unwrap();
        upsert_codex_mcp(&config, "forgefleet", "http://localhost:50001/mcp", false).unwrap();
        let second = std::fs::read_to_string(config).unwrap();

        assert_eq!(second, first);
        assert!(second.contains("[mcp_servers.other]"));
        assert!(second.contains("url = \"https://other.example/mcp\""));
        assert_eq!(second.matches("[mcp_servers.forgefleet]").count(), 1);
    }

    #[test]
    fn codex_explicit_remote_url_keeps_native_http_shape() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".codex").join("config.toml");

        upsert_codex_mcp(&config, "forgefleet", "https://fleet.example/mcp", false).unwrap();

        let updated = std::fs::read_to_string(config).unwrap();
        assert!(updated.contains("type = \"http\""));
        assert!(updated.contains("url = \"https://fleet.example/mcp\""));
        assert!(!updated.contains("command ="));
    }

    #[test]
    fn all_targets_include_gemini() {
        assert!(resolve_targets("all").contains(&"gemini"));
    }

    #[test]
    fn local_cli_installers_replace_stdio_with_native_http() {
        let temp = tempfile::tempdir().unwrap();
        let local_url = "http://localhost:50001/mcp";
        for relative in [
            ".claude/settings.json",
            ".kimi-code/mcp.json",
            ".gemini/settings.json",
        ] {
            let config = temp.path().join(relative);
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            std::fs::write(
                config,
                r#"{"mcpServers":{"forgefleet":{"command":"forgefleetd","args":["mcp","--stdio"]}}}"#,
            )
            .unwrap();
        }

        install_claude_code(temp.path(), local_url, false, false).unwrap();
        install_kimi(temp.path(), local_url, false, false).unwrap();
        install_gemini(temp.path(), local_url, false, false).unwrap();

        for relative in [
            ".claude/settings.json",
            ".kimi-code/mcp.json",
            ".gemini/settings.json",
        ] {
            let config = temp.path().join(relative);
            let contents = std::fs::read_to_string(&config).unwrap();
            assert!(!contents.contains("--stdio"), "{}", config.display());
            assert_eq!(
                config_forgefleet_transport(&config),
                Some("http"),
                "{}",
                config.display()
            );
        }
    }

    #[test]
    fn claude_desktop_keeps_http_bridge_transport() {
        let temp = tempfile::tempdir().unwrap();
        install_claude_desktop(temp.path(), "http://localhost:50001/mcp", false).unwrap();
        let config = claude_desktop_config_path(temp.path());
        assert_eq!(config_forgefleet_transport(&config), Some("http_bridge"));
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
