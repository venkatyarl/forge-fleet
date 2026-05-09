//! A2A Task types — request/response/update messages.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A task sent from one agent to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub session_id: Option<Uuid>,
    pub status: TaskStatus,
    pub messages: Vec<TaskMessage>,
    pub artifacts: Vec<TaskArtifact>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Status of an A2A task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Canceled,
}

/// A message within a task conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub role: String, // "user" | "agent"
    pub parts: Vec<MessagePart>,
}

/// A part of a message (text, file, or data).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text {
        text: String,
    },
    File {
        name: String,
        mime_type: String,
        bytes: Option<Vec<u8>>,
    },
    Data {
        mime_type: String,
        data: serde_json::Value,
    },
}

/// An artifact produced by a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskArtifact {
    pub name: String,
    pub description: Option<String>,
    pub parts: Vec<MessagePart>,
    pub index: i32,
    pub append: Option<bool>,
    pub last_chunk: Option<bool>,
}

/// A real-time update streamed via SSE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUpdate {
    pub task_id: Uuid,
    pub status: TaskStatus,
    pub message: Option<TaskMessage>,
    pub artifact: Option<TaskArtifact>,
    pub timestamp: DateTime<Utc>,
}
