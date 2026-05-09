//! `ra2a` — ForgeFleet A2A (Agent-to-Agent) protocol implementation.
//!
//! Implements the emerging Agent-to-Agent protocol for inter-agent
//! communication: agent cards, SSE streaming, task delegation, and
//! capability negotiation.
//!
//! # Concepts
//! - **Agent Card**: JSON metadata describing an agent's capabilities,
//!   endpoints, auth requirements, and skills.
//! - **Task Send**: POST a task to another agent's `/tasks/send` endpoint.
//! - **SSE Stream**: Server-Sent Events for real-time task updates.
//! - **Capability Negotiation**: Agents exchange cards before collaboration.

pub mod card;
pub mod client;
pub mod server;
pub mod task;

pub use card::{AgentCapability, AgentCard, AgentEndpoint, AgentSkill};
pub use client::A2aClient;
pub use server::routes;
pub use task::{Task, TaskMessage, TaskStatus, TaskUpdate};
