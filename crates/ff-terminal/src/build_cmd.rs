use crate::{CYAN, GREEN, RESET, YELLOW};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
struct PlannedTask {
    title: String,
    description: String,
}

pub async fn handle_build(
    goal: String,
    project: Option<String>,
    cwd: Option<PathBuf>,
    repo: Option<String>,
) -> Result<()> {
    let project = project.unwrap_or_else(|| "forge-fleet".to_string());
    let goal = goal.trim().to_string();
    if goal.is_empty() {
        return Err(anyhow!("goal cannot be empty"));
    }

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow!("run_postgres_migrations: {e}"))?;

    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1)")
            .bind(&project)
            .fetch_one(&pool)
            .await
            .map_err(|e| anyhow!("query project: {e}"))?;
    if !project_exists {
        return Err(anyhow!(
            "unknown project '{project}' - run `ff project seed` or pass --project <id>"
        ));
    }

    let repo_context =
        crate::repo_context::resolve_repo_context(&pool, &project, cwd, repo.as_deref()).await?;
    let planner_prompt = planner_prompt(&goal, repo_context.as_ref());
    println!("{CYAN}▶ Decomposing goal for project `{project}`...{RESET}");
    if let Some(ctx) = &repo_context {
        println!(
            "  target repo: {} ({}, {})",
            ctx.repo_url.as_deref().unwrap_or("unknown"),
            ctx.repo_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "no local path".to_string()),
            ctx.primary_language
        );
    }
    let plan = ff_agent::fleet_oneshot::fleet_oneshot(
        &pool,
        &planner_prompt,
        None,
        Some(Duration::from_secs(180)),
    )
    .await
    .map_err(|e| anyhow!("decompose goal: {e}"))?;

    let tasks = parse_tasks(&plan.text).with_context(|| {
        format!(
            "parse planner JSON from {} ({})",
            plan.worker_name, plan.model
        )
    })?;
    if tasks.is_empty() {
        return Err(anyhow!("planner returned no tasks"));
    }

    println!(
        "{GREEN}✓ Planner returned {} task(s){RESET} ({}, {}ms)",
        tasks.len(),
        plan.worker_name,
        plan.latency_ms
    );

    for task in tasks {
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO work_items \
                (project_id, kind, title, description, status, created_by, repo_id, repo_url, repo_path) \
             VALUES ($1, 'task', $2, $3, 'ready', 'ff build', $4, $5, $6) \
             RETURNING id",
        )
        .bind(&project)
        .bind(&task.title)
        .bind(&task.description)
        .bind(repo_context.as_ref().and_then(|ctx| ctx.repo_id))
        .bind(repo_context.as_ref().and_then(|ctx| ctx.repo_url.as_deref()))
        .bind(
            repo_context
                .as_ref()
                .and_then(|ctx| ctx.repo_path.as_ref())
                .map(|p| p.to_string_lossy().to_string()),
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| anyhow!("insert work_item '{}': {e}", task.title))?;

        println!("  {id}  {}", task.title);
    }

    println!(
        "{YELLOW}Note:{RESET} Pillar-4 scheduler will build, review, and open PRs for these ready work_items."
    );
    Ok(())
}

fn planner_prompt(goal: &str, repo_context: Option<&crate::repo_context::RepoContext>) -> String {
    let repo_block = repo_context
        .map(|ctx| format!("{}\n", ctx.prompt_block()))
        .unwrap_or_else(|| {
            "Target repository context:\n- unknown; infer cautiously from the goal and do not assume forge-fleet.\n\n".to_string()
        });
    format!(
        "Decompose this software build goal into 1 to 5 concrete leaf tasks.\n\
         Each task must be independently buildable by an autonomous coding agent.\n\
         Prefer small implementation tasks with clear verification scope.\n\
         Plan against the target repository context below. Do not use the \
         project's primary repository unless it is the target repository.\n\n\
         {repo_block}\
         Return ONLY a JSON array of objects in this exact shape:\n\
         [{{\"title\":\"...\",\"description\":\"...\"}}]\n\n\
         Goal:\n{goal}"
    )
}

fn parse_tasks(raw: &str) -> Result<Vec<PlannedTask>> {
    let block = first_json_array(raw).ok_or_else(|| anyhow!("no JSON array found"))?;
    let value: Value = serde_json::from_str(block).map_err(|e| anyhow!("invalid JSON: {e}"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("planner output was not a JSON array"))?;
    if arr.len() > 5 {
        return Err(anyhow!(
            "planner returned {} tasks; expected 1-5",
            arr.len()
        ));
    }

    let mut tasks = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("task {} missing non-empty title", idx + 1))?;
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("task {} missing non-empty description", idx + 1))?;

        tasks.push(PlannedTask {
            title: title.to_string(),
            description: description.to_string(),
        });
    }
    Ok(tasks)
}

fn first_json_array(raw: &str) -> Option<&str> {
    let start = raw.find('[')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in raw[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&raw[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tasks_extracts_first_array() {
        let raw = "Here:\n[{\"title\":\"A\",\"description\":\"B [ok]\"}]\nDone";
        let tasks = parse_tasks(raw).expect("parse tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "A");
        assert_eq!(tasks[0].description, "B [ok]");
    }

    #[test]
    fn parse_tasks_rejects_too_many() {
        let raw = r#"[
            {"title":"1","description":"d"},
            {"title":"2","description":"d"},
            {"title":"3","description":"d"},
            {"title":"4","description":"d"},
            {"title":"5","description":"d"},
            {"title":"6","description":"d"}
        ]"#;
        assert!(parse_tasks(raw).is_err());
    }

    #[test]
    fn planner_prompt_includes_target_repo_context() {
        let ctx = crate::repo_context::RepoContext {
            repo_id: None,
            repo_url: Some("https://github.com/acme/orders".into()),
            repo_path: Some(std::path::PathBuf::from("/tmp/orders")),
            primary_language: "Java".into(),
            build_system: Some("Maven".into()),
            key_dirs: vec!["src".into()],
        };
        let prompt = planner_prompt("add billing", Some(&ctx));
        assert!(prompt.contains("https://github.com/acme/orders"));
        assert!(prompt.contains("primary_language: Java"));
        assert!(prompt.contains("build_system: Maven"));
        assert!(!prompt.contains("forge-fleet Rust repo"));
    }
}
