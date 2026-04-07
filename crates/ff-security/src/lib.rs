//! `ff-security` — security and policy primitives for ForgeFleet.

pub mod approvals;
pub mod audit;
pub mod auth;
pub mod autonomy_policy;
pub mod node_auth;
pub mod policy;
pub mod rate_limit;
pub mod sandbox;
pub mod secrets;

pub use ff_core::{ForgeFleetError, Result as CoreResult};

/// Crate version from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
