use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use thiserror::Error;
use tracing::debug;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    Agent,
    User,
    Session,
}

impl MemorySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::User => "user",
            Self::Session => "session",
        }
    }
}

impl fmt::Display for MemorySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for MemorySource {
    type Err = MemoryStoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "agent" => Ok(Self::Agent),
            "user" => Ok(Self::User),
            "session" => Ok(Self::Session),
            other => Err(MemoryStoreError::InvalidSource(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: Uuid,
    pub workspace_id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source: MemorySource,
    pub importance: f32,
    pub created_at: DateTime<Utc>,
    pub accessed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMemory {
    pub id: Option<Uuid>,
    pub workspace_id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source: MemorySource,
    pub importance: Option<f32>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMemoriesParams {
    pub workspace_id: Option<String>,
    pub keyword: Option<String>,
    pub tags: Vec<String>,
    pub source: Option<MemorySource>,
    pub min_importance: Option<f32>,
    pub since: Option<DateTime<Utc>>,
    pub limit: usize,
}

impl Default for SearchMemoriesParams {
    fn default() -> Self {
        Self {
            workspace_id: None,
            keyword: None,
            tags: vec![],
            source: None,
            min_importance: None,
            since: None,
            limit: 25,
        }
    }
}

#[derive(Debug, Error)]
pub enum MemoryStoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("invalid memory source: {0}")]
    InvalidSource(String),

    #[error("memory not found: {0}")]
    NotFound(Uuid),
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    pool: PgPool,
}

impl MemoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn init_schema(&self) -> Result<(), MemoryStoreError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id UUID PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                content TEXT NOT NULL,
                tags TEXT[] NOT NULL DEFAULT '{}',
                source TEXT NOT NULL,
                importance DOUBLE PRECISION NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                accessed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_workspace_created ON memories (workspace_id, created_at DESC)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_importance ON memories (importance DESC, created_at DESC)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_tags_gin ON memories USING GIN (tags)",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn save_memory(&self, memory: NewMemory) -> Result<Memory, MemoryStoreError> {
        let id = memory.id.unwrap_or_else(Uuid::new_v4);
        let importance = memory.importance.unwrap_or(0.5).clamp(0.0, 1.0);
        let created_at = memory.created_at.unwrap_or_else(Utc::now);
        let accessed_at = created_at;

        sqlx::query(
            r#"
            INSERT INTO memories (id, workspace_id, content, tags, source, importance, created_at, accessed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(id)
        .bind(&memory.workspace_id)
        .bind(&memory.content)
        .bind(&memory.tags)
        .bind(memory.source.as_str())
        .bind(importance as f64)
        .bind(created_at)
        .bind(accessed_at)
        .execute(&self.pool)
        .await?;

        Ok(Memory {
            id,
            workspace_id: memory.workspace_id,
            content: memory.content,
            tags: memory.tags,
            source: memory.source,
            importance,
            created_at,
            accessed_at,
        })
    }

    pub async fn get_memory(&self, id: Uuid) -> Result<Option<Memory>, MemoryStoreError> {
        let row = sqlx::query(
            r#"
            SELECT id, workspace_id, content, tags, source, importance, created_at, accessed_at
            FROM memories
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let mut memory = match row {
            Some(row) => memory_from_row(&row)?,
            None => return Ok(None),
        };

        self.touch_memory(id).await?;
        memory.accessed_at = Utc::now();
        Ok(Some(memory))
    }

    pub async fn touch_memory(&self, id: Uuid) -> Result<(), MemoryStoreError> {
        let result = sqlx::query("UPDATE memories SET accessed_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(MemoryStoreError::NotFound(id));
        }

        Ok(())
    }

    pub async fn search_memories(
        &self,
        params: SearchMemoriesParams,
    ) -> Result<Vec<Memory>, MemoryStoreError> {
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT id, workspace_id, content, tags, source, importance, created_at, accessed_at FROM memories WHERE 1=1",
        );

        if let Some(workspace_id) = &params.workspace_id {
            query.push(" AND workspace_id = ").push_bind(workspace_id);
        }

        if let Some(keyword) = params
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            query
                .push(" AND content ILIKE ")
                .push_bind(format!("%{}%", keyword));
        }

        if !params.tags.is_empty() {
            query.push(" AND tags && ").push_bind(params.tags.clone());
        }

        if let Some(source) = params.source {
            query.push(" AND source = ").push_bind(source.as_str());
        }

        if let Some(min_importance) = params.min_importance {
            query
                .push(" AND importance >= ")
                .push_bind(min_importance as f64);
        }

        if let Some(since) = params.since {
            query.push(" AND created_at >= ").push_bind(since);
        }

        let limit = params.limit.clamp(1, 500) as i64;
        query
            .push(" ORDER BY importance DESC, accessed_at DESC, created_at DESC LIMIT ")
            .push_bind(limit);

        let rows = query.build().fetch_all(&self.pool).await?;
        let mut memories = Vec::with_capacity(rows.len());
        for row in rows {
            memories.push(memory_from_row(&row)?);
        }

        debug!(count = memories.len(), "memory search complete");
        Ok(memories)
    }

    pub async fn delete_memory(&self, id: Uuid) -> Result<bool, MemoryStoreError> {
        let result = sqlx::query("DELETE FROM memories WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_workspace_memories(
        &self,
        workspace_id: &str,
    ) -> Result<u64, MemoryStoreError> {
        let result = sqlx::query("DELETE FROM memories WHERE workspace_id = $1")
            .bind(workspace_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

fn memory_from_row(row: &sqlx::postgres::PgRow) -> Result<Memory, MemoryStoreError> {
    let source_raw: String = row.try_get("source")?;
    let source = MemorySource::from_str(&source_raw)?;

    Ok(Memory {
        id: row.try_get("id")?,
        workspace_id: row.try_get("workspace_id")?,
        content: row.try_get("content")?,
        tags: row.try_get("tags")?,
        source,
        importance: row.try_get::<f64, _>("importance")? as f32,
        created_at: row.try_get("created_at")?,
        accessed_at: row.try_get("accessed_at")?,
    })
}
