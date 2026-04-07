//! `ff-mc` — ForgeFleet Mission Control.
//!
//! This crate provides project management features for ForgeFleet:
//! - **Work items** — tickets/tasks with status, priority, assignee, labels
//! - **Epics** — groupings of related work items with progress tracking
//! - **Sprints** — time-boxed iterations with velocity and burndown
//! - **Board** — computed Kanban board view (not persisted)
//! - **Dashboard** — aggregate stats across all items/sprints/epics
//! - **Auto-link** — keyword-based related item suggestions
//! - **API** — Axum REST endpoints for all of the above
//! - **Legal/Compliance** — legal entities, obligations, and filing deadlines
//!
//! ## Storage
//!
//! Mission Control supports two runtime storage paths:
//! - Local SQLite via `McDb` (`rusqlite`) for embedded mode.
//! - `ff_db::OperationalStore` (SQLite/Postgres) for Postgres runtime/full cutover paths.
//!
//! SQLite databases are created/migrated automatically on first access.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use ff_mc::db::McDb;
//! use ff_mc::api::mc_router;
//!
//! let db = McDb::open("mission-control.db").unwrap();
//! let router = mc_router(db);
//! // Mount `router` into your axum application
//! ```

pub mod api;
pub mod auto_link;
pub mod board;
pub mod counsel;
pub mod dashboard;
pub mod db;
pub mod dependency;
pub mod epic;
pub mod error;
pub mod legal;
pub mod operational_api;
pub mod portfolio;
pub mod review_item;
pub mod sprint;
pub mod task_group;
pub mod work_item;

// Re-export key types at crate root for convenience.
pub use db::McDb;
pub use error::{McError, McResult};
