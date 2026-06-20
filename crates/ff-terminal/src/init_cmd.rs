use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::{CYAN, GREEN, RESET};

const BEGIN_MARKER: &str = "<!-- ff:begin -->";
const END_MARKER: &str = "<!-- ff:end -->";
const CLAUDE_SESSION_START_COMMAND: &str = "ff capabilities";

pub async fn handle_init(global: bool, project: Option<String>) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    let capabilities_path = home.join(".forgefleet").join("CAPABILITIES.md");
    write_file(&capabilities_path, capabilities_doc())?;
    println!(
        "{GREEN}✓{RESET} refreshed {CYAN}{}{RESET}",
        capabilities_path.display()
    );

    let targets = if global {
        vec![
            home.join(".claude").join("CLAUDE.md"),
            home.join(".codex").join("AGENTS.md"),
        ]
    } else {
        vec![PathBuf::from("CLAUDE.md"), PathBuf::from("AGENTS.md")]
    };

    let block = managed_block(project.as_deref());
    for target in targets {
        inject_managed_block(&target, &block)?;
        println!("{GREEN}✓{RESET} updated {CYAN}{}{RESET}", target.display());
    }

    install_mcp_best_effort();
    install_claude_session_start_hook_best_effort(&home);

    Ok(())
}

fn capabilities_doc() -> &'static str {
    "# ForgeFleet Capabilities\n\
\n\
## The ForgeFleet Way\n\
\n\
- Orchestrate-first: use ff to coordinate work before reaching for one-off local commands.\n\
- Decompose -> dispatch -> review -> merge: split work into focused pieces, send them to the fleet, review the outputs, then integrate deliberately.\n\
- Local-first + parallel: prefer warm local fleet capacity and run independent work concurrently.\n\
- Dispatch reviews with reviewer != builder: have a different model or worker review substantial generated changes.\n\
- NO LLM attribution in commits/PRs: do not add attribution lines.\n\
- Verify-before-merge: run the relevant checks before considering work complete.\n\
- Resume-first: continue existing sessions, queues, and artifacts before starting duplicate work.\n\
\n\
## Key verbs\n\
\n\
- `ff pm`\n\
- `ff run`\n\
- `ff offload`\n\
- `ff research`\n\
- `ff cli`\n\
- `ff fleet`\n\
- `ff cortex`\n\
- `ff brain`\n\
- `ff model`\n\
- `ff project`\n"
}

fn managed_block(project: Option<&str>) -> String {
    let mut content = String::from(
        "This project uses ForgeFleet (ff). Read ~/.forgefleet/CAPABILITIES.md for the full surface. Route work through ff and follow The ForgeFleet Way (orchestrate-first, dispatch to the fleet, dispatch reviews).",
    );

    if let Some(project) = project.filter(|value| !value.trim().is_empty()) {
        content.push_str("\nProject id: ");
        content.push_str(project.trim());
    }

    format!("{BEGIN_MARKER}\n{content}\n{END_MARKER}\n")
}

fn inject_managed_block(path: &Path, block: &str) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };

    let updated = replace_or_append_block(&existing, block);
    write_file(path, &updated)
}

fn replace_or_append_block(existing: &str, block: &str) -> String {
    if let Some(begin) = existing.find(BEGIN_MARKER) {
        let search_from = begin + BEGIN_MARKER.len();
        if let Some(relative_end) = existing[search_from..].find(END_MARKER) {
            let end = search_from + relative_end + END_MARKER.len();
            let mut updated = String::with_capacity(existing.len() + block.len());
            updated.push_str(&existing[..begin]);
            updated.push_str(block);
            updated.push_str(&existing[end..]);
            return updated;
        }
    }

    let mut updated = existing.to_string();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(block);
    updated
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

fn install_mcp_best_effort() {
    let ff = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("ff init: could not resolve current ff binary for MCP install: {err}");
            return;
        }
    };

    match Command::new(&ff)
        .args(["mcp", "install", "--for", "all"])
        .status()
    {
        Ok(status) if status.success() => {
            println!("{GREEN}✓{RESET} activated MCP clients via {CYAN}ff mcp install{RESET}");
        }
        Ok(status) => {
            eprintln!("ff init: ff mcp install --for all exited with status {status}; continuing");
        }
        Err(err) => {
            eprintln!("ff init: failed to run ff mcp install --for all: {err}; continuing");
        }
    }
}

fn install_claude_session_start_hook_best_effort(home: &Path) {
    let settings_path = home.join(".claude").join("settings.json");
    match upsert_claude_session_start_hook(&settings_path) {
        Ok(true) => println!(
            "{GREEN}✓{RESET} installed Claude SessionStart hook in {CYAN}{}{RESET}",
            settings_path.display()
        ),
        Ok(false) => println!(
            "{GREEN}✓{RESET} Claude SessionStart hook already present in {CYAN}{}{RESET}",
            settings_path.display()
        ),
        Err(err) => eprintln!(
            "ff init: could not install Claude SessionStart hook at {}: {err}; continuing",
            settings_path.display()
        ),
    }
}

fn upsert_claude_session_start_hook(path: &Path) -> Result<bool> {
    let mut doc = read_json_object_or_empty(path)?;

    let session_start = doc
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}.hooks is not a JSON object", path.display()))?
        .entry("SessionStart")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("{}.hooks.SessionStart is not an array", path.display()))?;

    if session_start.iter().any(runs_ff_capabilities) {
        return Ok(false);
    }

    session_start.push(json!({
        "hooks": [
            {
                "type": "command",
                "command": CLAUDE_SESSION_START_COMMAND
            }
        ]
    }));

    let pretty = serde_json::to_string_pretty(&doc)?;
    write_file(path, &pretty)?;
    Ok(true)
}

fn read_json_object_or_empty(path: &Path) -> Result<Value> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };

    if content.trim().is_empty() {
        return Ok(json!({}));
    }

    serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))
}

fn runs_ff_capabilities(value: &Value) -> bool {
    value
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .map(command_runs_ff_capabilities)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn command_runs_ff_capabilities(command: &str) -> bool {
    command.split_whitespace().eq(["ff", "capabilities"])
}
