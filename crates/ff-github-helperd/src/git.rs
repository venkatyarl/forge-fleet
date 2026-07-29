use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::protocol::{
    FileMode, MAX_FILE_BYTES, MAX_MANIFEST_BYTES, MAX_MANIFEST_ENTRIES, StructuralManifest,
};

#[derive(Debug, Error)]
pub enum GitError {
    #[error("invalid structural manifest")]
    InvalidManifest,
    #[error("private repository operation failed")]
    OperationFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(&'static str, &'static str)>,
}

pub trait GitRunner: Send + Sync {
    fn run(&self, cwd: &Path, spec: &CommandSpec) -> Result<(), GitError>;
}

pub struct ProcessGitRunner;

impl GitRunner for ProcessGitRunner {
    fn run(&self, cwd: &Path, spec: &CommandSpec) -> Result<(), GitError> {
        let status = Command::new(spec.program)
            .args(&spec.args)
            .current_dir(cwd)
            .env_clear()
            .envs(spec.env.iter().copied())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|_| GitError::OperationFailed)?;
        status
            .success()
            .then_some(())
            .ok_or(GitError::OperationFailed)
    }
}

pub fn safe_git(command: &[&str]) -> CommandSpec {
    let mut args = vec![
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "filter.lfs.required=false".into(),
        "-c".into(),
        "protocol.allow=never".into(),
        "-c".into(),
        "protocol.file.allow=never".into(),
    ];
    args.extend(command.iter().map(|s| (*s).to_owned()));
    CommandSpec {
        program: "/usr/bin/git",
        args,
        env: vec![
            ("HOME", "/var/empty"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("LC_ALL", "C"),
        ],
    }
}

pub fn manifest_digest(manifest: &StructuralManifest) -> Result<String, GitError> {
    validate_manifest(manifest)?;
    let canonical = serde_json::to_vec(manifest).map_err(|_| GitError::InvalidManifest)?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

pub fn materialize(root: &Path, manifest: &StructuralManifest) -> Result<(), GitError> {
    validate_manifest(manifest)?;
    fs::create_dir(root).map_err(|_| GitError::OperationFailed)?;
    for entry in &manifest.entries {
        let relative = Path::new(&entry.path);
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|_| GitError::OperationFailed)?;
        }
        fs::write(&destination, &entry.bytes).map_err(|_| GitError::OperationFailed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = match entry.mode {
                FileMode::Regular => 0o600,
                FileMode::Executable => 0o700,
            };
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode))
                .map_err(|_| GitError::OperationFailed)?;
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &StructuralManifest) -> Result<(), GitError> {
    if manifest.entries.len() > MAX_MANIFEST_ENTRIES {
        return Err(GitError::InvalidManifest);
    }
    let mut seen = BTreeSet::new();
    let mut total = 0usize;
    for entry in &manifest.entries {
        let path = PathBuf::from(&entry.path);
        if entry.path.is_empty()
            || path.is_absolute()
            || entry.bytes.len() > MAX_FILE_BYTES
            || !seen.insert(entry.path.as_str())
            || path.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                ) || matches!(part, Component::Normal(name) if name == ".git")
            })
        {
            return Err(GitError::InvalidManifest);
        }
        total = total
            .checked_add(entry.bytes.len())
            .ok_or(GitError::InvalidManifest)?;
        if total > MAX_MANIFEST_BYTES {
            return Err(GitError::InvalidManifest);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ManifestEntry;

    fn entry(path: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            mode: FileMode::Regular,
            bytes: b"candidate".to_vec(),
        }
    }

    #[test]
    fn hostile_metadata_and_path_escape_are_denied() {
        for path in [
            "../escape",
            ".git/config",
            "x/.git/hooks/post-commit",
            "/etc/passwd",
        ] {
            let manifest = StructuralManifest {
                entries: vec![entry(path)],
            };
            assert!(manifest_digest(&manifest).is_err(), "{path}");
        }
    }

    #[test]
    fn fixed_git_has_no_credentials_or_inherited_configuration() {
        let spec = safe_git(&["init", "--bare", "."]);
        let rendered = format!("{spec:?}");
        assert!(!rendered.contains("token"));
        assert!(spec.args.contains(&"credential.helper=".into()));
        assert!(spec.args.contains(&"protocol.allow=never".into()));
        assert!(spec.env.contains(&("GIT_CONFIG_NOSYSTEM", "1")));
    }

    #[test]
    fn manifest_materializes_only_declared_regular_files() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("repo");
        let manifest = StructuralManifest {
            entries: vec![entry("src/lib.rs")],
        };
        materialize(&root, &manifest).unwrap();
        assert_eq!(fs::read(root.join("src/lib.rs")).unwrap(), b"candidate");
        assert!(!root.join(".git").exists());
    }
}
