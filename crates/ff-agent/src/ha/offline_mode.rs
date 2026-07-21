//! Offline fallback runner support.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sysinfo::{Disks, System};
use tokio::sync::watch;
use tracing::{debug, warn};

const DEFAULT_MIN_RAM_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MIN_DISK_BYTES: u64 = 1024 * 1024 * 1024;

/// A local command executed directly, never through a shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl OfflineCommand {
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfflineResources {
    pub available_ram_bytes: u64,
    pub available_disk_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfflineRunResult {
    LocalSlm(String),
    DeterministicScripts(Vec<String>),
}

/// Runs useful, bounded local work when the fleet cannot be reached.
#[derive(Clone, Debug)]
pub struct OfflineRunner {
    work_dir: PathBuf,
    commands: Vec<OfflineCommand>,
    min_ram_bytes: u64,
    min_disk_bytes: u64,
    heartbeat_interval: Duration,
}

impl OfflineRunner {
    pub fn new(work_dir: impl Into<PathBuf>, commands: Vec<OfflineCommand>) -> Self {
        Self {
            work_dir: work_dir.into(),
            commands,
            min_ram_bytes: DEFAULT_MIN_RAM_BYTES,
            min_disk_bytes: DEFAULT_MIN_DISK_BYTES,
            heartbeat_interval: Duration::from_secs(15),
        }
    }

    pub fn with_resource_minimums(mut self, ram_bytes: u64, disk_bytes: u64) -> Self {
        self.min_ram_bytes = ram_bytes;
        self.min_disk_bytes = disk_bytes;
        self
    }

    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// Refresh and validate the resources needed for local execution.
    pub fn check_resources(&self) -> Result<OfflineResources, String> {
        let resources = available_resources(&self.work_dir)?;
        if resources.available_ram_bytes < self.min_ram_bytes {
            return Err(format!(
                "offline runner needs {} MiB RAM, only {} MiB is available",
                self.min_ram_bytes / (1024 * 1024),
                resources.available_ram_bytes / (1024 * 1024)
            ));
        }
        if resources.available_disk_bytes < self.min_disk_bytes {
            return Err(format!(
                "offline runner needs {} MiB disk, only {} MiB is available",
                self.min_disk_bytes / (1024 * 1024),
                resources.available_disk_bytes / (1024 * 1024)
            ));
        }
        Ok(resources)
    }

    /// Prefer the configured local GGUF model, then fall back to deterministic commands.
    pub fn run_once(&self, prompt: &str) -> Result<OfflineRunResult, String> {
        self.check_resources()?;

        if std::env::var_os("FORGEFLEET_SLM_MODEL").is_some() {
            let output = crate::slm::predict(prompt);
            if !output.starts_with("SLM error: ") {
                return Ok(OfflineRunResult::LocalSlm(output));
            }
            warn!(error = %output, "local SLM unavailable; using deterministic offline commands");
        }

        let mut outputs = Vec::with_capacity(self.commands.len());
        for command in &self.commands {
            outputs.push(self.execute(command)?);
        }
        Ok(OfflineRunResult::DeterministicScripts(outputs))
    }

    /// Emit local health heartbeats until shutdown, without requiring network or Postgres.
    pub async fn run_heartbeat_loop(&self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(self.heartbeat_interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => match self.check_resources() {
                    Ok(resources) => debug!(
                        available_ram_bytes = resources.available_ram_bytes,
                        available_disk_bytes = resources.available_disk_bytes,
                        "offline runner heartbeat"
                    ),
                    Err(error) => warn!(%error, "offline runner heartbeat resource check failed"),
                },
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    }

    fn execute(&self, command: &OfflineCommand) -> Result<String, String> {
        if !command.program.is_absolute() {
            return Err(format!(
                "offline command '{}' must be an absolute path",
                command.program.display()
            ));
        }
        let program = command.program.canonicalize().map_err(|error| {
            format!(
                "cannot resolve offline command '{}': {error}",
                command.program.display()
            )
        })?;
        let output = Command::new(&program)
            .args(&command.args)
            .current_dir(&self.work_dir)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
                format!("failed to execute '{}': {error}", command.program.display())
            })?;
        if !output.status.success() {
            return Err(format!(
                "offline command '{}' exited with {}: {}",
                command.program.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout)
            .map(|value| value.trim_end().to_string())
            .map_err(|_| {
                format!(
                    "offline command '{}' returned non-UTF-8 output",
                    command.program.display()
                )
            })
    }
}

fn available_resources(path: &Path) -> Result<OfflineResources, String> {
    let path = path.canonicalize().map_err(|error| {
        format!(
            "cannot resolve work directory '{}': {error}",
            path.display()
        )
    })?;
    let mut system = System::new();
    system.refresh_memory();
    let disks = Disks::new_with_refreshed_list();
    let disk = disks
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .ok_or_else(|| format!("cannot determine available disk for '{}'", path.display()))?;
    Ok(OfflineResources {
        available_ram_bytes: system.available_memory(),
        available_disk_bytes: disk.available_space(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn deterministic_fallback_executes_direct_argv() {
        let temp = tempfile::tempdir().unwrap();
        let printf = ["/usr/bin/printf", "/bin/printf"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .unwrap();
        let runner = OfflineRunner::new(
            temp.path(),
            vec![OfflineCommand::new(printf, ["%s", "hello; exit 9"])],
        )
        .with_resource_minimums(0, 0);

        assert_eq!(
            runner.run_once("unused").unwrap(),
            OfflineRunResult::DeterministicScripts(vec!["hello; exit 9".into()])
        );
    }

    #[tokio::test]
    async fn heartbeat_stops_on_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let runner = OfflineRunner::new(temp.path(), Vec::new())
            .with_resource_minimums(0, 0)
            .with_heartbeat_interval(Duration::from_millis(10));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(async move { runner.run_heartbeat_loop(shutdown_rx).await });

        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }
}
