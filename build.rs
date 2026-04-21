//! Build-time helper: capture the short git SHA of the current checkout and
//! expose it to the `forgefleetd` binary as `FF_GIT_SHA` (read via
//! `env!("FF_GIT_SHA")` in `src/main.rs`). Degrades to `"unknown"` outside
//! a git checkout. Mirrors `crates/ff-terminal/build.rs`.

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
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
