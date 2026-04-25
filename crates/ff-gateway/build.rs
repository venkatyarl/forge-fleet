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
}
