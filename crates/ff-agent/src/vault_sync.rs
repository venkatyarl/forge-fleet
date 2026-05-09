//! Fleet Memory — Vault Sync (Phase 16)
//!
//! Syncs ForgeFleet activity into the Obsidian vault:
//! - ForgeFleet/Index.md auto-generation
//! - Daily Notes/YYYY/YYYY-MM/YYYY-MM-DD.md append
//! - TODO scanning and management
//! - Vault file indexing in Postgres

use anyhow::Result;
use sqlx::{PgPool, Row};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::info;

const INDEX_TEMPLATE: &str = r#"---
generated_by: {agent}
generated_at: {timestamp}
vault_version: {version}
vault_files: {file_count}
vault_links: {link_count}
---

# ForgeFleet Vault Index

## Active Projects
| Project | Status | Last Activity | Key Notes |
|---------|--------|---------------|-----------|
{projects}

## Today's Activity ({today})
{activity}

## Open Tasks
{tasks}

## Recent Changes (last 7 days)
{changes}

## Directory Map
- **ForgeFleet/** — Fleet memory & configuration (FF-managed)
- **Projects/** — Active projects (mixed user + FF)
- **Daily Notes/** — Daily activity logs
- **Electronics/Computers/** — Hardware inventory

## Computers Status
| Node | Role | Status | Load |
|------|------|--------|------|
{nodes}
"#;

/// Auto-generate ForgeFleet/Index.md
pub async fn regenerate_index_md(
    vault_path: &Path,
    agent_id: &str,
    pg: &PgPool,
) -> Result<PathBuf> {
    let forgefleet_dir = vault_path.join("ForgeFleet");
    fs::create_dir_all(&forgefleet_dir).await?;

    let index_path = forgefleet_dir.join("Index.md");

    // Gather stats from Postgres
    let file_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vault_files")
        .fetch_one(pg)
        .await
        .unwrap_or(0);

    let link_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vault_links")
        .fetch_one(pg)
        .await
        .unwrap_or(0);

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Query active projects
    let projects_rows = sqlx::query(
        "SELECT id, display_name, status, main_last_synced_at FROM projects WHERE status = 'active' ORDER BY display_name LIMIT 20"
    )
    .fetch_all(pg).await.unwrap_or_default();
    let projects_md = if projects_rows.is_empty() {
        "| *No active projects* | | | |".to_string()
    } else {
        projects_rows
            .iter()
            .map(|r| {
                let id: String = r.get("id");
                let name: String = r.get("display_name");
                let status: String = r.get("status");
                let last: Option<chrono::DateTime<chrono::Utc>> = r.get("main_last_synced_at");
                let last_str = last
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "—".to_string());
                format!("| {} | {} | {} | {} |", id, name, status, last_str)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Query today's activity (completed fleet_tasks)
    let activity_rows = sqlx::query(
        "SELECT summary, status, completed_at FROM fleet_tasks WHERE completed_at > NOW() - INTERVAL '24 hours' ORDER BY completed_at DESC LIMIT 10"
    )
    .fetch_all(pg).await.unwrap_or_default();
    let activity_md = if activity_rows.is_empty() {
        "- *No activity in the last 24h*".to_string()
    } else {
        activity_rows
            .iter()
            .map(|r| {
                let summary: String = r.get("summary");
                let status: String = r.get("status");
                format!("- [{}] {}", status, summary)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Query open tasks (pending fleet_tasks + unfinished vault_todos)
    let task_rows = sqlx::query(
        "SELECT summary, status, created_at FROM fleet_tasks WHERE status IN ('pending', 'running', 'claimed') ORDER BY priority DESC, created_at LIMIT 10"
    )
    .fetch_all(pg).await.unwrap_or_default();
    let tasks_md = if task_rows.is_empty() {
        "- [ ] *No open fleet tasks*".to_string()
    } else {
        task_rows
            .iter()
            .map(|r| {
                let summary: String = r.get("summary");
                let status: String = r.get("status");
                let mark = if status == "completed" { "x" } else { " " };
                format!("- [{}] {} ({})", mark, summary, status)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Query recent changes (vault files modified in last 7 days)
    let change_rows = sqlx::query(
        "SELECT file_path, updated_at FROM vault_files WHERE updated_at > NOW() - INTERVAL '7 days' ORDER BY updated_at DESC LIMIT 10"
    )
    .fetch_all(pg).await.unwrap_or_default();
    let changes_md = if change_rows.is_empty() {
        "- *No vault changes in the last 7 days*".to_string()
    } else {
        change_rows
            .iter()
            .map(|r| {
                let path: String = r.get("file_path");
                let updated: Option<chrono::DateTime<chrono::Utc>> = r.get("updated_at");
                let date = updated
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "—".to_string());
                format!("- `{}` ({})", path, date)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Query computer/node status
    let node_rows =
        sqlx::query("SELECT name, role, status, cpu_percent FROM computers ORDER BY name LIMIT 30")
            .fetch_all(pg)
            .await
            .unwrap_or_default();
    let nodes_md = if node_rows.is_empty() {
        "| *No nodes registered* | | | |".to_string()
    } else {
        node_rows
            .iter()
            .map(|r| {
                let name: String = r.get("name");
                let role: String = r.get("role");
                let status: String = r.get("status");
                let load: Option<f64> = r.get("cpu_percent");
                let load_str = load
                    .map(|l| format!("{:.1}%", l))
                    .unwrap_or_else(|| "—".to_string());
                format!("| {} | {} | {} | {} |", name, role, status, load_str)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = INDEX_TEMPLATE
        .replace("{agent}", agent_id)
        .replace("{timestamp}", &chrono::Utc::now().to_rfc3339())
        .replace("{version}", "1")
        .replace("{file_count}", &file_count.to_string())
        .replace("{link_count}", &link_count.to_string())
        .replace("{today}", &today)
        .replace("{projects}", &projects_md)
        .replace("{activity}", &activity_md)
        .replace("{tasks}", &tasks_md)
        .replace("{changes}", &changes_md)
        .replace("{nodes}", &nodes_md);

    fs::write(&index_path, content).await?;
    info!(path = %index_path.display(), "Index.md regenerated");

    Ok(index_path)
}

/// Append FF activity to Daily Notes.
pub async fn append_daily_note(
    vault_path: &Path,
    session_id: &str,
    tasks_completed: u32,
    notes_created: u32,
    notes_modified: u32,
) -> Result<PathBuf> {
    let now = chrono::Local::now();
    let year = now.format("%Y").to_string();
    let month = now.format("%Y-%m").to_string();
    let day = now.format("%Y-%m-%d").to_string();

    let daily_dir = vault_path.join("Daily Notes").join(&year).join(&month);
    fs::create_dir_all(&daily_dir).await?;

    let note_path = daily_dir.join(format!("{}.md", day));

    let section = format!(
        r#"
## ForgeFleet Activity (auto-generated)

**Session**: {session}
**Tasks**: {completed} completed, {notes_created} notes created, {notes_modified} modified

### Completed
- ✅ Session {session}: {completed} tasks completed

### New Notes
- {notes_created} notes created

### Modified Notes
- {notes_modified} notes modified

---
"#,
        session = session_id,
        completed = tasks_completed,
        notes_created = notes_created,
        notes_modified = notes_modified,
    );

    if note_path.exists() {
        let existing = fs::read_to_string(&note_path).await?;
        if !existing.contains("ForgeFleet Activity") {
            fs::write(&note_path, format!("{}\n{}", existing.trim_end(), section)).await?;
        }
    } else {
        let frontmatter = format!(
            "---\nff_activity: true\nff_session: {}\nff_tasks_completed: {}\nff_notes_created: {}\n---\n\n# {}\n",
            session_id, tasks_completed, notes_created, day
        );
        fs::write(&note_path, format!("{}\n{}", frontmatter, section)).await?;
    }

    info!(path = %note_path.display(), "daily note updated");
    Ok(note_path)
}

/// Scan vault for TODOs and sync to Postgres vault_todos table.
pub async fn scan_vault_todos(vault_path: &Path, pg: &PgPool) -> Result<u32> {
    let mut count = 0u32;

    let mut entries = fs::read_dir(vault_path).await?;
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let path = entry.path();

                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if name.starts_with('.') || name == "node_modules" || name == "target" {
                        continue;
                    }
                    continue;
                }

                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }

                let content = match fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let rel_path = path.strip_prefix(vault_path).unwrap_or(&path);
                let rel_path_str = rel_path.to_string_lossy().to_string();

                for (line_num, line) in content.lines().enumerate() {
                    let trimmed: &str = line.trim();
                    if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") {
                        let todo_text = trimmed[5..].trim().to_string();
                        let done = trimmed.starts_with("- [x]");

                        let _ = sqlx::query(
                            r#"
                            INSERT INTO vault_todos (file_path, todo_text, done, line_number)
                            VALUES ($1, $2, $3, $4)
                            ON CONFLICT (file_path, todo_text) DO UPDATE
                            SET done = EXCLUDED.done,
                                line_number = EXCLUDED.line_number
                            "#,
                        )
                        .bind(&rel_path_str)
                        .bind(&todo_text)
                        .bind(done)
                        .bind(line_num as i32)
                        .execute(pg)
                        .await;

                        count += 1;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    info!(todos_found = count, "vault TODO scan complete");
    Ok(count)
}

/// Create the ForgeFleet/ directory structure in the vault.
pub async fn setup_forgefleet_vault(vault_path: &Path) -> Result<()> {
    let forgefleet = vault_path.join("ForgeFleet");

    for dir in &[
        "Hive Mind",
        "Brain",
        "Computers",
        "Projects",
        "Agents",
        "Templates",
    ] {
        fs::create_dir_all(forgefleet.join(dir)).await?;
    }

    info!(path = %forgefleet.display(), "ForgeFleet vault structure created");
    Ok(())
}
