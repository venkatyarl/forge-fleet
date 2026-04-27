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
        self.register(Self::fleet_models_catalog());
        self.register(Self::fleet_models_search());
        self.register(Self::fleet_models_library());
        self.register(Self::fleet_models_deployments());
        self.register(Self::fleet_models_disk_usage());

        // ── Virtual Brain tools ─────────────────────────────────────────
        self.register(Self::brain_search());
        self.register(Self::brain_vault_read());
        self.register(Self::brain_graph_neighbors());
        self.register(Self::brain_list_threads());
        self.register(Self::brain_stats());
        self.register(Self::brain_propose_node());
        self.register(Self::brain_propose_link());
        self.register(Self::brain_thread_append());
        self.register(Self::brain_stack_push());
        self.register(Self::brain_backlog_add());

        // ── Computer Use (Pillar 1) ─────────────────────────────────────
        self.register(Self::computer_use());
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

    fn fleet_models_catalog() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_models_catalog".to_string(),
            description: "List the curated model catalog — what models ForgeFleet can download and deploy. Returns id, name, family, parameters, tier, gated flag, and description.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn fleet_models_search() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_models_search".to_string(),
            description: "Search the curated model catalog by a query string (matches id, name, family, description). Returns the same shape as fleet_models_catalog.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query — matches id, name, family, or description"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn fleet_models_library() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_models_library".to_string(),
            description: "List models actually on disk across fleet nodes. Returns node_name, catalog_id, runtime, quant, file_path, size_bytes. Optionally filter to a single node.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "Optional node name to filter to a single node"
                    }
                },
                "required": []
            }),
        }
    }

    fn fleet_models_deployments() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_models_deployments".to_string(),
            description: "List currently running model deployments across the fleet. Returns node_name, catalog_id, runtime, port, health_status, started_at. Optionally filter to a single node.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "Optional node name to filter to a single node"
                    }
                },
                "required": []
            }),
        }
    }

    fn fleet_models_disk_usage() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_models_disk_usage".to_string(),
            description: "Latest disk usage sample per fleet node. Returns node_name, total_gb, used_gb, free_gb, models_gb, sampled_at.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    // ── Virtual Brain tool definitions ───────────────────────────────────

    fn brain_search() -> ToolDefinition {
        ToolDefinition {
            name: "brain_search".to_string(),
            description: "Search the Virtual Brain knowledge graph by text query. Matches titles, paths, and tags. Returns matching vault nodes with metadata.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text to search for (matches title, path, tags)"
                    },
                    "node_type": {
                        "type": "string",
                        "description": "Optional filter by node type (e.g., 'fact', 'decision', 'reference', 'preference')"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional filter — return only nodes that have at least one of these tags"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results to return (default 20)",
                        "default": 20
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn brain_vault_read() -> ToolDefinition {
        ToolDefinition {
            name: "brain_vault_read".to_string(),
            description: "Read a specific vault node by its path. Returns full metadata including tags, project, confidence, and hit count.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Vault node path (e.g., 'project/forge-fleet/architecture')"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn brain_graph_neighbors() -> ToolDefinition {
        ToolDefinition {
            name: "brain_graph_neighbors".to_string(),
            description: "Get the graph neighbors (edges) of a vault node. Shows how a node connects to other knowledge.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node_path": {
                        "type": "string",
                        "description": "Path of the node to get neighbors for"
                    }
                },
                "required": ["node_path"]
            }),
        }
    }

    fn brain_list_threads() -> ToolDefinition {
        ToolDefinition {
            name: "brain_list_threads".to_string(),
            description: "List the user's conversation threads in the Virtual Brain. Threads track ongoing work contexts.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "user": {
                        "type": "string",
                        "description": "User name (default: venkat)",
                        "default": "venkat"
                    }
                },
                "required": []
            }),
        }
    }

    fn brain_stats() -> ToolDefinition {
        ToolDefinition {
            name: "brain_stats".to_string(),
            description: "Get vault graph stats — node count, edge count, community count, active threads, and pending candidates.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn brain_propose_node() -> ToolDefinition {
        ToolDefinition {
            name: "brain_propose_node".to_string(),
            description: "Propose a new knowledge node to the Virtual Brain. Staged as a candidate for human review in the Inbox.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": "Node type: fact, decision, reference, preference, procedure, architecture"
                    },
                    "title": {
                        "type": "string",
                        "description": "Title of the knowledge node"
                    },
                    "body": {
                        "type": "string",
                        "description": "Full content / body of the knowledge node"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tags for categorization"
                    },
                    "project": {
                        "type": "string",
                        "description": "Optional project this knowledge belongs to"
                    }
                },
                "required": ["kind", "title", "body"]
            }),
        }
    }

    fn brain_propose_link() -> ToolDefinition {
        ToolDefinition {
            name: "brain_propose_link".to_string(),
            description:
                "Propose a link between two existing vault nodes. Staged for human review."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "src_path": {
                        "type": "string",
                        "description": "Source node path"
                    },
                    "dst_path": {
                        "type": "string",
                        "description": "Destination node path"
                    },
                    "edge_type": {
                        "type": "string",
                        "description": "Edge type: related_to, depends_on, extends, contradicts, supports, part_of"
                    }
                },
                "required": ["src_path", "dst_path", "edge_type"]
            }),
        }
    }

    fn brain_thread_append() -> ToolDefinition {
        ToolDefinition {
            name: "brain_thread_append".to_string(),
            description:
                "Add a message to a Virtual Brain thread. Creates the thread if it doesn't exist."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_slug": {
                        "type": "string",
                        "description": "Thread slug (URL-safe identifier)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Message content to append"
                    }
                },
                "required": ["thread_slug", "content"]
            }),
        }
    }

    fn brain_stack_push() -> ToolDefinition {
        ToolDefinition {
            name: "brain_stack_push".to_string(),
            description: "Push an item onto a thread's context stack. LIFO stack for tracking nested work items.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_slug": {
                        "type": "string",
                        "description": "Thread slug to push onto"
                    },
                    "title": {
                        "type": "string",
                        "description": "Title of the stack item"
                    }
                },
                "required": ["thread_slug", "title"]
            }),
        }
    }

    fn brain_backlog_add() -> ToolDefinition {
        ToolDefinition {
            name: "brain_backlog_add".to_string(),
            description:
                "Add an item to a project's backlog. Priority-sorted FIFO queue for work items."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Title of the backlog item"
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["urgent", "high", "medium", "low"],
                        "description": "Priority level (default: medium)",
                        "default": "medium"
                    },
                    "project": {
                        "type": "string",
                        "description": "Project name this item belongs to"
                    }
                },
                "required": ["title", "project"]
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

    fn computer_use() -> ToolDefinition {
        ToolDefinition {
            name: "computer_use".to_string(),
            description: "Drive the local fleet member's screen via screenshot/click/type/key/goto/move actions. Mirrors Anthropic's Computer Use API surface. Backed by `screencapture`+`cliclick` (macOS) or `scrot`+`xdotool` (Linux). Returns 503 with install hints when the helper binary is missing. Use this for browser automation, form fills, file dialogs — anything the human user could click on.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["screenshot", "click", "double_click", "move", "type", "key", "goto"],
                        "description": "Which screen-control action to perform."
                    },
                    "x": { "type": "integer", "description": "X coord (click/double_click/move only)" },
                    "y": { "type": "integer", "description": "Y coord (click/double_click/move only)" },
                    "text": { "type": "string", "description": "Text to type (action=type)" },
                    "key": { "type": "string", "description": "Keystroke name (e.g. 'Return', 'cmd+c'); action=key" },
                    "url": { "type": "string", "description": "URL to open in default browser (action=goto)" },
                    "region": { "type": "string", "description": "Optional region for screenshot: 'x,y,w,h'. Defaults to full screen." }
                },
                "required": ["action"]
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
            "fleet_models_catalog",
            "fleet_models_search",
            "fleet_models_library",
            "fleet_models_deployments",
            "fleet_models_disk_usage",
            // Virtual Brain
            "brain_search",
            "brain_vault_read",
            "brain_graph_neighbors",
            "brain_list_threads",
            "brain_stats",
            "brain_propose_node",
            "brain_propose_link",
            "brain_thread_append",
            "brain_stack_push",
            "brain_backlog_add",
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
