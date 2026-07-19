//! High-availability orchestration for ForgeFleet.
//!
//! Currently contains the backup orchestrator (Postgres + Redis
//! snapshots, distributed across the fleet via the deferred-task
//! queue). Future additions: replica-lag monitor, promote/demote
//! coordinator, failover state machine.

pub mod backup;
pub mod handoff;
pub mod node_info;
pub mod pg_failover;
pub mod restore_drill;
