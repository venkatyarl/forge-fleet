use std::error::Error;
use std::process::Command;

pub fn commit_and_push() -> Result<(), Box<dyn Error>> {
    let commit_status = Command::new("git")
        .args(&["commit", "-m", "Auto-update by ff", "--author", "ff <ff@forgefleet>"])
        .output()?;

    if !commit_status.status.success() {
        let stderr = String::from_utf8_lossy(&commit_status.stderr);
        return Err(format!("Git commit failed: {}", stderr).into());
    }

    let push_status = Command::new("git")
        .args(&["push"])
        .output()?;

    if !push_status.status.success() {
        let stderr = String::from_utf8_lossy(&push_status.stderr);
        return Err(format!("Git push failed: {}", stderr).into());
    }

    Ok(())
}
