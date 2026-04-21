//! Build-time helper: capture version metadata and expose it to the
//! `ff` binary as `FF_GIT_SHA`, `FF_BUILD_VERSION`, `FF_GIT_STATE` env
//! vars, read via `env!(...)` in source.
//!
//! Version format: `YYYY.M.D_N (STATE sha)` where
//! - `YYYY.M.D` is today's local date
//! - `_N` is a same-day build counter (stored in `~/.forgefleet/builds/`)
//! - `STATE` is `pushed` | `unpushed` | `dirty` | `unknown`
//!
//! All probes degrade to `"unknown"` — the build NEVER fails on a counter
//! or git probe issue. Mirrors the root-level `build.rs` via shared helper.

use std::process::Command;

include!("build_version.rs");

fn main() {
    emit_version_env();
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
