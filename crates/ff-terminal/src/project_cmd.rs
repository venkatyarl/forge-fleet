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
        crate::ProjectCommand::Repo { command } => handle_repo(&pool, command).await?,
        crate::ProjectCommand::Folder { command } => handle_folder(&pool, command).await?,
        crate::ProjectCommand::Discover { path, project } => {
            handle_discover(&pool, &path, project).await?
        }
    }
    Ok(())
}

async fn handle_repo(pool: &sqlx::PgPool, cmd: crate::ProjectRepoCommand) -> Result<()> {
    match cmd {
        crate::ProjectRepoCommand::List { project } => {
            let repos = ff_db::pm::pg_list_project_repos(pool, &project)
                .await
                .map_err(|e| anyhow::anyhow!("list repos: {e}"))?;
            if repos.is_empty() {
                println!("(no repos for project '{project}')");
                return Ok(());
            }
            println!("{:<38} {:<7} {:<8} REPO", "ID", "PRIMARY", "ROLE");
            for r in &repos {
                println!(
                    "{:<38} {:<7} {:<8} {}",
                    r.id,
                    if r.is_primary { "✓" } else { "" },
                    r.role.as_deref().unwrap_or("-"),
                    r.github_url
                );
            }
        }
        crate::ProjectRepoCommand::Add {
            project,
            url,
            name,
            branch,
            role,
            primary,
        } => {
            let r = ff_db::pm::pg_add_project_repo(
                pool,
                &project,
                &url,
                name.as_deref(),
                &branch,
                role.as_deref(),
                primary,
            )
            .await
            .map_err(|e| anyhow::anyhow!("add repo: {e}"))?;
            println!("{GREEN}✓ repo attached{RESET}  {} → {}", r.id, r.github_url);
        }
        crate::ProjectRepoCommand::Rm { id } => {
            let removed = ff_db::pm::pg_delete_project_repo(pool, &id)
                .await
                .map_err(|e| anyhow::anyhow!("rm repo: {e}"))?;
            if removed {
                println!("{GREEN}✓ removed{RESET} {id}");
            } else {
                println!("{YELLOW}no repo with id {id}{RESET}");
            }
        }
    }
    Ok(())
}

async fn handle_folder(pool: &sqlx::PgPool, cmd: crate::ProjectFolderCommand) -> Result<()> {
    match cmd {
        crate::ProjectFolderCommand::List { project } => {
            let folders = ff_db::pm::pg_list_project_folders(pool, &project)
                .await
                .map_err(|e| anyhow::anyhow!("list folders: {e}"))?;
            if folders.is_empty() {
                println!("(no folders for project '{project}')");
                return Ok(());
            }
            println!(
                "{:<38} {:<10} {:<7} {:<8} PATH",
                "ID", "HOST", "PRIMARY", "ROLE"
            );
            for f in &folders {
                println!(
                    "{:<38} {:<10} {:<7} {:<8} {}",
                    f.id,
                    f.computer_name.as_deref().unwrap_or("(all)"),
                    if f.is_primary { "✓" } else { "" },
                    f.role.as_deref().unwrap_or("-"),
                    f.path
                );
            }
        }
        crate::ProjectFolderCommand::Add {
            project,
            path,
            host,
            role,
            primary,
        } => {
            let computer_id = match host.as_deref() {
                Some(h) => Some(resolve_computer_id(pool, h).await?),
                None => None,
            };
            let f = ff_db::pm::pg_add_project_folder(
                pool,
                &project,
                computer_id.as_deref(),
                &path,
                role.as_deref(),
                primary,
            )
            .await
            .map_err(|e| anyhow::anyhow!("add folder: {e}"))?;
            println!("{GREEN}✓ folder attached{RESET}  {} → {}", f.id, f.path);
        }
        crate::ProjectFolderCommand::Rm { id } => {
            let removed = ff_db::pm::pg_delete_project_folder(pool, &id)
                .await
                .map_err(|e| anyhow::anyhow!("rm folder: {e}"))?;
            if removed {
                println!("{GREEN}✓ removed{RESET} {id}");
            } else {
                println!("{YELLOW}no folder with id {id}{RESET}");
            }
        }
    }
    Ok(())
}

/// Resolve a computer name → its UUID (case-insensitive).
async fn resolve_computer_id(pool: &sqlx::PgPool, name: &str) -> Result<String> {
    let row: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT id FROM computers WHERE LOWER(name) = LOWER($1)")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("resolve computer '{name}': {e}"))?;
    row.map(|r| r.0.to_string())
        .ok_or_else(|| anyhow::anyhow!("no computer named '{name}'"))
}

/// Auto-discover a project's repos + local folders: scan `path` (and its
/// immediate subdirs, for a polyrepo layout) for git repos, read each one's
/// `origin` remote, and register a project_repos row (the remote) + a
/// project_folders row (the local clone on THIS host). Section I of the PM
/// platform plan — replaces the manual "re-point the project at the repos" step.
async fn handle_discover(pool: &sqlx::PgPool, path: &str, project: Option<String>) -> Result<()> {
    use std::path::Path;
    let root = expand_tilde(path);
    let root = Path::new(&root);
    if !root.is_dir() {
        return Err(anyhow::anyhow!("not a directory: {}", root.display()));
    }

    // Resolve project id: explicit arg → .forgefleet/project.toml marker →
    // directory name lowercased.
    let project_id = match project {
        Some(p) => p,
        None => read_project_marker(root).unwrap_or_else(|| {
            root.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project")
                .to_lowercase()
        }),
    };

    // Ensure the project row exists (minimal) so the repo/folder FKs resolve.
    let display = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&project_id);
    sqlx::query(
        "INSERT INTO projects (id, display_name) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&project_id)
    .bind(display)
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("ensure project '{project_id}': {e}"))?;

    let this_host = ff_agent::fleet_info::resolve_this_worker_name().await;
    let computer_id = resolve_computer_id(pool, &this_host).await.ok();

    // Candidate dirs: the root itself + immediate subdirs (polyrepo layout).
    let mut candidates = vec![root.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                candidates.push(e.path());
            }
        }
    }

    println!(
        "{CYAN}▶ discovering git repos under {} → project '{project_id}' (host {this_host}){RESET}",
        root.display()
    );
    let mut found = 0;
    for dir in candidates {
        if !dir.join(".git").exists() {
            continue;
        }
        let remote = git_origin(&dir);
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        let role = name.as_deref().and_then(infer_role);
        // primary = the dir whose name matches the project id (best guess).
        let primary = name
            .as_deref()
            .map(|n| n.eq_ignore_ascii_case(&project_id))
            .unwrap_or(false);

        // Register the local folder (always — we know the path on this host).
        if let Err(e) = ff_db::pm::pg_add_project_folder(
            pool,
            &project_id,
            computer_id.as_deref(),
            &dir.to_string_lossy(),
            Some("source"),
            primary,
        )
        .await
        {
            println!("  {YELLOW}folder {} skipped: {e}{RESET}", dir.display());
        }

        // Register the GitHub repo if it has an origin remote.
        match remote {
            Some(url) => {
                match ff_db::pm::pg_add_project_repo(
                    pool,
                    &project_id,
                    &url,
                    name.as_deref(),
                    "main",
                    role.as_deref(),
                    primary,
                )
                .await
                {
                    Ok(_) => {
                        found += 1;
                        println!("  {GREEN}✓{RESET} {} → {url}", dir.display());
                    }
                    Err(e) => println!("  {YELLOW}repo {} skipped: {e}{RESET}", dir.display()),
                }
            }
            None => println!(
                "  {YELLOW}• {} (folder only — no git origin){RESET}",
                dir.display()
            ),
        }
    }
    println!("{GREEN}✓ discovered {found} repo(s) for '{project_id}'{RESET}");
    Ok(())
}

/// Read `.forgefleet/project.toml`'s `project_id` if present (the thin
/// folder→project pointer; section K).
fn read_project_marker(dir: &std::path::Path) -> Option<String> {
    let marker = dir.join(".forgefleet").join("project.toml");
    let body = std::fs::read_to_string(marker).ok()?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("project_id") {
            return rest
                .trim_start_matches([' ', '=', '"'])
                .trim_end_matches('"')
                .split('"')
                .next()
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty());
        }
    }
    None
}

/// `git -C <dir> remote get-url origin`, trimmed; `None` if no origin.
fn git_origin(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

/// Best-effort role inference from a repo/folder name.
fn infer_role(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    if n.contains("api") || n.contains("backend") || n.contains("server") {
        Some("api".into())
    } else if n.contains("web") || n.contains("app") || n.contains("frontend") || n.contains("ui") {
        Some("web".into())
    } else if n.contains("infra") || n.contains("deploy") || n.contains("ops") {
        Some("infra".into())
    } else if n.contains("doc") {
        Some("docs".into())
    } else {
        Some("code".into())
    }
}

/// Expand a leading `~` to $HOME.
fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().to_string();
    }
    p.to_string()
}
