//! Build-time helper: capture version metadata and expose it to the
//! `forgefleetd` binary as env vars (`FF_GIT_SHA`, `FF_BUILD_VERSION`,
//! `FF_GIT_STATE`) read via `env!(...)` in `src/main.rs`.
//!
//! Emits `ff YYYY.M.D_N (STATE sha)` version format where
//! - `YYYY.M.D` is the local calendar date the build was produced
//! - `_N` is a same-day build counter stored at `~/.forgefleet/builds/YYYY-M-D.count`
//! - `STATE` is one of `pushed` | `unpushed` | `dirty` | `unknown`
//!
//! All git / fs failures degrade to `"unknown"` — the build NEVER fails
//! because of a counter-file or git-state probe issue.

use std::process::Command;

include!("crates/ff-terminal/build_version.rs");

fn main() {
    emit_version_env();
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
