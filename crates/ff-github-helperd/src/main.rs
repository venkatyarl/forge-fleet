use anyhow::{Context, bail};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn main() -> anyhow::Result<()> {
    let socket = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: ff-github-helperd /run/forgefleet/github-helperd.sock")?;
    validate_socket_path(&socket)?;
    // Wiring the daemon to scheduler authority is intentionally deployment
    // work: this crate contains the typed protocol and service boundary only.
    bail!("authority backend is not configured; refusing to start")
}

fn validate_socket_path(path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() || path.parent() != Some(Path::new("/run/forgefleet")) {
        bail!("socket must be directly below /run/forgefleet");
    }
    let parent = fs::symlink_metadata("/run/forgefleet")
        .context("/run/forgefleet must be provisioned by systemd")?;
    if !parent.file_type().is_dir() || parent.permissions().mode() & 0o022 != 0 {
        bail!("/run/forgefleet is not a safe service directory");
    }
    Ok(())
}
