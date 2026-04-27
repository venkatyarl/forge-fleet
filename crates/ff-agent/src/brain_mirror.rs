//! Mirror per-CLI memory writes into the Obsidian vault's Inbox.
//!
//! Each vendor CLI has its own native memory mechanism that writes
//! markdown to a per-tool dir. ForgeFleet's Virtual Brain (V13) is the
//! shared substrate; the mirror watches the CLI dirs and copies any new
//! file into `<vault>/Inbox/<source>/<filename>` so an operator can
//! promote it via the existing `/api/brain/*` flow. Provenance is
//! preserved via `source: "claude-code"` etc. in the frontmatter.
//!
//! AI writes ONLY to `Inbox/` per the V13 design — promotion to
//! canonical folders requires user approval via the dashboard.
//!
//! Polling-based (30s tick) instead of inotify so we don't pull a new
//! crate dep. Tracks last-seen mtime per file in an in-memory map; on
//! daemon restart the map resets and any pre-existing files are
//! mirrored once (idempotent — overwriting an Inbox copy with itself
//! is a no-op).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default poll interval. 30s is fine for memory writes — they're not
/// latency-sensitive (the user reads them after the fact).
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// One row per CLI we know how to mirror. Empty `dir_glob` means the
/// CLI doesn't have a per-project memory dir on this host (e.g. fresh
/// install). Mirror skips silently in that case.
#[derive(Debug, Clone, Copy)]
pub struct MirrorSource {
    /// Public name used in the vault subfolder + frontmatter
    /// `source: "<name>"`.
    pub name: &'static str,
    /// Directory to walk. `~/` expands to `$HOME`. Files matching
    /// `**/*.md` are mirrored; everything else is ignored.
    pub source_dir: &'static str,
}

pub const MIRROR_SOURCES: &[MirrorSource] = &[
    // Claude Code stores per-project memory under
    // `~/.claude/projects/<hash>/memory/<name>.md`. We walk recursively
    // so all projects fan into the same Inbox/claude-code/ folder; if
    // two projects collide on `<name>.md` the latest write wins. That's
    // acceptable for the inbox staging pattern.
    MirrorSource {
        name: "claude-code",
        source_dir: "~/.claude/projects",
    },
    // Codex memory layout is TBD per research item; placeholder for now.
    MirrorSource {
        name: "codex",
        source_dir: "~/.codex/memory",
    },
    // Gemini CLI memory layout TBD.
    MirrorSource {
        name: "gemini",
        source_dir: "~/.gemini/memory",
    },
];

fn expand_home(p: &str) -> Option<PathBuf> {
    if let Some(rest) = p.strip_prefix("~/") {
        Some(dirs::home_dir()?.join(rest))
    } else {
        Some(PathBuf::from(p))
    }
}

/// Recursively walk `dir`, returning every `.md` file path with its
/// last-modified time. Errors are swallowed (a missing dir is fine).
fn walk_md_files(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in entries.flatten() {
            let p = ent.path();
            if let Ok(meta) = ent.metadata() {
                if meta.is_dir() {
                    walk(&p, out);
                } else if meta.is_file()
                    && p.extension().and_then(|s| s.to_str()) == Some("md")
                {
                    if let Ok(mtime) = meta.modified() {
                        out.push((p, mtime));
                    }
                }
            }
        }
    }
    walk(dir, &mut out);
    out
}

/// Resolve the vault root from `fleet_secrets[brain.vault_path]` if
/// set, else `~/projects/Yarli_KnowledgeBase`.
async fn resolve_vault_root(pool: &sqlx::PgPool) -> Option<PathBuf> {
    let from_secrets = ff_db::pg_get_secret(pool, "brain.vault_path")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let raw = from_secrets.unwrap_or_else(|| "~/projects/Yarli_KnowledgeBase".into());
    expand_home(&raw)
}

/// Copy `src` to `<vault>/Inbox/<source_name>/<filename>`. If the
/// inbox subfolder doesn't exist yet, create it. Overwrites silently
/// (we want last-write-wins for repeated mirrors).
fn mirror_file(vault: &Path, source_name: &str, src: &Path) -> std::io::Result<PathBuf> {
    let inbox = vault.join("Inbox").join(source_name);
    std::fs::create_dir_all(&inbox)?;
    let filename = src
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("unnamed.md"));
    let dst = inbox.join(filename);
    std::fs::copy(src, &dst)?;
    Ok(dst)
}

/// Long-lived foreground loop. Every `POLL_INTERVAL` ticks, walk each
/// configured source dir and mirror any file whose mtime changed since
/// the last tick. Exits when `shutdown` flips to true.
pub fn spawn_brain_mirror(
    pool: sqlx::PgPool,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Per-source map of (path → last seen mtime). Survives the
        // process lifetime; resets on daemon restart (idempotent).
        let mut seen: HashMap<&'static str, HashMap<PathBuf, SystemTime>> = HashMap::new();

        loop {
            let Some(vault) = resolve_vault_root(&pool).await else {
                debug!("brain_mirror: no vault root configured; idle");
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
                continue;
            };

            for src in MIRROR_SOURCES {
                let Some(dir) = expand_home(src.source_dir) else { continue };
                if !dir.is_dir() {
                    continue;
                }
                let map = seen.entry(src.name).or_default();
                for (path, mtime) in walk_md_files(&dir) {
                    let prev = map.insert(path.clone(), mtime);
                    let changed = prev.map(|t| t != mtime).unwrap_or(true);
                    if !changed {
                        continue;
                    }
                    match mirror_file(&vault, src.name, &path) {
                        Ok(dst) => info!(
                            source = src.name,
                            from = %path.display(),
                            to = %dst.display(),
                            "brain_mirror: copied to vault Inbox"
                        ),
                        Err(e) => warn!(
                            source = src.name,
                            from = %path.display(),
                            error = %e,
                            "brain_mirror: copy failed"
                        ),
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}
