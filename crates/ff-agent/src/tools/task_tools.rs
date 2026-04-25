//! Task management tools — create, get, update, list, stop, and get output of background tasks.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

// ---------------------------------------------------------------------------
// In-memory task store (shared across tools via LazyLock)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String, // pending, in_progress, completed, deleted
    pub output: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub metadata: HashMap<String, Value>,
    /// Which node/session created this task.
    pub origin_node: Option<String>,
    /// ID of parent task that spawned this (for sub-tasks).
    pub parent_task_id: Option<String>,
    /// Node URL to POST a result callback to when task completes.
    pub reply_to_node: Option<String>,
}

static TASK_STORE: std::sync::LazyLock<Arc<DashMap<String, AgentTask>>> =
    std::sync::LazyLock::new(|| Arc::new(DashMap::new()));

/// Public accessor for the task store (used by /tasks command).
pub static TASK_STORE_PUB: std::sync::LazyLock<Arc<DashMap<String, AgentTask>>> =
    std::sync::LazyLock::new(|| TASK_STORE.clone());

fn next_task_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// TaskCreate
// ---------------------------------------------------------------------------

pub struct TaskCreateTool;

#[async_trait]
impl AgentTool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }

    fn description(&self) -> &str {
        "Create a task to track work. Use this to break complex work into steps and track progress."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "Brief title for the task" },
                "description": { "type": "string", "description": "What needs to be done" },
                "origin_node": { "type": "string", "description": "Node that originated this task" },
                "parent_task_id": { "type": "string", "description": "Parent task ID if this is a sub-task" },
                "reply_to_node": { "type": "string", "description": "Node URL to POST result callback to when done" }
            },
            "required": ["subject", "description"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let subject = input
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if subject.is_empty() {
            return AgentToolResult::err("Missing 'subject'");
        }

        let origin_node = input
            .get("origin_node")
            .and_then(Value::as_str)
            .map(str::to_string);
        let parent_task_id = input
            .get("parent_task_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let reply_to_node = input
            .get("reply_to_node")
            .and_then(Value::as_str)
            .map(str::to_string);

        let id = next_task_id();
        let now = now_iso();
        let task = AgentTask {
            id: id.clone(),
            subject: subject.clone(),
            description,
            status: "pending".into(),
            output: None,
            created_at: now.clone(),
            updated_at: now,
            metadata: HashMap::new(),
            origin_node,
            parent_task_id,
            reply_to_node,
        };

        TASK_STORE.insert(id.clone(), task);
        AgentToolResult::ok(format!("Task #{id} created: {subject}"))
    }
}

// ---------------------------------------------------------------------------
// TaskGet
// ---------------------------------------------------------------------------

pub struct TaskGetTool;

#[async_trait]
impl AgentTool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }

    fn description(&self) -> &str {
        "Get details of a specific task by ID."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task ID to retrieve" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let id = input.get("task_id").and_then(Value::as_str).unwrap_or("");
        match TASK_STORE.get(id) {
            Some(task) => {
                AgentToolResult::ok(serde_json::to_string_pretty(task.value()).unwrap_or_default())
            }
            None => AgentToolResult::err(format!("Task '{id}' not found")),
        }
    }
}

// ---------------------------------------------------------------------------
// TaskUpdate
// ---------------------------------------------------------------------------

pub struct TaskUpdateTool;

#[async_trait]
impl AgentTool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }

    fn description(&self) -> &str {
        "Update a task's status or details. Use to mark tasks as in_progress or completed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task ID to update" },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"] },
                "subject": { "type": "string", "description": "New subject" },
                "description": { "type": "string", "description": "New description" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let id = input.get("task_id").and_then(Value::as_str).unwrap_or("");
        let mut task = match TASK_STORE.get_mut(id) {
            Some(t) => t,
            None => return AgentToolResult::err(format!("Task '{id}' not found")),
        };

        if let Some(status) = input.get("status").and_then(Value::as_str) {
            task.status = status.to_string();
        }
        if let Some(subject) = input.get("subject").and_then(Value::as_str) {
            task.subject = subject.to_string();
        }
        if let Some(desc) = input.get("description").and_then(Value::as_str) {
            task.description = desc.to_string();
        }
        task.updated_at = now_iso();

        let status_clone = task.status.clone();
        let task_id_clone = task.id.clone();
        let output_clone = task.output.clone();
        let reply_to = task.reply_to_node.clone();
        let session_id = ctx.session_id.clone();

        // Fire best-effort callback if task just completed and has a reply_to_node.
        if status_clone == "completed" {
            if let Some(reply_url) = reply_to {
                let callback_url = format!("{}/agent/message", reply_url.trim_end_matches('/'));
                let payload = json!({
                    "task_id": task_id_clone,
                    "from_node": session_id,
                    "status": "completed",
                    "output": output_clone.unwrap_or_default(),
                });
                tokio::spawn(async move {
                    let client = Client::new();
                    let _ = client.post(&callback_url).json(&payload).send().await;
                });
            }
        }

        AgentToolResult::ok(format!("Task #{id} updated (status: {})", status_clone))
    }
}

// ---------------------------------------------------------------------------
// TaskList
// ---------------------------------------------------------------------------

pub struct TaskListTool;

#[async_trait]
impl AgentTool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        "List all tasks with their current status."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let tasks: Vec<Value> = TASK_STORE
            .iter()
            .filter(|t| t.status != "deleted")
            .map(|t| {
                json!({
                    "id": t.id,
                    "subject": t.subject,
                    "status": t.status,
                })
            })
            .collect();

        if tasks.is_empty() {
            AgentToolResult::ok("No tasks.")
        } else {
            AgentToolResult::ok(serde_json::to_string_pretty(&tasks).unwrap_or_default())
        }
    }
}

// ---------------------------------------------------------------------------
// TaskStop
// ---------------------------------------------------------------------------

pub struct TaskStopTool;

#[async_trait]
impl AgentTool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stop/cancel a running task."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task ID to stop" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let id = input.get("task_id").and_then(Value::as_str).unwrap_or("");
        match TASK_STORE.get_mut(id) {
            Some(mut task) => {
                task.status = "cancelled".into();
                task.updated_at = now_iso();
                AgentToolResult::ok(format!("Task #{id} cancelled"))
            }
            None => AgentToolResult::err(format!("Task '{id}' not found")),
        }
    }
}

// ---------------------------------------------------------------------------
// TaskOutput
// ---------------------------------------------------------------------------

pub struct TaskOutputTool;

#[async_trait]
impl AgentTool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn description(&self) -> &str {
        "Get the output/result of a completed task."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task ID" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let id = input.get("task_id").and_then(Value::as_str).unwrap_or("");
        match TASK_STORE.get(id) {
            Some(task) => {
                let output = task.output.as_deref().unwrap_or("(no output recorded)");
                AgentToolResult::ok(format!("Task #{id} ({}):\n{output}", task.status))
            }
            None => AgentToolResult::err(format!("Task '{id}' not found")),
        }
    }
}
