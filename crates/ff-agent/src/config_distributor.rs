//! Distribute non-secret CLI-backend config files from the leader to every
//! fleet member.
//!
//! `oauth_distributor` fans out each backend's *credential*; this fans out its
//! *config* — the tuned `~/.kimi/config.toml` (model routing, loop_control,
//! MCP wiring) that makes kimi-as-a-dispatch-backend behave the same on every
//! node. Without it a follower's kimi runs unconfigured (wrong model, no
//! forgefleet MCP), so a Lane-2 `ff cli kimi` dispatch there diverges from the
//! leader. The payload is plain config (mode 0644), NOT a credential — so no
//! Keychain source and no TOS gate; it reuses the same SSH + `fleet_tasks`
//! shell-dispatch fan-out as the OAuth path.

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use sqlx::{PgPool, Row};
use tracing::{info, warn};

use crate::task_runner::pg_enqueue_shell_task;

/// One config file to sync: a leader-local source and its remote destination
/// (both may start with `~`, expanded on each side by the login shell).
#[derive(Debug, Clone, Copy)]
pub struct ConfigFile {
    pub source: &'static str,
    pub dest: &'static str,
}

/// The kimi backend config set. Both live under `~/.kimi` and are non-secret:
/// `config.toml`'s `api_key` is empty (OAuth is a *file reference* resolved
/// from the separately-distributed credential), and `config.json` only points
/// at the local forgefleet MCP endpoint.
pub const KIMI_CONFIG_FILES: &[ConfigFile] = &[
    ConfigFile {
        source: "~/.kimi/config.toml",
        dest: "~/.kimi/config.toml",
    },
    ConfigFile {
        source: "~/.kimi/config.json",
        dest: "~/.kimi/config.json",
    },
];

/// Expand a leading `~/` against the leader's `$HOME`.
fn expand_home(path: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    expand_home_with(path, home.as_deref())
}

/// Pure core of [`expand_home`]: `~/` → `<home>/…`, absolute/relative paths
/// pass through untouched. Split out so tests need not mutate the global `$HOME`
/// (a process-wide hazard in a parallel test binary).
fn expand_home_with(path: &str, home: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    Some(std::path::PathBuf::from(path))
}

/// Read + base64 every source file that actually exists on the leader. A
/// missing source is skipped with a warning (not an error) so a partial config
/// set still distributes what it has.
async fn collect_payloads(files: &[ConfigFile]) -> Result<Vec<(ConfigFile, String)>> {
    let mut out = Vec::new();
    for f in files {
        let Some(local) = expand_home(f.source) else {
            warn!(
                source = f.source,
                "config_distributor: cannot expand source path — skipping"
            );
            continue;
        };
        match tokio::fs::read(&local).await {
            Ok(bytes) => out.push((*f, BASE64.encode(&bytes))),
            Err(e) => warn!(
                source = f.source, error = %e,
                "config_distributor: leader source file absent — skipping"
            ),
        }
    }
    Ok(out)
}

/// Render the per-host distribute shell command. Pure (no I/O) so it's unit-
/// testable. The task is enqueued with `preferred_computer = target`, so it
/// almost always runs ON the target already: in that case write LOCALLY and
/// skip the SSH entirely — a self-SSH to one's own IP is pure fragility (priya's
/// loopback SSH wedges with exit 255, failing an otherwise-fine sync). Only when
/// a DIFFERENT worker claimed the task do we SSH to the real target (preferred is
/// a preference, not a guarantee — so correctness is preserved either way).
/// On-target detection is portable: this host's IPv4s from `hostname -I` (Linux)
/// + `ifconfig` (macOS), tested for the target IP.
fn render_distribute_command(
    label: &str,
    target: &str,
    ssh_user: &str,
    primary_ip: &str,
    payloads: &[(ConfigFile, String)],
) -> String {
    // The write-block: one mkdir+base64-decode per file. The b64 blobs are
    // single-quoted so `$` is never shell-expanded. Config is non-secret →
    // mode 0644. `stat` differs macOS/Linux, hence the `||` fallback.
    let mut writes = String::new();
    for (f, b64) in payloads {
        writes.push_str(&format!(
            "mkdir -p \"$(dirname {dest})\"\n\
             printf '%s' '{b64}' | base64 -d > {dest}\n\
             chmod 644 {dest}\n\
             echo wrote: {dest} \\($(stat -c %s {dest} 2>/dev/null || stat -f %z {dest}) bytes\\)\n",
            dest = f.dest,
            b64 = b64,
        ));
    }
    format!(
        "set -e\n\
         echo \"== distributing {label} config to {target} ==\"\n\
         __ff_local_ips() {{ (hostname -I 2>/dev/null; \
             ifconfig 2>/dev/null | awk '/inet /{{print $2}}') | tr ' ' '\\n'; }}\n\
         if __ff_local_ips | grep -Fxq {primary_ip}; then\n\
         echo '(on target — local write, no SSH)'\n\
         {writes}\
         else\n\
         ssh -T {ssh_bypass} -o StrictHostKeyChecking=accept-new \
             {ssh_user}@{primary_ip} bash -l <<'FF_CFG_EOF'\n\
         {writes}\
         FF_CFG_EOF\n\
         fi\n",
        ssh_bypass = crate::ssh_opts::SSH_AGENT_BYPASS,
    )
}

/// Distribute `files` from the leader to every online fleet member except the
/// leader. Returns the number of per-host distribute tasks enqueued. Each host
/// gets ONE task that writes all present files (mode 0644) via an SSH heredoc,
/// mirroring `oauth_distributor::distribute_token`.
pub async fn distribute_config_files(
    pool: &PgPool,
    label: &str,
    files: &[ConfigFile],
) -> Result<usize> {
    let payloads = collect_payloads(files).await?;
    if payloads.is_empty() {
        anyhow::bail!("no {label} config files present on the leader — nothing to distribute");
    }

    let leader_id = ff_db::pg_get_current_leader(pool)
        .await
        .ok()
        .flatten()
        .map(|l| l.computer_id);

    let rows = sqlx::query(
        "SELECT id, name, ssh_user, primary_ip
           FROM computers
          WHERE status IN ('online', 'ok', 'pending', 'maintenance')",
    )
    .fetch_all(pool)
    .await
    .context("list computers")?;

    let mut enqueued = 0usize;
    for row in rows {
        let id: uuid::Uuid = row.get("id");
        if Some(id) == leader_id {
            continue; // leader's local copy is authoritative
        }
        let name: String = row.get("name");
        let ssh_user: String = row.get("ssh_user");
        let primary_ip: String = row.get("primary_ip");

        let cmd = render_distribute_command(label, &name, &ssh_user, &primary_ip, &payloads);

        pg_enqueue_shell_task(
            pool,
            &format!("config-distribute/{label}: {label} → {name}"),
            &cmd,
            &[],
            Some(&name),
            None,
            70,
            None,
        )
        .await
        .with_context(|| format!("enqueue {label} config distribute task for {name}"))?;
        enqueued += 1;
    }

    info!(label, enqueued, "config distribute tasks enqueued");
    Ok(enqueued)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_expands_tilde() {
        let home = std::path::Path::new("/home/test");
        assert_eq!(
            expand_home_with("~/.kimi/config.toml", Some(home)).unwrap(),
            std::path::PathBuf::from("/home/test/.kimi/config.toml")
        );
        // Absolute paths pass through untouched (home irrelevant).
        assert_eq!(
            expand_home_with("/etc/thing", Some(home)).unwrap(),
            std::path::PathBuf::from("/etc/thing")
        );
        // `~/` with no home → None (caller skips the file).
        assert!(expand_home_with("~/.kimi/config.toml", None).is_none());
    }

    #[test]
    fn kimi_config_set_covers_toml_and_json() {
        let dests: Vec<&str> = KIMI_CONFIG_FILES.iter().map(|f| f.dest).collect();
        assert!(dests.contains(&"~/.kimi/config.toml"));
        assert!(dests.contains(&"~/.kimi/config.json"));
    }

    #[test]
    fn rendered_command_has_local_and_ssh_branches() {
        let payloads = vec![(
            ConfigFile {
                source: "~/.kimi/config.toml",
                dest: "~/.kimi/config.toml",
            },
            "YWJj".to_string(), // base64("abc")
        )];
        let cmd = render_distribute_command("kimi", "priya", "priya", "192.168.5.104", &payloads);
        // On-target fast path avoids the fragile self-SSH.
        assert!(cmd.contains("__ff_local_ips | grep -Fxq 192.168.5.104"));
        assert!(cmd.contains("on target — local write, no SSH"));
        // Fallback still SSHes when a different worker claimed the task.
        assert!(cmd.contains("ssh -T"));
        assert!(cmd.contains("priya@192.168.5.104"));
        // The write-block appears in BOTH branches (local + heredoc).
        assert_eq!(cmd.matches("base64 -d > ~/.kimi/config.toml").count(), 2);
        // Non-secret perms.
        assert!(cmd.contains("chmod 644"));
    }
}
