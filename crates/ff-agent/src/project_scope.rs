//! Stable project-id resolution for memory scoping.
//!
//! Derives a project id from a directory so the agent Scratchpad can be SHARED
//! per-project across CLIs (Claude Code's project memory recalled by Codex/Kimi
//! on the same repo). Used by both the `ff memory` CLI (resolves the process
//! cwd) and the `memory_*` MCP tools (resolve an explicit `cwd` param, since the
//! shared HTTP daemon's own cwd is not the caller's project).
//!
//! Council verdict 2026-06-19 (kimi, codex CLI wedged): one canonicalization,
//! server-side, keyed off the working directory. Format is stable across clone
//! paths and SSH aliases: `github.com/org/repo`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use sqlx::PgPool;

/// Project resolution plus best-effort session binding metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProjectSession {
    pub project_id: String,
    pub session_id: Option<String>,
}

/// Path-independent identities discovered for a project root.  Callers that
/// have access to Postgres must still apply `ff_workstreams.aliases` last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub explicit: Option<String>,
    pub git_remote: Option<String>,
    pub basename: Option<String>,
}

/// Resolve a stable project id from `dir` (or the process cwd when `None`).
///
/// Precedence:
///   1. a `.forgefleet-project` marker file (walking up from `dir`) — explicit
///      operator intent, ranks ABOVE git. Its first non-comment line IS the id;
///      use it to scope a non-git directory, a monorepo subtree, or to pin a
///      stable id independent of the remote.
///   2. the git origin remote canonicalized to `github.com/org/repo` (stable
///      across clone paths);
///   3. the git toplevel basename as `local:<basename>`;
///   4. `None`.
///
/// The returned value is the bare id — callers set `scope_type = "project"`.
pub fn resolve_from_dir(dir: Option<&Path>) -> Option<String> {
    let identity = identity_from_dir(dir)?;
    identity
        .explicit
        .or(identity.git_remote)
        .or(identity.basename)
}

/// Resolve the project root before deriving identity, so opening `repo/src`
/// never scopes the session to a generic leaf such as `src`.
pub fn identity_from_dir(dir: Option<&Path>) -> Option<ProjectIdentity> {
    let start = dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())?;
    let explicit = marker_project_id(Some(&start));
    let root = project_root(&start);
    let git_remote = git_remote_at(&root).or_else(|| one_level_down_remote(&root));
    let basename = human_project_dir(&start, &root)
        .file_name()
        .and_then(|s| s.to_str())
        .map(normalize_project_name);
    Some(ProjectIdentity {
        explicit,
        git_remote,
        basename,
    })
}

fn project_root(start: &Path) -> PathBuf {
    let mut markers = Vec::new();
    for ancestor in start.ancestors() {
        if has_project_marker(ancestor) {
            markers.push(ancestor.to_path_buf());
        }
    }
    // The outermost manifest is the project root; an explicit marker or git
    // checkout is a stronger boundary and therefore wins when encountered.
    markers
        .iter()
        .find(|path| {
            path.join(".forgefleet/project").is_file()
                || path.join(".forgefleet-project").is_file()
                || path.join(".git").exists()
        })
        .cloned()
        .or_else(|| markers.last().cloned())
        .unwrap_or_else(|| start.to_path_buf())
}

fn has_project_marker(path: &Path) -> bool {
    [
        ".git",
        ".forgefleet/project",
        ".forgefleet-project",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
    ]
    .iter()
    .any(|marker| path.join(marker).exists())
}

fn git_remote_at(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .and_then(|url| canonical_remote(&url))
}

fn one_level_down_remote(root: &Path) -> Option<String> {
    let mut children = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join(".git").exists())
        .collect::<Vec<_>>();
    children.sort();
    children.into_iter().find_map(|path| git_remote_at(&path))
}

fn human_project_dir<'a>(start: &'a Path, detected_root: &'a Path) -> &'a Path {
    const WORKSPACES: &[&str] = &["projects", "business", "downloads"];
    for ancestor in start.ancestors() {
        if let Some(parent) = ancestor.parent()
            && parent
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|name| WORKSPACES.contains(&name.to_ascii_lowercase().as_str()))
        {
            return ancestor;
        }
        if ancestor
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.starts_with("sub-agent-"))
        {
            return ancestor;
        }
    }
    detected_root
}

fn normalize_project_name(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Resolve a stable project id from `dir` and attach this client session to the
/// project's workstream when one exists.
///
/// Resolution keeps the same precedence as [`resolve_from_dir`]. Attach failures
/// are returned with context because a resolved project with a broken session
/// binding should be visible to the caller.
pub async fn resolve_from_dir_and_attach(
    pg: &PgPool,
    dir: Option<&Path>,
    tool: &str,
    goal: Option<&str>,
) -> Result<Option<ResolvedProjectSession>> {
    let Some(project_id) = resolve_from_dir(dir) else {
        return Ok(None);
    };
    let session_id = attach_client_session(pg, dir, &project_id, tool, goal).await?;
    Ok(Some(ResolvedProjectSession {
        project_id,
        session_id,
    }))
}

/// Attach the current worker's client session to the workstream for a resolved
/// project id. Returns `Ok(None)` when the project has no active workstream yet.
pub async fn attach_client_session(
    pg: &PgPool,
    dir: Option<&Path>,
    project_id: &str,
    tool: &str,
    goal: Option<&str>,
) -> Result<Option<String>> {
    let cwd = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().context("resolve current directory for client attach")?,
    };
    let ws = crate::workstreams::workstream_for_project(pg, project_id)
        .await
        .with_context(|| format!("resolve workstream for project '{project_id}'"))?;
    let ws = match ws {
        Some(ws) => Some(ws),
        None => crate::workstreams::workstream_for_dir(pg, &cwd)
            .await
            .with_context(|| {
                format!(
                    "resolve workstream for project '{}' from {}",
                    project_id,
                    cwd.display()
                )
            })?,
    };
    let Some(ws) = ws else {
        return Ok(None);
    };
    let worker = crate::fleet_info::resolve_this_worker_name().await;
    let session_id =
        crate::workstreams::attach(pg, &ws, &worker, tool, &cwd.display().to_string(), goal)
            .await
            .with_context(|| format!("attach client session to project '{project_id}'"))?;
    Ok(Some(session_id))
}

/// Walk up from `dir` (or cwd) looking for a `.forgefleet-project` marker. Its
/// first non-empty, non-`#`-comment line is the project id (trimmed). A subtree
/// inherits the nearest ancestor's marker (like `.gitignore` discovery). The
/// walk is bounded so a pathological path can't loop forever.
fn marker_project_id(dir: Option<&Path>) -> Option<String> {
    let start = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    let mut cur: &Path = start.as_path();
    for _ in 0..64 {
        for marker in [
            cur.join(".forgefleet/project"),
            cur.join(".forgefleet-project"),
        ] {
            if let Ok(content) = std::fs::read_to_string(marker)
                && let Some(id) = marker_id_from_contents(&content)
            {
                return Some(id);
            }
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    None
}

/// Parse a `.forgefleet-project` body: the first non-empty, non-comment line.
fn marker_id_from_contents(content: &str) -> Option<String> {
    content
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
}

/// Normalize a git remote URL to a stable `host/org/repo` (lowercase, no `.git`).
/// Both `git@github.com:Org/Repo.git` and `https://github.com/Org/Repo.git`
/// become `github.com/org/repo`.
pub fn canonical_remote(url: &str) -> Option<String> {
    let u = url.trim();
    // scp-style: git@host:org/repo(.git)
    let body = if let Some(rest) = u.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else if let Some(rest) = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .or_else(|| u.strip_prefix("ssh://git@"))
    {
        // strip any user@ and a trailing /
        rest.split_once('@')
            .map(|(_, h)| h.to_string())
            .unwrap_or_else(|| rest.to_string())
    } else {
        return None;
    };
    let body = body
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase();
    // Split host/path and normalize the host: strip an SSH-config alias suffix on
    // the last domain label (github.com-venkat -> github.com) so the SAME repo
    // resolves to the SAME project id regardless of which SSH alias a CLI uses.
    let (host, path) = body.split_once('/')?;
    if path.is_empty() {
        return None;
    }
    Some(format!("{}/{}", normalize_host(host), path))
}

/// Strip an SSH-config alias suffix from a git host's last label
/// (`github.com-venkat` -> `github.com`); leaves a plain host unchanged.
fn normalize_host(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    if let Some((last, rest)) = parts.split_last() {
        let base = last.split_once('-').map(|(b, _)| b).unwrap_or(last);
        let mut out: Vec<&str> = rest.to_vec();
        out.push(base);
        return out.join(".");
    }
    host.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_scp_and_https() {
        assert_eq!(
            canonical_remote("git@github.com:VenkatYarl/Forge-Fleet.git").as_deref(),
            Some("github.com/venkatyarl/forge-fleet")
        );
        assert_eq!(
            canonical_remote("https://github.com/VenkatYarl/Forge-Fleet.git").as_deref(),
            Some("github.com/venkatyarl/forge-fleet")
        );
        assert_eq!(
            canonical_remote("https://x-access-token:TOKEN@github.com/org/repo").as_deref(),
            Some("github.com/org/repo")
        );
        assert_eq!(
            canonical_remote("ssh://git@gitlab.example.com/group/proj.git").as_deref(),
            Some("gitlab.example.com/group/proj")
        );
    }

    #[test]
    fn strips_ssh_alias_suffix_on_host() {
        // SSH-config alias host (github.com-venkat) collapses to the real host.
        assert_eq!(
            canonical_remote("git@github.com-venkat:org/repo.git").as_deref(),
            Some("github.com/org/repo")
        );
        assert_eq!(normalize_host("github.com"), "github.com");
        assert_eq!(normalize_host("github.com-venkat"), "github.com");
        assert_eq!(normalize_host("localhost"), "localhost");
    }

    #[test]
    fn rejects_non_git_urls() {
        assert_eq!(canonical_remote("not a url"), None);
        assert_eq!(canonical_remote("git@github.com:"), None);
    }

    #[test]
    fn marker_takes_first_non_comment_line() {
        assert_eq!(
            marker_id_from_contents("# my project\n\n  acme/api  \nignored\n").as_deref(),
            Some("acme/api")
        );
        assert_eq!(
            marker_id_from_contents("github.com/org/repo").as_deref(),
            Some("github.com/org/repo")
        );
        assert_eq!(marker_id_from_contents("# only comments\n\n  \n"), None);
        assert_eq!(marker_id_from_contents(""), None);
    }

    #[test]
    fn marker_walks_up_and_ranks_above_git() {
        // A marker in an ancestor dir scopes a subtree, and resolve_from_dir
        // returns it verbatim even inside this git repo (marker > git origin).
        let tmp = std::env::temp_dir().join(format!("ffmarker_{}", std::process::id()));
        let sub = tmp.join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join(".forgefleet-project"), "acme/monorepo\n").unwrap();
        assert_eq!(
            resolve_from_dir(Some(&sub)).as_deref(),
            Some("acme/monorepo")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nested_folder_uses_project_root_not_leaf() {
        let tmp = std::env::temp_dir().join(format!("ffroot_{}", std::process::id()));
        let sub = tmp.join("HireFlow360/src/api");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("HireFlow360/package.json"), "{}").unwrap();
        let identity = identity_from_dir(Some(&sub)).unwrap();
        assert_eq!(identity.basename.as_deref(), Some("hireflow360"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn forgefleet_project_file_ranks_above_other_identity() {
        let tmp = std::env::temp_dir().join(format!("ffexplicit_{}", std::process::id()));
        let sub = tmp.join("repo/src");
        std::fs::create_dir_all(tmp.join("repo/.forgefleet")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("repo/.forgefleet/project"), "operator-project\n").unwrap();
        assert_eq!(
            resolve_from_dir(Some(&sub)).as_deref(),
            Some("operator-project")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
