//! `ff agents` — manage the V112 fleet_agents catalog: list / show / import.
//!
//! Source of truth: Postgres `fleet_agents` table. This is the AGENTS analogue
//! of `ff skills` (V105 `skills` table); the two are deliberately parallel. The
//! crew / orchestrator instantiates agents FROM this catalog by name and routes
//! each one through the agent-swarm capability router (`pg_pick_agent_endpoint`).

use anyhow::{Result, anyhow};
use clap::Subcommand;
use ff_agent::agents_db;
use std::path::PathBuf;

#[derive(Debug, Clone, Subcommand)]
pub enum AgentsCommand {
    /// List agents in the catalog (one row per agent).
    List {
        /// Include disabled agents too (default: enabled only).
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Filter by source (e.g. forgefleet).
        #[arg(long)]
        source: Option<String>,
    },
    /// Print the full definition (incl. system prompt) of one agent.
    Show {
        /// Agent name as it appears in the catalog (e.g. code-writer).
        name: String,
    },
    /// Import AGENT.md files from a local directory tree into the catalog.
    Import {
        /// Path to the directory tree containing AGENT.md files.
        path: PathBuf,
        /// Source identifier recorded on each row.
        #[arg(long, default_value = "imported")]
        source: String,
        /// Optional upstream URL recorded on the row.
        #[arg(long)]
        source_url: Option<String>,
    },
    /// Enable an agent so the crew/router will consider it.
    Enable { name: String },
    /// Disable an agent without deleting it.
    Disable { name: String },
}

pub async fn handle_agents(cmd: AgentsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow!("connect Postgres: {e}"))?;

    match cmd {
        AgentsCommand::List { all, source } => list_cmd(&pool, all, source).await,
        AgentsCommand::Show { name } => show_cmd(&pool, &name).await,
        AgentsCommand::Import {
            path,
            source,
            source_url,
        } => import_cmd(&pool, &path, &source, source_url.as_deref()).await,
        AgentsCommand::Enable { name } => set_enabled_cmd(&pool, &name, true).await,
        AgentsCommand::Disable { name } => set_enabled_cmd(&pool, &name, false).await,
    }
}

async fn list_cmd(pool: &sqlx::PgPool, all: bool, source: Option<String>) -> Result<()> {
    let rows = agents_db::list_all(pool, !all).await?;
    let filtered: Vec<_> = rows
        .into_iter()
        .filter(|a| match &source {
            Some(s) => &a.source == s,
            None => true,
        })
        .collect();

    println!(
        "{:<16} {:<18} {:<10} {:<8} {:<3} DESCRIPTION",
        "NAME", "ROLE", "SOURCE", "MIN_CTX", "EN"
    );
    for a in &filtered {
        let desc = a.description.as_deref().unwrap_or("");
        let desc = if desc.chars().count() > 52 {
            let t: String = desc.chars().take(52).collect();
            format!("{t}…")
        } else {
            desc.to_string()
        };
        println!(
            "{:<16} {:<18} {:<10} {:<8} {:<3} {}",
            truncate(&a.name, 16),
            truncate(&a.role, 18),
            truncate(&a.source, 10),
            a.min_ctx,
            if a.enabled { "y" } else { "n" },
            desc
        );
    }
    println!();
    println!("{} agent(s)", filtered.len());
    Ok(())
}

async fn show_cmd(pool: &sqlx::PgPool, name: &str) -> Result<()> {
    let Some(a) = agents_db::get_by_name(pool, name).await? else {
        return Err(anyhow!("no agent named '{name}' in the catalog"));
    };
    println!("# {} ({})", a.name, a.role);
    println!("id:                   {}", a.id);
    println!(
        "description:          {}",
        a.description.as_deref().unwrap_or("-")
    );
    println!("source:               {}", a.source);
    if let Some(url) = &a.source_url {
        println!("source_url:           {url}");
    }
    println!("enabled:              {}", a.enabled);
    println!("require_tool_calling: {}", a.require_tool_calling);
    println!("min_ctx:              {}", a.min_ctx);
    println!(
        "allowed_tools:        {}",
        agents_db::allowed_tools(&a).join(", ")
    );
    println!(
        "triggers:             {}",
        agents_db::triggers(&a).join(", ")
    );
    println!();
    println!("--- system_prompt ---");
    println!("{}", a.system_prompt);
    Ok(())
}

async fn import_cmd(
    pool: &sqlx::PgPool,
    path: &std::path::Path,
    source: &str,
    source_url: Option<&str>,
) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("path does not exist: {}", path.display()));
    }
    let (imported, errors) = agents_db::import_repo_agents(pool, path, source, source_url).await?;
    println!("import summary: imported/updated={imported} errors={errors}");
    Ok(())
}

async fn set_enabled_cmd(pool: &sqlx::PgPool, name: &str, enabled: bool) -> Result<()> {
    let changed = ff_db::pg_set_agent_enabled(pool, name, enabled)
        .await
        .map_err(|e| anyhow!("update fleet_agents: {e}"))?;
    if changed {
        println!(
            "{} agent '{name}'",
            if enabled { "enabled" } else { "disabled" }
        );
    } else {
        return Err(anyhow!("no agent named '{name}' in the catalog"));
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}
