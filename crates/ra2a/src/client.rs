//! A2A HTTP client for talking to remote agents.

use crate::card::AgentCard;
use crate::task::{Task, TaskMessage};
use reqwest::Client;
use tracing::{debug, info};

/// Client for Agent-to-Agent protocol calls.
pub struct A2aClient {
    http: Client,
}

impl Default for A2aClient {
    fn default() -> Self {
        Self::new()
    }
}

impl A2aClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("build reqwest client"),
        }
    }

    /// Fetch an agent's card from its well-known URL.
    pub async fn fetch_card(&self, agent_url: &str) -> anyhow::Result<AgentCard> {
        let url = format!("{}/.well-known/agent.json", agent_url.trim_end_matches('/'));
        let resp = self.http.get(&url).send().await?;
        let card = resp.json::<AgentCard>().await?;
        debug!(agent = %agent_url, name = %card.name, "fetched agent card");
        Ok(card)
    }

    /// Send a task to a remote agent.
    pub async fn send_task(
        &self,
        agent_url: &str,
        messages: Vec<TaskMessage>,
    ) -> anyhow::Result<Task> {
        let url = format!("{}/tasks/send", agent_url.trim_end_matches('/'));
        let payload = serde_json::json!({ "messages": messages });
        let resp = self.http.post(&url).json(&payload).send().await?;
        let task = resp.json::<Task>().await?;
        info!(task_id = %task.id, "a2a task sent");
        Ok(task)
    }
}
