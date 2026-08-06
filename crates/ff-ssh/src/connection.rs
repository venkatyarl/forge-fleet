use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::oneshot;

use crate::config::SshNodeConfig;

const DEFAULT_DIRECT_COMMAND_TIMEOUT_SECS: u64 = 60;
const MAX_OPENSSH_DEBUG_LOG_BYTES: usize = 256 * 1024;
const OPENSSH_REMOTE_EXIT_255: &str = "debug1: Exit status 255";

/// SSH authentication mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SshAuth {
    /// Use SSH agent / default OpenSSH auth behavior.
    Agent,
    /// Use explicit private key file.
    KeyFile(PathBuf),
    /// Use password-based SSH auth (requires `sshpass`).
    Password(String),
}

/// Connection and invocation options for an SSH session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshConnectionOptions {
    pub host: String,
    pub username: String,
    pub port: u16,
    pub auth: SshAuth,
    #[serde(default = "default_batch_mode")]
    pub batch_mode: bool,
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default)]
    pub command_timeout_secs: Option<u64>,
    #[serde(default = "default_strict_host_key_checking")]
    pub strict_host_key_checking: bool,
    #[serde(default)]
    pub known_hosts_path: Option<PathBuf>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Optional name → IP roster. When `host` is a node name/alias rather than a
    /// literal IP, the connection logic resolves it to the roster IP before
    /// invoking ssh, ensuring fleet SSH checks always target the canonical IP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roster: Option<HashMap<String, String>>,
}

fn default_batch_mode() -> bool {
    true
}

fn default_strict_host_key_checking() -> bool {
    true
}

impl SshConnectionOptions {
    pub fn from_node(node: &SshNodeConfig) -> Self {
        let auth = match (node.key_path.clone(), node.password.clone()) {
            (Some(path), _) => SshAuth::KeyFile(path),
            (None, Some(password)) => SshAuth::Password(password),
            (None, None) => SshAuth::Agent,
        };

        Self {
            host: node.host.clone(),
            username: node.username.clone(),
            port: node.port,
            auth,
            batch_mode: node.batch_mode,
            connect_timeout_secs: node.connect_timeout_secs,
            // Direct/synchronous callers (notably key distribution) do not
            // pass through `RemoteExecutor`, so they still need a finite
            // command deadline. Preserve an explicitly configured node bound
            // and otherwise use a conservative default.
            command_timeout_secs: Some(
                node.connect_timeout_secs
                    .unwrap_or(DEFAULT_DIRECT_COMMAND_TIMEOUT_SECS)
                    .max(1),
            ),
            strict_host_key_checking: true,
            known_hosts_path: node.known_hosts_path.clone(),
            extra_args: Vec::new(),
            roster: None,
        }
    }
}

/// Output captured from an SSH command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshCommandOutput {
    pub target: String,
    pub command: String,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u128,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum SshConnectionError {
    #[error("failed to spawn SSH command: {0}")]
    Spawn(#[from] std::io::Error),

    #[error("ssh command timed out after {timeout_secs}s")]
    TimedOut { timeout_secs: u64 },

    #[error("ssh command was cancelled")]
    Cancelled,

    #[error("ssh transport failed: {message}")]
    Transport { message: String },

    #[error("password auth requested but `sshpass` was not found in PATH")]
    MissingSshPass,
}

/// A lightweight SSH connection wrapper using OpenSSH binary calls.
#[derive(Debug, Clone)]
pub struct SshConnection {
    options: SshConnectionOptions,
}

impl SshConnection {
    pub fn new(options: SshConnectionOptions) -> Self {
        Self { options }
    }

    pub fn options(&self) -> &SshConnectionOptions {
        &self.options
    }

    /// Verify remote reachability/auth by running `echo connected`.
    ///
    /// The connection always targets the roster IP when `host` resolves to a
    /// name/alias present in `options.roster`, falling back to the literal host
    /// string only when it is already an IP or absent from the roster.
    pub fn connect(&self) -> Result<SshCommandOutput, SshConnectionError> {
        self.execute("echo connected")
    }

    /// Execute a remote command over SSH and capture stdout/stderr/exit code.
    pub fn execute(&self, remote_command: &str) -> Result<SshCommandOutput, SshConnectionError> {
        // Some legacy key/connectivity APIs are synchronous. Run their request
        // on a short-lived dedicated thread so they use the exact same async
        // supervisor without attempting to nest a Tokio runtime.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(SshConnectionError::Spawn)?
                        .block_on(self.execute_async(remote_command))
                })
                .join()
                .map_err(|_| SshConnectionError::Transport {
                    message: "SSH supervisor thread panicked".to_string(),
                })?
        })
    }

    /// Execute SSH without blocking a Tokio worker thread.
    ///
    /// The supervisor owns the child until it is reaped. Dropping this future
    /// signals the supervisor to terminate the SSH process group, so an MCP
    /// request cancellation cannot orphan `ssh` or its pipe-holding children.
    pub async fn execute_async(
        &self,
        remote_command: &str,
    ) -> Result<SshCommandOutput, SshConnectionError> {
        let started_at = Utc::now();
        let started = Instant::now();
        let command = remote_command.to_string();
        let host = self.resolve_host();
        let timeout_secs = self.options.command_timeout_secs.filter(|secs| *secs > 0);
        let (output, exit_code) = run_with_openssh_diagnostics(
            |debug_log| self.build_command(remote_command, debug_log),
            timeout_secs,
            None,
        )
        .await?;

        let result = SshCommandOutput {
            target: format!("{}@{}:{}", self.options.username, host, self.options.port),
            command,
            started_at,
            duration_ms: started.elapsed().as_millis(),
            exit_code,
            success: exit_code == Some(0),
            stdout: bounded_text(&output.stdout),
            stderr: bounded_text(&output.stderr),
        };
        Ok(result)
    }

    /// Resolve `options.host` to the roster IP when the host is a node
    /// name/alias, or return it unchanged when it is already an IP address.
    fn resolve_host(&self) -> String {
        let host = &self.options.host;
        if host.parse::<IpAddr>().is_ok() {
            return host.clone();
        }
        if let Some(roster) = &self.options.roster {
            if let Some(ip) = roster.get(host) {
                return ip.clone();
            }
        }
        host.clone()
    }

    fn build_command(
        &self,
        remote_command: &str,
        debug_log: &Path,
    ) -> Result<Command, SshConnectionError> {
        let mut cmd = match &self.options.auth {
            SshAuth::Password(password) => {
                if !command_exists("sshpass") {
                    return Err(SshConnectionError::MissingSshPass);
                }
                let mut c = Command::new("sshpass");
                c.arg("-p").arg(password).arg("ssh");
                c
            }
            _ => Command::new("ssh"),
        };

        // OpenSSH's locally generated debug stream is a trusted channel which
        // the remote payload cannot read or forge. `-E` is supported by the
        // fleet's Linux and macOS OpenSSH clients and keeps payload stderr on
        // the ordinary stderr pipe.
        cmd.arg("-v").arg("-E").arg(debug_log);
        cmd.arg("-p").arg(self.options.port.to_string());

        cmd.arg("-o")
            .arg(format!("BatchMode={}", yes_no(self.options.batch_mode)));

        if let Some(timeout_secs) = self.options.connect_timeout_secs {
            cmd.arg("-o").arg(format!("ConnectTimeout={timeout_secs}"));
        }

        cmd.arg("-o").arg(format!(
            "StrictHostKeyChecking={}",
            yes_no(self.options.strict_host_key_checking)
        ));

        if let Some(path) = &self.options.known_hosts_path {
            cmd.arg("-o")
                .arg(format!("UserKnownHostsFile={}", path.display()));
        }

        if let SshAuth::KeyFile(path) = &self.options.auth {
            cmd.arg("-i").arg(path);
        }

        for arg in &self.options.extra_args {
            cmd.arg(arg);
        }

        cmd.arg(format!("{}@{}", self.options.username, self.resolve_host()));
        cmd.arg(remote_command);

        Ok(cmd)
    }
}

const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const TERMINATE_GRACE: Duration = Duration::from_millis(250);
const REAP_GRACE: Duration = Duration::from_secs(1);

struct CancelOnDrop(Option<oneshot::Sender<()>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
struct AsyncOutput {
    status: std::process::ExitStatus,
    stdout: BoundedBytes,
    stderr: BoundedBytes,
}

/// A securely-created, caller-owned OpenSSH diagnostic file. Keeping the
/// original file descriptor open means classification reads the inode created
/// with `O_EXCL`, not a path an attacker may have replaced. The randomized
/// file is 0600 and is unlinked by `NamedTempFile::drop` on every return path,
/// including future cancellation.
struct OpenSshDiagnosticLog {
    file: tempfile::NamedTempFile,
}

impl OpenSshDiagnosticLog {
    fn new_in(parent: Option<&Path>) -> Result<Self, SshConnectionError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("ff-ssh-debug-");
        let file = match parent {
            Some(parent) => builder.tempfile_in(parent),
            None => builder.tempfile(),
        }
        .map_err(|error| SshConnectionError::Transport {
            message: format!("failed to create private OpenSSH diagnostic log: {error}"),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| SshConnectionError::Transport {
                    message: format!("failed to secure OpenSSH diagnostic log: {error}"),
                })?;
        }
        Ok(Self { file })
    }

    fn path(&self) -> &Path {
        self.file.path()
    }

    fn read_bounded(&mut self) -> Result<Vec<u8>, SshConnectionError> {
        self.verify_path_identity()?;
        let size = self
            .file
            .as_file()
            .metadata()
            .map_err(|error| SshConnectionError::Transport {
                message: format!("failed to inspect OpenSSH diagnostic log: {error}"),
            })?
            .len();
        if size >= MAX_OPENSSH_DEBUG_LOG_BYTES as u64 {
            return Err(SshConnectionError::Transport {
                message: "OpenSSH diagnostic log reached its hard size limit".to_string(),
            });
        }
        let file = self.file.as_file_mut();
        file.seek(SeekFrom::Start(0))
            .and_then(|_| {
                let mut bytes = Vec::new();
                file.take((MAX_OPENSSH_DEBUG_LOG_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            })
            .map_err(|error| SshConnectionError::Transport {
                message: format!("failed to read OpenSSH diagnostic log: {error}"),
            })
            .and_then(|bytes| {
                if bytes.len() > MAX_OPENSSH_DEBUG_LOG_BYTES {
                    Err(SshConnectionError::Transport {
                        message: "OpenSSH diagnostic log exceeded the trusted size limit"
                            .to_string(),
                    })
                } else {
                    Ok(bytes)
                }
            })
    }

    fn verify_path_identity(&self) -> Result<(), SshConnectionError> {
        let path_metadata =
            std::fs::symlink_metadata(self.path()).map_err(|_| SshConnectionError::Transport {
                message: "OpenSSH diagnostic log path disappeared".to_string(),
            })?;
        if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_file() {
            return Err(SshConnectionError::Transport {
                message: "OpenSSH diagnostic log path is not the private regular file".to_string(),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let open_metadata =
                self.file
                    .as_file()
                    .metadata()
                    .map_err(|error| SshConnectionError::Transport {
                        message: format!("failed to verify OpenSSH diagnostic log: {error}"),
                    })?;
            if path_metadata.dev() != open_metadata.dev()
                || path_metadata.ino() != open_metadata.ino()
            {
                return Err(SshConnectionError::Transport {
                    message: "OpenSSH diagnostic log path was replaced".to_string(),
                });
            }
        }
        Ok(())
    }
}

async fn run_with_openssh_diagnostics<F>(
    build_command: F,
    timeout_secs: Option<u64>,
    temp_parent: Option<&Path>,
) -> Result<(AsyncOutput, Option<i32>), SshConnectionError>
where
    F: FnOnce(&Path) -> Result<Command, SshConnectionError>,
{
    let mut debug_log = OpenSshDiagnosticLog::new_in(temp_parent)?;
    // Validate the original path immediately before handing it to OpenSSH.
    debug_log.verify_path_identity()?;
    let mut command = build_command(debug_log.path())?;
    apply_diagnostic_write_limit(&mut command)?;
    let output = run_supervised_command(command, timeout_secs).await?;
    let exit_code = classify_transport_exit(output.status.code(), &output.stderr, &mut debug_log)?;
    Ok((output, exit_code))
}

/// Apply a kernel-enforced file-size ceiling to OpenSSH and every local helper
/// it spawns. Both Linux and macOS enforce `RLIMIT_FSIZE` on regular-file
/// writes. Resetting SIGXFSZ to its default guarantees a writer cannot ignore
/// the limit and spin while repeatedly receiving `EFBIG`.
#[cfg(unix)]
fn apply_diagnostic_write_limit(command: &mut Command) -> Result<(), SshConnectionError> {
    use std::os::unix::process::CommandExt;

    // SAFETY: the pre-exec closure calls only async-signal-safe libc operations
    // and constructs an io::Error only on immediate syscall failure.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGXFSZ, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            let limit = libc::rlimit {
                rlim_cur: MAX_OPENSSH_DEBUG_LOG_BYTES as libc::rlim_t,
                rlim_max: MAX_OPENSSH_DEBUG_LOG_BYTES as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_diagnostic_write_limit(_command: &mut Command) -> Result<(), SshConnectionError> {
    Err(SshConnectionError::Transport {
        message: "OpenSSH diagnostic hard limit is unsupported on this platform".to_string(),
    })
}

type DrainResult = Result<(BoundedBytes, BoundedBytes), (&'static str, std::io::Error)>;
type DrainTask = tokio::task::JoinHandle<DrainResult>;

async fn run_supervised_command(
    command: Command,
    timeout_secs: Option<u64>,
) -> Result<AsyncOutput, SshConnectionError> {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let mut cancel = CancelOnDrop(Some(cancel_tx));
    let supervisor = tokio::spawn(supervise_command(command, timeout_secs, cancel_rx));
    let output = supervisor
        .await
        .map_err(|error| SshConnectionError::Transport {
            message: format!("SSH supervisor failed: {error}"),
        })??;
    cancel.0.take();
    Ok(output)
}

async fn drain_bounded<R: AsyncRead + Unpin>(mut reader: R) -> std::io::Result<BoundedBytes> {
    let mut bytes = Vec::with_capacity(MAX_CAPTURE_BYTES);
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < count;
    }
    Ok(BoundedBytes { bytes, truncated })
}

async fn supervise_command(
    std_command: Command,
    timeout_secs: Option<u64>,
    mut cancel: oneshot::Receiver<()>,
) -> Result<AsyncOutput, SshConnectionError> {
    let deadline = timeout_secs.map(|secs| tokio::time::Instant::now() + Duration::from_secs(secs));
    let mut command = tokio::process::Command::from(std_command);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(SshConnectionError::Spawn)?;
    let process_group = child.id().ok_or_else(|| SshConnectionError::Transport {
        message: "spawned SSH process has no process id".to_string(),
    })? as i32;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SshConnectionError::Transport {
            message: "SSH stdout pipe was not created".to_string(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SshConnectionError::Transport {
            message: "SSH stderr pipe was not created".to_string(),
        })?;
    // One join handle owns both readers. This keeps the reads concurrent while
    // making it impossible for timeout/cancellation cleanup to re-poll a
    // handle whose output was already consumed.
    let mut drains = tokio::spawn(async move {
        let (stdout, stderr) = tokio::join!(drain_bounded(stdout), drain_bounded(stderr));
        let stdout = stdout.map_err(|error| ("stdout", error))?;
        let stderr = stderr.map_err(|error| ("stderr", error))?;
        Ok::<_, (&'static str, std::io::Error)>((stdout, stderr))
    });

    enum Completion {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut(u64),
        Cancelled,
    }

    let completion = tokio::select! {
        status = child.wait() => Completion::Exited(status),
        _ = sleep_until_deadline(deadline) => {
            Completion::TimedOut(timeout_secs.expect("timeout branch requires a timeout"))
        },
        _ = &mut cancel => Completion::Cancelled,
    };

    let status = match completion {
        Completion::Exited(status) => status.map_err(SshConnectionError::Spawn)?,
        Completion::TimedOut(secs) => {
            terminate_process_group(&mut child, process_group).await;
            finish_drains(drains).await;
            return Err(SshConnectionError::TimedOut { timeout_secs: secs });
        }
        Completion::Cancelled => {
            terminate_process_group(&mut child, process_group).await;
            finish_drains(drains).await;
            return Err(SshConnectionError::Cancelled);
        }
    };

    enum DrainCompletion {
        Complete(Result<(BoundedBytes, BoundedBytes), SshConnectionError>),
        TimedOut,
        Cancelled,
    }
    let completion = {
        let drains = async { join_drains(&mut drains).await };
        tokio::pin!(drains);
        tokio::select! {
            result = &mut drains => DrainCompletion::Complete(result),
            _ = sleep_until_deadline(deadline) => DrainCompletion::TimedOut,
            _ = &mut cancel => DrainCompletion::Cancelled,
        }
    };
    match completion {
        DrainCompletion::Complete(result) => {
            let (stdout, stderr) = result?;
            Ok(AsyncOutput {
                status,
                stdout,
                stderr,
            })
        }
        DrainCompletion::TimedOut => {
            terminate_process_group(&mut child, process_group).await;
            finish_drains(drains).await;
            Err(SshConnectionError::TimedOut {
                timeout_secs: timeout_secs.expect("timeout branch requires a timeout"),
            })
        }
        DrainCompletion::Cancelled => {
            terminate_process_group(&mut child, process_group).await;
            finish_drains(drains).await;
            Err(SshConnectionError::Cancelled)
        }
    }
}

async fn join_drains(
    drains: &mut DrainTask,
) -> Result<(BoundedBytes, BoundedBytes), SshConnectionError> {
    drains
        .await
        .map_err(|error| SshConnectionError::Transport {
            message: format!("SSH output reader failed: {error}"),
        })?
        .map_err(|(stream, error)| SshConnectionError::Transport {
            message: format!("SSH {stream} read failed: {error}"),
        })
}

async fn sleep_until_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

async fn finish_drains(mut drains: DrainTask) {
    let _ = tokio::time::timeout(REAP_GRACE, &mut drains).await;
    if !drains.is_finished() {
        drains.abort();
    }
}

async fn terminate_process_group(child: &mut tokio::process::Child, process_group: i32) {
    #[cfg(unix)]
    {
        signal_process_group(process_group, 15);
        let deadline = tokio::time::Instant::now() + TERMINATE_GRACE;
        while process_group_exists(process_group) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if process_group_exists(process_group) {
            signal_process_group(process_group, 9);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }

    if tokio::time::timeout(REAP_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(REAP_GRACE, child.wait()).await;
    }
}

#[cfg(unix)]
fn signal_process_group(process_group: i32, signal: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: a negative pid targets only the process group created for this child.
    let _ = unsafe { kill(-process_group, signal) };
}

#[cfg(unix)]
fn process_group_exists(process_group: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: signal zero probes the process group without changing its state.
    unsafe { kill(-process_group, 0) == 0 }
}

fn bounded_text(output: &BoundedBytes) -> String {
    let mut text = String::from_utf8_lossy(&output.bytes).trim().to_string();
    if output.truncated {
        text.push_str("\n[output truncated]");
    }
    text
}

fn classify_transport_exit(
    openssh_exit_code: Option<i32>,
    stderr: &BoundedBytes,
    debug_log: &mut OpenSshDiagnosticLog,
) -> Result<Option<i32>, SshConnectionError> {
    // Always inspect the trusted log first so a child killed by SIGXFSZ, or a
    // helper which catches the signal and exits normally, cannot disguise an
    // overflow as an ordinary remote status.
    let bytes = debug_log.read_bounded()?;
    if openssh_exit_code.is_none() {
        return Err(SshConnectionError::Transport {
            message: "OpenSSH terminated by signal".to_string(),
        });
    }
    if openssh_exit_code != Some(255) {
        return Ok(openssh_exit_code);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| SshConnectionError::Transport {
        message: "OpenSSH diagnostic log was malformed".to_string(),
    })?;
    if text
        .lines()
        .any(|line| line.trim_end_matches('\r') == OPENSSH_REMOTE_EXIT_255)
    {
        return Ok(Some(255));
    }
    let stderr_text = bounded_text(stderr);
    let detail = if stderr_text.is_empty() {
        "OpenSSH exited with status 255 without trusted remote-exit evidence".to_string()
    } else {
        format!(
            "OpenSSH exited with status 255 without trusted remote-exit evidence: {stderr_text}"
        )
    };
    Err(SshConnectionError::Transport { message: detail })
}

fn command_exists(binary: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {binary} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn yes_no(enabled: bool) -> &'static str {
    if enabled { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    fn opts_with_host(host: impl Into<String>) -> SshConnectionOptions {
        SshConnectionOptions {
            host: host.into(),
            username: "user".into(),
            port: 22,
            auth: SshAuth::Agent,
            batch_mode: true,
            connect_timeout_secs: None,
            command_timeout_secs: None,
            strict_host_key_checking: true,
            known_hosts_path: None,
            extra_args: Vec::new(),
            roster: None,
        }
    }

    fn shell_command(script: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[cfg(unix)]
    async fn assert_recorded_process_gone(marker: &Path) {
        for _ in 0..200 {
            if let Ok(raw) = std::fs::read_to_string(marker)
                && let Ok(pid) = raw.trim().parse::<i32>()
            {
                unsafe extern "C" {
                    fn kill(pid: i32, sig: i32) -> i32;
                }
                // SAFETY: signal zero only probes the pid written by the test child.
                if unsafe { kill(pid, 0) } == -1 {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("descendant recorded in {} is still alive", marker.display());
    }

    #[test]
    fn resolve_host_uses_literal_ip_unchanged() {
        let conn = SshConnection::new(opts_with_host("192.168.5.100"));
        assert_eq!(conn.resolve_host(), "192.168.5.100");
    }

    #[test]
    fn resolve_host_maps_alias_to_roster_ip() {
        let mut roster = HashMap::new();
        roster.insert("vinny".into(), "192.168.5.100".into());
        let mut opts = opts_with_host("vinny");
        opts.roster = Some(roster);
        let conn = SshConnection::new(opts);
        assert_eq!(conn.resolve_host(), "192.168.5.100");
    }

    #[test]
    fn resolve_host_falls_back_when_alias_not_in_roster() {
        let mut roster = HashMap::new();
        roster.insert("other".into(), "192.168.5.101".into());
        let mut opts = opts_with_host("unknown-alias");
        opts.roster = Some(roster);
        let conn = SshConnection::new(opts);
        assert_eq!(conn.resolve_host(), "unknown-alias");
    }

    #[test]
    fn resolve_host_falls_back_when_roster_is_none() {
        let conn = SshConnection::new(opts_with_host("some-host"));
        assert_eq!(conn.resolve_host(), "some-host");
    }

    #[tokio::test]
    async fn requested_timeout_is_exact_and_session_remains_usable() {
        let started = Instant::now();
        let error = run_supervised_command(shell_command("sleep 30"), Some(1))
            .await
            .expect_err("command should time out");
        assert!(matches!(
            error,
            SshConnectionError::TimedOut { timeout_secs: 1 }
        ));
        assert!(started.elapsed() < Duration::from_secs(3));

        let follow_up = run_supervised_command(shell_command("printf session-alive"), Some(1))
            .await
            .expect("same Tokio session should remain usable after timeout");
        assert!(follow_up.status.success());
        assert_eq!(follow_up.stdout.bytes, b"session-alive");
    }

    #[tokio::test]
    async fn verbose_stdout_and_stderr_are_drained_without_deadlock() {
        let output = run_supervised_command(
            shell_command("head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2"),
            Some(5),
        )
        .await
        .expect("verbose command should complete");
        assert!(output.status.success());
        assert_eq!(output.stdout.bytes.len(), MAX_CAPTURE_BYTES);
        assert_eq!(output.stderr.bytes.len(), MAX_CAPTURE_BYTES);
        assert!(output.stdout.truncated && output.stderr.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_descendants_and_reaps_direct_child() {
        let marker = std::env::temp_dir().join(format!("ff-ssh-timeout-{}", uuid::Uuid::new_v4()));
        let script = format!("sleep 30 & echo $! > '{}'; wait", marker.display());
        let error = run_supervised_command(shell_command(script), Some(1))
            .await
            .expect_err("command should time out");
        assert!(matches!(error, SshConnectionError::TimedOut { .. }));
        assert_recorded_process_gone(&marker).await;
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_remains_active_while_descendant_holds_output_pipes() {
        let marker =
            std::env::temp_dir().join(format!("ff-ssh-pipe-holder-{}", uuid::Uuid::new_v4()));
        let script = format!("sleep 30 & echo $! > '{}'; exit 0", marker.display());
        let error = run_supervised_command(shell_command(script), Some(1))
            .await
            .expect_err("pipe-holding descendant should remain under the command deadline");
        assert!(matches!(
            error,
            SshConnectionError::TimedOut { timeout_secs: 1 }
        ));
        assert_recorded_process_gone(&marker).await;
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    // Adapted from the attested Rihanna GLM test proposal into a real
    // asymmetric pipe-holder process tree with an externally probeable PID.
    async fn timeout_with_only_stderr_held_does_not_repoll_completed_stdout_drain() {
        let marker =
            std::env::temp_dir().join(format!("ff-ssh-stderr-holder-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "exec 1>&-; sleep 30 >&2 & echo $! > '{}'; exit 0",
            marker.display()
        );
        let error = run_supervised_command(shell_command(script), Some(1))
            .await
            .expect_err("stderr-only pipe holder should time out cleanly");
        assert!(matches!(
            error,
            SshConnectionError::TimedOut { timeout_secs: 1 }
        ));
        assert_recorded_process_gone(&marker).await;
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_with_only_stdout_held_does_not_repoll_completed_stderr_drain() {
        let marker =
            std::env::temp_dir().join(format!("ff-ssh-stdout-holder-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "exec 2>&-; sleep 30 >&1 2>&1 & echo $! > '{}'; exit 0",
            marker.display()
        );
        let error = run_supervised_command(shell_command(script), Some(1))
            .await
            .expect_err("stdout-only pipe holder should time out cleanly");
        assert!(matches!(
            error,
            SshConnectionError::TimedOut { timeout_secs: 1 }
        ));
        assert_recorded_process_gone(&marker).await;
        let _ = std::fs::remove_file(marker);
    }

    // Generated by the attested Sia Qwen3-Coder route and independently
    // corrected to record the descendant PID (`echo $!`) before acceptance.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_escalates_to_kill_for_term_ignoring_process_group() {
        let marker =
            std::env::temp_dir().join(format!("ff-ssh-term-ignore-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "trap '' TERM; (trap '' TERM; sleep 30) & echo $! > '{}'; wait",
            marker.display()
        );
        let started = Instant::now();

        let error = run_supervised_command(shell_command(script), Some(1))
            .await
            .expect_err("TERM-ignoring command should time out");

        assert!(matches!(
            error,
            SshConnectionError::TimedOut { timeout_secs: 1 }
        ));
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_recorded_process_gone(&marker).await;
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_future_still_cleans_process_group() {
        let marker = std::env::temp_dir().join(format!("ff-ssh-cancel-{}", uuid::Uuid::new_v4()));
        let script = format!("sleep 30 & echo $! > '{}'; wait", marker.display());
        let task = tokio::spawn(run_supervised_command(shell_command(script), None));
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.abort();
        assert!(
            task.await
                .expect_err("task should be cancelled")
                .is_cancelled()
        );
        assert_recorded_process_gone(&marker).await;
        let _ = std::fs::remove_file(marker);
    }

    fn stderr(bytes: impl Into<Vec<u8>>) -> BoundedBytes {
        BoundedBytes {
            bytes: bytes.into(),
            truncated: false,
        }
    }

    fn diagnostic_log(contents: &[u8]) -> OpenSshDiagnosticLog {
        let log = OpenSshDiagnosticLog::new_in(None).expect("create diagnostic log");
        std::fs::write(log.path(), contents).expect("write diagnostic log");
        log
    }

    #[test]
    fn trusted_openssh_exit_status_preserves_remote_255_and_ignores_spoofed_stderr() {
        let payload_stderr =
            stderr(b"debug1: Exit status 255\n__FF_REMOTE_EXIT_255_forged__\n".to_vec());
        let mut log = diagnostic_log(b"debug1: channel 0: free\ndebug1: Exit status 255\n");
        assert_eq!(
            classify_transport_exit(Some(255), &payload_stderr, &mut log).unwrap(),
            Some(255)
        );
    }

    #[tokio::test]
    async fn fake_openssh_round_trip_preserves_remote_254_and_255() {
        let (output_254, exit_254) = run_with_openssh_diagnostics(
            |_| {
                Ok(shell_command(
                    "printf '__FF_REMOTE_EXIT_255_forged__\\n' >&2; exit 254",
                ))
            },
            Some(2),
            None,
        )
        .await
        .expect("remote 254 should not require diagnostic evidence");
        assert_eq!(exit_254, Some(254));
        assert_eq!(
            bounded_text(&output_254.stderr),
            "__FF_REMOTE_EXIT_255_forged__"
        );

        let (output_255, exit_255) = run_with_openssh_diagnostics(
            |path| {
                Ok(shell_command(format!(
                    "printf 'debug1: Exit status 255\\n' > \"{}\"; printf payload-error >&2; exit 255",
                    path.display()
                )))
            },
            Some(2),
            None,
        )
        .await
        .expect("trusted diagnostic evidence should preserve remote 255");
        assert_eq!(exit_255, Some(255));
        assert_eq!(bounded_text(&output_255.stderr), "payload-error");
    }

    #[test]
    fn transport_255_fails_closed_even_when_payload_stderr_spoofs_debug_line() {
        let payload_stderr = stderr(b"debug1: Exit status 255\n".to_vec());
        let mut log = diagnostic_log(b"debug1: Connection established.\nConnection timed out\n");
        assert!(matches!(
            classify_transport_exit(Some(255), &payload_stderr, &mut log),
            Err(SshConnectionError::Transport { .. })
        ));
    }

    #[test]
    fn remote_254_is_exact_even_if_payload_reads_parent_cmdline_and_forges_old_frame() {
        let exploit = "tr '\\0' '\\n' </proc/$PPID/cmdline >&2; printf '%s\\n' '__FF_REMOTE_EXIT_255_forged__' >&2; exit 254";
        let conn = SshConnection::new(opts_with_host("host"));
        let log_path = Path::new("/tmp/ff ssh diagnostic.log");
        let command = conn
            .build_command(exploit, log_path)
            .expect("build OpenSSH command");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some(exploit));
        assert!(!args.last().unwrap().contains("01234567-89ab"));

        let mut log = diagnostic_log(b"");
        assert_eq!(
            classify_transport_exit(
                Some(254),
                &stderr(b"__FF_REMOTE_EXIT_255_forged__\n".to_vec()),
                &mut log,
            )
            .unwrap(),
            Some(254)
        );
    }

    #[test]
    fn missing_malformed_and_oversized_diagnostics_all_fail_closed() {
        let payload_stderr = stderr(Vec::new());

        let mut missing_evidence = diagnostic_log(b"");
        assert!(matches!(
            classify_transport_exit(Some(255), &payload_stderr, &mut missing_evidence),
            Err(SshConnectionError::Transport { .. })
        ));

        let mut malformed = diagnostic_log(&[0xff, 0xfe, b'\n']);
        assert!(matches!(
            classify_transport_exit(Some(255), &payload_stderr, &mut malformed),
            Err(SshConnectionError::Transport { .. })
        ));

        let mut oversized = diagnostic_log(&vec![b'x'; MAX_OPENSSH_DEBUG_LOG_BYTES + 1]);
        assert!(matches!(
            classify_transport_exit(Some(255), &payload_stderr, &mut oversized),
            Err(SshConnectionError::Transport { .. })
        ));

        let mut disappeared = diagnostic_log(OPENSSH_REMOTE_EXIT_255.as_bytes());
        std::fs::remove_file(disappeared.path()).expect("remove diagnostic path");
        assert!(matches!(
            classify_transport_exit(Some(255), &payload_stderr, &mut disappeared),
            Err(SshConnectionError::Transport { .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_diagnostic_overflow_never_exceeds_kernel_cap_and_fails_closed() {
        let parent = tempfile::tempdir().expect("temp parent");
        let retained = parent.path().join("retained-debug-log");
        let child_pid = parent.path().join("writer-pid");
        let retained_for_builder = retained.clone();
        let child_pid_for_builder = child_pid.clone();

        let result = run_with_openssh_diagnostics(
            move |path| {
                std::fs::hard_link(path, &retained_for_builder)
                    .expect("retain trusted inode for post-run size assertion");
                Ok(shell_command(format!(
                    "echo $$ > \"{}\"; dd if=/dev/zero of=\"{}\" bs=1024 count={} 2>/dev/null || :; exit 0",
                    child_pid_for_builder.display(),
                    path.display(),
                    MAX_OPENSSH_DEBUG_LOG_BYTES / 1024 + 32,
                )))
            },
            Some(3),
            Some(parent.path()),
        )
        .await;

        assert!(matches!(result, Err(SshConnectionError::Transport { .. })));
        assert_eq!(
            std::fs::metadata(&retained)
                .expect("retained log metadata")
                .len(),
            MAX_OPENSSH_DEBUG_LOG_BYTES as u64,
            "RLIMIT_FSIZE must stop the write at the exact cross-platform cap"
        );
        assert_recorded_process_gone(&child_pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overflow_then_timeout_still_reaps_the_entire_process_group() {
        let parent = tempfile::tempdir().expect("temp parent");
        let retained = parent.path().join("retained-timeout-log");
        let descendant_pid = parent.path().join("descendant-pid");
        let retained_for_builder = retained.clone();
        let descendant_pid_for_builder = descendant_pid.clone();

        let result = run_with_openssh_diagnostics(
            move |path| {
                std::fs::hard_link(path, &retained_for_builder)
                    .expect("retain trusted inode for post-timeout size assertion");
                Ok(shell_command(format!(
                    "sleep 30 & echo $! > \"{}\"; dd if=/dev/zero of=\"{}\" bs=1024 count={} 2>/dev/null || :; wait",
                    descendant_pid_for_builder.display(),
                    path.display(),
                    MAX_OPENSSH_DEBUG_LOG_BYTES / 1024 + 32,
                )))
            },
            Some(1),
            Some(parent.path()),
        )
        .await;

        assert!(matches!(result, Err(SshConnectionError::TimedOut { .. })));
        assert!(
            std::fs::metadata(&retained)
                .expect("retained timeout log metadata")
                .len()
                <= MAX_OPENSSH_DEBUG_LOG_BYTES as u64
        );
        assert_recorded_process_gone(&descendant_pid).await;
    }

    #[cfg(unix)]
    #[test]
    fn diagnostic_log_is_private_regular_and_replacement_is_rejected() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let mut log = diagnostic_log(b"debug1: Exit status 255\n");
        let path = log.path().to_path_buf();
        let open_metadata = log.file.as_file().metadata().expect("open metadata");
        let path_metadata = std::fs::symlink_metadata(&path).expect("path metadata");
        assert!(path_metadata.file_type().is_file());
        assert!(!path_metadata.file_type().is_symlink());
        assert_eq!(path_metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(path_metadata.ino(), open_metadata.ino());

        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, OPENSSH_REMOTE_EXIT_255).expect("replacement file");
        std::fs::remove_file(&path).expect("unlink original path");
        symlink(&replacement, &path).expect("replace path with symlink");
        assert!(matches!(
            classify_transport_exit(Some(255), &stderr(Vec::new()), &mut log),
            Err(SshConnectionError::Transport { .. })
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(replacement);
    }

    #[test]
    fn openssh_arguments_are_macos_compatible_and_remote_command_is_unframed() {
        let conn = SshConnection::new(opts_with_host("host"));
        let debug_path = Path::new("/private/tmp/ff ssh debug.log");
        let command = conn
            .build_command("exit 255", debug_path)
            .expect("build command");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], &["-v", "-E", "/private/tmp/ff ssh debug.log"]);
        assert_eq!(args.last().map(String::as_str), Some("exit 255"));
        assert!(!args.iter().any(|arg| arg.contains("__FF_REMOTE_EXIT_255_")));
    }

    #[tokio::test]
    async fn timeout_removes_private_diagnostic_log() {
        let parent = tempfile::tempdir().expect("temp parent");
        let result = run_with_openssh_diagnostics(
            |_| Ok(shell_command("sleep 30")),
            Some(1),
            Some(parent.path()),
        )
        .await;
        assert!(matches!(result, Err(SshConnectionError::TimedOut { .. })));
        assert_eq!(
            std::fs::read_dir(parent.path())
                .expect("read temp parent")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn cancellation_removes_private_diagnostic_log() {
        let parent = tempfile::tempdir().expect("temp parent");
        let parent_path = parent.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_with_openssh_diagnostics(
                |_| Ok(shell_command("sleep 30")),
                None,
                Some(&parent_path),
            )
            .await
        });
        for _ in 0..100 {
            if std::fs::read_dir(parent.path())
                .expect("read temp parent")
                .next()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.abort();
        assert!(task.await.expect_err("task cancelled").is_cancelled());
        for _ in 0..100 {
            if std::fs::read_dir(parent.path())
                .expect("read temp parent")
                .next()
                .is_none()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("diagnostic log survived cancellation");
    }

    #[test]
    fn from_node_applies_a_finite_direct_command_timeout() {
        let mut node = SshNodeConfig {
            name: "node".into(),
            host: "127.0.0.1".into(),
            port: 22,
            username: "user".into(),
            key_path: None,
            password: None,
            alternate_ips: Vec::new(),
            batch_mode: true,
            connect_timeout_secs: None,
            known_hosts_path: None,
        };
        assert_eq!(
            SshConnectionOptions::from_node(&node).command_timeout_secs,
            Some(DEFAULT_DIRECT_COMMAND_TIMEOUT_SECS)
        );

        node.connect_timeout_secs = Some(7);
        assert_eq!(
            SshConnectionOptions::from_node(&node).command_timeout_secs,
            Some(7)
        );
    }
}
