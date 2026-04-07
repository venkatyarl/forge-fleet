//! `ff-mesh` — Leader/worker coordination and distributed compute for ForgeFleet.
//!
//! This crate provides:
//! - **leader** — Leader daemon: registration, health monitoring, task dispatch
//! - **worker** — Worker agent: registration, heartbeat, task execution, result reporting
//! - **election** — Leader election with preferred-leader and auto-failover
//! - **scheduler** — Task scheduling by hardware, load, activity, model availability
//! - **resource_pool** — Fleet-wide resource aggregation and per-node tracking
//! - **work_queue** — Distributed work queue with priority, retry, and claim semantics

pub mod election;
pub mod leader;
pub mod resource_pool;
pub mod scheduler;
pub mod work_queue;
pub mod worker;

pub use election::ElectionManager;
pub use leader::LeaderDaemon;
pub use resource_pool::ResourcePool;
pub use scheduler::TaskScheduler;
pub use work_queue::WorkQueue;
pub use worker::WorkerAgent;
