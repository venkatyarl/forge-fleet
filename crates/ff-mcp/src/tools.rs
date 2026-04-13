//! MCP Tool Registry — defines available ForgeFleet tools for AI assistants.
//!
//! Each tool has a name, description, and JSON Schema for its input parameters.
//! The registry is queried during `tools/list` and used for input validation.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

// ─── Tool Definition ─────────────────────────────────────────────────────────

/// A single MCP tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name (e.g., "fleet_status").
    pub name: String,

    /// Human-readable description of what the tool does.
    pub description: String,

    /// JSON Schema describing the tool's input parameters.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

// ─── Tool Registry ───────────────────────────────────────────────────────────

/// Registry of all available MCP tools.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolDefinition>,
}

impl ToolRegistry {
    /// Build the registry with all ForgeFleet tools pre-registered.
    pub fn new() -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
        };
        registry.register_all();
        registry
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name)
    }

    /// List all registered tools.
    pub fn list(&self) -> Vec<&ToolDefinition> {
        let mut tools: Vec<_> = self.tools.values().collect();
        tools.sort_by_key(|t| &t.name);
        tools
    }

    /// Check if a tool exists.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    // ── Internal registration ────────────────────────────────────────────

    fn register(&mut self, tool: ToolDefinition) {
        self.tools.insert(tool.name.clone(), tool);
    }

    fn register_all(&mut self) {
        self.register(Self::fleet_status());
        self.register(Self::fleet_config());
        self.register(Self::fleet_ssh());
        self.register(Self::fleet_run());
        self.register(Self::fleet_scan());
        self.register(Self::fleet_install_model());
        self.register(Self::fleet_wait());
        self.register(Self::fleet_crew());
        self.register(Self::mcp_federation_status());
        self.register(Self::model_recommend());
        self.register(Self::model_stats());
        self.register(Self::project_profile_upsert());
        self.register(Self::project_profile_get());
        self.register(Self::project_profile_list());
        self.register(Self::project_profile_delete());
        self.register(Self::project_policy_resolve());
        self.register(Self::fleet_pulse());
        self.register(Self::fleet_nodes_db());
        self.register(Self::fleet_node_detail());
        self.register(Self::fleet_models_db());
        self.register(Self::task_lineage());
    }

    // ── Tool definitions ─────────────────────────────────────────────────

    fn fleet_status() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_status".to_string(),
            description: "Get health and busy status of all fleet nodes and models. Shows which LLMs are running, their tier, context size, and current load.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "refresh": {
                        "type": "boolean",
                        "description": "Force re-scan of all endpoints (default: use cached)",
                        "default": false
                    }
                },
                "required": []
            }),
        }
    }

    fn fleet_config() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_config".to_string(),
            description: "Get or set ForgeFleet configuration. Single source of truth for all fleet data — nodes, services, notifications, ports.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["get_all", "get_nodes", "get_node", "get_services", "set"],
                        "default": "get_all",
                        "description": "What to do"
                    },
                    "key": {
                        "type": "string",
                        "description": "Config key (for get/set)"
                    },
                    "value": {
                        "type": "string",
                        "description": "Value to set (for set action)"
                    }
                },
                "required": []
            }),
        }
    }

    fn fleet_ssh() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_ssh".to_string(),
            description: "Run a command on a remote fleet node via SSH. Use node name (taylor, james, marcus, sophie, priya, ace) or IP address.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "Node name or IP address"
                    },
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds",
                        "default": 30
                    }
                },
                "required": ["node", "command"]
            }),
        }
    }

    fn fleet_run() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_run".to_string(),
            description: "Execute a prompt through the tiered LLM pipeline. Starts at the fastest model, escalates to more powerful ones if needed.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The prompt/task to execute"
                    },
                    "start_tier": {
                        "type": "integer",
                        "description": "Start at tier (1=9B fast, 2=32B code, 3=72B review, 4=235B expert)",
                        "default": 1
                    },
                    "max_tier": {
                        "type": "integer",
                        "description": "Maximum tier to escalate to",
                        "default": 4
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn fleet_scan() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_scan".to_string(),
            description: "Scan the local network for new LLM endpoints. Discovers llama.cpp, Ollama, and vLLM servers automatically.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["known", "full"],
                        "default": "known",
                        "description": "known=scan known IPs only (fast), full=scan entire /24 subnet"
                    }
                },
                "required": []
            }),
        }
    }

    fn fleet_install_model() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_install_model".to_string(),
            description: "Download and start an LLM model on a fleet node. Downloads the GGUF file, starts llama-server, and verifies the endpoint.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "Node name or IP to install on"
                    },
                    "model_url": {
                        "type": "string",
                        "description": "URL to download the GGUF model file"
                    },
                    "model_path": {
                        "type": "string",
                        "description": "Where to save the model on the node"
                    },
                    "port": {
                        "type": "integer",
                        "description": "Port to start llama-server on",
                        "default": 55000
                    },
                    "ctx_size": {
                        "type": "integer",
                        "description": "Context window size",
                        "default": 8192
                    }
                },
                "required": ["node", "model_url", "model_path"]
            }),
        }
    }

    fn fleet_wait() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_wait".to_string(),
            description: "Wait until a fleet condition is met (e.g., model finishes loading, node comes online).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "condition": {
                        "type": "string",
                        "enum": ["all_healthy", "tier_available", "model_loaded"],
                        "description": "What to wait for"
                    },
                    "tier": {
                        "type": "integer",
                        "description": "For tier_available: which tier to wait for (1-4)"
                    },
                    "endpoint": {
                        "type": "string",
                        "description": "For model_loaded: IP:port to wait for"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Max seconds to wait",
                        "default": 300
                    }
                },
                "required": ["condition"]
            }),
        }
    }

    fn fleet_crew() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_crew".to_string(),
            description: "Run a multi-agent coding crew on a task. Three agents work sequentially: Context Engineer (9B), Code Writer (32B), Code Reviewer (72B).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Description of the coding task"
                    },
                    "repo_dir": {
                        "type": "string",
                        "description": "Path to the repository",
                        "default": "."
                    }
                },
                "required": ["task"]
            }),
        }
    }

    fn mcp_federation_status() -> ToolDefinition {
        ToolDefinition {
            name: "mcp_federation_status".to_string(),
            description: "Inspect federated MCP client targets and validate required/optional topology links and tool availability.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Default timeout per federated MCP probe (seconds)",
                        "default": 5
                    }
                },
                "required": []
            }),
        }
    }

    fn model_recommend() -> ToolDefinition {
        ToolDefinition {
            name: "model_recommend".to_string(),
            description:
                "Recommend a model/mode for a task type using ForgeFleet governance history."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_type": {
                        "type": "string",
                        "description": "Task type to recommend a model for"
                    }
                },
                "required": ["task_type"]
            }),
        }
    }

    fn model_stats() -> ToolDefinition {
        ToolDefinition {
            name: "model_stats".to_string(),
            description: "Show historical model performance for a task type from the ForgeFleet governance database.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_type": {
                        "type": "string",
                        "description": "Task type to inspect"
                    }
                },
                "required": ["task_type"]
            }),
        }
    }

    fn project_profile_upsert() -> ToolDefinition {
        ToolDefinition {
            name: "project_profile_upsert".to_string(),
            description:
                "Create or update a project execution profile and derive its execution policy."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "profile": {
                        "type": "object",
                        "description": "Project profile payload (project_id, stack, deployment targets, review strictness, test/compliance settings)."
                    }
                },
                "required": ["profile"]
            }),
        }
    }

    fn project_profile_get() -> ToolDefinition {
        ToolDefinition {
            name: "project_profile_get".to_string(),
            description: "Load a single project profile and its computed execution policy."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Unique project identifier"
                    }
                },
                "required": ["project_id"]
            }),
        }
    }

    fn project_profile_list() -> ToolDefinition {
        ToolDefinition {
            name: "project_profile_list".to_string(),
            description: "List all stored project profiles with effective execution policies."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn project_profile_delete() -> ToolDefinition {
        ToolDefinition {
            name: "project_profile_delete".to_string(),
            description: "Delete a stored project profile by id.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Unique project identifier"
                    }
                },
                "required": ["project_id"]
            }),
        }
    }

    fn project_policy_resolve() -> ToolDefinition {
        ToolDefinition {
            name: "project_policy_resolve".to_string(),
            description: "Resolve effective routing + approval behavior for an operation using a project profile.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Unique project identifier"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Operation text used for approval trigger matching"
                    },
                    "start_tier": {
                        "type": "integer",
                        "description": "Requested minimum model tier"
                    },
                    "max_tier": {
                        "type": "integer",
                        "description": "Requested maximum model tier"
                    },
                    "model": {
                        "type": "string",
                        "description": "Requested model selector"
                    }
                },
                "required": ["project_id"]
            }),
        }
    }

    fn fleet_pulse() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_pulse".to_string(),
            description: "Get real-time fleet metrics from Redis. Returns FleetSnapshot with per-node CPU/RAM/disk/tokens_per_sec/active_tasks. Optionally filter by node name.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "Optional node name to get metrics for a single node"
                    }
                },
                "required": []
            }),
        }
    }

    fn fleet_nodes_db() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_nodes_db".to_string(),
            description: "List all fleet nodes from the Postgres registry (persistent, not just fleet.toml). Returns node details including capabilities, resources, hardware, and status.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn fleet_node_detail() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_node_detail".to_string(),
            description: "Get detailed info for a single node: Postgres record (persistent registry) combined with live Redis metrics (CPU/RAM/disk/tokens_per_sec).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "Node name to look up"
                    }
                },
                "required": ["node"]
            }),
        }
    }

    fn fleet_models_db() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_models_db".to_string(),
            description: "List all models from the Postgres registry with their node assignments, tiers, families, ports, and preferred workloads.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "Optional node name to filter models for a specific node"
                    }
                },
                "required": []
            }),
        }
    }

    fn task_lineage() -> ToolDefinition {
        ToolDefinition {
            name: "task_lineage".to_string(),
            description: "Get the full routing and ownership lineage for a task — shows which nodes it originated from, was routed through, and ownership handoffs.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Task ID to retrieve lineage for"
                    }
                },
                "required": ["task_id"]
            }),
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_all_tools() {
        let registry = ToolRegistry::new();
        let expected = [
            "fleet_status",
            "fleet_config",
            "fleet_ssh",
            "fleet_run",
            "fleet_scan",
            "fleet_install_model",
            "fleet_wait",
            "fleet_crew",
            "mcp_federation_status",
            "model_recommend",
            "model_stats",
            "project_profile_upsert",
            "project_profile_get",
            "project_profile_list",
            "project_profile_delete",
            "project_policy_resolve",
            "fleet_pulse",
            "fleet_nodes_db",
            "fleet_node_detail",
            "fleet_models_db",
            "task_lineage",
        ];
        for name in &expected {
            assert!(registry.contains(name), "missing tool: {name}");
        }
        assert_eq!(registry.list().len(), expected.len());
    }

    #[test]
    fn tool_has_valid_schema() {
        let registry = ToolRegistry::new();
        for tool in registry.list() {
            assert!(
                tool.input_schema.is_object(),
                "{} schema is not an object",
                tool.name
            );
            assert!(
                tool.input_schema.get("type").is_some(),
                "{} schema missing 'type'",
                tool.name
            );
        }
    }
}
