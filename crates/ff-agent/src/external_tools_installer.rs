//! External-tools installer / dispatcher.
//!
//! Given a tool id and a target computer, resolves the per-os-family
//! `upgrade_playbook` command and enqueues a `kind='shell'` deferred
//! task whose worker runs the install on the target node.
//!
//! The payload carries `meta.external_tool = {id, computer}` so the
//! worker's finalizer (see `crates/ff-terminal/src/main.rs`
//! `finalize_external_tool_event`) can flip
//! `computer_external_tools.status` to `'ok'` or `'install_failed'` and
//! best-effort extract `installed_version` / `install_path` from task
//! stdout after completion.
//!
//! Mirrors [`crate::auto_upgrade::enqueue_plans`]. TODO: when MCP
//! auto-registration lands, the finalizer will also flip
//! `mcp_registered=true`.
//!
//! ## Artifact cache
//!
//! When `external_tools.metadata.artifact_cache` carries an entry for the
//! target's playbook key (same key precedence as `upgrade_playbook`), the
//! enqueued command is wrapped cache-first: check the node-local artifact,
//! then rsync it from the fleet cache on a miss, validate its SHA256, and run
//! the entry's `install_cmd` (which sees the local path as `$FF_ARTIFACT`). On
//! cache/rsync failure or checksum mismatch the wrapper falls back to the
//! original playbook command (WAN download).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Row};

/// One target computer + the resolved playbook command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallPlan {
    pub tool_id: String,
    pub display_name: String,
    pub computer_name: String,
    pub os_family: String,
    pub install_method: String,
    pub install_source: Option<String>,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub playbook_key: String,
    pub command: String,
    /// True when `command` was wrapped cache-first (rsync + SHA256 check
    /// with WAN fallback) from a `metadata.artifact_cache` entry.
    #[serde(default)]
    pub artifact_cache: bool,
    pub register_as_mcp: bool,
    pub mcp_server_command: Option<String>,
}

/// One `metadata.artifact_cache` entry, keyed like `upgrade_playbook`
/// (`"<os_family>-<source>"` / `"<os_family>"` / `"linux"` / `"all"` …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactCacheSpec {
    /// rsync source spec, e.g. `ff@sophie:/srv/ff-artifacts/kimi/kimi-linux-x86_64`.
    pub source: String,
    /// Expected SHA256 (lowercase hex) of the fetched artifact.
    pub sha256: String,
    /// Install command run after validation; the validated local artifact
    /// path is exported as `$FF_ARTIFACT`.
    pub install_cmd: String,
}

/// Result of enqueuing one plan.
#[derive(Debug, Clone)]
pub struct EnqueueResult {
    pub computer_name: String,
    pub defer_id: String,
    pub tool_id: String,
}

/// Resolve install plans for `tool_id`. Covers three cases:
///
///   - `only_computer = Some(name)` → one explicit target. A row is
///     created (status='installing') if it doesn't already exist so the
///     finalizer has something to flip.
///   - `only_computer = None, all_online = true` → every online
///     computer that doesn't already have the tool OR whose status is
///     `upgrade_available`.
///   - neither is set → error.
pub async fn resolve_install_plans(
    pool: &PgPool,
    tool_id: &str,
    only_computer: Option<&str>,
    all_online: bool,
) -> Result<(Vec<InstallPlan>, Vec<(String, String)>)> {
    let tool_row = sqlx::query(
        "SELECT id, display_name, install_method, upgrade_playbook,
                latest_version, register_as_mcp, mcp_server_command, metadata
           FROM external_tools
          WHERE id = $1",
    )
    .bind(tool_id)
    .fetch_optional(pool)
    .await
    .context("select external_tools")?;

    let Some(tool_row) = tool_row else {
        anyhow::bail!("no external_tools entry for id='{tool_id}'");
    };

    let display_name: String = tool_row.get("display_name");
    let install_method: String = tool_row.get("install_method");
    let playbook: JsonValue = tool_row.get("upgrade_playbook");
    let latest_version: Option<String> = tool_row.get("latest_version");
    let register_as_mcp: bool = tool_row.get("register_as_mcp");
    let mcp_server_command: Option<String> = tool_row.get("mcp_server_command");
    let metadata: JsonValue = tool_row.get("metadata");

    // Figure out which computers to target.
    let target_rows = if let Some(name) = only_computer {
        sqlx::query(
            "SELECT c.id          AS id,
                    c.name        AS name,
                    c.os_family   AS os_family,
                    cet.install_source     AS install_source,
                    cet.installed_version  AS installed_version
               FROM computers c
          LEFT JOIN computer_external_tools cet
                 ON cet.computer_id = c.id AND cet.tool_id = $1
              WHERE LOWER(c.name) = LOWER($2)
              ORDER BY c.name",
        )
        .bind(tool_id)
        .bind(name)
        .fetch_all(pool)
        .await
    } else if all_online {
        sqlx::query(
            "SELECT c.id          AS id,
                    c.name        AS name,
                    c.os_family   AS os_family,
                    cet.install_source     AS install_source,
                    cet.installed_version  AS installed_version
               FROM computers c
          LEFT JOIN computer_external_tools cet
                 ON cet.computer_id = c.id AND cet.tool_id = $1
              WHERE c.status = 'online'
              ORDER BY c.name",
        )
        .bind(tool_id)
        .fetch_all(pool)
        .await
    } else {
        anyhow::bail!("pass only_computer or all_online=true");
    }
    .context("select computers")?;

    let mut plans = Vec::with_capacity(target_rows.len());
    let mut skipped = Vec::new();

    for row in &target_rows {
        let name: String = row.get("name");
        let os_family: String = row.get("os_family");
        let install_source: Option<String> = row.get("install_source");
        let installed_version: Option<String> = row.get("installed_version");

        let candidates: Vec<String> = {
            let mut v = Vec::new();
            if let Some(src) = &install_source {
                v.push(format!("{os_family}-{src}"));
            }
            v.push(os_family.clone());
            // Broad OS class: catalog playbooks commonly key on the family
            // ("linux"/"macos") rather than the specific os_family
            // ("linux-ubuntu"/"linux-dgx"). Without this, `ff ext install`
            // SKIPS every linux node whose playbook uses the broad "linux" key
            // — which is the kimi-cli (V150) AND the existing codex/claude
            // (V46) rows. Try the class before falling to "all".
            let os_class = if os_family.starts_with("linux") {
                "linux"
            } else if os_family.starts_with("macos") || os_family.starts_with("darwin") {
                "macos"
            } else {
                os_family.as_str()
            };
            if os_class != os_family {
                v.push(os_class.to_string());
            }
            v.push("all".to_string());
            v
        };

        let mut matched: Option<(String, String)> = None;
        for key in &candidates {
            if let Some(val) = playbook.get(key).and_then(|v| v.as_str()) {
                matched = Some((key.clone(), val.to_string()));
                break;
            }
        }

        match matched {
            Some((playbook_key, command)) => {
                // Cache-first: if the tool metadata carries an artifact-cache
                // entry for this target, wrap the playbook command so the
                // WAN download only runs when the cache misses.
                let cache_spec = resolve_artifact_cache(&metadata, &candidates);
                let (command, artifact_cache) = match &cache_spec {
                    Some(spec) => (wrap_with_artifact_cache(tool_id, spec, &command), true),
                    None => (command, false),
                };
                plans.push(InstallPlan {
                    tool_id: tool_id.to_string(),
                    display_name: display_name.clone(),
                    computer_name: name,
                    os_family,
                    install_method: install_method.clone(),
                    install_source,
                    installed_version,
                    latest_version: latest_version.clone(),
                    playbook_key,
                    command,
                    artifact_cache,
                    register_as_mcp,
                    mcp_server_command: mcp_server_command.clone(),
                })
            }
            None => skipped.push((
                name,
                format!(
                    "no playbook key for os='{os_family}' source='{}' (tried {:?})",
                    install_source.as_deref().unwrap_or("-"),
                    candidates
                ),
            )),
        }
    }

    Ok((plans, skipped))
}

/// Enqueue the given plans as `kind='shell'` deferred tasks.
///
/// Each payload carries:
///   - `command`            → the playbook command the worker runs
///   - `meta.external_tool` → `{id, display_name, computer, old_version,
///                              latest_version, playbook_key, install_method,
///                              register_as_mcp, mcp_server_command,
///                              artifact_cache}`
///
/// After enqueuing, a placeholder `computer_external_tools` row is
/// upserted with `status='installing'` so the finalizer can flip it and
/// subsequent drift ticks don't double-fire.
pub async fn enqueue_plans(
    pool: &PgPool,
    plans: &[InstallPlan],
    who: &str,
) -> Result<Vec<EnqueueResult>> {
    let mut out = Vec::with_capacity(plans.len());
    for p in plans {
        let payload = json!({
            "command": p.command,
            "meta": {
                "external_tool": {
                    "id":                 p.tool_id,
                    "display_name":       p.display_name,
                    "computer":           p.computer_name,
                    "old_version":        p.installed_version,
                    "latest_version":     p.latest_version,
                    "playbook_key":       p.playbook_key,
                    "install_method":     p.install_method,
                    "register_as_mcp":    p.register_as_mcp,
                    "mcp_server_command": p.mcp_server_command,
                    "artifact_cache":     p.artifact_cache,
                    "source":             who,
                }
            }
        });
        let trigger_spec = json!({ "node": p.computer_name });
        let title = format!("Install {} on {}", p.tool_id, p.computer_name);
        let id = ff_db::pg_enqueue_deferred(
            pool,
            &title,
            "shell",
            &payload,
            "node_online",
            &trigger_spec,
            Some(&p.computer_name),
            &json!([]),
            Some(who),
            Some(3),
        )
        .await
        .context("enqueue deferred task")?;

        // Upsert placeholder install row so the finalizer has something
        // to flip. We key the status on whether this looks like an
        // upgrade (had a prior installed_version) vs a first install.
        let new_status = if p.installed_version.is_some() {
            "upgrading"
        } else {
            "installing"
        };

        let install_source = derive_install_source(&p.install_method);

        let _ = sqlx::query(
            "INSERT INTO computer_external_tools
                 (computer_id, tool_id, install_source, status)
             SELECT c.id, $1, $2, $3
               FROM computers c
              WHERE LOWER(c.name) = LOWER($4)
             ON CONFLICT (computer_id, tool_id) DO UPDATE
               SET status = EXCLUDED.status,
                   install_source = COALESCE(computer_external_tools.install_source, EXCLUDED.install_source)",
        )
        .bind(&p.tool_id)
        .bind(install_source)
        .bind(new_status)
        .bind(&p.computer_name)
        .execute(pool)
        .await;

        out.push(EnqueueResult {
            computer_name: p.computer_name.clone(),
            defer_id: id,
            tool_id: p.tool_id.clone(),
        });
    }
    Ok(out)
}

/// High-level one-shot: look up the playbook for `tool_id` on
/// `computer_name` and enqueue exactly one install task.
///
/// This mirrors `auto_upgrade::resolve_upgrade_plans` + `enqueue_plans`
/// in one call for CLI ergonomics. Returns an error if the resolver
/// produced no plans (e.g. unknown computer, no playbook match).
pub async fn install_on(
    pool: &PgPool,
    tool_id: &str,
    computer_name: &str,
    who: &str,
) -> Result<EnqueueResult> {
    let (plans, skipped) = resolve_install_plans(pool, tool_id, Some(computer_name), false).await?;
    if plans.is_empty() {
        if let Some((_, why)) = skipped.first() {
            anyhow::bail!("no install plan for {tool_id} on {computer_name}: {why}");
        }
        anyhow::bail!(
            "no install plan for {tool_id} on {computer_name} (is the computer in `computers`?)"
        );
    }
    let mut enq = enqueue_plans(pool, &plans, who).await?;
    Ok(enq.remove(0))
}

/// Resolve the artifact-cache entry for a target from
/// `external_tools.metadata.artifact_cache`, using the same key
/// precedence as the playbook lookup. Malformed entries (missing
/// fields, non-hex/wrong-length sha256) are skipped so a bad cache
/// config degrades to the plain WAN install rather than failing.
fn resolve_artifact_cache(
    metadata: &JsonValue,
    candidates: &[String],
) -> Option<ArtifactCacheSpec> {
    let cache = metadata.get("artifact_cache")?;
    for key in candidates {
        let Some(entry) = cache.get(key) else {
            continue;
        };
        let (Some(source), Some(sha256), Some(install_cmd)) = (
            entry.get("source").and_then(|v| v.as_str()),
            entry.get("sha256").and_then(|v| v.as_str()),
            entry.get("install_cmd").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let sha = sha256.trim().to_ascii_lowercase();
        if source.is_empty()
            || install_cmd.is_empty()
            || sha.len() != 64
            || !sha.chars().all(|c| c.is_ascii_hexdigit())
        {
            continue;
        }
        return Some(ArtifactCacheSpec {
            source: source.to_string(),
            sha256: sha,
            install_cmd: install_cmd.to_string(),
        });
    }
    None
}

/// Wrap a playbook command cache-first. POSIX sh (the defer worker runs
/// commands via `sh -c`, dash on Ubuntu): validate an existing node-local
/// artifact first, rsync only on a miss, validate SHA256 (`sha256sum` with
/// `shasum -a 256` fallback for macOS), then run `install_cmd` with
/// `$FF_ARTIFACT` exported. Any failure — rsync unreachable, checksum mismatch
/// — drops the fetched file and falls back to the original WAN command.
fn wrap_with_artifact_cache(tool_id: &str, spec: &ArtifactCacheSpec, wan_command: &str) -> String {
    // Basename of the rsync source ("user@host:/a/b/tool.tar.gz" → "tool.tar.gz").
    let file_name = spec
        .source
        .rsplit('/')
        .next()
        .and_then(|s| s.rsplit(':').next())
        .filter(|s| !s.is_empty())
        .unwrap_or("artifact");
    let sha = &spec.sha256;
    let q_tool = shell_quote(tool_id);
    let q_src = shell_quote(&spec.source);
    let q_file = shell_quote(file_name);
    let hit_msg = shell_quote(&format!(
        "[ff] {tool_id}: artifact cache hit (sha256 ok); installing from cache"
    ));
    let miss_msg = shell_quote(&format!(
        "[ff] {tool_id}: artifact cache miss or sha256 mismatch; falling back to WAN install"
    ));
    let install_cmd = &spec.install_cmd;
    format!(
        "FF_CACHE_DIR=\"${{FORGEFLEET_ARTIFACT_CACHE:-$HOME/.forgefleet/artifact-cache}}\"/{q_tool}\n\
         FF_ARTIFACT=\"$FF_CACHE_DIR\"/{q_file}\n\
         export FF_ARTIFACT\n\
         ff_artifact_valid() {{\n\
           [ -f \"$FF_ARTIFACT\" ] && [ \"$({{ command -v sha256sum >/dev/null 2>&1 && sha256sum \"$FF_ARTIFACT\" || shasum -a 256 \"$FF_ARTIFACT\"; }} | awk '{{print $1}}')\" = \"{sha}\" ]\n\
         }}\n\
         if ff_artifact_valid \\\n\
            || {{ mkdir -p \"$FF_CACHE_DIR\" \\\n\
                 && rsync -a --partial {q_src} \"$FF_ARTIFACT\" \\\n\
                 && ff_artifact_valid; }}; then\n\
           echo {hit_msg}\n\
           {install_cmd}\n\
         else\n\
           echo {miss_msg} >&2\n\
           rm -f \"$FF_ARTIFACT\"\n\
           {wan_command}\n\
         fi"
    )
}

/// Conservative single-quote shell quoting.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Map an install_method to the column value stored in
/// `computer_external_tools.install_source`.
fn derive_install_source(install_method: &str) -> &'static str {
    match install_method {
        "cargo_install" => "cargo",
        "npm_global" => "npm",
        "pip" => "pip",
        "git_build" => "git_build",
        "binary_release" => "direct",
        _ => "direct",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ArtifactCacheSpec {
        ArtifactCacheSpec {
            source: "ff@sophie:/srv/ff-artifacts/crg/crg-linux-x86_64".into(),
            sha256: "a".repeat(64),
            install_cmd: "install -m 755 \"$FF_ARTIFACT\" ~/.local/bin/crg".into(),
        }
    }

    /// Run a wrapped command through a real `sh` (mirrors the defer
    /// worker's `sh -c`), with the cache rooted in a temp dir.
    fn run_sh(cmd: &str, cache_root: &std::path::Path) -> std::process::Output {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("FORGEFLEET_ARTIFACT_CACHE", cache_root)
            .output()
            .expect("spawn sh")
    }

    fn have_cmd(cmd: &str) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {cmd} >/dev/null 2>&1"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn artifact_cache_resolves_first_matching_candidate_key() {
        let meta = json!({
            "artifact_cache": {
                "linux": {
                    "source": "ff@sophie:/srv/a/tool-linux",
                    "sha256": "b".repeat(64),
                    "install_cmd": "install \"$FF_ARTIFACT\" ~/.local/bin/tool",
                },
                "all": {
                    "source": "ff@sophie:/srv/a/tool-any",
                    "sha256": "c".repeat(64),
                    "install_cmd": "install \"$FF_ARTIFACT\" ~/.local/bin/tool",
                },
            }
        });
        let candidates = vec![
            "linux-ubuntu".to_string(),
            "linux".to_string(),
            "all".to_string(),
        ];
        let got = resolve_artifact_cache(&meta, &candidates).expect("resolves");
        assert_eq!(got.source, "ff@sophie:/srv/a/tool-linux");
        assert_eq!(got.sha256, "b".repeat(64));
    }

    #[test]
    fn artifact_cache_skips_malformed_entries() {
        // No artifact_cache key at all.
        assert_eq!(
            resolve_artifact_cache(&json!({}), &["all".to_string()]),
            None
        );
        // Missing install_cmd.
        let missing_field = json!({
            "artifact_cache": { "all": { "source": "x", "sha256": "d".repeat(64) } }
        });
        assert_eq!(
            resolve_artifact_cache(&missing_field, &["all".to_string()]),
            None
        );
        // sha256 not 64 hex chars.
        let bad_sha = json!({
            "artifact_cache": { "all": {
                "source": "x", "sha256": "not-hex", "install_cmd": "true",
            } }
        });
        assert_eq!(resolve_artifact_cache(&bad_sha, &["all".to_string()]), None);
    }

    #[test]
    fn wrapped_command_embeds_cache_check_and_wan_fallback() {
        let wrapped = wrap_with_artifact_cache("crg", &spec(), "cargo install crg");
        assert!(wrapped.contains("if ff_artifact_valid"));
        assert!(wrapped.contains("rsync -a --partial"));
        assert!(wrapped.contains("'ff@sophie:/srv/ff-artifacts/crg/crg-linux-x86_64'"));
        assert!(wrapped.contains(&"a".repeat(64)));
        assert!(wrapped.contains("install -m 755 \"$FF_ARTIFACT\""));
        // WAN command survives as the fallback branch.
        assert!(wrapped.contains("cargo install crg"));
        // Artifact lands under the per-tool cache dir, named after the source.
        assert!(wrapped.contains("/'crg'"));
        assert!(wrapped.contains("/'crg-linux-x86_64'"));
    }

    #[test]
    fn wrapped_command_uses_existing_local_artifact_before_rsync() {
        if !(have_cmd("sha256sum") || have_cmd("shasum")) {
            return;
        }
        let cache = tempfile::tempdir().expect("tempdir");
        let local_dir = cache.path().join("crg");
        std::fs::create_dir_all(&local_dir).expect("create local cache");
        std::fs::write(local_dir.join("tool-bin"), b"artifact-payload")
            .expect("seed local artifact");
        let installed = cache.path().join("installed-bin");

        let mut s = spec();
        // If the wrapper invokes rsync this source fails, proving a successful
        // install came from the node-local artifact.
        s.source = cache
            .path()
            .join("missing")
            .join("tool-bin")
            .display()
            .to_string();
        s.sha256 = "5c6fd60a6ad0ce3fffdf2f2c61fbf1e9677f780c64a1ee33563bb2a40f29ef80".into();
        s.install_cmd = format!(
            "cp \"$FF_ARTIFACT\" {}",
            shell_quote(&installed.display().to_string())
        );

        let wrapped = wrap_with_artifact_cache("crg", &s, "echo WAN_INSTALL_RAN");
        let out = run_sh(&wrapped, cache.path());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "wrapper exited nonzero: {out:?}");
        assert!(!stdout.contains("WAN_INSTALL_RAN"), "stdout: {stdout}");
        assert_eq!(
            std::fs::read(&installed).expect("installed artifact"),
            b"artifact-payload"
        );
    }

    #[test]
    fn wrapped_command_falls_back_to_wan_on_cache_miss() {
        let cache = tempfile::tempdir().expect("tempdir");
        let mut s = spec();
        // Local nonexistent source: rsync fails (or rsync itself is
        // missing) → either way the wrapper must take the WAN branch.
        s.source = cache.path().join("no-such-artifact").display().to_string();
        s.install_cmd = "echo CACHE_INSTALL_RAN".into();
        let wrapped = wrap_with_artifact_cache("crg", &s, "echo WAN_INSTALL_RAN");
        let out = run_sh(&wrapped, cache.path());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "wrapper exited nonzero: {out:?}");
        assert!(stdout.contains("WAN_INSTALL_RAN"), "stdout: {stdout}");
        assert!(!stdout.contains("CACHE_INSTALL_RAN"), "stdout: {stdout}");
    }

    #[test]
    fn wrapped_command_installs_from_cache_when_sha_matches() {
        if !have_cmd("rsync") || !(have_cmd("sha256sum") || have_cmd("shasum")) {
            return; // host can't exercise the hit path
        }
        let cache = tempfile::tempdir().expect("tempdir");
        let src = cache.path().join("tool-bin");
        std::fs::write(&src, b"artifact-payload").expect("write src");
        let installed = cache.path().join("installed-bin");
        let mut s = spec();
        s.source = src.display().to_string();
        // sha256 of b"artifact-payload" (printf 'artifact-payload' | sha256sum)
        s.sha256 = "5c6fd60a6ad0ce3fffdf2f2c61fbf1e9677f780c64a1ee33563bb2a40f29ef80".into();
        s.install_cmd = format!(
            "cp \"$FF_ARTIFACT\" {}",
            shell_quote(&installed.display().to_string())
        );
        let wrapped = wrap_with_artifact_cache("crg", &s, "echo WAN_INSTALL_RAN");
        let out = run_sh(&wrapped, cache.path());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "wrapper exited nonzero: {out:?}");
        assert!(!stdout.contains("WAN_INSTALL_RAN"), "stdout: {stdout}");
        assert_eq!(
            std::fs::read(&installed).expect("installed artifact"),
            b"artifact-payload"
        );
    }

    #[test]
    fn wrapped_command_rejects_sha_mismatch_and_falls_back() {
        if !have_cmd("rsync") || !(have_cmd("sha256sum") || have_cmd("shasum")) {
            return;
        }
        let cache = tempfile::tempdir().expect("tempdir");
        let src = cache.path().join("tool-bin");
        std::fs::write(&src, b"tampered-payload").expect("write src");
        let mut s = spec();
        s.source = src.display().to_string();
        s.sha256 = "0".repeat(64); // wrong on purpose
        s.install_cmd = "echo CACHE_INSTALL_RAN".into();
        let wrapped = wrap_with_artifact_cache("crg", &s, "echo WAN_INSTALL_RAN");
        let out = run_sh(&wrapped, cache.path());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("WAN_INSTALL_RAN"), "stdout: {stdout}");
        assert!(!stdout.contains("CACHE_INSTALL_RAN"), "stdout: {stdout}");
        // The mismatched artifact must not linger in the cache.
        assert!(!cache.path().join("crg").join("tool-bin").exists());
    }

    #[test]
    fn install_source_mapping_covers_known_methods() {
        assert_eq!(derive_install_source("cargo_install"), "cargo");
        assert_eq!(derive_install_source("npm_global"), "npm");
        assert_eq!(derive_install_source("pip"), "pip");
        assert_eq!(derive_install_source("git_build"), "git_build");
        assert_eq!(derive_install_source("binary_release"), "direct");
        // Unknown methods fall back to "direct" rather than blowing up.
        assert_eq!(derive_install_source("mystery"), "direct");
    }
}
