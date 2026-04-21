//! Build-time helper: capture the short git SHA of the current checkout and
//! expose it to the `ff` binary as the `FF_GIT_SHA` env var (read via
//! `env!("FF_GIT_SHA")` in source). Degrades to `"unknown"` outside a git
//! checkout. Kept intentionally small — this is the ONLY drift signal the
//! upstream checker has for self-built binaries.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=FF_GIT_SHA={sha}");
    // Re-run if HEAD moves (new commit, branch switch, etc).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
