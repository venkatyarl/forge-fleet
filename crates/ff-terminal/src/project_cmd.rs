use crate::{CYAN, GREEN, RED, RESET, YELLOW};
use anyhow::Result;

pub async fn handle_project(cmd: crate::ProjectCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::ProjectCommand::List => {
            let rows: Vec<(
                String,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
                String,
            )> = sqlx::query_as(
                "SELECT id, display_name, repo_url, default_branch, main_commit_sha, \
                        main_last_synced_at, status \
                 FROM projects ORDER BY id",
            )
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("list projects: {e}"))?;

            if rows.is_empty() {
                println!("(no projects — run `ff project seed` to load config/projects.toml)");
                return Ok(());
            }

            println!(
                "{:<14} {:<14} {:<8} {:<10} {:<18} REPO",
                "ID", "NAME", "BRANCH", "SHA", "SYNCED"
            );
            let now = chrono::Utc::now();
            for (id, name, repo, branch, sha, synced, _status) in rows {
                let sha_s = sha.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("-");
                let synced_s = match synced {
                    Some(t) => {
                        let age = now.signed_duration_since(t);
                        if age.num_days() > 0 {
                            format!("{}d ago", age.num_days())
                        } else if age.num_hours() > 0 {
                            format!("{}h ago", age.num_hours())
                        } else if age.num_minutes() > 0 {
                            format!("{}m ago", age.num_minutes())
                        } else {
                            "just now".to_string()
                        }
                    }
                    None => "never".to_string(),
                };
                println!(
                    "{:<14} {:<14} {:<8} {:<10} {:<18} {}",
                    id,
                    name,
                    branch,
                    sha_s,
                    synced_s,
                    repo.as_deref().unwrap_or("-"),
                );
            }
        }
        crate::ProjectCommand::Status { id } => {
            let project: Option<(
                String,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT id, display_name, repo_url, default_branch, \
                        main_commit_sha, main_commit_message, main_committed_at, main_committed_by \
                 FROM projects WHERE id = $1",
            )
            .bind(&id)
            .fetch_optional(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query project: {e}"))?;

            let Some((id, name, repo, branch, sha, msg, committed_at, committed_by)) = project
            else {
                return Err(anyhow::anyhow!("project '{id}' not found"));
            };

            println!("{CYAN}Project{RESET} {id} — {name}");
            println!("  repo:          {}", repo.as_deref().unwrap_or("-"));
            println!("  default branch: {branch}");
            println!(
                "  main:          {} — {}",
                sha.as_deref().unwrap_or("-"),
                msg.as_deref().unwrap_or("-")
            );
            if let Some(at) = committed_at {
                println!(
                    "  committed:     {} by {}",
                    at.format("%Y-%m-%d %H:%M UTC"),
                    committed_by.as_deref().unwrap_or("-")
                );
            }

            let branches: Vec<(
                String,
                Option<String>,
                Option<i32>,
                Option<String>,
                Option<String>,
                String,
            )> = sqlx::query_as(
                "SELECT branch_name, last_commit_sha, pr_number, pr_state, pr_url, status \
                 FROM project_branches WHERE project_id = $1 \
                 ORDER BY branch_name",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query branches: {e}"))?;

            if !branches.is_empty() {
                println!();
                println!("{CYAN}Branches ({}){RESET}", branches.len());
                println!(
                    "  {:<30} {:<10} {:<6} {:<8} PR URL",
                    "BRANCH", "SHA", "PR#", "PR STATE"
                );
                for (br, sha, num, st, url, _status) in branches {
                    let sha_s = sha.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("-");
                    let num_s = num.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
                    println!(
                        "  {:<30} {:<10} {:<6} {:<8} {}",
                        br,
                        sha_s,
                        num_s,
                        st.as_deref().unwrap_or("-"),
                        url.as_deref().unwrap_or("-"),
                    );
                }
            }

            let envs: Vec<(
                String,
                Option<String>,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
            )> = sqlx::query_as(
                "SELECT name, deployed_commit_sha, health_status, deployed_at \
                     FROM project_environments WHERE project_id = $1 \
                     ORDER BY name",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query environments: {e}"))?;

            if !envs.is_empty() {
                println!();
                println!("{CYAN}Environments ({}){RESET}", envs.len());
                for (name, sha, health, deployed_at) in envs {
                    let sha_s = sha.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("-");
                    let deployed_s = deployed_at
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "  {:<14} sha={sha_s:<10} health={} deployed={deployed_s}",
                        name,
                        health.as_deref().unwrap_or("-"),
                    );
                }
            }

            let ci: Vec<(
                String,
                String,
                String,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT branch_name, commit_sha, status, started_at, run_url \
                     FROM project_ci_runs WHERE project_id = $1 \
                     ORDER BY started_at DESC NULLS LAST LIMIT 5",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("query ci runs: {e}"))?;

            if !ci.is_empty() {
                println!();
                println!("{CYAN}Recent CI runs{RESET}");
                for (br, sha, st, at, url) in ci {
                    let sha_s = &sha[..sha.len().min(8)];
                    let at_s = at
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "  {:<30} {sha_s:<10} {st:<10} {at_s:<18} {}",
                        br,
                        url.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        crate::ProjectCommand::Sync { all: _ } => {
            // Today `--all` is the only behavior; we leave the flag in the schema so a
            // future single-project sync can coexist without breaking callers.
            println!("{CYAN}▶ Syncing projects from GitHub...{RESET}");
            let sync = ff_agent::project_github_sync::GitHubSync::new(pool.clone());
            let report = sync
                .sync_all_projects()
                .await
                .map_err(|e| anyhow::anyhow!("github sync: {e}"))?;
            println!("  total:              {}", report.total);
            println!("  main updated:       {}", report.updated_main);
            println!("  branches upserted:  {}", report.branches_upserted);
            println!("  PRs attached:       {}", report.prs_attached);
            println!("  skipped (no repo):  {}", report.skipped_no_repo);
            println!("  skipped (bad url):  {}", report.skipped_bad_url);
            if !report.missing_repos.is_empty() {
                println!(
                    "  {}missing on GitHub:{} {}",
                    YELLOW,
                    RESET,
                    report.missing_repos.join(", ")
                );
            }
            if !report.errors.is_empty() {
                println!("{RED}  errors:{RESET}");
                for (pid, msg) in &report.errors {
                    println!("    [{pid}] {msg}");
                }
            } else {
                println!("{GREEN}✓ Done{RESET}");
            }
        }
    }
    Ok(())
}
