//! A deliberately narrow privilege boundary for ForgeFleet GitHub operations.
//!
//! The caller cannot supply a repository URL, refspec, base ref, Git arguments,
//! credentials, or a pull-request mutation. Those values are either typed or
//! reconstructed from authoritative work-item and repository records.

pub mod credentials;
pub mod git;
pub mod github;
pub mod protocol;
pub mod service;
pub mod socket;

pub use service::{CapabilityService, ServiceError};
