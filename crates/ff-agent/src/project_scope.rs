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

use std::path::Path;
use std::process::Command;

/// Resolve a stable project id from `dir` (or the process cwd when `None`).
///
/// Prefers the git origin remote canonicalized to `github.com/org/repo` (stable
/// across clone paths); else the git toplevel basename as `local:<basename>`;
/// else `None`. The returned value is the bare id — callers set
/// `scope_type = "project"` as the namespace.
pub fn resolve_from_dir(dir: Option<&Path>) -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let mut cmd = Command::new("git");
        if let Some(d) = dir {
            cmd.arg("-C").arg(d);
        }
        let out = cmd.args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    };
    if let Some(url) = git(&["remote", "get-url", "origin"])
        && let Some(canon) = canonical_remote(&url)
    {
        return Some(canon);
    }
    if let Some(top) = git(&["rev-parse", "--show-toplevel"])
        && let Some(base) = Path::new(&top).file_name().and_then(|s| s.to_str())
    {
        return Some(format!("local:{}", base.to_lowercase()));
    }
    None
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
}
