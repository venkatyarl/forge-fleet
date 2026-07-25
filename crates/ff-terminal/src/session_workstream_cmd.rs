use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use sqlx::{FromRow, PgPool};

const PRESENCE_TTL: &str = "5 minutes";

#[derive(Debug, FromRow)]
struct Workstream {
    id: uuid::Uuid,
    project_id: String,
    root_identity: String,
    basename: Option<String>,
    aliases: Value,
    goal: Option<String>,
    working_summary: Option<String>,
    focus: Option<String>,
    open_threads: Value,
    status: String,
    updated_at: chrono::DateTime<chrono::Utc>,
    leader_generation: i32,
}

#[derive(Debug)]
struct ProjectIdentity {
    remote: Option<String>,
    basename: String,
    root: String,
}

#[derive(Debug)]
struct LocalIdentity {
    owner_scope: String,
    thread_id: String,
    source: String,
}

pub async fn status(pool: &PgPool, cwd: &Path) -> Result<()> {
    let project = project_identity(cwd)?;
    let local = local_identity()?;
    let Some(workstream) = resolve_workstream(pool, &project, &local.owner_scope, None).await?
    else {
        bail!(
            "no attached active workstream for project '{}' and owner '{}' \
             (run `ff session attach --project {}`)",
            project.basename,
            local.owner_scope,
            project.basename
        );
    };
    refresh_presence(pool, workstream.id, &local).await?;
    print_resume_packet(&workstream);
    Ok(())
}

pub async fn attach(pool: &PgPool, cwd: &Path, project: &str) -> Result<()> {
    let project_name = normalize_token(project);
    if project_name.is_empty() {
        bail!("--project must contain at least one letter or digit");
    }
    let project = project_identity(cwd)?;
    let local = local_identity()?;
    let mut tx = pool.begin().await.context("begin workstream attachment")?;
    let workstream = match resolve_workstream_in(
        &mut *tx,
        &project,
        &local.owner_scope,
        Some(&project_name),
        true,
    )
    .await?
    {
        Some(row) => row,
        None => sqlx::query_as::<_, Workstream>(
            "INSERT INTO ff_workstreams
                    (project_id, project_key, root_identity, owner_scope,
                     git_remote, basename, status)
             VALUES ($1, $1, $2, $3, $4, $5, 'active')
             ON CONFLICT (root_identity) DO UPDATE SET
                git_remote = COALESCE(ff_workstreams.git_remote, EXCLUDED.git_remote),
                basename = COALESCE(ff_workstreams.basename, EXCLUDED.basename),
                updated_at = NOW()
             RETURNING id, project_id, root_identity, git_remote, basename, aliases,
                       goal, working_summary, focus, open_threads, status, updated_at,
                       leader_generation",
        )
        .bind(&project_name)
        .bind(&project.root)
        .bind(&local.owner_scope)
        .bind(&project.remote)
        .bind(&project.basename)
        .fetch_one(&mut *tx)
        .await
        .context("attach project workstream")?,
    };
    sqlx::query(
        "INSERT INTO session_attachments
                (workstream_id, owner_scope, thread_id, source, presence_expires_at)
         VALUES ($1, $2, $3, $4, NOW() + $5::interval)
         ON CONFLICT (workstream_id, owner_scope, thread_id) DO UPDATE SET
            source = EXCLUDED.source,
            last_seen_at = NOW(),
            presence_expires_at = NOW() + $5::interval",
    )
    .bind(workstream.id)
    .bind(&local.owner_scope)
    .bind(&local.thread_id)
    .bind(&local.source)
    .bind(PRESENCE_TTL)
    .execute(&mut *tx)
    .await
    .context("record owner-scoped session attachment")?;
    tx.commit().await.context("commit workstream attachment")?;
    print_resume_packet(&workstream);
    Ok(())
}

pub async fn note(pool: &PgPool, cwd: &Path, note: &str) -> Result<()> {
    if note.trim().is_empty() {
        bail!("note must not be empty");
    }
    let project = project_identity(cwd)?;
    let local = local_identity()?;
    let mut tx = pool.begin().await.context("begin workstream note")?;
    let workstream = resolve_workstream_in(&mut *tx, &project, &local.owner_scope, None, false)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "no attached active workstream for project '{}' and owner '{}'",
                project.basename,
                local.owner_scope
            )
        })?;
    let seq: i64 = sqlx::query_scalar(
        "UPDATE ff_workstreams
            SET note_seq = note_seq + 1,
                working_summary = concat_ws(E'\n', NULLIF(working_summary, ''), $2),
                updated_at = NOW()
          WHERE id = $1
          RETURNING note_seq",
    )
    .bind(workstream.id)
    .bind(note.trim())
    .fetch_one(&mut *tx)
    .await
    .context("allocate atomic workstream note sequence")?;
    sqlx::query(
        "INSERT INTO workstream_notes (workstream_id, seq, owner_scope, note, source)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(workstream.id)
    .bind(seq)
    .bind(&local.owner_scope)
    .bind(note.trim())
    .bind(&local.source)
    .execute(&mut *tx)
    .await
    .context("append sequenced workstream note")?;
    refresh_presence_in(&mut *tx, workstream.id, &local).await?;
    tx.commit().await.context("commit workstream note")?;
    println!("note_seq: {seq}");
    Ok(())
}

async fn resolve_workstream(
    pool: &PgPool,
    project: &ProjectIdentity,
    owner_scope: &str,
    explicit_project: Option<&str>,
) -> Result<Option<Workstream>> {
    let mut conn = pool
        .acquire()
        .await
        .context("acquire workstream connection")?;
    resolve_workstream_in(&mut *conn, project, owner_scope, explicit_project, false).await
}

async fn resolve_workstream_in<'e, E>(
    executor: E,
    project: &ProjectIdentity,
    owner_scope: &str,
    explicit_project: Option<&str>,
    allow_unattached: bool,
) -> Result<Option<Workstream>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query_as::<_, Workstream>(
        "SELECT DISTINCT w.id, w.project_id, w.root_identity, w.git_remote, w.basename,
                w.aliases, w.goal, w.working_summary, w.focus, w.open_threads,
                w.status, w.updated_at, w.leader_generation
           FROM ff_workstreams w
           LEFT JOIN session_attachments a
             ON a.workstream_id = w.id AND a.owner_scope = $1
          WHERE w.status = 'active'
            AND ($2 OR w.owner_scope = $1 OR a.id IS NOT NULL)",
    )
    .bind(owner_scope)
    .bind(allow_unattached)
    .fetch_all(executor)
    .await
    .context("load owner-scoped active project workstreams")?;

    let mut aliases = Vec::new();
    let mut roots = Vec::new();
    let mut projects = Vec::new();
    let mut basenames = Vec::new();
    for row in rows {
        if alias_matches(&row.aliases, explicit_project, project) {
            aliases.push(row);
        } else if row.root_identity == project.root {
            roots.push(row);
        } else if explicit_project.is_some_and(|name| name == normalize_token(&row.project_id)) {
            projects.push(row);
        } else if explicit_project.is_none()
            && row
                .basename
                .as_deref()
                .is_some_and(|name| normalize_token(name) == project.basename)
        {
            basenames.push(row);
        }
    }
    choose_unique(aliases, "alias")
        .or_else(|| choose_unique(roots, "root identity"))
        .or_else(|| choose_unique(projects, "project"))
        .or_else(|| choose_unique(basenames, "basename"))
        .transpose()
}

async fn refresh_presence(
    pool: &PgPool,
    workstream_id: uuid::Uuid,
    local: &LocalIdentity,
) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("acquire presence connection")?;
    refresh_presence_in(&mut *conn, workstream_id, local).await
}

async fn refresh_presence_in<'e, E>(
    executor: E,
    workstream_id: uuid::Uuid,
    local: &LocalIdentity,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let updated = sqlx::query(
        "UPDATE session_attachments
            SET last_seen_at = NOW(),
                presence_expires_at = NOW() + $4::interval
          WHERE workstream_id = $1 AND owner_scope = $2 AND thread_id = $3",
    )
    .bind(workstream_id)
    .bind(&local.owner_scope)
    .bind(&local.thread_id)
    .bind(PRESENCE_TTL)
    .execute(executor)
    .await
    .context("refresh session presence")?
    .rows_affected();
    if updated == 0 {
        bail!("current thread is not attached to this owner-scoped workstream");
    }
    Ok(())
}

fn choose_unique(mut rows: Vec<Workstream>, kind: &'static str) -> Option<Result<Workstream>> {
    match rows.len() {
        0 => None,
        1 => Some(Ok(rows.pop().expect("one row"))),
        count => Some(Err(anyhow!(
            "ambiguous {kind} matched {count} active workstreams; add a unique alias"
        ))),
    }
}

fn alias_matches(
    aliases: &Value,
    explicit_project: Option<&str>,
    project: &ProjectIdentity,
) -> bool {
    let mut candidates = vec![project.basename.as_str(), project.root.as_str()];
    if let Some(remote) = project.remote.as_deref() {
        candidates.push(remote);
    }
    if let Some(name) = explicit_project {
        candidates.push(name);
    }
    let matches = |alias: &str| {
        let alias = normalize_token(alias);
        !alias.is_empty()
            && candidates
                .iter()
                .any(|candidate| alias == normalize_token(candidate))
    };
    match aliases {
        Value::Array(values) => values.iter().filter_map(Value::as_str).any(matches),
        Value::Object(values) => values.keys().any(|alias| matches(alias)),
        _ => false,
    }
}

fn project_identity(cwd: &Path) -> Result<ProjectIdentity> {
    let root_path = find_project_root(cwd)?;
    let basename = root_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(normalize_token)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("cannot derive project name from {}", root_path.display()))?;
    let remote = git_origin(&root_path).map(|value| normalize_remote(&value));
    let root = remote
        .as_deref()
        .map(|value| format!("remote:{value}"))
        .unwrap_or_else(|| format!("project:{basename}"));
    Ok(ProjectIdentity {
        remote,
        basename,
        root,
    })
}

fn local_identity() -> Result<LocalIdentity> {
    let owner_scope = env::var("FORGEFLEET_OWNER_SCOPE")
        .or_else(|_| env::var("USER"))
        .map(|value| normalize_scope(&value))
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("set FORGEFLEET_OWNER_SCOPE to a stable local owner identity"))?;
    let thread_id = [
        "FORGEFLEET_SESSION_ID",
        "CODEX_THREAD_ID",
        "CLAUDE_SESSION_ID",
    ]
    .iter()
    .find_map(|key| env::var(key).ok())
    .map(|value| normalize_scope(&value))
    .filter(|value| !value.is_empty())
    .unwrap_or_else(|| format!("local-{owner_scope}"));
    let source = env::var("FORGEFLEET_SESSION_SOURCE")
        .ok()
        .map(|value| redact_source(&value))
        .unwrap_or_else(|| "local".to_owned());
    Ok(LocalIdentity {
        owner_scope,
        thread_id,
        source,
    })
}

fn find_project_root(cwd: &Path) -> Result<PathBuf> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("resolve current directory {}", cwd.display()))?;
    let mut manifest_root = None;
    for directory in cwd.ancestors() {
        if directory.join(".git").exists() || directory.join(".forgefleet/project").is_file() {
            return Ok(directory.to_path_buf());
        }
        if ["Cargo.toml", "package.json", "pyproject.toml"]
            .iter()
            .any(|marker| directory.join(marker).is_file())
        {
            manifest_root = Some(directory.to_path_buf());
        }
    }
    Ok(manifest_root.unwrap_or(cwd))
}

fn git_origin(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|remote| !remote.is_empty())
}

fn normalize_remote(remote: &str) -> String {
    let remote = remote
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    let path = if let Some(path) = remote.strip_prefix("git@") {
        path.replace(':', "/")
    } else {
        remote
            .split_once("://")
            .map(|(_, path)| path.to_owned())
            .unwrap_or(remote)
    };
    normalize_token(&path)
}

fn normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalize_scope(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .take(128)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn redact_source(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if ["token", "secret", "password", "credential", "private_key"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return "[redacted]".to_owned();
    }
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .map(normalize_scope)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| normalize_scope(value))
}

fn print_resume_packet(row: &Workstream) {
    println!("project: {}", row.project_id);
    println!("root_identity: {}", row.root_identity);
    println!("status: {}", row.status);
    println!("updated_at: {}", row.updated_at.to_rfc3339());
    println!("leader_generation: {}", row.leader_generation);
    println!("goal: {}", row.goal.as_deref().unwrap_or("-"));
    println!("focus: {}", row.focus.as_deref().unwrap_or("-"));
    println!(
        "working_summary: {}",
        row.working_summary.as_deref().unwrap_or("-")
    );
    println!("open_threads: {}", row.open_threads);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_git_root_from_nested_src() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("HireFlow360");
        let nested = root.join("src/api");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_root(&nested).unwrap(), root);
    }

    #[test]
    fn normalizes_equivalent_git_origins() {
        assert_eq!(
            normalize_remote("git@GitHub.com:Acme/HireFlow360.git"),
            normalize_remote("https://github.com/acme/hireflow360/")
        );
    }

    #[test]
    fn owner_scope_is_bounded_and_safe() {
        assert_eq!(normalize_scope("Team A/../../Admin"), "teama....admin");
        assert_eq!(normalize_scope(&"a".repeat(200)).len(), 128);
    }

    #[test]
    fn source_redaction_drops_paths_and_secrets() {
        assert_eq!(
            redact_source("/home/alice/sessions/codex.jsonl"),
            "codex.jsonl"
        );
        assert_eq!(redact_source("token=super-secret"), "[redacted]");
    }
}
