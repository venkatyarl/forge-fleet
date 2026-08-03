use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::oneshot;

use crate::config::SshNodeConfig;

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
            command_timeout_secs: node.connect_timeout_secs,
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
        let started_at = Utc::now();
        let started = Instant::now();
        let mut cmd = self.build_command(remote_command)?;
        let output = self.run_command(&mut cmd)?;
        let host = self.resolve_host();

        Ok(SshCommandOutput {
            target: format!("{}@{}:{}", self.options.username, host, self.options.port),
            command: remote_command.to_string(),
            started_at,
            duration_ms: started.elapsed().as_millis(),
            exit_code: output.status.code(),
            success: output.status.success(),
            stdout: bounded_redacted(&output.stdout, false),
            stderr: bounded_redacted(&output.stderr, false),
        })
    }

    /// Execute SSH without blocking a Tokio worker. A dedicated supervisor owns
    /// the child until it is reaped; dropping this future asks that supervisor
    /// to terminate the entire process group rather than orphaning it.
    pub async fn execute_async(
        &self,
        remote_command: &str,
    ) -> Result<SshCommandOutput, SshConnectionError> {
        let started_at = Utc::now();
        let started = Instant::now();
        let command = remote_command.to_string();
        let host = self.resolve_host();
        let std_command = self.build_command(remote_command)?;
        let timeout_secs = self.options.command_timeout_secs.filter(|secs| *secs > 0);
        let output = run_supervised_command(std_command, timeout_secs).await?;

        if let Some(error) = openssh_transport_error(&output) {
            return Err(error);
        }

        Ok(SshCommandOutput {
            target: format!("{}@{}:{}", self.options.username, host, self.options.port),
            command,
            started_at,
            duration_ms: started.elapsed().as_millis(),
            exit_code: output.status.code(),
            success: output.status.success(),
            stdout: bounded_redacted(&output.stdout.bytes, output.stdout.truncated),
            stderr: bounded_redacted(&output.stderr.bytes, output.stderr.truncated),
        })
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

    fn build_command(&self, remote_command: &str) -> Result<Command, SshConnectionError> {
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

    fn run_command(&self, cmd: &mut Command) -> Result<Output, SshConnectionError> {
        match self.options.command_timeout_secs {
            Some(timeout_secs) if timeout_secs > 0 => {
                run_with_timeout(cmd, Duration::from_secs(timeout_secs))
            }
            _ => cmd
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(SshConnectionError::Spawn),
        }
    }
}

const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const TERMINATE_GRACE: Duration = Duration::from_millis(250);

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

fn openssh_transport_error(output: &AsyncOutput) -> Option<SshConnectionError> {
    (output.status.code() == Some(255)).then(|| {
        let diagnostic = bounded_redacted(&output.stderr.bytes, output.stderr.truncated);
        SshConnectionError::Transport {
            message: if diagnostic.is_empty() {
                "OpenSSH exited with status 255".to_string()
            } else {
                format!("OpenSSH exited with status 255: {diagnostic}")
            },
        }
    })
}

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
    let stdout = tokio::spawn(drain_bounded(stdout));
    let stderr = tokio::spawn(drain_bounded(stderr));

    enum Completion {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut(u64),
        Cancelled,
    }

    let completion = tokio::select! {
        status = child.wait() => Completion::Exited(status),
        _ = async {
            match timeout_secs {
                Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
                None => std::future::pending::<()>().await,
            }
        } => Completion::TimedOut(timeout_secs.expect("timeout branch requires a timeout")),
        _ = &mut cancel => Completion::Cancelled,
    };

    let status = match completion {
        Completion::Exited(status) => status.map_err(SshConnectionError::Spawn)?,
        Completion::TimedOut(secs) => {
            terminate_process_group(&mut child, process_group).await;
            let _ = stdout.await;
            let _ = stderr.await;
            return Err(SshConnectionError::TimedOut { timeout_secs: secs });
        }
        Completion::Cancelled => {
            terminate_process_group(&mut child, process_group).await;
            return Err(SshConnectionError::Transport {
                message: "SSH command was cancelled".to_string(),
            });
        }
    };

    let stdout = stdout
        .await
        .map_err(|error| SshConnectionError::Transport {
            message: format!("SSH stdout reader failed: {error}"),
        })?
        .map_err(|error| SshConnectionError::Transport {
            message: format!("SSH stdout read failed: {error}"),
        })?;
    let stderr = stderr
        .await
        .map_err(|error| SshConnectionError::Transport {
            message: format!("SSH stderr reader failed: {error}"),
        })?
        .map_err(|error| SshConnectionError::Transport {
            message: format!("SSH stderr read failed: {error}"),
        })?;

    Ok(AsyncOutput {
        status,
        stdout,
        stderr,
    })
}

async fn terminate_process_group(child: &mut tokio::process::Child, process_group: i32) -> bool {
    signal_process_group(process_group, 15);
    match tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
        Ok(Ok(_)) => false,
        Ok(Err(_)) | Err(_) => {
            signal_process_group(process_group, 9);
            let _ = child.wait().await;
            true
        }
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

#[cfg(not(unix))]
fn signal_process_group(_process_group: i32, _signal: i32) {}

fn bounded_redacted(input: &[u8], truncated: bool) -> String {
    let retained = &input[..input.len().min(MAX_CAPTURE_BYTES)];
    let mut text = String::from_utf8_lossy(retained).trim().to_string();
    for marker in ["token=", "password=", "api_key=", "authorization:"] {
        let mut offset = 0;
        loop {
            let lower = text[offset..].to_ascii_lowercase();
            let Some(relative) = lower.find(marker) else {
                break;
            };
            let value_start = offset + relative + marker.len();
            let value_end = text[value_start..]
                .find(char::is_whitespace)
                .map_or(text.len(), |end| value_start + end);
            text.replace_range(value_start..value_end, "<redacted>");
            offset = value_start + "<redacted>".len();
        }
    }
    if truncated || input.len() > MAX_CAPTURE_BYTES {
        text.push_str("\n[output truncated]");
    }
    text
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, SshConnectionError> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let started = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(SshConnectionError::Spawn);
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SshConnectionError::TimedOut {
                timeout_secs: timeout.as_secs(),
            });
        }

        std::thread::sleep(Duration::from_millis(25));
    }
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
    use std::sync::{Mutex, OnceLock};

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
    fn ssh_path_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[cfg(unix)]
    struct PathGuard(std::ffi::OsString);

    #[cfg(unix)]
    impl PathGuard {
        fn prepend(directory: &Path) -> Self {
            let original = std::env::var_os("PATH").unwrap_or_default();
            let mut path = directory.as_os_str().to_os_string();
            path.push(":");
            path.push(&original);
            // SAFETY: these tests hold ssh_path_lock for the guard's lifetime.
            unsafe { std::env::set_var("PATH", path) };
            Self(original)
        }
    }

    #[cfg(unix)]
    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: these tests hold ssh_path_lock for the guard's lifetime.
            unsafe { std::env::set_var("PATH", &self.0) };
        }
    }

    #[cfg(unix)]
    fn fake_ssh(directory: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let executable = directory.join("ssh");
        std::fs::write(&executable, format!("#!/bin/sh\n{script}\n")).expect("write fake ssh");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("make fake ssh executable");
    }

    #[cfg(unix)]
    async fn assert_recorded_process_gone(marker: &Path) {
        for _ in 0..100 {
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
    async fn requested_timeout_is_reported_exactly() {
        let error = run_supervised_command(shell_command("sleep 30"), Some(1))
            .await
            .expect_err("command should time out");
        assert!(matches!(
            error,
            SshConnectionError::TimedOut { timeout_secs: 1 }
        ));
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
    async fn graceful_term_reaps_without_sigkill() {
        let mut command = tokio::process::Command::from(shell_command(
            "trap 'exit 0' TERM; while :; do sleep 1; done",
        ));
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("spawn TERM-aware child");
        let process_group = child.id().expect("child pid") as i32;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!terminate_process_group(&mut child, process_group).await);
        assert!(child.try_wait().expect("query reaped child").is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ignored_term_escalates_and_kills_descendants() {
        let marker = std::env::temp_dir().join(format!("ff-ssh-timeout-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "trap '' TERM; sleep 30 & echo $! > '{}'; wait",
            marker.display()
        );
        let mut command = tokio::process::Command::from(shell_command(script));
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("spawn TERM-ignoring child");
        let process_group = child.id().expect("child pid") as i32;
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(terminate_process_group(&mut child, process_group).await);
        assert!(child.try_wait().expect("query reaped child").is_some());
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

    #[test]
    fn captured_output_is_bounded_and_redacted() {
        let mut input = b"token=top-secret password=hunter2 ".to_vec();
        input.resize(MAX_CAPTURE_BYTES + 100, b'x');
        let output = bounded_redacted(&input, true);
        assert!(!output.contains("top-secret"));
        assert!(!output.contains("hunter2"));
        assert!(output.contains("token=<redacted>"));
        assert!(output.ends_with("[output truncated]"));
        assert!(output.len() <= MAX_CAPTURE_BYTES + 64);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn openssh_255_is_transport_with_bounded_redacted_diagnostic() {
        let _lock = ssh_path_lock().lock().expect("PATH lock");
        let directory = std::env::temp_dir().join(format!("ff-ssh-255-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("create fake ssh directory");
        fake_ssh(
            &directory,
            "printf 'token=top-secret transport down' >&2; exit 255",
        );
        let _path = PathGuard::prepend(&directory);
        let mut options = opts_with_host("127.0.0.1");
        options.command_timeout_secs = Some(5);
        let connection = SshConnection::new(options);
        let error = connection
            .execute_async("true")
            .await
            .expect_err("255 must be a transport error");
        let rendered = error.to_string();
        assert!(matches!(error, SshConnectionError::Transport { .. }));
        assert!(rendered.contains("status 255"));
        assert!(rendered.contains("token=<redacted>"));
        assert!(!rendered.contains("top-secret"));
        assert!(rendered.len() <= MAX_CAPTURE_BYTES + 128);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_remote_nonzero_remains_command_output() {
        let _lock = ssh_path_lock().lock().expect("PATH lock");
        let directory = std::env::temp_dir().join(format!("ff-ssh-23-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("create fake ssh directory");
        fake_ssh(&directory, "printf ordinary >&2; exit 23");
        let _path = PathGuard::prepend(&directory);
        let mut options = opts_with_host("127.0.0.1");
        options.command_timeout_secs = Some(5);
        let output = SshConnection::new(options)
            .execute_async("false")
            .await
            .expect("ordinary remote failure is output");
        assert_eq!(output.exit_code, Some(23));
        assert!(!output.success);
        assert_eq!(output.stderr, "ordinary");
        let _ = std::fs::remove_dir_all(directory);
    }
}
