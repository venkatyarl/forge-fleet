//! `ff-ssh` — SSH management for ForgeFleet.
//!
//! This crate provides:
//! - SSH command execution (`connection`)
//! - Key generation/distribution (`key_manager`)
//! - Tunnel lifecycle management (`tunnel`)
//! - Fleet-wide connectivity checks (`connectivity`)
//! - High-level remote execution fan-out (`remote_exec`)
//! - Per-node SSH config loading (`config`)

pub mod config;
pub mod connection;
pub mod connectivity;
pub mod key_manager;
pub mod remote_exec;
pub mod tunnel;

pub use config::{FleetSshConfig, SshNodeConfig, load_fleet_ssh_config};
pub use connection::{
    SshAuth, SshCommandOutput, SshConnection, SshConnectionError, SshConnectionOptions,
};
pub use connectivity::{
    ConnectivityChecker, ConnectivityMatrix, ConnectivityStatus, NodeConnectivityResult,
};
pub use key_manager::{KeyManagerError, KeyPair, SshKeyManager};
pub use remote_exec::{FanoutCommandResult, NodeCommandResult, RemoteExecError, RemoteExecutor};
pub use tunnel::{TunnelDirection, TunnelError, TunnelHandle, TunnelManager, TunnelSpec};
