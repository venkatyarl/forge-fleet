//! Work Item Batch Manager (Phase 15c)
//!
//! Handles weighted partitioning of tasks into batches, work item creation,
//! claiming, yielding, and progress tracking.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use tracing::{debug, info};
use uuid::Uuid;

// ─── Data Types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: Uuid,
    pub parent_task_id: Uuid,
    pub batch_id: i32,
    pub item_index: i32,
    pub item_key: String,
    pub item_type: String,
    pub estimated_weight: f64,
    pub status: String,
    pub assigned_node_id: Option<Uuid>,
    pub checkpoint_data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkBatch {
    pub id: Uuid,
    pub parent_task_id: Uuid,
    pub batch_index: i32,
    pub total_estimated_weight: f64,
    pub items_count: i32,
    pub status: String,
    pub assigned_node_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct ItemWeight {
    pub base: f64,
    pub pages: f64,
    pub words: f64,
    pub has_images: f64,
    pub has_code: f64,
}

impl Default for ItemWeight {
    fn default() -> Self {
        Self {
            base: 1.0,
            pages: 0.0,
            words: 0.0,
            has_images: 0.0,
            has_code: 0.0,
        }
    }
}

impl ItemWeight {
    pub fn total(&self) -> f64 {
        self.base + self.pages + self.words + self.has_images + self.has_code
    }
}

// ─── Weighted Partitioning ──────────────────────────────────────────────────

/// Partition items into `num_batches` using greedy bin-packing.
/// Sorts by weight descending, then assigns each item to the batch
/// with the lowest current weight.
pub fn weighted_partition(items: Vec<(String, ItemWeight)>, num_batches: usize) -> Vec<Vec<(String, ItemWeight)>> {
    if num_batches == 0 || items.is_empty() {
        return vec![];
    }

    let mut items = items;
    // Sort by weight descending (heaviest first — critical for bin-packing quality)
    items.sort_by(|a, b| b.1.total().partial_cmp(&a.1.total()).unwrap());

    let mut batches: Vec<Vec<(String, ItemWeight)>> = (0..num_batches).map(|_| Vec::new()).collect();
    let mut batch_weights: Vec<f64> = vec![0.0; num_batches];

    for (key, weight) in items {
        // Assign to batch with lowest current weight
        let min_idx = batch_weights
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(idx, _)| idx)
            .unwrap_or(0);

        batch_weights[min_idx] += weight.total();
        batches[min_idx].push((key, weight));
    }

    debug!(
        batches = num_batches,
        weights = ?batch_weights,
        "weighted partition complete"
    );

    batches
}

// ─── Database Operations ────────────────────────────────────────────────────

/// Create fleet_work_items and fleet_work_batches for a decomposed task.
pub async fn create_work_items(
    pg: &PgPool,
    parent_task_id: Uuid,
    items: &[(String, String, ItemWeight)], // (key, type, weight)
    num_batches: usize,
) -> Result<Vec<WorkBatch>> {
    let num_batches = num_batches.max(1);

    // Convert to weighted partition format
    let weighted: Vec<(String, ItemWeight)> = items
        .iter()
        .map(|(key, _, weight)| (key.clone(), weight.clone()))
        .collect();

    let batches = weighted_partition(weighted, num_batches);

    let mut created_batches = Vec::new();

    for (batch_idx, batch_items) in batches.iter().enumerate() {
        if batch_items.is_empty() {
            continue;
        }

        let total_weight: f64 = batch_items.iter().map(|(_, w)| w.total()).sum();

        // Create batch row
        let batch_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO fleet_work_batches (
                parent_task_id, batch_index, total_estimated_weight, items_count, status
            )
            VALUES ($1, $2, $3, $4, 'pending')
            RETURNING id
            "#,
        )
        .bind(parent_task_id)
        .bind(batch_idx as i32)
        .bind(total_weight)
        .bind(batch_items.len() as i32)
        .fetch_one(pg)
        .await?;

        // Create work items for this batch
        for (item_idx, (key, weight)) in batch_items.iter().enumerate() {
            let item_type = items
                .iter()
                .find(|(k, _, _)| k == key)
                .map(|(_, t, _)| t.as_str())
                .unwrap_or("document");

            sqlx::query(
                r#"
                INSERT INTO fleet_work_items (
                    parent_task_id, batch_id, item_index, item_key, item_type,
                    estimated_weight, complexity_factors, status
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending')
                "#,
            )
            .bind(parent_task_id)
            .bind(batch_idx as i32)
            .bind(item_idx as i32)
            .bind(key)
            .bind(item_type)
            .bind(weight.total())
            .bind(json!({
                "base": weight.base,
                "pages": weight.pages,
                "words": weight.words,
                "has_images": weight.has_images > 0.0,
                "has_code": weight.has_code > 0.0,
            }))
            .execute(pg)
            .await?;
        }

        created_batches.push(WorkBatch {
            id: batch_id,
            parent_task_id,
            batch_index: batch_idx as i32,
            total_estimated_weight: total_weight,
            items_count: batch_items.len() as i32,
            status: "pending".to_string(),
            assigned_node_id: None,
        });
    }

    info!(
        parent_task_id = %parent_task_id,
        batches = created_batches.len(),
        total_items = items.len(),
        "work items created"
    );

    Ok(created_batches)
}

/// Atomically claim an unclaimed work item for this node.
pub async fn claim_work_item(
    pg: &PgPool,
    computer_id: Uuid,
    agent_id: Option<&str>,
) -> Result<Option<WorkItem>> {
    let row = sqlx::query(
        r#"
        UPDATE fleet_work_items
           SET status = 'claimed',
               assigned_node_id = $1,
               assigned_agent_id = $2,
               claimed_at = NOW()
         WHERE id = (
            SELECT id FROM fleet_work_items
             WHERE status = 'pending'
               AND (assigned_node_id IS NULL OR assigned_node_id = $1)
             ORDER BY estimated_weight DESC, item_index ASC
               FOR UPDATE SKIP LOCKED
             LIMIT 1
         )
        RETURNING id, parent_task_id, batch_id, item_index, item_key, item_type,
                  estimated_weight, status, assigned_node_id, checkpoint_data
        "#,
    )
    .bind(computer_id)
    .bind(agent_id)
    .fetch_optional(pg)
    .await?;

    Ok(row.map(|r| WorkItem {
        id: r.get("id"),
        parent_task_id: r.get("parent_task_id"),
        batch_id: r.get("batch_id"),
        item_index: r.get("item_index"),
        item_key: r.get("item_key"),
        item_type: r.get("item_type"),
        estimated_weight: r.get("estimated_weight"),
        status: r.get("status"),
        assigned_node_id: r.get("assigned_node_id"),
        checkpoint_data: r.get("checkpoint_data"),
    }))
}

/// Yield a work item back to pending so another node can claim it.
pub async fn yield_work_item(
    pg: &PgPool,
    item_id: Uuid,
    checkpoint: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE fleet_work_items
           SET status = 'pending',
               checkpoint_data = $1,
               yielded_at = NOW(),
               assigned_node_id = NULL,
               assigned_agent_id = NULL,
               assigned_session_id = NULL
         WHERE id = $2
           AND status = 'claimed'
        "#,
    )
    .bind(checkpoint)
    .bind(item_id)
    .execute(pg)
    .await?;

    info!(item_id = %item_id, "work item yielded");
    Ok(())
}

/// Resume a yielded work item from checkpoint.
pub async fn resume_work_item(
    pg: &PgPool,
    item_id: Uuid,
    computer_id: Uuid,
    agent_id: Option<&str>,
) -> Result<Option<WorkItem>> {
    let row = sqlx::query(
        r#"
        UPDATE fleet_work_items
           SET status = 'claimed',
               assigned_node_id = $1,
               assigned_agent_id = $2,
               claimed_at = NOW(),
               stolen_from = assigned_node_id
         WHERE id = $3
           AND status = 'pending'
        RETURNING id, parent_task_id, batch_id, item_index, item_key, item_type,
                  estimated_weight, status, assigned_node_id, checkpoint_data
        "#,
    )
    .bind(computer_id)
    .bind(agent_id)
    .bind(item_id)
    .fetch_optional(pg)
    .await?;

    Ok(row.map(|r| WorkItem {
        id: r.get("id"),
        parent_task_id: r.get("parent_task_id"),
        batch_id: r.get("batch_id"),
        item_index: r.get("item_index"),
        item_key: r.get("item_key"),
        item_type: r.get("item_type"),
        estimated_weight: r.get("estimated_weight"),
        status: r.get("status"),
        assigned_node_id: r.get("assigned_node_id"),
        checkpoint_data: r.get("checkpoint_data"),
    }))
}

/// Mark a work item as completed.
pub async fn complete_work_item(
    pg: &PgPool,
    item_id: Uuid,
    result_summary: &str,
    tokens_in: i32,
    tokens_out: i32,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE fleet_work_items
           SET status = 'completed',
               result_summary = $1,
               result_tokens_in = $2,
               result_tokens_out = $3,
               completed_at = NOW()
         WHERE id = $4
        "#,
    )
    .bind(result_summary)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(item_id)
    .execute(pg)
    .await?;

    Ok(())
}

/// Mark a work item as failed.
pub async fn fail_work_item(
    pg: &PgPool,
    item_id: Uuid,
    error: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE fleet_work_items
           SET status = 'failed',
               error_message = $1,
               completed_at = NOW(),
               retry_count = retry_count + 1
         WHERE id = $2
        "#,
    )
    .bind(error)
    .bind(item_id)
    .execute(pg)
    .await?;

    Ok(())
}

/// Update batch progress based on its work items.
pub async fn update_batch_progress(pg: &PgPool, parent_task_id: Uuid, batch_id: i32) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE fleet_work_batches
           SET progress_percent = (
               SELECT COALESCE(AVG(
                   CASE status
                       WHEN 'completed' THEN 100
                       WHEN 'failed' THEN 100
                       WHEN 'yielded' THEN 0
                       ELSE progress_percent
                   END
               ), 0)::INT
               FROM fleet_work_items
               WHERE parent_task_id = $1 AND batch_id = $2
           ),
           status = CASE
               WHEN (
                   SELECT COUNT(*) FROM fleet_work_items
                   WHERE parent_task_id = $1 AND batch_id = $2 AND status = 'completed'
               ) = items_count THEN 'completed'
               WHEN (
                   SELECT COUNT(*) FROM fleet_work_items
                   WHERE parent_task_id = $1 AND batch_id = $2 AND status IN ('claimed', 'in_progress')
               ) > 0 THEN 'in_progress'
               ELSE 'pending'
           END
         WHERE parent_task_id = $1 AND batch_index = $2
        "#,
    )
    .bind(parent_task_id)
    .bind(batch_id)
    .execute(pg)
    .await?;

    Ok(())
}

/// Get overall progress for a parent task.
pub async fn get_task_progress(pg: &PgPool, parent_task_id: Uuid) -> Result<TaskProgress> {
    let row = sqlx::query(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE status = 'pending') as pending,
            COUNT(*) FILTER (WHERE status = 'claimed') as claimed,
            COUNT(*) FILTER (WHERE status = 'in_progress') as in_progress,
            COUNT(*) FILTER (WHERE status = 'completed') as completed,
            COUNT(*) FILTER (WHERE status = 'failed') as failed,
            COUNT(*) FILTER (WHERE status = 'yielded') as yielded,
            COUNT(*) as total
        FROM fleet_work_items
        WHERE parent_task_id = $1
        "#,
    )
    .bind(parent_task_id)
    .fetch_one(pg)
    .await?;

    Ok(TaskProgress {
        pending: row.get("pending"),
        claimed: row.get("claimed"),
        in_progress: row.get("in_progress"),
        completed: row.get("completed"),
        failed: row.get("failed"),
        yielded: row.get("yielded"),
        total: row.get("total"),
    })
}

#[derive(Debug, Clone)]
pub struct TaskProgress {
    pub pending: i64,
    pub claimed: i64,
    pub in_progress: i64,
    pub completed: i64,
    pub failed: i64,
    pub yielded: i64,
    pub total: i64,
}

impl TaskProgress {
    pub fn percent_complete(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.completed as f64 / self.total as f64) * 100.0
    }

    pub fn is_done(&self) -> bool {
        self.total > 0 && self.completed + self.failed >= self.total
    }
}

// ─── Parent Task Completion Watcher ─────────────────────────────────────────

/// Check all `running` decomposed tasks and mark them complete when
/// their fleet_work_items are all done (completed or failed).
/// Also bumps `last_heartbeat_at` on all running decomposed tasks so
/// the fleet_tasks watchdog doesn't re-queue them while work is in
/// progress.
pub async fn complete_finished_parents(pg: &PgPool) -> Result<usize> {
    // 1. Bump heartbeat on ALL running decomposed tasks (prevents
    //    the 120s watchdog from handing them off mid-flight).
    let _ = sqlx::query(
        r#"
        UPDATE fleet_tasks
           SET last_heartbeat_at = NOW()
         WHERE task_type = 'decomposed'
           AND status = 'running'
        "#,
    )
    .execute(pg)
    .await;

    // 2. Find parents whose fleet_work_items are all done.
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        r#"
        SELECT t.id
          FROM fleet_tasks t
         WHERE t.task_type = 'decomposed'
           AND t.status = 'running'
           AND EXISTS (
               SELECT 1 FROM fleet_work_items w
                WHERE w.parent_task_id = t.id
           )
           AND NOT EXISTS (
               SELECT 1 FROM fleet_work_items w
                WHERE w.parent_task_id = t.id
                  AND w.status NOT IN ('completed', 'failed')
           )
        "#,
    )
    .fetch_all(pg)
    .await?;

    for id in &ids {
        let progress = get_task_progress(pg, *id).await?;
        let result = serde_json::json!({
            "percent": progress.percent_complete(),
            "completed": progress.completed,
            "failed": progress.failed,
            "total": progress.total,
        });

        sqlx::query(
            r#"
            UPDATE fleet_tasks
               SET status = 'completed',
                   completed_at = NOW(),
                   progress_pct = 100.0,
                   progress_message = 'all work items finished',
                   result = $1
             WHERE id = $2 AND status = 'running'
            "#,
        )
        .bind(&result)
        .bind(id)
        .execute(pg)
        .await?;

        info!(task_id = %id, "decomposed parent task completed");
    }

    Ok(ids.len())
}

/// Spawn the parent completion watcher as a background tick.
pub fn spawn_completion_watcher(
    pg: PgPool,
    interval_secs: u64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(interval_secs.max(5));
        let mut ticker = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
            match complete_finished_parents(&pg).await {
                Ok(n) if n > 0 => info!(completed = n, "parent completion watcher"),
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "completion watcher failed"),
            }
        }
    })
}

// ─── Enqueue Helpers ────────────────────────────────────────────────────────

/// Enqueue a decomposed task whose payload contains a list of work items.
/// The task runner will claim it, call `create_work_items()`, and then
/// the work-item processor + completion watcher handle the rest.
pub async fn pg_enqueue_decomposed_task(
    pg: &PgPool,
    summary: &str,
    items: &[(String, String, f64)], // (key, item_type, base_weight)
    num_batches: usize,
    capabilities: &[String],
    preferred_computer: Option<&str>,
    priority: i32,
    created_by_computer_id: Option<uuid::Uuid>,
    routing_mode: &str,
) -> Result<uuid::Uuid, sqlx::Error> {
    let preferred_id: Option<uuid::Uuid> = if let Some(name) = preferred_computer {
        sqlx::query_scalar("SELECT id FROM computers WHERE name = $1")
            .bind(name)
            .fetch_optional(pg)
            .await?
    } else {
        None
    };

    let items_json: Vec<Value> = items
        .iter()
        .map(|(key, item_type, weight)| {
            serde_json::json!({
                "key": key,
                "item_type": item_type,
                "base_weight": weight,
            })
        })
        .collect();

    let payload = serde_json::json!({
        "items": items_json,
        "num_batches": num_batches,
    });

    let caps = serde_json::Value::Array(
        capabilities.iter().map(|c| Value::String(c.clone())).collect(),
    );

    let id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO fleet_tasks (
            task_type, summary, payload, priority,
            requires_capability, preferred_computer_id,
            created_by_computer_id, routing_mode
        )
        VALUES ('decomposed', $1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(summary)
    .bind(&payload)
    .bind(priority)
    .bind(&caps)
    .bind(preferred_id)
    .bind(created_by_computer_id)
    .bind(routing_mode)
    .fetch_one(pg)
    .await?;

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_item_weight_total() {
        let w = ItemWeight {
            base: 1.0,
            pages: 2.0,
            words: 3.0,
            has_images: 0.5,
            has_code: 0.5,
        };
        assert_eq!(w.total(), 7.0);
    }

    #[test]
    fn test_item_weight_default() {
        let w = ItemWeight::default();
        assert_eq!(w.total(), 1.0);
    }

    #[test]
    fn test_weighted_partition_basic() {
        let items = vec![
            ("a".to_string(), ItemWeight { base: 10.0, ..Default::default() }),
            ("b".to_string(), ItemWeight { base: 5.0, ..Default::default() }),
            ("c".to_string(), ItemWeight { base: 3.0, ..Default::default() }),
            ("d".to_string(), ItemWeight { base: 2.0, ..Default::default() }),
        ];
        let batches = weighted_partition(items, 2);
        assert_eq!(batches.len(), 2);
        // Heaviest first (10) goes to batch 0. Next heaviest (5) goes to batch 1.
        // Then 3 goes to batch 1 (5+3=8 < 10). Then 2 goes to batch 1 (8+2=10 == 10).
        // Batch 0: [10], Batch 1: [5, 3, 2]
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 3);
    }

    #[test]
    fn test_weighted_partition_empty() {
        let batches = weighted_partition(vec![], 2);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_weighted_partition_zero_batches() {
        let items = vec![
            ("a".to_string(), ItemWeight::default()),
        ];
        let batches = weighted_partition(items, 0);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_task_progress_percent() {
        let p = TaskProgress {
            pending: 0,
            claimed: 0,
            in_progress: 0,
            completed: 3,
            failed: 1,
            yielded: 0,
            total: 4,
        };
        assert_eq!(p.percent_complete(), 75.0);
        assert!(p.is_done());
    }

    #[test]
    fn test_task_progress_empty() {
        let p = TaskProgress {
            pending: 0, claimed: 0, in_progress: 0,
            completed: 0, failed: 0, yielded: 0, total: 0,
        };
        assert_eq!(p.percent_complete(), 0.0);
        assert!(!p.is_done());
    }
}
