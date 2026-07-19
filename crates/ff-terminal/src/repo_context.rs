use anyhow::{Context, Result, anyhow};
use sqlx::{PgPool, Row};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoContext {
    pub repo_id: Option<Uuid>,
    pub repo_url: Option<String>,
    pub repo_path: Option<PathBuf>,
    pub primary_language: String,
    pub build_system: Option<String>,
    pub key_dirs: Vec<String>,
}

impl RepoContext {
    pub fn prompt_block(&self) -> String {
        let repo = self.repo_url.as_deref().unwrap_or("unknown");
        let path = self
            .repo_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let build = self.build_system.as_deref().unwrap_or("unknown");
        let dirs = if self.key_dirs.is_empty() {
            "unknown".to_string()
        } else {
            self.key_dirs.join(", ")
        };
        format!(
            "Target repository context:\n\
             - repo_url: {repo}\n\
             - local_path: {path}\n\
             - primary_language: {}\n\
             - build_system: {build}\n\
             - key_dirs: {dirs}\n",
            self.primary_language
        )
    }
}

pub async fn resolve_repo_context(
    pool: &PgPool,
    project: &str,
    cwd: Option<PathBuf>,
    explicit_repo: Option<&str>,
) -> Result<Option<RepoContext>> {
    let cwd = cwd
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .context("resolve cwd")?;
    let cwd_repo = detect_repo_from_cwd(&cwd).ok();

    if let Some(repo_arg) = explicit_repo.map(str::trim).filter(|s| !s.is_empty()) {
        let repo_row = find_project_repo(pool, project, repo_arg).await?;
        let (repo_id, repo_url) = match repo_row {
            Some((id, url)) => (Some(id), Some(url)),
            None if looks_like_repo_url(repo_arg) => (None, Some(repo_arg.to_string())),
            None => {
                return Err(anyhow!(
                    "unknown repo '{repo_arg}' for project '{project}' (expected project_repos.id or URL)"
                ));
            }
        };

        let matched_cwd = cwd_repo
            .filter(|ctx| {
                repo_url
                    .as_deref()
                    .and_then(normalize_repo_url)
                    .zip(ctx.repo_url.as_deref().and_then(normalize_repo_url))
                    .map(|(a, b)| a == b)
                    .unwrap_or(false)
            })
            .or(match repo_url.as_deref() {
                Some(url) => detect_project_folder(pool, project, url).await?,
                None => None,
            });

        return Ok(Some(match matched_cwd {
            Some(mut ctx) => {
                ctx.repo_id = repo_id;
                ctx.repo_url = repo_url;
                ctx
            }
            None => RepoContext {
                repo_id,
                repo_url,
                repo_path: None,
                primary_language: "unknown".to_string(),
                build_system: None,
                key_dirs: Vec::new(),
            },
        }));
    }

    if let Some(mut ctx) = cwd_repo {
        if let Some(url) = ctx.repo_url.as_deref() {
            ctx.repo_id = find_project_repo_by_url(pool, project, url).await?;
        }
        return Ok(Some(ctx));
    }

    primary_project_repo(pool, project).await
}

pub fn detect_repo_from_cwd(cwd: &Path) -> Result<RepoContext> {
    let root = git_output(cwd, ["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root);
    let repo_url = git_output(&root, ["remote", "get-url", "origin"]).ok();
    let (primary_language, build_system) = detect_language_and_build(&root);
    let key_dirs = detect_key_dirs(&root);
    Ok(RepoContext {
        repo_id: None,
        repo_url,
        repo_path: Some(root),
        primary_language,
        build_system,
        key_dirs,
    })
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", cwd.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn detect_language_and_build(root: &Path) -> (String, Option<String>) {
    if root.join("pom.xml").exists() {
        return ("Java".to_string(), Some("Maven".to_string()));
    }
    if root.join("build.gradle").exists() || root.join("build.gradle.kts").exists() {
        return ("Java".to_string(), Some("Gradle".to_string()));
    }
    if root.join("Cargo.toml").exists() {
        return ("Rust".to_string(), Some("Cargo".to_string()));
    }
    if root.join("go.mod").exists() {
        return ("Go".to_string(), Some("Go modules".to_string()));
    }
    if root.join("pyproject.toml").exists() {
        return ("Python".to_string(), Some("pyproject".to_string()));
    }
    if root.join("package.json").exists() {
        let build = if root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else {
            "npm"
        };
        let language = if root.join("tsconfig.json").exists() {
            "TypeScript"
        } else {
            "JavaScript"
        };
        return (language.to_string(), Some(build.to_string()));
    }
    ("unknown".to_string(), None)
}

fn detect_key_dirs(root: &Path) -> Vec<String> {
    let mut dirs = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return dirs;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if matches!(
            name,
            ".git" | ".idea" | ".vscode" | ".next" | "target" | "node_modules" | "dist" | "build"
        ) {
            continue;
        }
        dirs.push(name.to_string());
    }
    dirs.sort();
    dirs.truncate(12);
    dirs
}

async fn find_project_repo(
    pool: &PgPool,
    project: &str,
    repo_arg: &str,
) -> Result<Option<(Uuid, String)>> {
    if let Ok(id) = Uuid::parse_str(repo_arg) {
        let row = sqlx::query(
            "SELECT id, github_url FROM project_repos WHERE project_id = $1 AND id = $2",
        )
        .bind(project)
        .bind(id)
        .fetch_optional(pool)
        .await?;
        return Ok(row.map(|r| (r.get("id"), r.get("github_url"))));
    }

    let rows = sqlx::query("SELECT id, github_url, name FROM project_repos WHERE project_id = $1")
        .bind(project)
        .fetch_all(pool)
        .await?;
    let wanted = normalize_repo_url(repo_arg);
    Ok(rows.into_iter().find_map(|r| {
        let id: Uuid = r.get("id");
        let url: String = r.get("github_url");
        let name: Option<String> = r.try_get("name").ok().flatten();
        let url_matches = wanted
            .as_deref()
            .zip(normalize_repo_url(&url).as_deref())
            .map(|(a, b)| a == b)
            .unwrap_or(false)
            || url == repo_arg;
        if url_matches || name.as_deref() == Some(repo_arg) {
            Some((id, url))
        } else {
            None
        }
    }))
}

async fn find_project_repo_by_url(
    pool: &PgPool,
    project: &str,
    repo_url: &str,
) -> Result<Option<Uuid>> {
    let rows = sqlx::query("SELECT id, github_url FROM project_repos WHERE project_id = $1")
        .bind(project)
        .fetch_all(pool)
        .await?;
    let wanted = normalize_repo_url(repo_url);
    Ok(rows.into_iter().find_map(|r| {
        let id: Uuid = r.get("id");
        let url: String = r.get("github_url");
        wanted
            .as_deref()
            .zip(normalize_repo_url(&url).as_deref())
            .and_then(|(a, b)| (a == b).then_some(id))
    }))
}

async fn detect_project_folder(
    pool: &PgPool,
    project: &str,
    repo_url: &str,
) -> Result<Option<RepoContext>> {
    let rows = sqlx::query(
        "SELECT path FROM project_folders WHERE project_id = $1 ORDER BY is_primary DESC, created_at ASC",
    )
    .bind(project)
    .fetch_all(pool)
    .await?;
    let Some(wanted) = normalize_repo_url(repo_url) else {
        return Ok(None);
    };
    for row in rows {
        let raw_path: String = row.get("path");
        let home = std::env::var("HOME").unwrap_or_default();
        let path = crate::expand_tilde(&raw_path, &home);
        let Ok(ctx) = detect_repo_from_cwd(&path) else {
            continue;
        };
        let matches = ctx
            .repo_url
            .as_deref()
            .and_then(normalize_repo_url)
            .map(|got| got == wanted)
            .unwrap_or(false);
        if matches {
            return Ok(Some(ctx));
        }
    }
    Ok(None)
}

async fn primary_project_repo(pool: &PgPool, project: &str) -> Result<Option<RepoContext>> {
    let row = sqlx::query(
        "SELECT id, github_url FROM project_repos
          WHERE project_id = $1 AND is_primary = TRUE
          ORDER BY created_at ASC LIMIT 1",
    )
    .bind(project)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| RepoContext {
        repo_id: Some(r.get("id")),
        repo_url: Some(r.get("github_url")),
        repo_path: None,
        primary_language: "unknown".to_string(),
        build_system: None,
        key_dirs: Vec::new(),
    }))
}

fn looks_like_repo_url(value: &str) -> bool {
    value.starts_with("https://")
        || value.starts_with("http://")
        || value.starts_with("git@")
        || value.starts_with("ssh://")
}

fn normalize_repo_url(url: &str) -> Option<String> {
    let mut s = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string();
    if let Some(rest) = s.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            s = format!("{host}/{path}");
        }
    } else {
        s = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .or_else(|| s.strip_prefix("ssh://git@"))
            .unwrap_or(&s)
            .to_string();
    }
    let s = s.to_ascii_lowercase();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .status()
            .expect("run command");
        assert!(status.success(), "{args:?} failed");
    }

    #[test]
    fn detect_repo_from_cwd_finds_git_root_origin_and_java_build() {
        let tmp = tempfile::tempdir().expect("tempdir");
        run(tmp.path(), &["git", "init", "-q"]);
        run(
            tmp.path(),
            &[
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:acme/orders.git",
            ],
        );
        std::fs::create_dir_all(tmp.path().join("src/main/java")).expect("java dirs");
        std::fs::write(tmp.path().join("pom.xml"), "<project/>").expect("pom");
        let nested = tmp.path().join("src/main/java");

        let ctx = detect_repo_from_cwd(&nested).expect("detect repo");
        assert_eq!(
            ctx.repo_url.as_deref(),
            Some("git@github.com:acme/orders.git")
        );
        assert_eq!(
            ctx.repo_path.as_deref().and_then(|p| p.canonicalize().ok()),
            tmp.path().canonicalize().ok()
        );
        assert_eq!(ctx.primary_language, "Java");
        assert_eq!(ctx.build_system.as_deref(), Some("Maven"));
        assert!(ctx.key_dirs.iter().any(|d| d == "src"));
    }

    #[test]
    fn normalize_repo_urls_matches_https_and_ssh_forms() {
        assert_eq!(
            normalize_repo_url("git@github.com:Acme/Orders.git"),
            normalize_repo_url("https://github.com/acme/orders")
        );
    }
}
