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
        self.register(Self::cortex_show());
        self.register(Self::cortex_explain());
        self.register(Self::cortex_outline());
        self.register(Self::cortex_callers());
        self.register(Self::cortex_callees());
        self.register(Self::cortex_impact());
        self.register(Self::cortex_tests());
        self.register(Self::cortex_review());

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
            description: "Cortex code graph: list the callers of a code symbol (who calls it). Token-cheaper than grepping for call sites. Symbol may be a bare leaf (e.g. 'load_model') or a fully-qualified name.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "corpus": {
                        "type": "string",
                        "description": "Indexed repo slug (see cortex_corpora), e.g. 'forge-fleet'"
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Code symbol — bare leaf ('load_model') or qualified ('ff_agent::model_runtime::load_model')"
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (high-trust, primary resolver named a real internal symbol), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default). Pass 1.0 for high-precision traversal that drops the ~40% of edges from heuristic redirects.",
                        "default": 0.0
                    }
                },
                "required": ["corpus", "symbol"]
            }),
        }
    }

    fn cortex_callees() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_callees".to_string(),
            description: "Cortex code graph: list the callees of a code symbol (what it calls). Symbol may be a bare leaf or a fully-qualified name.".to_string(),
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
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (high-trust), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default).",
                        "default": 0.0
                    }
                },
                "required": ["corpus", "symbol"]
            }),
        }
    }

    fn cortex_impact() -> ToolDefinition {
        ToolDefinition {
            name: "cortex_impact".to_string(),
            description: "Cortex code graph: transitive caller closure (blast radius) of a code symbol — every symbol that could be affected by changing it, up to max_depth hops.".to_string(),
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
                        "description": "Max transitive hops (1-20, default 5)",
                        "default": 5
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Only traverse `calls` edges at/above this resolution-confidence tier: 1.0 = EXTRACTED only (high-trust), 0.6 = +INFERRED (heuristic redirect), 0.0 = all (default). Use 1.0 for a high-precision blast radius that excludes heuristic-redirect edges.",
                        "default": 0.0
                    }
                },
                "required": ["corpus", "symbol"]
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
            "cortex_show",
            "cortex_explain",
            "cortex_outline",
            "cortex_callers",
            "cortex_callees",
            "cortex_impact",
            "cortex_tests",
            "cortex_review",
            // Pillar 1 — Computer Use (PR-H, #37)
            "computer_use",
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
