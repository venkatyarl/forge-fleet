//! Focus Stack & Backlog — conversation context management.
//!
//! **Focus Stack (FILO):** When a conversation drifts, the current topic pushes
//! onto the stack. When you're done with the tangent, pop back to where you were.
//! This prevents losing track of partially-completed work when side conversations
//! happen. Displayed as a sidebar showing the stack depth.
//!
//! **Backlog (FIFO):** Items queued for later. The user mentions 10 things to build —
//! items that aren't being worked on go to the backlog and get processed in order
//! after the focus stack clears.
//!
//! Together they form a "conversation memory" that helps agents and users stay
//! on track during long, complex sessions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Focus Stack (FILO — First In, Last Out)
// ---------------------------------------------------------------------------

/// A single item on the focus stack — represents a paused conversation topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusItem {
    /// Unique ID.
    pub id: String,
    /// Short summary of the topic (shown in sidebar).
    pub title: String,
    /// Detailed context that gets injected when resumed.
    pub context: String,
    /// When this topic was pushed (paused).
    pub pushed_at: DateTime<Utc>,
    /// What triggered the push (user went off-topic, explicit push, etc.).
    pub push_reason: PushReason,
    /// How much progress was made before pushing (0.0 = just started, 1.0 = nearly done).
    pub progress: f64,
    /// Related items (work item IDs, file paths, etc.).
    pub related: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushReason {
    /// User explicitly said "let's come back to this later".
    Explicit,
    /// Detected topic drift — conversation moved to a different subject.
    TopicDrift,
    /// User asked a question that requires a side investigation.
    SideInvestigation,
    /// A dependency or blocker was discovered.
    Blocked,
    /// Agent suggested parking this to handle something more urgent.
    AgentSuggested,
}

/// The Focus Stack — FILO (Last pushed = first resumed).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusStack {
    /// Stack of paused topics (top = most recent push).
    items: Vec<FocusItem>,
}

impl FocusStack {
    pub fn new() -> Self { Self::default() }

    /// Push the current topic onto the stack (pause it).
    pub fn push(&mut self, title: String, context: String, reason: PushReason) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.items.push(FocusItem {
            id: id.clone(),
            title,
            context,
            pushed_at: Utc::now(),
            push_reason: reason,
            progress: 0.0,
            related: Vec::new(),
        });
        id
    }

    /// Pop the most recent topic (resume it).
    /// Returns the item and its context for injection into the conversation.
    pub fn pop(&mut self) -> Option<FocusItem> {
        self.items.pop()
    }

    /// Peek at the top of the stack without removing.
    pub fn peek(&self) -> Option<&FocusItem> {
        self.items.last()
    }

    /// View the entire stack (bottom to top).
    pub fn items(&self) -> &[FocusItem] {
        &self.items
    }

    /// Get stack depth.
    pub fn depth(&self) -> usize {
        self.items.len()
    }

    /// Check if the stack is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Remove a specific item by ID (e.g., user says "forget about that").
    pub fn remove(&mut self, id: &str) -> Option<FocusItem> {
        if let Some(pos) = self.items.iter().position(|i| i.id == id) {
            Some(self.items.remove(pos))
        } else {
            None
        }
    }

    /// Update progress on an item (e.g., "we're 60% done with this").
    pub fn update_progress(&mut self, id: &str, progress: f64) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.progress = progress.clamp(0.0, 1.0);
        }
    }

    /// Build a summary for display in the sidebar.
    pub fn summary(&self) -> Vec<FocusStackEntry> {
        self.items.iter().rev().enumerate().map(|(depth, item)| {
            let age = Utc::now().signed_duration_since(item.pushed_at);
            let age_str = if age.num_hours() > 0 {
                format!("{}h ago", age.num_hours())
            } else if age.num_minutes() > 0 {
                format!("{}m ago", age.num_minutes())
            } else {
                "just now".into()
            };

            FocusStackEntry {
                depth,
                id: item.id.clone(),
                title: item.title.clone(),
                progress: item.progress,
                age: age_str,
                reason: item.push_reason,
            }
        }).collect()
    }

    /// Build context injection string for the agent system prompt.
    /// This reminds the agent what's on the stack.
    pub fn context_injection(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }

        let mut context = String::from("\n## Focus Stack (paused topics — come back to these)\n\n");
        for (i, item) in self.items.iter().rev().enumerate() {
            let progress_bar = progress_bar(item.progress, 10);
            context.push_str(&format!(
                "{}. **{}** [{progress_bar}] — {}\n",
                i + 1, item.title,
                match item.push_reason {
                    PushReason::Explicit => "paused by user",
                    PushReason::TopicDrift => "topic drifted",
                    PushReason::SideInvestigation => "side investigation",
                    PushReason::Blocked => "blocked",
                    PushReason::AgentSuggested => "parked",
                }
            ));
        }
        context.push_str("\nAfter completing the current task, check if any stack items should be resumed.\n");
        context
    }
}

/// Displayable focus stack entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusStackEntry {
    pub depth: usize,
    pub id: String,
    pub title: String,
    pub progress: f64,
    pub age: String,
    pub reason: PushReason,
}

// ---------------------------------------------------------------------------
// Backlog (FIFO — First In, First Out)
// ---------------------------------------------------------------------------

/// A backlog item — queued work for later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklogItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub priority: BacklogPriority,
    pub created_at: DateTime<Utc>,
    pub tags: Vec<String>,
    /// If this came from the focus stack (was popped and deferred).
    pub from_focus_stack: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BacklogPriority {
    Low,
    Medium,
    High,
    Urgent,
}

/// The Backlog — FIFO (First queued = first processed).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Backlog {
    items: Vec<BacklogItem>,
}

impl Backlog {
    pub fn new() -> Self { Self::default() }

    /// Add an item to the backlog.
    pub fn add(&mut self, title: String, description: String, priority: BacklogPriority) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.items.push(BacklogItem {
            id: id.clone(),
            title,
            description,
            priority,
            created_at: Utc::now(),
            tags: Vec::new(),
            from_focus_stack: false,
        });
        id
    }

    /// Add an item that was deferred from the focus stack.
    pub fn add_from_stack(&mut self, focus_item: FocusItem) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.items.push(BacklogItem {
            id: id.clone(),
            title: focus_item.title,
            description: focus_item.context,
            priority: BacklogPriority::Medium,
            created_at: Utc::now(),
            tags: focus_item.related,
            from_focus_stack: true,
        });
        id
    }

    /// Get the next item to work on (FIFO within priority).
    /// Urgent > High > Medium > Low, then by creation time.
    pub fn next(&self) -> Option<&BacklogItem> {
        let mut sorted: Vec<&BacklogItem> = self.items.iter().collect();
        sorted.sort_by(|a, b| {
            b.priority.cmp(&a.priority)
                .then(a.created_at.cmp(&b.created_at))
        });
        sorted.first().copied()
    }

    /// Remove and return the next item (dequeue).
    pub fn dequeue(&mut self) -> Option<BacklogItem> {
        if self.items.is_empty() { return None; }

        // Find highest priority, oldest item
        let mut best_idx = 0;
        for (i, item) in self.items.iter().enumerate() {
            if item.priority > self.items[best_idx].priority
                || (item.priority == self.items[best_idx].priority
                    && item.created_at < self.items[best_idx].created_at)
            {
                best_idx = i;
            }
        }

        Some(self.items.remove(best_idx))
    }

    /// Remove a specific item.
    pub fn remove(&mut self, id: &str) -> Option<BacklogItem> {
        if let Some(pos) = self.items.iter().position(|i| i.id == id) {
            Some(self.items.remove(pos))
        } else {
            None
        }
    }

    /// List all backlog items.
    pub fn items(&self) -> &[BacklogItem] {
        &self.items
    }

    pub fn len(&self) -> usize { self.items.len() }
    pub fn is_empty(&self) -> bool { self.items.is_empty() }

    /// Build context injection for the agent.
    pub fn context_injection(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }

        let mut context = String::from("\n## Backlog (queued items — handle after current work)\n\n");
        for (i, item) in self.items.iter().enumerate().take(10) {
            let priority = match item.priority {
                BacklogPriority::Urgent => "🔴",
                BacklogPriority::High => "🟠",
                BacklogPriority::Medium => "🟡",
                BacklogPriority::Low => "🟢",
            };
            context.push_str(&format!("{}. {priority} **{}**", i + 1, item.title));
            if !item.description.is_empty() {
                let desc = if item.description.len() > 80 {
                    format!("{}...", &item.description[..80])
                } else {
                    item.description.clone()
                };
                context.push_str(&format!(" — {desc}"));
            }
            context.push('\n');
        }

        if self.items.len() > 10 {
            context.push_str(&format!("\n... and {} more items\n", self.items.len() - 10));
        }

        context
    }
}

// ---------------------------------------------------------------------------
// Combined context manager
// ---------------------------------------------------------------------------

/// Manages both the Focus Stack and Backlog for a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationTracker {
    pub focus_stack: FocusStack,
    pub backlog: Backlog,
}

impl ConversationTracker {
    pub fn new() -> Self { Self::default() }

    /// Build combined context injection for the system prompt.
    pub fn context_injection(&self) -> String {
        let mut context = String::new();
        context.push_str(&self.focus_stack.context_injection());
        context.push_str(&self.backlog.context_injection());
        context
    }

    /// Move an item from focus stack to backlog (defer it).
    pub fn defer_to_backlog(&mut self, focus_id: &str) -> Option<String> {
        if let Some(item) = self.focus_stack.remove(focus_id) {
            let backlog_id = self.backlog.add_from_stack(item);
            Some(backlog_id)
        } else {
            None
        }
    }

    /// Promote a backlog item to the focus stack (start working on it).
    pub fn promote_from_backlog(&mut self, backlog_id: &str) -> Option<String> {
        if let Some(item) = self.backlog.remove(backlog_id) {
            let focus_id = self.focus_stack.push(
                item.title,
                item.description,
                PushReason::Explicit,
            );
            Some(focus_id)
        } else {
            None
        }
    }

    /// Summary for display.
    pub fn summary(&self) -> TrackerSummary {
        TrackerSummary {
            stack_depth: self.focus_stack.depth(),
            stack_items: self.focus_stack.summary(),
            backlog_count: self.backlog.len(),
            backlog_next: self.backlog.next().map(|i| i.title.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackerSummary {
    pub stack_depth: usize,
    pub stack_items: Vec<FocusStackEntry>,
    pub backlog_count: usize,
    pub backlog_next: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn progress_bar(progress: f64, width: usize) -> String {
    let filled = (progress * width as f64) as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}",
        "█".repeat(filled),
        "░".repeat(empty),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_stack_filo() {
        let mut stack = FocusStack::new();
        stack.push("First topic".into(), "context 1".into(), PushReason::Explicit);
        stack.push("Second topic".into(), "context 2".into(), PushReason::TopicDrift);
        stack.push("Third topic".into(), "context 3".into(), PushReason::SideInvestigation);

        assert_eq!(stack.depth(), 3);

        // FILO: last pushed = first popped
        let popped = stack.pop().unwrap();
        assert_eq!(popped.title, "Third topic");

        let popped = stack.pop().unwrap();
        assert_eq!(popped.title, "Second topic");

        let popped = stack.pop().unwrap();
        assert_eq!(popped.title, "First topic");

        assert!(stack.is_empty());
    }

    #[test]
    fn backlog_fifo_with_priority() {
        let mut backlog = Backlog::new();
        backlog.add("Low item".into(), "".into(), BacklogPriority::Low);
        backlog.add("High item".into(), "".into(), BacklogPriority::High);
        backlog.add("Medium item".into(), "".into(), BacklogPriority::Medium);

        // Dequeue by priority first
        let next = backlog.dequeue().unwrap();
        assert_eq!(next.title, "High item");

        let next = backlog.dequeue().unwrap();
        assert_eq!(next.title, "Medium item");

        let next = backlog.dequeue().unwrap();
        assert_eq!(next.title, "Low item");
    }

    #[test]
    fn defer_from_stack_to_backlog() {
        let mut tracker = ConversationTracker::new();
        let focus_id = tracker.focus_stack.push("Paused work".into(), "context".into(), PushReason::Explicit);

        assert_eq!(tracker.focus_stack.depth(), 1);
        assert_eq!(tracker.backlog.len(), 0);

        tracker.defer_to_backlog(&focus_id);

        assert_eq!(tracker.focus_stack.depth(), 0);
        assert_eq!(tracker.backlog.len(), 1);
        assert!(tracker.backlog.items()[0].from_focus_stack);
    }

    #[test]
    fn context_injection_includes_both() {
        let mut tracker = ConversationTracker::new();
        tracker.focus_stack.push("Stack item".into(), "".into(), PushReason::Explicit);
        tracker.backlog.add("Backlog item".into(), "".into(), BacklogPriority::Medium);

        let context = tracker.context_injection();
        assert!(context.contains("Focus Stack"));
        assert!(context.contains("Stack item"));
        assert!(context.contains("Backlog"));
        assert!(context.contains("Backlog item"));
    }
}
