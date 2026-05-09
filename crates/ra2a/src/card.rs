//! Agent Card — self-describing metadata for an A2A agent.

use serde::{Deserialize, Serialize};

/// Top-level agent card following the A2A spec draft.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    pub capabilities: AgentCapability,
    pub skills: Vec<AgentSkill>,
    pub endpoints: Vec<AgentEndpoint>,
}

/// Runtime capabilities of the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapability {
    pub streaming: bool,
    pub push_notifications: bool,
    pub state_transition_history: bool,
}

/// A skill exposed by the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub examples: Vec<String>,
}

/// A network endpoint for the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEndpoint {
    pub name: String,
    pub path: String,
    pub methods: Vec<String>,
}
