//! `ff-updater` — ForgeFleet self-update system.
//!
//! Provides safe, atomic self-updates for ForgeFleet binaries across the fleet:
//!
//! - **checker** — Detect when a new version is available (git SHA / GitHub API)
//! - **builder** — Pull source, `cargo build --release`, run tests
//! - **verifier** — Verify the new binary starts, passes sanity checks
//! - **swapper** — Atomic binary swap with `.bak` rollback safety net
//! - **orchestrator** — Full update state machine with rolling fleet updates
//! - **canary** — Canary deployment: update a subset of nodes first, bake, then proceed
//! - **rollout** — Rollout controller: phased deployment with manual controls
//! - **rollback** — Restore previous binary if post-update health checks fail
//!
//! # Safety guarantees
//!
//! 1. Never restart mid-update — the binary is only swapped after build + verify succeed.
//! 2. Rolling updates — nodes update one at a time, never all simultaneously.
//! 3. Canary deployments — update a subset first, verify health, then roll out.
//! 4. Automatic rollback — if a new binary fails health checks, the `.bak` is restored.

pub mod builder;
pub mod canary;
pub mod checker;
pub mod error;
pub mod orchestrator;
pub mod rollback;
pub mod rollout;
pub mod swapper;
pub mod verifier;

pub use error::{UpdateError, UpdateResult};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
