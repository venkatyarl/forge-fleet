use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::{CYAN, GREEN, RESET};

const BEGIN_MARKER: &str = "<!-- ff:begin -->";
const END_MARKER: &str = "<!-- ff:end -->";

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
