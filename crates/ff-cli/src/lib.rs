//! ff-cli is a binary-only crate (see `src/main.rs`). This empty library
//! target exists so `cargo test --lib` has something to build against —
//! without it, cargo reports "no library targets found" and the workspace
//! self-verify gate fails on every branch that touches this crate.
