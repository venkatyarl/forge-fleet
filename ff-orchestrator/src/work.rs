use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Represents the current state of a work item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkItemStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Priority level for work items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

/// A work item tracked by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    /// Unique identifier for this work item.
    pub id: String,
    /// Current status of the work item.
    pub status: WorkItemStatus,
    /// Priority level.
    pub priority: Priority,
    /// Arbitrary metadata associated with the work item.
    pub metadata: HashMap<String, String>,
    /// Optional description of the work item.
    pub description: Option<String>,
}

impl WorkItem {
    /// Creates a new pending work item.
    pub fn new(
        id: impl Into<String>,
        priority: Priority,
        metadata: HashMap<String, String>,
    ) -> Self {
        Self {
            id: id.into(),
            status: WorkItemStatus::Pending,
            priority,
            metadata,
            description: None,
        }
    }

    /// Transitions the work item from Pending to InProgress.
    /// Returns true if the transition succeeded, false otherwise.
    pub fn start(&mut self) -> bool {
        if matches!(self.status, WorkItemStatus::Pending) {
            self.status = WorkItemStatus::InProgress;
            true
        } else {
            false
        }
    }

    /// Transitions the work item from InProgress to Completed.
    /// Returns true if the transition succeeded, false otherwise.
    pub fn complete(&mut self) -> bool {
        if matches!(self.status, WorkItemStatus::InProgress) {
            self.status = WorkItemStatus::Completed;
            true
        } else {
            false
        }
    }

    /// Transitions the work item to Failed state.
    /// Returns true if the transition succeeded, false otherwise.
    pub fn fail(&mut self) -> bool {
        self.status = WorkItemStatus::Failed;
        true
    }

    /// Transitions the work item back to Pending from Failed state.
    /// Returns true if the transition succeeded, false otherwise.
    pub fn retry(&mut self) -> bool {
        if matches!(self.status, WorkItemStatus::Failed) {
            self.status = WorkItemStatus::Pending;
            true
        } else {
            false
        }
    }

    /// Sets a metadata field.
    pub fn set_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.metadata.insert(key.into(), value.into());
    }

    /// Gets a metadata field by key.
    pub fn get_metadata(&self, key: &str) -> Option<&String> {
        self.metadata.get(key)
    }

    /// Sets the description.
    pub fn set_description(&mut self, description: impl Into<String>) {
        self.description = Some(description.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_work_item_creation() {
        let mut metadata = HashMap::new();
        metadata.insert("key".to_string(), "value".to_string());
        let item = WorkItem::new("test-1", Priority::High, metadata.clone());

        assert_eq!(item.id, "test-1");
        assert!(matches!(item.status, WorkItemStatus::Pending));
        assert_eq!(item.priority, Priority::High);
        assert_eq!(item.metadata, metadata);
        assert_eq!(item.description, None);
    }

    #[test]
    fn test_state_transitions() {
        let mut item = WorkItem::new("test-1", Priority::Medium, HashMap::new());

        // Pending -> InProgress
        assert!(item.start());
        assert!(matches!(item.status, WorkItemStatus::InProgress));

        // InProgress -> Completed
        assert!(item.complete());
        assert!(matches!(item.status, WorkItemStatus::Completed));

        // Already completed, cannot start again
        assert!(!item.start());

        // Create new item, fail it, then retry
        let mut item2 = WorkItem::new("test-2", Priority::Low, HashMap::new());
        assert!(item2.fail());
        assert!(matches!(item2.status, WorkItemStatus::Failed));
        assert!(item2.retry());
        assert!(matches!(item2.status, WorkItemStatus::Pending));
    }

    #[test]
    fn test_metadata_operations() {
        let mut item = WorkItem::new("test-1", Priority::Low, HashMap::new());

        item.set_metadata("env", "production");
        assert_eq!(item.get_metadata("env"), Some(&"production".to_string()));

        item.set_metadata("env", "staging");
        assert_eq!(item.get_metadata("env"), Some(&"staging".to_string()));
    }

    #[test]
    fn test_serialization() {
        let mut item = WorkItem::new("test-1", Priority::High, HashMap::new());
        item.set_description("A test work item");

        let json = serde_json::to_string(&item).unwrap();
        let deserialized: WorkItem = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, "test-1");
        assert_eq!(deserialized.description, Some("A test work item".to_string()));
        assert_eq!(deserialized.priority, Priority::High);
    }
}
