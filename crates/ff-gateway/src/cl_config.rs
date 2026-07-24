//! Ensure a fleet user's Claude Code config has the ForgeFleet MCP server
//! wired in and dangerous-mode permission prompts disabled.
//!
//! The gateway runs as root, but Claude Code refuses to run in
//! `bypassPermissions` mode against a config it doesn't own, so every
//! command here is spawned as the target `user` via `sudo -H -u <user>`,
//! never executed directly by the root process.
//!
//! `-H` is load-bearing, not cosmetic: without it, `sudo -u <user>` does
//! not necessarily reset `HOME` to the target user's home directory — on
//! hosts where the sudoers policy doesn't set `always_set_home`, the
//! invoking (root) process's `HOME` leaks through to the child. Since both
//! `ff mcp install` (via `dirs::home_dir()`, which just reads `$HOME`) and
//! this module resolve paths from `HOME`, a missing `-H` silently
//! reconciles `/root/.claude/settings.json` instead of the target user's,
//! and `ensure_claude_config` would report success without having changed
//! anything the target user's Claude Code process actually reads. `-H`
//! forces sudo to set `HOME` from the target user's passwd entry
//! regardless of that policy setting.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Run `program args...` as `user`, never as root. See module docs for why
/// `-H` is required.
async fn run_as_user(user: &str, program: &str, args: &[&str]) -> Result<Output> {
    Command::new("sudo")
        .arg("-H")
        .arg("-u")
        .arg(user)
        .arg("--")
        .arg(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawning `{program}` as user {user}"))
}

/// Resolve `user`'s home directory the same way sudo itself would set
/// `HOME` for that user (via `-H`), rather than trusting the *invoking*
/// process's `$HOME`.
async fn resolve_home_dir(user: &str) -> Result<PathBuf> {
    let output = run_as_user(user, "sh", &["-c", "printf '%s' \"$HOME\""]).await?;
    if !output.status.success() {
        bail!(
            "could not resolve home directory for user {user}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let home = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if home.is_empty() {
        bail!("empty $HOME resolved for user {user}");
    }
    Ok(PathBuf::from(home))
}

/// Read a JSON file as `user`. A missing file (or unreadable one) is
/// treated as an empty object so callers can create it on first run.
async fn read_json_as_user(user: &str, path: &Path) -> Result<Value> {
    let path_str = path.to_str().context("settings path is not valid UTF-8")?;
    let output = run_as_user(user, "cat", &[path_str]).await?;
    if !output.status.success() {
        return Ok(json!({}));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write a JSON file as `user`, creating the parent directory if needed.
/// The path is passed positionally (`$1`), never interpolated into the
/// shell script, so it can't reintroduce a command-injection path.
async fn write_json_as_user(user: &str, path: &Path, value: &Value) -> Result<()> {
    let path_str = path.to_str().context("settings path is not valid UTF-8")?;
    let contents = serde_json::to_string_pretty(value)?;

    let mut child = Command::new("sudo")
        .arg("-H")
        .arg("-u")
        .arg(user)
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(r#"mkdir -p "$(dirname "$1")" && cat > "$1""#)
        .arg("_")
        .arg(path_str)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning writer for {} as user {user}", path.display()))?;

    child
        .stdin
        .take()
        .context("child stdin was not piped")?
        .write_all(contents.as_bytes())
        .await
        .with_context(|| format!("writing {}", path.display()))?;

    let status = child.wait().await?;
    if !status.success() {
        bail!("failed to write {} as user {user}", path.display());
    }
    Ok(())
}

/// Set `permissions.defaultMode = "bypassPermissions"` and
/// `skipDangerousModePermissionPrompt = true` on `settings`, preserving
/// every other key already present.
fn apply_permission_settings(settings: &mut Value) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let obj = settings
        .as_object_mut()
        .expect("normalized to object above");

    let permissions = obj
        .entry("permissions".to_string())
        .or_insert_with(|| json!({}));
    if !permissions.is_object() {
        *permissions = json!({});
    }
    permissions
        .as_object_mut()
        .expect("normalized to object above")
        .insert("defaultMode".to_string(), json!("bypassPermissions"));

    obj.insert("skipDangerousModePermissionPrompt".to_string(), json!(true));
}

/// `true` once `settings` reflects both required keys.
fn settings_satisfy_policy(settings: &Value) -> bool {
    settings
        .get("permissions")
        .and_then(|p| p.get("defaultMode"))
        .and_then(Value::as_str)
        == Some("bypassPermissions")
        && settings
            .get("skipDangerousModePermissionPrompt")
            .and_then(Value::as_bool)
            == Some(true)
}

/// Wire the ForgeFleet MCP server into `user`'s Claude Code config and set
/// it to run in `bypassPermissions` mode, entirely as `user` (never root).
///
/// Returns `Ok(())` only once the target's `~/.claude/settings.json` has
/// been read back and confirmed to carry both settings — i.e. only if the
/// drift is actually resolved, not merely if the write commands exited 0.
pub async fn ensure_claude_config(user: &str) -> Result<()> {
    if user.is_empty() || user == "root" {
        bail!("refusing to configure Claude Code for user {user:?}; must be a non-root user");
    }

    let home = resolve_home_dir(user).await?;
    let settings_path = home.join(".claude").join("settings.json");

    let install = run_as_user(user, "ff", &["mcp", "install", "--for", "all"]).await?;
    if !install.status.success() {
        bail!(
            "`ff mcp install --for all` failed for user {user}: {}",
            String::from_utf8_lossy(&install.stderr)
        );
    }

    let mut settings = read_json_as_user(user, &settings_path).await?;
    apply_permission_settings(&mut settings);
    write_json_as_user(user, &settings_path, &settings).await?;

    let verified = read_json_as_user(user, &settings_path).await?;
    if !settings_satisfy_policy(&verified) {
        bail!(
            "drift not resolved: {} does not carry bypassPermissions after write",
            settings_path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_permission_settings_sets_both_keys_on_empty_object() {
        let mut settings = json!({});
        apply_permission_settings(&mut settings);
        assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
    }

    #[test]
    fn apply_permission_settings_preserves_other_keys() {
        let mut settings = json!({
            "permissions": {"allow": ["Bash(git *)"]},
            "otherSetting": "keep-me",
        });
        apply_permission_settings(&mut settings);
        assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(settings["permissions"]["allow"][0], "Bash(git *)");
        assert_eq!(settings["otherSetting"], "keep-me");
    }

    #[test]
    fn apply_permission_settings_replaces_non_object_permissions() {
        let mut settings = json!({"permissions": "not-an-object"});
        apply_permission_settings(&mut settings);
        assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
    }

    #[test]
    fn settings_satisfy_policy_detects_missing_keys() {
        assert!(!settings_satisfy_policy(&json!({})));
        assert!(!settings_satisfy_policy(&json!({
            "permissions": {"defaultMode": "bypassPermissions"}
        })));
        assert!(settings_satisfy_policy(&json!({
            "permissions": {"defaultMode": "bypassPermissions"},
            "skipDangerousModePermissionPrompt": true,
        })));
    }
}
