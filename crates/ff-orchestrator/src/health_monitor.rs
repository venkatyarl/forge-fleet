pub mod health_monitor {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub struct LlmHealthMonitor {
        pub agents: HashMap<String, AgentHealth>,
    }

    impl LlmHealthMonitor {
        pub fn new() -> Self {
            Self {
                agents: HashMap::new(),
            }
        }

        pub fn add_agent(&mut self, agent_id: &str, health: AgentHealth) {
            self.agents.insert(agent_id.to_string(), health);
        }

        pub fn get_agent(&self, agent_id: &str) -> Option<&AgentHealth> {
            self.agents.get(agent_id)
        }
    }
