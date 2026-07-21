//! Structured view of the fleet project-management board.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::handlers::HandlerResult;

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardParams {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    DEFAULT_LIMIT
}

#[derive(Debug, FromRow, Serialize)]
struct BoardItem {
    id: Uuid,
    kind: String,
    title: String,
    status: String,
    assigned_computer: Option<String>,
    live_host: Option<String>,
    worktree_status: Option<String>,
    merge_queue_status: Option<String>,
    pr_url: Option<String>,
    repo_url: Option<String>,
    repo_path: Option<String>,
    is_parent: bool,
    created_at: DateTime<Utc>,
}

pub async fn pm_board(params: Option<Value>) -> HandlerResult {
    let limit = parse_limit(params)?;
    let pool = crate::pool::shared_pg_pool().await?;
    let status_counts = load_status_counts(&pool).await?;
    let active_leases = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM work_item_leases \
         WHERE released_at IS NULL AND lease_expires_at > NOW()",
    )
    .fetch_one(&pool)
    .await
    .map_err(|error| format!("failed to count active work-item leases: {error}"))?;
    let items = load_items(&pool, limit).await?;

    Ok(json!({
        "summary": {
            "total": status_counts.values().sum::<i64>(),
            "by_status": status_counts,
            "active_leases": active_leases,
        },
        "items": items,
    }))
}

fn parse_limit(params: Option<Value>) -> Result<i64, String> {
    let params: BoardParams = serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| format!("invalid pm_board parameters: {error}"))?;
    if !(1..=MAX_LIMIT).contains(&params.limit) {
        return Err(format!("limit must be between 1 and {MAX_LIMIT}"));
    }
    Ok(params.limit)
}

async fn load_status_counts(pool: &PgPool) -> Result<BTreeMap<String, i64>, String> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT status, COUNT(*) FROM work_items GROUP BY status ORDER BY status",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("failed to query work-item status totals: {error}"))?;
    Ok(rows.into_iter().collect())
}

async fn load_items(pool: &PgPool, limit: i64) -> Result<Vec<BoardItem>, String> {
    sqlx::query_as(
        "SELECT w.id, w.kind, w.title, w.status, w.assigned_computer, \
                lease.live_host, worktree.status AS worktree_status, \
                merge_queue.status AS merge_queue_status, w.pr_url, w.repo_url, \
                w.repo_path, EXISTS (SELECT 1 FROM work_items child WHERE child.parent_id = w.id) AS is_parent, \
                w.created_at \
           FROM work_items w \
           LEFT JOIN LATERAL ( \
               SELECT c.name AS live_host FROM work_item_leases l \
               LEFT JOIN computers c ON c.id = l.computer_id \
               WHERE l.work_item_id = w.id AND l.released_at IS NULL AND l.lease_expires_at > NOW() \
               ORDER BY l.created_at DESC LIMIT 1 \
           ) lease ON TRUE \
           LEFT JOIN LATERAL ( \
               SELECT wt.status FROM work_item_worktrees wt \
               WHERE wt.work_item_id = w.id AND wt.status <> 'cleaned' \
               ORDER BY wt.created_at DESC LIMIT 1 \
           ) worktree ON TRUE \
           LEFT JOIN LATERAL ( \
               SELECT mq.status FROM work_item_merge_queue mq \
               WHERE mq.work_item_id = w.id ORDER BY mq.enqueued_at DESC LIMIT 1 \
           ) merge_queue ON TRUE \
          WHERE w.status NOT IN ('idea', 'cancelled') OR w.pr_url IS NOT NULL \
          ORDER BY w.created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("failed to query PM board: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_defaults_and_validates_bounds() {
        assert_eq!(parse_limit(None).unwrap(), DEFAULT_LIMIT);
        assert_eq!(parse_limit(Some(json!({ "limit": 1 }))).unwrap(), 1);
        assert_eq!(parse_limit(Some(json!({ "limit": 500 }))).unwrap(), 500);
        assert!(parse_limit(Some(json!({ "limit": 0 }))).is_err());
        assert!(parse_limit(Some(json!({ "limit": 501 }))).is_err());
    }

    #[test]
    fn rejects_unknown_parameters() {
        assert!(parse_limit(Some(json!({ "project": "unexpected" }))).is_err());
    }
}
