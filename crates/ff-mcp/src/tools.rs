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
        self.register(Self::fleet_offload());
        self.register(Self::fleet_cascade());
        self.register(Self::fleet_route());
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
        self.register(Self::fleet_worker_detail());
        self.register(Self::fleet_models_db());
        self.register(Self::task_lineage());
        self.register(Self::fleet_models_catalog());
        self.register(Self::fleet_models_search());
        self.register(Self::fleet_models_library());
        self.register(Self::fleet_models_deployments());
        self.register(Self::fleet_models_disk_usage());
        self.register(Self::fleet_agents());
        self.register(Self::work_item_context());
        self.register(Self::fabric_topology());
        self.register(Self::pm_board());
        self.register(Self::pm_claim());
        self.register(Self::pm_create());
        self.register(Self::pm_list());
        self.register(Self::pm_ready());

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

        // ── Cortex code-graph tools ─────────────────────────────────────
        self.register(Self::cortex_corpora());
        self.register(Self::cortex_find());
        self.register(Self::cortex_cross_repo_find());
        self.register(Self::cortex_search());
        self.register(Self::cortex_show());
        self.register(Self::cortex_context());
        self.register(Self::cortex_explain());
        self.register(Self::cortex_outline());
        self.register(Self::cortex_callers());
        self.register(Self::cortex_callees());
        self.register(Self::cortex_impact());
        self.register(Self::cortex_affected_flows());
        self.register(Self::cortex_deps());
        self.register(Self::cortex_readers());
        self.register(Self::cortex_writers());
        self.register(Self::cortex_config_key());
        self.register(Self::cortex_path());
        self.register(Self::cortex_tests());
        self.register(Self::cortex_review());

        // ── Computer Use (Pillar 1) ─────────────────────────────────────
        self.register(Self::computer_use());

        // ── Scratchpad (agent working memory) ───────────────────────────
        self.register(Self::memory_get());
        self.register(Self::memory_add());
        self.register(Self::memory_replace());
        self.register(Self::memory_remove());
    }

    // ── Tool definitions ─────────────────────────────────────────────────

    fn work_item_context() -> ToolDefinition {
        ToolDefinition {
            name: "work_item_context".to_string(),
            description: "Retrieve bounded, ready-to-inject context for a Mission Control work item, optionally including repository state.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "work_item_id": {
                        "type": "string",
                        "description": "Mission Control work item id"
                    },
                    "repo_path": {
                        "type": "string",
                        "description": "Optional local Git checkout to include"
                    },
                    "max_commits": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 50,
                        "default": 10
                    }
                },
                "required": ["work_item_id"]
            }),
        }
    }

    fn fabric_topology() -> ToolDefinition {
        ToolDefinition {
            name: "fabric_topology".to_string(),
            description: "Show the private-fabric ring nodes, edges, and verification state."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn pm_board() -> ToolDefinition {
        ToolDefinition {
            name: "pm_board".to_string(),
            description: "Retrieve the fleet project-management board, including status totals and current work items.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 500,
                        "default": 100
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn pm_claim() -> ToolDefinition {
        ToolDefinition {
            name: "pm_claim".to_string(),
            description:
                "Claim a project-management work item for an agent and attach claim context."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "work_item_id": {
                        "type": "string",
                        "format": "uuid",
                        "description": "Work item UUID to claim"
                    },
                    "agent": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Agent or execution context claiming the item"
                    },
                    "context": {
                        "type": "object",
                        "description": "Optional claim-time context merged into the work item",
                        "default": {}
                    }
                },
                "required": ["work_item_id", "agent"],
                "additionalProperties": false
            }),
        }
    }

    fn pm_create() -> ToolDefinition {
        ToolDefinition {
            name: "pm_create".to_string(),
            description: "Create a Mission Control project-management work item.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Work item title"
                    },
                    "description": {
                        "type": "string",
                        "default": "",
                        "description": "Detailed work item description"
                    },
                    "priority": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 5,
                        "description": "Priority from 1 (critical) to 5 (low); defaults to 3"
                    }
                },
                "required": ["title"],
                "additionalProperties": false
            }),
        }
    }

    fn pm_list() -> ToolDefinition {
        ToolDefinition {
            name: "pm_list".to_string(),
            description: "List Mission Control project-management work items, optionally filtered by backlog fields.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Work-item status" },
                    "assignee": { "type": "string", "description": "Assigned agent or operator" },
                    "epic_id": { "type": "string", "description": "Epic identifier" },
                    "sprint_id": { "type": "string", "description": "Sprint identifier" },
                    "task_group_id": { "type": "string", "description": "Task-group identifier" },
                    "label": { "type": "string", "description": "Required label" }
                },
                "additionalProperties": false
            }),
        }
    }

    fn pm_ready() -> ToolDefinition {
        ToolDefinition {
            name: "pm_ready".to_string(),
            description: "Flag a project-management work item ready for fleet scheduling. Repeated calls are idempotent.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "work_item_id": {
                        "type": "string",
                        "format": "uuid",
                        "description": "Work item UUID to flag ready"
                    },
                    "on": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Optional computer to pin execution to"
                    }
                },
                "required": ["work_item_id"],
                "additionalProperties": false
            }),
        }
    }

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
            description: "Single-turn LLM call. \n\n\
                Two routing modes:\n\
                - strategy=\"tier\" (default, legacy): tiered escalator \
                (9B→32B→72B→235B). Starts at the fastest model and escalates \
                only if the cheaper tier hard-fails.\n\
                - strategy=\"auto\": outcome-aware — small classifier picks \
                the right shape (single-tier / cascade / judge-escalate) per \
                prompt. Stops shallow tier-1 answers from slipping through \
                on complex prompts. Same engine as fleet_cascade.\n\n\
                Other strategies (\"single\", \"cascade\", \"judge_escalate\") \
                are explicit overrides. tier and validator params are honoured \
                for non-tier strategies."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The prompt/task to execute"
                    },
                    "strategy": {
                        "type": "string",
                        "enum": ["tier", "auto", "single", "cascade", "judge_escalate"],
                        "default": "tier",
                        "description": "Routing strategy. tier=legacy escalator (default, unchanged). auto=classifier picks. single/cascade/judge_escalate=explicit override."
                    },
                    "start_tier": {
                        "type": "integer",
                        "description": "(strategy=tier only) Start at tier (1=9B fast, 2=32B code, 3=72B review, 4=235B expert)",
                        "default": 1
                    },
                    "max_tier": {
                        "type": "integer",
                        "description": "(strategy=tier only) Maximum tier to escalate to",
                        "default": 4
                    },
                    "tier": {
                        "type": "integer",
                        "description": "(strategy=single|judge_escalate only) Specific tier hint (1-4)",
                        "minimum": 1,
                        "maximum": 4
                    },
                    "validator": {
                        "type": "string",
                        "enum": ["none", "json", "yaml"],
                        "default": "none",
                        "description": "(strategy=cascade only) Validator gate between cascade tiers. 'json' parses each tier's output."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn fleet_offload() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_offload".to_string(),
            description: "Credit-saver: offload heavy, low-architectural-subtlety \
                work to a WARM tool-capable local LLM on the fleet so you (the \
                cloud orchestrator) don't burn cloud tokens on the bulk. \
                \n\nWhen to call: bulk code generation, multi-file mechanical \
                edits, research, summarization, test/doc generation, data \
                extraction — anything high-token where the *shape* of the answer \
                is clear and you'll review the output anyway. Keep \
                architectural / load-bearing decisions in-cloud. \
                \n\nBehaviour: picks the best WARM tool-capable deployment via \
                the capability router, KIND-AWARE — codegen/edits/tests route to \
                a coder-family model first (falling back to any tool-capable \
                model), with smaller-tier-first then least-loaded-host ranking. \
                Pass `kind` so code work lands on a coder. Dispatches over the \
                OpenAI-compatible API (model 'thinking' disabled so you get the \
                answer, not chain-of-thought) and returns the result for you to \
                review. If NO warm tool-capable endpoint exists it returns \
                {offloaded:false, decision:\"do_in_cloud\"} — proceed and do the \
                work yourself. Prefer-warm only; demand-driven autoscaling of \
                the model mix is orchestrator P3."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The self-contained task to offload. Include all context the local model needs — it does not see your conversation."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Optional task shape hint for logging/triage: codegen | edits | research | summarize | tests | docs | extract | other."
                    },
                    "est_output_tokens": {
                        "type": "integer",
                        "description": "Optional estimate of output size — caps the local model's max_tokens (clamped 256..=8192, default 4096)."
                    },
                    "min_ctx": {
                        "type": "integer",
                        "description": "Required usable per-slot context on the local endpoint so the task + tool-schema prompt fits. Default 16384.",
                        "default": 16384
                    }
                },
                "required": ["task"]
            }),
        }
    }

    fn fleet_cascade() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_cascade".to_string(),
            description: "Outcome-aware routing. Runs a small classifier on the \
                prompt to estimate (complexity × shape), then dispatches via the \
                right strategy: SingleTier for simple tasks, Cascade (drafter → \
                verifier → finalizer with validator gates and judge early-exit) \
                for complex+structured tasks (JSON/code/schemas), or \
                JudgeEscalate for complex+open-ended tasks (single dispatch + \
                Gemma-4 judge, escalate one tier on low score). \
                \n\nUse this when you want the fleet to *think before dispatching* \
                — stops a tier-1 model from confidently shipping a shallow \
                answer to a hard question, and stops tier-3 from re-doing \
                boilerplate that tier-1 could have drafted for free."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The task to run."
                    },
                    "strategy": {
                        "type": "string",
                        "enum": ["auto", "single", "cascade", "judge_escalate"],
                        "default": "auto",
                        "description": "auto = classifier decides. single = one tier (pass tier hint). cascade = drafter→verifier→finalizer. judge_escalate = single dispatch + judge + escalate."
                    },
                    "tier": {
                        "type": "integer",
                        "description": "Tier hint (1-4) for strategy=single or judge_escalate.",
                        "minimum": 1,
                        "maximum": 4
                    },
                    "validator": {
                        "type": "string",
                        "enum": ["none", "json", "yaml"],
                        "default": "none",
                        "description": "Validator gate to run between cascade tiers. 'json' parses each tier's output; tier N+1 sees the parse error if N failed."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn fleet_route() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_route".to_string(),
            description: "Workload-aware routing: given a workload tag (e.g. \
                \"code\", \"embedding\", \"reranking\", \"reasoning\", \"chat\", \
                \"tool_calling\", \"vision\"), returns the best healthy deployment \
                on the fleet to send that kind of request to — plus runner-up \
                candidates. Use this BEFORE building a request when you don't know \
                which node serves what; stops callers from hardcoding endpoints. \
                For AGENT dispatch set tool_calling=true and min_ctx (e.g. 32768) \
                so you only get tool-calling endpoints with enough per-slot context \
                — never a non-tool model like gemma. Read-only — does not dispatch."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workload": {
                        "type": "string",
                        "description": "The workload tag to route. Common: \"code\", \"chat\", \"embedding\", \"reranking\", \"reasoning\", \"tool_calling\", \"vision\". Matches against fleet_model_catalog.preferred_workloads."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max candidates to return (default 3)",
                        "default": 3
                    },
                    "tool_calling": {
                        "type": "boolean",
                        "description": "Require a tool-calling model (fleet_model_catalog.tool_calling=true). Use for agent dispatch. (workload=\"tool_calling\" implies this.)",
                        "default": false
                    },
                    "min_ctx": {
                        "type": "integer",
                        "description": "Require this much usable per-slot context (fleet_model_deployments.usable_agent_ctx). e.g. 32768 for an agent so the tool-schema system prompt fits."
                    },
                    "exclude_hosts": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Worker names to exclude (case-insensitive), e.g. [\"taylor\"] to keep agent load off the leader."
                    }
                },
                "required": ["workload"]
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
            description: "Multi-agent coding pipeline driven by the fleet_agents catalog \
                (V112). The default crew is Code Writer → Code Reviewer (catalog agents); \
                stricter project policies add a context Explorer pass and a second review. \
                Each agent's endpoint is chosen by the agent-swarm capability router \
                (tool-calling model + enough per-slot ctx), not hardcoded to one host. \
                Best when the task benefits from a review pass over the writer's output: \
                refactors, bug fixes touching multiple files, new functions with edge \
                cases, anything where 'works but might be wrong' is a real risk. Slower \
                than `fleet_run` (multiple model calls vs 1), so skip it for self-contained \
                one-shot tasks. See `fleet_agents` for the catalog."
                .to_string(),
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
            description: "Get real-time fleet metrics plus the Postgres count of unschedulable ready bug/feature parents. Optionally filter by node name.".to_string(),
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

    fn fleet_worker_detail() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_worker_detail".to_string(),
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
            description: "List models actually on disk across fleet nodes. Returns worker_name, catalog_id, runtime, quant, file_path, size_bytes. Optionally filter to a single node.".to_string(),
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
            description: "List currently running model deployments across the fleet. Returns worker_name, catalog_id, runtime, port, health_status, started_at. Optionally filter to a single node.".to_string(),
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
            description: "Latest disk usage sample per fleet node. Returns worker_name, total_gb, used_gb, free_gb, models_gb, sampled_at.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn fleet_agents() -> ToolDefinition {
        ToolDefinition {
            name: "fleet_agents".to_string(),
            description: "List or show the fleet_agents catalog (V112) — the specialized \
                agents the crew/orchestrator can instantiate (code-writer, code-reviewer, \
                researcher, refactorer, test-writer, doc-writer, planner, explorer). Each \
                agent carries a role, system prompt, allowed tool set, and the routing \
                capability (tool_calling + min_ctx) used by the agent-swarm router. \
                Read-only — pass 'name' to show one agent's full definition."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Show a single agent by its catalog name (e.g. \"code-writer\"). Omit to list all."
                    },
                    "enabled_only": {
                        "type": "boolean",
                        "description": "When listing, return only enabled agents.",
                        "default": true
                    }
                },
                "required": []
            }),
        }
    }

    // ── Virtual Brain tool definitions ───────────────────────────────────

    fn cortex_corpora() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_corpora".to_string(),
            description: "List the Cortex code-graph corpora (indexed repos) and their sizes. Use this first to discover the `corpus` slug to pass to cortex_callers/callees/impact.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn cortex_find() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_find".to_string(),
            description: "Cortex code graph: find code symbols, ranked with file:line. Default: by name fragment (case-insensitive substring), ranked by fan-in (most depended-on first). With semantic=true: rank by embedding similarity (bge-m3) so you can search by INTENT ('where we publish heartbeats') when you don't know the name — requires the corpus to have been embedded (ff cortex embed). The discovery entrypoint — resolve a partial name or intent into exact qualified names, then pass those to cortex_callers/callees/impact. Cheaper than grepping the tree.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "query": {
                        "type": "string",
                        "description": "Name fragment (substring mode) or natural-language intent (semantic mode), e.g. 'load_model' or 'where models are launched'"
                    },
                    "semantic": {
                        "type": "boolean",
                        "description": "Rank by embedding similarity instead of name substring — search by intent. Default false.",
                        "default": false
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max hits to return (1-500, default 20)",
                        "default": 20
                    },
                    "kind": {
                        "type": "string",
                        "description": "Narrow to one node-type class: function, struct, enum, trait, impl, mod, class, interface, or 'type' (the type-defining symbols across languages: struct/enum/trait/class/interface). Default: all code symbols."
                    }
                },
                "required": ["corpus", "query"]
            }),
        }
    }

    fn cortex_cross_repo_find() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_cross_repo_find".to_string(),
            description: "Cortex code graph: find code symbols across EVERY indexed repo at once (monorepo / multi-repo navigation), each hit tagged with its corpus + file:line, ranked by fan-in. The multi-corpus counterpart of cortex_find — answers 'where does `foo` live across all my repos?' without first picking a corpus (use cortex_corpora to see them).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Name fragment (case-insensitive substring), e.g. 'ApiException' or 'load_model'"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max hits across all repos (1-500, default 20)",
                        "default": 20
                    },
                    "kind": {
                        "type": "string",
                        "description": "Narrow to one node-type class: function, struct, enum, trait, impl, mod, class, interface, or 'type'. Default: all code symbols."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn cortex_search() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_search".to_string(),
            description: "Hybrid code search: semantic vector search + graph-neighborhood expansion + cross-encoder rerank → the most relevant symbols for an intent. Use when you do not know the symbol name.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language intent, e.g. 'where model endpoints are routed'"
                    },
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (optional; defaults to the current working directory name)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max hits to return (1-50, default 8)",
                        "default": 8
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn cortex_show() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_show".to_string(),
            description: "Cortex code graph: show a code symbol's SOURCE — resolve a name to its file + line span and return just that symbol's definition. One call instead of cortex_find → read the file → slice the span (the Cortex get_review_context). An exact qualified-name match wins, else an exact leaf match (highest fan-in), else the top fan-in hit; other matches are returned so you can disambiguate. Needs the indexed checkout present on the host (like cortex_review).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf ('load_model') or qualified ('ff_agent::model_runtime::load_model'), case-insensitive"
                    },
                    "kind": {
                        "type": "string",
                        "description": "Narrow to one node-type class: function, struct, enum, trait, impl, mod, class, interface, or 'type'. Default: all code symbols."
                    },
                    "max_lines": {
                        "type": "integer",
                        "description": "Cap the returned source at this many lines (1-2000, default 200); truncated=true when cut.",
                        "default": 200
                    },
                    "context": {
                        "type": "integer",
                        "description": "Include N lines of surrounding context above and below the symbol (like grep -C, 0-500, default 0). The source then starts at display_start (= start_line when 0).",
                        "default": 0
                    }
                },
                "required": ["corpus", "symbol"]
            }),
        }
    }

    fn cortex_context() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_context".to_string(),
            description: "Cortex code graph: one call for a code symbol's definition + callers + callees + impact + community summary. This is the agent loop's default Cortex call when orienting on a symbol, replacing cortex_find → cortex_show → cortex_callers/callees/impact/explain. Resolves like cortex_show (exact qualified → exact leaf by fan-in → top fan-in hit), caps direct relationships, and returns the community's stored natural-language summary instead of every member.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf ('load_model') or qualified ('ff_agent::model_runtime::load_model'), case-insensitive"
                    },
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'. Optional; defaults to the current working directory's basename."
                    },
                    "include_snippet": {
                        "type": "boolean",
                        "description": "Include the symbol definition source using cortex_show's source extraction logic. Default true.",
                        "default": true
                    },
                    "max_callers": {
                        "type": "integer",
                        "description": "Max direct callers to return (1-100, default 10).",
                        "default": 10
                    },
                    "max_callees": {
                        "type": "integer",
                        "description": "Max direct callees to return (1-100, default 10).",
                        "default": 10
                    }
                },
                "required": ["symbol"]
            }),
        }
    }

    fn cortex_explain() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_explain".to_string(),
            description: "Cortex code graph: EXPLAIN the subsystem a symbol belongs to — resolve a symbol to its code-graph community (the cluster of densely-connected symbols it lives in) and return that community's natural-language summary plus its highest-fan-in members. The GraphRAG 'what is this cluster responsible for?' answer in one call, so you can orient on a subsystem without reading every file in it. Symbol resolves like cortex_show (exact qualified → exact leaf → top fan-in). summary is null until `ff cortex summarize` has covered the community; community_id is null until the graph is community-detected (`ff cortex embed`).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf ('load_model') or qualified ('ff_agent::model_runtime::load_model'), case-insensitive. The community is whichever cluster owns it."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Narrow symbol resolution to one node-type class: function, struct, enum, trait, impl, mod, class, interface, or 'type'. Default: all code symbols."
                    },
                    "members": {
                        "type": "integer",
                        "description": "How many of the community's top members (by fan-in) to return (1-200, default 15).",
                        "default": 15
                    }
                },
                "required": ["corpus", "symbol"]
            }),
        }
    }

    fn cortex_outline() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_outline".to_string(),
            description: "Cortex code graph: outline a FILE — every code symbol it defines (kind, line span, fan-in) in source order, a file-level table of contents. Orient in an unknown file from the graph in one call instead of reading the whole file. Resolve the file by exact path or a path suffix ('cortex.rs' or 'src/cortex.rs'); multiple matches error with the candidates. Pure graph query (no source read), so it works even if the indexed checkout isn't on this host. Pair with cortex_show to then pull one symbol's source.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "file": {
                        "type": "string",
                        "description": "File path or path suffix, e.g. 'cortex.rs' or 'crates/ff-brain/src/cortex.rs'. An exact path wins; a unique suffix is taken; multiple matches error with candidates."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Narrow to one node-type class: function, struct, enum, trait, impl, mod, class, interface, or 'type'. Default: all code symbols."
                    }
                },
                "required": ["corpus", "file"]
            }),
        }
    }

    fn cortex_callers() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_callers".to_string(),
            description: "Cortex code graph: list the callers of a code symbol (who calls it). Token-cheaper than grepping for call sites. Symbol may be a bare leaf (e.g. 'load_model') or a fully-qualified name. Pass all_corpora=true to find callers in EVERY indexed repo (omit corpus).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'. Required UNLESS all_corpora=true."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf ('load_model') or qualified ('ff_agent::model_runtime::load_model')"
                    },
                    "all_corpora": {
                        "type": "boolean",
                        "description": "Cross-repo: match the symbol name in EVERY indexed corpus and union the callers, each tagged with its corpus (ignores corpus). Answers 'who calls this name anywhere in our codebases?'.",
                        "default": false
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (high-trust, primary resolver named a real internal symbol), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default). Pass 1.0 for high-precision traversal that drops the ~40% of edges from heuristic redirects.",
                        "default": 0.0
                    }
                },
                "required": ["symbol"]
            }),
        }
    }

    fn cortex_callees() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_callees".to_string(),
            description: "Cortex code graph: list the callees of a code symbol (what it calls). Symbol may be a bare leaf or a fully-qualified name. Pass all_corpora=true to search EVERY indexed repo (omit corpus).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'. Required UNLESS all_corpora=true."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf or qualified name"
                    },
                    "all_corpora": {
                        "type": "boolean",
                        "description": "Cross-repo: match the symbol name in EVERY indexed corpus and union the callees, each tagged with its corpus (ignores corpus).",
                        "default": false
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (high-trust), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default).",
                        "default": 0.0
                    }
                },
                "required": ["symbol"]
            }),
        }
    }

    fn cortex_impact() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_impact".to_string(),
            description: "Cortex code graph: transitive caller closure (blast radius) of a code symbol — every symbol that could be affected by changing it, up to max_depth hops. Pass all_corpora=true to compute the blast radius in EVERY indexed repo (omit corpus).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'. Required UNLESS all_corpora=true."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf or qualified name"
                    },
                    "all_corpora": {
                        "type": "boolean",
                        "description": "Cross-repo: seed the closure from the symbol name in EVERY indexed corpus and tag each affected symbol with its corpus (ignores corpus).",
                        "default": false
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Max transitive hops (1-20, default 5)",
                        "default": 5
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (high-trust), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default). Use 1.0 for a high-precision blast radius that excludes heuristic-redirect edges.",
                        "default": 0.0
                    }
                },
                "required": ["symbol"]
            }),
        }
    }

    fn cortex_affected_flows() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_affected_flows".to_string(),
            description: "Cortex code graph: one-call change-impact view for a symbol, combining direct callers, direct callees, transitive caller impact, and covering tests. Use before changing a symbol to see its upstream consumers, downstream dependencies, blast radius, and test coverage without four separate calls.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf or qualified name"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Max transitive hops for impact and test coverage (1-20, default 5)",
                        "default": 5
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse calls edges at/above this confidence (0.0-1.0, default 0.0)",
                        "default": 0.0
                    }
                },
                "required": ["corpus", "symbol"]
            }),
        }
    }

    fn cortex_deps() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_deps".to_string(),
            description: "Cortex dependency graph. With NO `crate`: list dependency packages and how many crates depend on each (the most-depended-on first). With a `crate`: that crate's forward dependencies (what it needs) AND reverse dependents (what needs it — the workspace rebuild blast radius); set transitive=true to also get the full transitive-dependents closure. Use before a refactor to see what recompiles / what breaks.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'. Defaults to the cwd's slug."
                    },
                    "crate": {
                        "type": "string",
                        "description": "A crate/package name. Omit to list all dependency packages with their dependent counts."
                    },
                    "transitive": {
                        "type": "boolean",
                        "description": "When a `crate` is given, also compute the full transitive-dependents closure (everything that would rebuild).",
                        "default": false
                    }
                }
            }),
        }
    }

    fn cortex_readers() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_readers".to_string(),
            description: "Cortex data-flow: functions that READ a database column — who consumes its value. Use before changing a column's type/meaning to see every reader. Pair with cortex_writers (who sets it).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": { "type": "string", "description": "Indexed repo slug (see cortex_corpora). Defaults to the cwd's slug." },
                    "column": { "type": "string", "description": "DB column, e.g. 'work_items.status' or bare 'status'." }
                },
                "required": ["column"]
            }),
        }
    }

    fn cortex_writers() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_writers".to_string(),
            description: "Cortex data-flow: functions that WRITE a database column — who produces its value. Use before changing a column's invariants to see every write site. Pair with cortex_readers (who consumes it).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": { "type": "string", "description": "Indexed repo slug (see cortex_corpora). Defaults to the cwd's slug." },
                    "column": { "type": "string", "description": "DB column, e.g. 'work_items.status' or bare 'status'." }
                },
                "required": ["column"]
            }),
        }
    }

    fn cortex_config_key() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_config_key".to_string(),
            description: "Cortex config impact. With NO `key`: list every config/env/secret/feature-flag key extracted in the corpus. With a `key`: the functions that READ it — who breaks if you rename, retire, or change that env var / flag. The config analogue of cortex_readers for DB columns.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": { "type": "string", "description": "Indexed repo slug (see cortex_corpora). Defaults to the cwd's slug." },
                    "key": { "type": "string", "description": "A config/env/secret/feature-flag key, e.g. 'FORGEFLEET_REDIS_URL'. Omit to list all keys." }
                }
            }),
        }
    }

    fn cortex_path() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_path".to_string(),
            description: "Cortex code graph: shortest call chain between two symbols — HOW does `from` reach `to`. cortex_callers/callees answer one hop and cortex_impact the whole closure; this returns the ordered FROM → … → TO path (each hop a real `calls` edge). `found=false` with an empty path means the symbols exist but don't connect within max_depth (not an error); an unresolved from/to symbol errors.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "from": {
                        "type": "string",
                        "description": "Start symbol — bare leaf ('handle_cortex') or qualified name"
                    },
                    "to": {
                        "type": "string",
                        "description": "Target symbol — bare leaf or qualified name"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Max chain length in hops (1-30, default 12)",
                        "default": 12
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (high-trust), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default).",
                        "default": 0.0
                    }
                },
                "required": ["corpus", "from", "to"]
            }),
        }
    }

    fn cortex_tests() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_tests".to_string(),
            description: "Cortex code graph: which tests cover a code symbol — the transitive caller closure filtered to callers that are tests (test-file path or test-named symbol), ranked nearest-first (depth 1 = a test calls it directly). An empty result is a coverage gap. Use to check whether a risky change is tested without grepping the test tree.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf or qualified name"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Max transitive hops to search for a covering test (1-20, default 5)",
                        "default": 5
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (tests that provably reach the symbol via directly-resolved calls), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default). Use 1.0 for a strict coverage claim.",
                        "default": 0.0
                    }
                },
                "required": ["corpus", "symbol"]
            }),
        }
    }

    fn cortex_review() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_review".to_string(),
            description: "Cortex change-aware code review: shell `git diff` in a repo checkout and score every changed file against the code graph so you know WHERE TO LOOK FIRST. Each changed symbol gets its fan-in (direct callers), external fan-in (cross-file callers = de-facto API surface), transitive blast radius, and a High/Med/Low risk tier; files are ranked riskiest-first. Use before reviewing a diff instead of reading whole files.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "repo_dir": {
                        "type": "string",
                        "description": "Absolute path to the git checkout that was indexed (the daemon shells git here)"
                    },
                    "base": {
                        "type": "string",
                        "description": "Optional base ref. With it, reviews the branch's own commits (base...HEAD) plus uncommitted edits; without it, just uncommitted work (staged+unstaged+untracked) vs HEAD."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Max transitive blast-radius hops (1-20, default 5)",
                        "default": 5
                    }
                },
                "required": ["corpus", "repo_dir"]
            }),
        }
    }

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

    // ── Scratchpad (agent working memory) ────────────────────────────────
    // A small, byte-capped (6KB/scope), agent-self-editable text memory with
    // fixed blocks and consolidate-and-forget. Sits beside session memory.

    fn memory_scope_props() -> Value {
        json!({
            "scope_type": {
                "type": "string",
                "enum": ["session", "agent", "project"],
                "description": "Memory scope. 'agent'/'project' persist across sessions; 'session' is ephemeral. Default: session."
            },
            "scope_key": {
                "type": "string",
                "description": "Identifier within the scope (agent id / project id / session id). Default: 'default'."
            },
            "block": {
                "type": "string",
                "enum": ["task", "decisions", "findings", "state", "scratch"],
                "description": "Which fixed working-memory block to operate on."
            },
            "cwd": {
                "type": "string",
                "description": "Your absolute working directory. When scope is left at the default, the server derives a stable project id from it (project:github.com/org/repo) so this repo's memory is SHARED with other CLIs (Claude Code/Codex/Kimi) working in the SAME repo. Pass it on every call; omit only for ephemeral session memory."
            }
        })
    }

    fn memory_get() -> ToolDefinition {
        ToolDefinition {
            name: "memory_get".to_string(),
            description: "Read your curated working memory (the Scratchpad). Returns all blocks for a scope, or one block if 'block' is given. Use at the start of work to recall task/decisions/findings/state.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": Self::memory_scope_props(),
            }),
        }
    }

    fn memory_add() -> ToolDefinition {
        ToolDefinition {
            name: "memory_add".to_string(),
            description: "Append a line to a working-memory block. Use to record a decision, a finding, current task state. When the scope exceeds its byte cap, the lowest-priority block is auto-summarized (consolidate-and-forget) and the full text is preserved in Brain.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "scope_type": Self::memory_scope_props()["scope_type"],
                    "scope_key": Self::memory_scope_props()["scope_key"],
                    "cwd": Self::memory_scope_props()["cwd"],
                    "block": Self::memory_scope_props()["block"],
                    "text": { "type": "string", "description": "Text to append (newline-separated)." }
                },
                "required": ["block", "text"]
            }),
        }
    }

    fn memory_replace() -> ToolDefinition {
        ToolDefinition {
            name: "memory_replace".to_string(),
            description: "Replace the single occurrence of 'old' with 'new' in a working-memory block. Errors unless 'old' matches exactly once. Use to update a decision/state in place.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "scope_type": Self::memory_scope_props()["scope_type"],
                    "scope_key": Self::memory_scope_props()["scope_key"],
                    "cwd": Self::memory_scope_props()["cwd"],
                    "block": Self::memory_scope_props()["block"],
                    "old": { "type": "string", "description": "Existing substring to replace (must be unique in the block)." },
                    "new": { "type": "string", "description": "Replacement text." }
                },
                "required": ["block", "old", "new"]
            }),
        }
    }

    fn memory_remove() -> ToolDefinition {
        ToolDefinition {
            name: "memory_remove".to_string(),
            description: "Remove one occurrence of 'text' from a working-memory block, or clear the whole block when 'text' is omitted.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "scope_type": Self::memory_scope_props()["scope_type"],
                    "scope_key": Self::memory_scope_props()["scope_key"],
                    "cwd": Self::memory_scope_props()["cwd"],
                    "block": Self::memory_scope_props()["block"],
                    "text": { "type": "string", "description": "Substring to remove. Omit to clear the entire block." }
                },
                "required": ["block"]
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
            "fleet_offload",
            "fleet_cascade",
            "fleet_route",
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
            "fleet_worker_detail",
            "fleet_models_db",
            "task_lineage",
            "fleet_models_catalog",
            "fleet_models_search",
            "fleet_models_library",
            "fleet_models_deployments",
            "fleet_models_disk_usage",
            "fleet_agents",
            "work_item_context",
            "fabric_topology",
            "pm_board",
            "pm_claim",
            "pm_create",
            "pm_list",
            "pm_ready",
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
            // Cortex code graph
            "cortex_corpora",
            "cortex_find",
            "cortex_cross_repo_find",
            "cortex_search",
            "cortex_show",
            "cortex_context",
            "cortex_explain",
            "cortex_outline",
            "cortex_callers",
            "cortex_callees",
            "cortex_impact",
            "cortex_affected_flows",
            "cortex_deps",
            "cortex_readers",
            "cortex_writers",
            "cortex_config_key",
            "cortex_path",
            "cortex_tests",
            "cortex_review",
            // Pillar 1 — Computer Use (PR-H, #37)
            "computer_use",
            // Pillar 2 — Scratchpad (agent working memory)
            "memory_get",
            "memory_add",
            "memory_replace",
            "memory_remove",
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
