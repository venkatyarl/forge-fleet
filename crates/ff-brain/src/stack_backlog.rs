//! Redis-backed stacks (LIFO per thread) and backlogs (priority FIFO per project).
//!
//! Live state in Redis; completed/overflow items archive to Postgres.
//! Pub/sub on `brain:events` for cross-device live updates.

use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

const STACK_CAP: usize = 50;
const BACKLOG_CAP: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackItem {
    pub id: String,
    pub title: String,
    pub context: Option<String>,
    pub push_reason: Option<String>,
    pub progress: f32,
    pub pushed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklogItem {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: String,
    pub tags: Vec<String>,
    pub from_thread_id: Option<String>,
    pub created_at: String,
}

fn priority_bucket(priority: &str) -> f64 {
    match priority {
        "urgent" => 0.0,
        "high" => 1.0,
        "medium" => 2.0,
        "low" => 3.0,
        _ => 2.0,
    }
}

fn backlog_score(priority: &str, created_at_ms: f64) -> f64 {
    priority_bucket(priority) * 1e12 + created_at_ms
}

pub struct BrainStateClient {
    redis: redis::aio::ConnectionManager,
    pool: PgPool,
}

impl BrainStateClient {
    pub async fn new(redis_url: &str, pool: PgPool) -> Result<Self, String> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| format!("redis client: {e}"))?;
        let conn = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|e| format!("redis connect: {e}"))?;
        Ok(Self { redis: conn, pool })
    }

    // ─── Stack operations (LIFO per thread) ──────────────────────────

    fn stack_key(&self, user_id: &Uuid, thread_id: &Uuid) -> String {
        format!("brain:stack:{}:{}", user_id, thread_id)
    }

    pub async fn stack_push(
        &mut self,
        user_id: &Uuid,
        thread_id: &Uuid,
        item: &StackItem,
    ) -> Result<usize, String> {
        let key = self.stack_key(user_id, thread_id);
        let json = serde_json::to_string(item).map_err(|e| e.to_string())?;
        let _: () = self.redis.lpush(&key, &json).await.map_err(|e| format!("lpush: {e}"))?;
        let len: usize = self.redis.llen(&key).await.map_err(|e| format!("llen: {e}"))?;

        // Cap enforcement: overflow tail items to Postgres archive.
        if len > STACK_CAP {
            let overflow: Vec<String> = self.redis.lrange(&key, STACK_CAP as isize, -1)
                .await.map_err(|e| format!("lrange overflow: {e}"))?;
            let _: () = self.redis.ltrim(&key, 0, (STACK_CAP - 1) as isize)
                .await.map_err(|e| format!("ltrim: {e}"))?;
            for raw in &overflow {
                if let Ok(si) = serde_json::from_str::<StackItem>(raw) {
                    let _ = self.archive_stack_item(user_id, Some(thread_id), &si).await;
                }
            }
        }

        // Publish event for cross-device sync.
        let event = serde_json::json!({
            "kind": "stack_push",
            "user": user_id.to_string(),
            "thread": thread_id.to_string(),
            "item": item,
        });
        let _: () = self.redis.publish("brain:events", event.to_string())
            .await.map_err(|e| format!("publish: {e}"))?;

        Ok(len.min(STACK_CAP))
    }

    pub async fn stack_pop(
        &mut self,
        user_id: &Uuid,
        thread_id: &Uuid,
    ) -> Result<Option<StackItem>, String> {
        let key = self.stack_key(user_id, thread_id);
        let raw: Option<String> = self.redis.lpop(&key, None).await.map_err(|e| format!("lpop: {e}"))?;
        match raw {
            Some(json) => {
                let item: StackItem = serde_json::from_str(&json).map_err(|e| e.to_string())?;
                // Archive popped item with popped_at set.
                let _ = self.archive_stack_item_popped(user_id, Some(thread_id), &item).await;
                let event = serde_json::json!({
                    "kind": "stack_pop",
                    "user": user_id.to_string(),
                    "thread": thread_id.to_string(),
                    "item": item,
                });
                let _: () = self.redis.publish("brain:events", event.to_string())
                    .await.map_err(|e| format!("publish: {e}"))?;
                Ok(Some(item))
            }
            None => Ok(None),
        }
    }

    pub async fn stack_list(
        &mut self,
        user_id: &Uuid,
        thread_id: &Uuid,
        limit: usize,
    ) -> Result<Vec<StackItem>, String> {
        let key = self.stack_key(user_id, thread_id);
        let raw: Vec<String> = self.redis.lrange(&key, 0, (limit as isize) - 1)
            .await.map_err(|e| format!("lrange: {e}"))?;
        Ok(raw.iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect())
    }

    async fn archive_stack_item(&self, user_id: &Uuid, thread_id: Option<&Uuid>, item: &StackItem) -> Result<(), String> {
        let id = Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::new_v4());
        sqlx::query(
            "INSERT INTO brain_stack_archive (id, user_id, thread_id, title, context, push_reason, pushed_at, archived_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, NOW())
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id).bind(user_id).bind(thread_id)
        .bind(&item.title).bind(&item.context).bind(&item.push_reason)
        .bind(&item.pushed_at)
        .execute(&self.pool).await.map_err(|e| format!("archive: {e}"))?;
        Ok(())
    }

    async fn archive_stack_item_popped(&self, user_id: &Uuid, thread_id: Option<&Uuid>, item: &StackItem) -> Result<(), String> {
        let id = Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::new_v4());
        sqlx::query(
            "INSERT INTO brain_stack_archive (id, user_id, thread_id, title, context, push_reason, pushed_at, popped_at, archived_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, NOW(), NOW())
             ON CONFLICT (id) DO UPDATE SET popped_at = NOW()",
        )
        .bind(id).bind(user_id).bind(thread_id)
        .bind(&item.title).bind(&item.context).bind(&item.push_reason)
        .bind(&item.pushed_at)
        .execute(&self.pool).await.map_err(|e| format!("archive popped: {e}"))?;
        Ok(())
    }

    // ─── Backlog operations (priority FIFO per project) ──────────────

    fn backlog_key(&self, user_id: &Uuid, project: &str) -> String {
        format!("brain:backlog:{}:{}", user_id, project)
    }

    pub async fn backlog_add(
        &mut self,
        user_id: &Uuid,
        project: &str,
        item: &BacklogItem,
    ) -> Result<usize, String> {
        let key = self.backlog_key(user_id, project);
        let json = serde_json::to_string(item).map_err(|e| e.to_string())?;
        let now_ms = chrono::Utc::now().timestamp_millis() as f64;
        let score = backlog_score(&item.priority, now_ms);
        let _: () = self.redis.zadd(&key, &json, score)
            .await.map_err(|e| format!("zadd: {e}"))?;

        // Cap: archive oldest low-priority items if over cap.
        let count: usize = self.redis.zcard(&key).await.map_err(|e| format!("zcard: {e}"))?;
        if count > BACKLOG_CAP {
            let overflow: Vec<String> = self.redis.zrange(&key, BACKLOG_CAP as isize, -1)
                .await.map_err(|e| format!("zrange overflow: {e}"))?;
            let _: () = self.redis.zremrangebyrank(&key, BACKLOG_CAP as isize, -1)
                .await.map_err(|e| format!("zremrangebyrank: {e}"))?;
            for raw in &overflow {
                if let Ok(bi) = serde_json::from_str::<BacklogItem>(raw) {
                    let _ = self.archive_backlog_completed(user_id, project, &bi, "overflow").await;
                }
            }
        }

        let event = serde_json::json!({
            "kind": "backlog_add",
            "user": user_id.to_string(),
            "project": project,
            "item": item,
        });
        let _: () = self.redis.publish("brain:events", event.to_string())
            .await.map_err(|e| format!("publish: {e}"))?;

        Ok(count.min(BACKLOG_CAP))
    }

    pub async fn backlog_list(
        &mut self,
        user_id: &Uuid,
        project: &str,
        limit: usize,
    ) -> Result<Vec<BacklogItem>, String> {
        let key = self.backlog_key(user_id, project);
        let raw: Vec<String> = self.redis.zrange(&key, 0, (limit as isize) - 1)
            .await.map_err(|e| format!("zrange: {e}"))?;
        Ok(raw.iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect())
    }

    pub async fn backlog_complete(
        &mut self,
        user_id: &Uuid,
        project: &str,
        item_id: &str,
    ) -> Result<bool, String> {
        let key = self.backlog_key(user_id, project);
        // Scan all members to find the one matching item_id.
        let all: Vec<String> = self.redis.zrange(&key, 0, -1)
            .await.map_err(|e| format!("zrange: {e}"))?;
        for raw in &all {
            if let Ok(bi) = serde_json::from_str::<BacklogItem>(raw) {
                if bi.id == item_id {
                    let _: () = self.redis.zrem(&key, raw)
                        .await.map_err(|e| format!("zrem: {e}"))?;
                    let _ = self.archive_backlog_completed(user_id, project, &bi, "completed").await;
                    let event = serde_json::json!({
                        "kind": "backlog_done",
                        "user": user_id.to_string(),
                        "project": project,
                        "item_id": item_id,
                    });
                    let _: () = self.redis.publish("brain:events", event.to_string())
                        .await.map_err(|e| format!("publish: {e}"))?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn archive_backlog_completed(
        &self,
        user_id: &Uuid,
        project: &str,
        item: &BacklogItem,
        channel: &str,
    ) -> Result<(), String> {
        let id = Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::new_v4());
        sqlx::query(
            "INSERT INTO brain_backlog_archive (id, user_id, project, title, priority, completed_at, completed_by_channel, tags)
             VALUES ($1, $2, $3, $4, $5, NOW(), $6, $7)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id).bind(user_id).bind(project)
        .bind(&item.title).bind(&item.priority).bind(channel)
        .bind(&item.tags)
        .execute(&self.pool).await.map_err(|e| format!("archive backlog: {e}"))?;
        Ok(())
    }
}
