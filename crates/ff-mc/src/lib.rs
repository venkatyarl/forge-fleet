//! `ff-mc` — ForgeFleet Mission Control.
//!
//! This crate provides project management features for ForgeFleet:
//! - **Work items** — tickets/tasks with status, priority, assignee, labels
//! - **Epics** — groupings of related work items with progress tracking
//! - **Sprints** — time-boxed iterations with velocity and burndown
//! - **Board** — computed Kanban board view (not persisted)
//! - **Dashboard** — aggregate stats across all items/sprints/epics
//! - **Auto-link** — keyword-based related item suggestions
//! - **Operational API** — Axum REST endpoints backed by `ff_db::OperationalStore`
//! - **Legal/Compliance** — legal entities, obligations, and filing deadlines
//! - **Alerts** — aggregation of repeated alerts before observability export
//!
//! ## Storage
//!
//! Mission Control uses `ff_db::OperationalStore` for Postgres runtime paths.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use ff_db::OperationalStore;
//! use ff_mc::operational_api::mc_router_operational;
//!
//! let store: OperationalStore = todo!();
//! let router = mc_router_operational(store);
//! // Mount `router` into your axum application
//! ```

pub mod alerts;
pub mod auto_link;
pub mod board;
pub mod counsel;
pub mod dashboard;
pub mod dependency;
pub mod epic;
pub mod error;
pub mod legal;
pub mod operational_api;
pub mod operational_portfolio;
pub mod portfolio;
pub mod review_item;
pub mod sprint;
pub mod task_group;
pub mod work_item;

// Re-export key types at crate root for convenience.
pub use error::{McError, McResult};
