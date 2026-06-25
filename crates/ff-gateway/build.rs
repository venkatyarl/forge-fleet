// Ensure dashboard/dist/ contains at least a placeholder index.html so the
// rust-embed proc macro in static_files.rs can resolve the folder at compile
// time. On a fresh clone the JS build hasn't run yet and the folder is absent,
// which causes a compile error: "no function `get` found for DashboardAssets".
// The real build artifact replaces this placeholder via `npm run build`.

use std::fs;
use std::path::Path;

fn main() {
    let dist = Path::new("../../dashboard/dist");
    if !dist.exists() {
        fs::create_dir_all(dist).expect("failed to create dashboard/dist");
    }
    let index = dist.join("index.html");
    if !index.exists() {
        fs::write(
            &index,
            "ForgeFleet dashboard — build with `cd dashboard && npm install && npm run build` to replace this placeholder.\n",
        )
        .expect("failed to write dashboard/dist/index.html placeholder");
    }
    // Re-run if the placeholder or the real dist output changes.
    println!("cargo:rerun-if-changed=../../dashboard/dist");

    // Bake the build-time git SHA so the gateway's /health endpoint can report
    // the code the RUNNING daemon was compiled from. The forgefleetd binary
    // already bakes FF_GIT_SHA (root build.rs), but that env is per-crate and
    // not visible to this dependency crate — so probe git here too. `--short=10`
    // matches build_version.rs so the value equals forgefleetd's own
    // `(pushed <sha>)`. Never fails: falls back to "unknown".
    let sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FF_GATEWAY_GIT_SHA={sha}");
    // On a branch, `.git/HEAD` is a static `ref: refs/heads/main` line — it does
    // NOT change when `main` advances; only the ref target file does. Watch BOTH
    // so the baked fallback re-bakes on new commits. (The authoritative value is
    // injected at runtime via set_runtime_build_sha, so this is belt-and-braces.)
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/main");
}

fn git_short_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
