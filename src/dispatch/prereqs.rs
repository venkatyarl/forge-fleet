use std::env;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

const TMP_MOUNT: &str = "/tmp";
const BYTES_PER_MB: u64 = 1024 * 1024;

pub fn verify_prerequisites(expected_size_mb: u64, required_tools: &[&str]) -> Result<()> {
    let required_bytes = expected_size_mb
        .checked_mul(BYTES_PER_MB)
        .ok_or_else(|| anyhow!("required disk space is too large: {expected_size_mb} MB"))?;
    let available_bytes = available_space(Path::new(TMP_MOUNT))?;

    if available_bytes < required_bytes {
        bail!(
            "insufficient disk space on the filesystem containing {TMP_MOUNT}: \
             {expected_size_mb} MB required, {} MB available",
            available_bytes / BYTES_PER_MB
        );
    }

    for tool in required_tools {
        if !tool_on_path(tool) {
            bail!("required tool `{tool}` was not found on PATH");
        }
    }

    Ok(())
}

fn available_space(path: &Path) -> Result<u64> {
    let path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("invalid filesystem path: {}", path.display()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();

    // SAFETY: `path` is a valid, NUL-terminated C string and `stats` points to
    // writable memory that `statvfs` initializes when it succeeds.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to inspect filesystem containing {TMP_MOUNT}"));
    }

    // SAFETY: A successful `statvfs` call initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

fn tool_on_path(tool: &str) -> bool {
    if tool.is_empty() || tool.contains(std::path::MAIN_SEPARATOR) {
        return false;
    }

    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(tool))
        .any(|candidate| {
            candidate
                .metadata()
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::verify_prerequisites;

    #[test]
    fn accepts_available_space_and_tool() {
        verify_prerequisites(0, &["cargo"]).unwrap();
    }

    #[test]
    fn rejects_missing_tool() {
        let error = verify_prerequisites(0, &["forgefleet-tool-that-does-not-exist"]).unwrap_err();
        assert!(error.to_string().contains("was not found on PATH"));
    }
}
