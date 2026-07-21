//! Safe, per-OS execution of package-manager upgrade playbooks.

use std::process::{Command, Stdio};

use ff_core::schema::software::{OS, SoftwareEntry, SourceType};
use thiserror::Error;

/// Result of applying (or previewing) an upgrade registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeResult {
    pub command: String,
    pub dry_run: bool,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum UpgradeError {
    #[error("{0}")]
    UnsafeCommand(String),
    #[error("failed to execute {manager}: {source}")]
    Execute {
        manager: &'static str,
        #[source]
        source: std::io::Error,
    },
}

/// Apply the upgrade command from a software-registry entry.
///
/// Only package-manager entries are accepted. The command is passed directly to
/// `apt-get`, `brew`, or `winget` as an argument vector; it is never interpreted
/// by a shell. In dry-run mode the validated command is returned without spawning
/// a process.
pub fn apply_upgrade(entry: &SoftwareEntry, dry_run: bool) -> Result<UpgradeResult, UpgradeError> {
    if entry.source_type != SourceType::PackageManager {
        return Err(UpgradeError::UnsafeCommand(format!(
            "{} is not a package-manager registry entry",
            entry.name
        )));
    }

    let manager = package_manager(entry.os);
    let args = validated_args(&entry.upgrade_cmd, manager)?;
    let command = std::iter::once(manager)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");

    if dry_run {
        return Ok(UpgradeResult {
            command,
            dry_run: true,
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        });
    }

    let output = Command::new(manager)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| UpgradeError::Execute { manager, source })?;

    Ok(UpgradeResult {
        command,
        dry_run: false,
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn package_manager(os: OS) -> &'static str {
    match os {
        OS::Linux => "apt-get",
        OS::MacOS => "brew",
        OS::Windows => "winget",
    }
}

fn validated_args(command: &str, manager: &'static str) -> Result<Vec<String>, UpgradeError> {
    let command = command.trim();
    if command.is_empty() || command.len() > 4096 {
        return Err(UpgradeError::UnsafeCommand(
            "upgrade command is empty or too long".into(),
        ));
    }
    if command
        .chars()
        .any(|ch| ch.is_control() || ";&|`$><\\'\"".contains(ch))
    {
        return Err(UpgradeError::UnsafeCommand(
            "upgrade command contains shell syntax".into(),
        ));
    }

    let mut args: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
    let aliases: &[&str] = match manager {
        "apt-get" => &["apt", "apt-get"],
        "brew" => &["brew"],
        "winget" => &["winget", "winget.exe"],
        _ => &[],
    };
    if args
        .first()
        .is_some_and(|arg| aliases.contains(&arg.as_str()))
    {
        args.remove(0);
    }
    if args.is_empty()
        || args
            .iter()
            .any(|arg| arg.starts_with('-') && arg.contains('='))
    {
        return Err(UpgradeError::UnsafeCommand(
            "upgrade command has no safe package-manager arguments".into(),
        ));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(os: OS, command: &str) -> SoftwareEntry {
        SoftwareEntry {
            name: "example".into(),
            current_version: "1".into(),
            desired_version: "2".into(),
            os,
            source_type: SourceType::PackageManager,
            detection_cmd: String::new(),
            upgrade_cmd: command.into(),
        }
    }

    #[test]
    fn dry_run_selects_manager_without_execution() {
        let linux = apply_upgrade(&entry(OS::Linux, "apt upgrade -y example"), true).unwrap();
        let mac = apply_upgrade(&entry(OS::MacOS, "upgrade example"), true).unwrap();
        let windows =
            apply_upgrade(&entry(OS::Windows, "winget upgrade --id example"), true).unwrap();

        assert_eq!(linux.command, "apt-get upgrade -y example");
        assert_eq!(mac.command, "brew upgrade example");
        assert_eq!(windows.command, "winget upgrade --id example");
        assert!(linux.dry_run && linux.success);
    }

    #[test]
    fn rejects_shell_syntax_and_non_package_entries() {
        assert!(matches!(
            apply_upgrade(&entry(OS::Linux, "upgrade example; touch /tmp/pwned"), true),
            Err(UpgradeError::UnsafeCommand(_))
        ));

        let mut binary = entry(OS::Linux, "upgrade example");
        binary.source_type = SourceType::Binary;
        assert!(apply_upgrade(&binary, true).is_err());
    }
}
