//! Copy a model file from one fleet node to another via rsync-over-SSH.
//!
//! Runs from the caller's host (which may be neither the source nor the
//! target). We SSH into the **target** node and have it pull from the
//! **source** node — this requires the target to have SSH keys for the
//! source (which it does on this fleet).
//!
//! After the transfer we insert a new `fleet_model_library` row on the
//! target side pointing at the new file path. Runtime/quant/catalog_id/etc
//! are copied from the source row.

use sqlx::{PgPool, Row};
use tokio::process::Command;

/// Options for [`transfer_model`].
#[derive(Debug, Clone)]
pub struct TransferOptions {
    pub source_node: String,
    pub target_node: String,
    /// UUID of the `fleet_model_library` row on the source node to copy.
    pub library_id: String,
}

/// Result of a successful transfer.
#[derive(Debug, Clone)]
pub struct TransferResult {
    /// UUID of the new `fleet_model_library` row on the target node.
    pub target_library_id: String,
    pub bytes_transferred: u64,
}

/// Minimal subset of the library row we need for transfer.
struct LibraryRow {
    node_name: String,
    catalog_id: String,
    runtime: String,
    quant: Option<String>,
    file_path: String,
    size_bytes: i64,
    sha256: Option<String>,
    source_url: Option<String>,
}

async fn fetch_library(pool: &PgPool, id: &str) -> Result<LibraryRow, String> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| format!("bad library uuid {id}: {e}"))?;
    let row = sqlx::query(
        "SELECT node_name, catalog_id, runtime, quant, file_path, size_bytes, sha256, source_url
         FROM fleet_model_library WHERE id = $1",
    )
    .bind(uuid)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("query library {id}: {e}"))?
    .ok_or_else(|| format!("library row {id} not found"))?;

    Ok(LibraryRow {
        node_name: row.get("node_name"),
        catalog_id: row.get("catalog_id"),
        runtime: row.get("runtime"),
        quant: row.get("quant"),
        file_path: row.get("file_path"),
        size_bytes: row.get("size_bytes"),
        sha256: row.get("sha256"),
        source_url: row.get("source_url"),
    })
}

/// Run a command over SSH against a fleet node.
///
/// Returns `(exit_code, stdout, stderr)`. Uses `BatchMode=yes` so it fails
/// immediately on auth prompts rather than hanging.
pub async fn ssh_exec(user: &str, host: &str, cmd: &str) -> Result<(i32, String, String), String> {
    let target = format!("{user}@{host}");
    let output = Command::new("ssh")
        .arg("-o").arg("ConnectTimeout=10")
        .arg("-o").arg("StrictHostKeyChecking=accept-new")
        .arg("-o").arg("BatchMode=yes")
        .arg(&target)
        .arg(cmd)
        .output()
        .await
        .map_err(|e| format!("spawn ssh {target}: {e}"))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((code, stdout, stderr))
}

/// Expand a leading `~` / `~/` in a remote path by prefixing the user's
/// home directory on the remote node. Non-tilde paths are returned as-is.
/// This is used to turn `~/models` (as stored in `fleet_nodes.models_dir`)
/// into an absolute path usable as an rsync destination.
fn expand_tilde_with_home(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", home.trim_end_matches('/'), rest)
    } else {
        path.to_string()
    }
}

/// Transfer a model file from one node to another and register a new
/// library row on the target node.
///
/// Preconditions: both nodes must have the same `runtime`. Target must
/// have SSH access to source (keys). This function does not preflight
/// free-disk on the target — rsync will just fail if there's no room.
pub async fn transfer_model(
    pool: &PgPool,
    opts: TransferOptions,
) -> Result<TransferResult, String> {
    // 1. Fetch source library row.
    let src_lib = fetch_library(pool, &opts.library_id).await?;
    if src_lib.node_name != opts.source_node {
        return Err(format!(
            "library {} belongs to node {}, not {}",
            opts.library_id, src_lib.node_name, opts.source_node
        ));
    }

    // 2. Look up source + target node rows.
    let src_node = ff_db::pg_get_node(pool, &opts.source_node)
        .await
        .map_err(|e| format!("pg_get_node({}): {e}", opts.source_node))?
        .ok_or_else(|| format!("source node {} not found", opts.source_node))?;
    let dst_node = ff_db::pg_get_node(pool, &opts.target_node)
        .await
        .map_err(|e| format!("pg_get_node({}): {e}", opts.target_node))?
        .ok_or_else(|| format!("target node {} not found", opts.target_node))?;

    // 3. Runtime must match.
    if src_node.runtime != dst_node.runtime {
        return Err(format!(
            "runtime mismatch: source {} is {}, target {} is {} — cannot transfer across runtimes",
            src_node.name, src_node.runtime, dst_node.name, dst_node.runtime
        ));
    }

    // 4. Compute destination path on target.
    // We assume the target's home is /home/<user> on linux or /Users/<user>
    // on macOS. Rather than hardcoding, ask the target shell.
    let (hc, home_stdout, home_stderr) =
        ssh_exec(&dst_node.ssh_user, &dst_node.ip, "echo $HOME").await?;
    if hc != 0 {
        return Err(format!(
            "ssh to target {}@{} failed (exit {hc}): {home_stderr}",
            dst_node.ssh_user, dst_node.ip
        ));
    }
    let dst_home = home_stdout.trim().to_string();
    if dst_home.is_empty() {
        return Err("target $HOME resolved empty".to_string());
    }
    let dst_models_dir = expand_tilde_with_home(&dst_node.models_dir, &dst_home);

    let basename = std::path::Path::new(&src_lib.file_path)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("source file_path has no basename: {}", src_lib.file_path))?
        .to_string();
    let dst_path = format!("{}/{}", dst_models_dir.trim_end_matches('/'), basename);

    // 5. Ensure the destination directory exists.
    let mkdir_cmd = format!("mkdir -p {}", shell_quote(&dst_models_dir));
    let (mc, _, mstderr) = ssh_exec(&dst_node.ssh_user, &dst_node.ip, &mkdir_cmd).await?;
    if mc != 0 {
        return Err(format!(
            "mkdir -p {dst_models_dir} on {} failed (exit {mc}): {mstderr}",
            dst_node.name
        ));
    }

    // 6. Run rsync on the target, pulling from the source.
    println!(
        "Transferring {} ({}) from {} to {} …",
        basename,
        human_bytes(src_lib.size_bytes as u64),
        src_node.name,
        dst_node.name,
    );
    let remote_src = format!(
        "{}@{}:{}",
        src_node.ssh_user,
        src_node.ip,
        shell_quote(&src_lib.file_path),
    );
    let rsync_cmd = format!(
        "rsync -av --partial -e 'ssh -o StrictHostKeyChecking=accept-new -o BatchMode=yes' {} {}",
        remote_src,
        shell_quote(&dst_path),
    );
    let (rc, rstdout, rstderr) = ssh_exec(&dst_node.ssh_user, &dst_node.ip, &rsync_cmd).await?;
    if rc != 0 {
        return Err(format!(
            "rsync failed (exit {rc}).\nstdout:\n{rstdout}\nstderr:\n{rstderr}",
        ));
    }

    // 7. Verify size on target (best effort — also serves as existence check).
    let stat_cmd = format!(
        "stat -c %s {} 2>/dev/null || stat -f %z {}",
        shell_quote(&dst_path),
        shell_quote(&dst_path),
    );
    let (sc, sstdout, _sstderr) = ssh_exec(&dst_node.ssh_user, &dst_node.ip, &stat_cmd).await?;
    let bytes_transferred = if sc == 0 {
        sstdout.trim().parse::<u64>().unwrap_or(src_lib.size_bytes.max(0) as u64)
    } else {
        src_lib.size_bytes.max(0) as u64
    };

    // 8. Register new library row on target.
    let new_id = ff_db::pg_upsert_library(
        pool,
        &dst_node.name,
        &src_lib.catalog_id,
        &src_lib.runtime,
        src_lib.quant.as_deref(),
        &dst_path,
        bytes_transferred as i64,
        src_lib.sha256.as_deref(),
        src_lib.source_url.as_deref(),
    )
    .await
    .map_err(|e| format!("pg_upsert_library on target: {e}"))?;

    Ok(TransferResult {
        target_library_id: new_id,
        bytes_transferred,
    })
}

/// Conservative single-quote shell quoting for paths.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Tiny byte-count formatter so we don't pull in a crate just for logging.
fn human_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.2} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.2} KB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        assert_eq!(expand_tilde_with_home("~/models", "/home/alice"), "/home/alice/models");
        assert_eq!(expand_tilde_with_home("~", "/home/alice"), "/home/alice");
        assert_eq!(expand_tilde_with_home("/abs/path", "/home/alice"), "/abs/path");
        assert_eq!(expand_tilde_with_home("~/a/b/", "/home/alice/"), "/home/alice/a/b/");
    }

    #[test]
    fn shell_quote_handles_apostrophes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
