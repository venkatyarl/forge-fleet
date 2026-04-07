//! ForgeFleet Terminal — rich interactive TUI for the ForgeFleet agent platform.
//!
//! Provides a full-featured terminal experience with:
//! - Split-panel layout (messages, fleet status, input)
//! - Tool execution cards with collapsible output
//! - Syntax highlighting for code blocks
//! - Slash command autocomplete
//! - Fleet node status display
//! - Token/context usage visualization
//! - Session management

pub mod app;
pub mod render;
pub mod input;
pub mod messages;
pub mod widgets;
pub mod theme;
