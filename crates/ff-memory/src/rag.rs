use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use tokio::fs;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Text,
    Markdown,
    Pdf,
    Unknown,
}

impl DocumentKind {
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("txt") => Self::Text,
            Some("md") | Some("markdown") => Self::Markdown,
            Some("pdf") => Self::Pdf,
            Some(_) => Self::Text,
            None => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Pdf => "pdf",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagChunk {
    pub id: Uuid,
    pub workspace_id: String,
    pub document_id: Uuid,
    pub source_path: String,
    pub chunk_index: i32,
    pub content: String,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestReport {
    pub document_id: Uuid,
    pub workspace_id: String,
    pub source_path: String,
    pub kind: DocumentKind,
    pub bytes_read: usize,
    pub chunks_created: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagQuery {
    pub query: String,
    pub workspace_id: Option<String>,
    pub limit: usize,
}

impl Default for RagQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            workspace_id: None,
            limit: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagResult {
    pub chunk: RagChunk,
    pub score: f32,
    pub matched_terms: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RagEngine {
    pool: PgPool,
    pub chunk_size: usize,
    pub overlap: usize,
}

impl RagEngine {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            chunk_size: 900,
            overlap: 120,
        }
    }

    pub fn with_chunking(mut self, chunk_size: usize, overlap: usize) -> Self {
        self.chunk_size = chunk_size.max(200);
        self.overlap = overlap.min(self.chunk_size.saturating_sub(1));
        self
    }

    pub async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS rag_chunks (
                id UUID PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                document_id UUID NOT NULL,
                source_path TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                content TEXT NOT NULL,
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("create rag_chunks table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_rag_workspace_doc ON rag_chunks (workspace_id, document_id, chunk_index)",
        )
        .execute(&self.pool)
        .await
        .context("create rag workspace/doc index")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_rag_workspace_created ON rag_chunks (workspace_id, created_at DESC)",
        )
        .execute(&self.pool)
        .await
        .context("create rag workspace/created index")?;

        Ok(())
    }

    pub async fn ingest_document_path(
        &self,
        workspace_id: &str,
        path: impl AsRef<Path>,
    ) -> Result<IngestReport> {
        let path = path.as_ref();
        let kind = DocumentKind::from_path(path);
        let canonical = canonicalize_or_self(path);
        let source_path = canonical.display().to_string();

        let raw_text = match kind {
            DocumentKind::Pdf => read_pdf_text(path).await?,
            _ => read_text_file(path).await?,
        };

        let normalized = normalize_whitespace(&raw_text);
        let chunks = chunk_text(&normalized, self.chunk_size, self.overlap);
        let document_id = Uuid::new_v4();

        let mut tx = self
            .pool
            .begin()
            .await
            .context("start rag ingest transaction")?;
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            let metadata = serde_json::json!({
                "kind": kind.as_str(),
                "chunk_size": self.chunk_size,
                "overlap": self.overlap,
            })
            .to_string();

            sqlx::query(
                r#"
                INSERT INTO rag_chunks (id, workspace_id, document_id, source_path, chunk_index, content, metadata, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
                "#,
            )
            .bind(Uuid::new_v4())
            .bind(workspace_id)
            .bind(document_id)
            .bind(&source_path)
            .bind(chunk_index as i32)
            .bind(chunk)
            .bind(metadata)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("insert rag chunk {chunk_index} for {source_path}"))?;
        }
        tx.commit().await.context("commit rag ingest transaction")?;

        Ok(IngestReport {
            document_id,
            workspace_id: workspace_id.to_string(),
            source_path,
            kind,
            bytes_read: raw_text.len(),
            chunks_created: chunks.len(),
        })
    }

    pub async fn retrieve(&self, query: RagQuery) -> Result<Vec<RagResult>> {
        let terms = tokenize(&query.query);
        let mut sql = QueryBuilder::<Postgres>::new(
            "SELECT id, workspace_id, document_id, source_path, chunk_index, content, metadata, created_at FROM rag_chunks WHERE 1=1",
        );

        if let Some(workspace_id) = &query.workspace_id {
            sql.push(" AND workspace_id = ").push_bind(workspace_id);
        }

        if !terms.is_empty() {
            sql.push(" AND (");
            for (idx, term) in terms.iter().enumerate() {
                if idx > 0 {
                    sql.push(" OR ");
                }
                sql.push("content ILIKE ").push_bind(format!("%{}%", term));
            }
            sql.push(")");
        }

        let candidate_limit = query.limit.clamp(1, 100) * 5;
        sql.push(" ORDER BY created_at DESC LIMIT ")
            .push_bind(candidate_limit as i64);

        let rows = sql
            .build()
            .fetch_all(&self.pool)
            .await
            .context("fetch rag chunks")?;

        let mut results = rows
            .into_iter()
            .map(rag_chunk_from_row)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|chunk| score_chunk(chunk, &terms))
            .collect::<Vec<_>>();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(query.limit.clamp(1, 100));

        Ok(results)
    }

    pub async fn retrieve_context(&self, query: RagQuery) -> Result<String> {
        let hits = self.retrieve(query).await?;
        let mut output = String::new();

        for (idx, hit) in hits.iter().enumerate() {
            output.push_str(&format!(
                "{}. [score {:.2}] {}#{}\n{}\n\n",
                idx + 1,
                hit.score,
                hit.chunk.source_path,
                hit.chunk.chunk_index,
                hit.chunk.content
            ));
        }

        Ok(output)
    }
}

fn rag_chunk_from_row(row: sqlx::postgres::PgRow) -> Result<RagChunk> {
    let metadata_str: String = row.try_get("metadata")?;
    let metadata = serde_json::from_str(&metadata_str).unwrap_or_else(|_| serde_json::json!({}));

    Ok(RagChunk {
        id: row.try_get("id")?,
        workspace_id: row.try_get("workspace_id")?,
        document_id: row.try_get("document_id")?,
        source_path: row.try_get("source_path")?,
        chunk_index: row.try_get("chunk_index")?,
        content: row.try_get("content")?,
        metadata,
        created_at: row.try_get("created_at")?,
    })
}

fn score_chunk(chunk: RagChunk, terms: &[String]) -> RagResult {
    let content_lower = chunk.content.to_ascii_lowercase();
    let mut matched = Vec::new();

    for term in terms {
        if content_lower.contains(term) {
            matched.push(term.clone());
        }
    }

    let match_ratio = if terms.is_empty() {
        0.5
    } else {
        matched.len() as f32 / terms.len() as f32
    };

    let age_days = (Utc::now() - chunk.created_at).num_days().max(0) as f32;
    let recency = (1.0 / (1.0 + age_days / 14.0)).clamp(0.0, 1.0);

    let score = (match_ratio * 0.75) + (recency * 0.25);
    RagResult {
        chunk,
        score,
        matched_terms: matched,
    }
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn read_text_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("read text document {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

async fn read_pdf_text(path: &Path) -> Result<String> {
    // Lightweight PDF text extraction without a heavyweight parser dependency.
    // Preserve readable ASCII runs as a best-effort fallback.
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("read pdf document {}", path.display()))?;

    let mut out = String::with_capacity(bytes.len() / 2);
    let mut last_was_space = false;

    for byte in bytes {
        let ch = byte as char;
        let keep = ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                ' ' | '\n' | '.' | ',' | ';' | ':' | '-' | '_' | '/' | '(' | ')'
            );

        if keep {
            out.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }

    Ok(out)
}

fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    if text.trim().is_empty() {
        return vec![];
    }

    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let chunk_size = chunk_size.max(200);
    let overlap = overlap.min(chunk_size.saturating_sub(1));

    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < total {
        let end = (start + chunk_size).min(total);
        let chunk = chars[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();

        if !chunk.is_empty() {
            chunks.push(chunk);
        }

        if end >= total {
            break;
        }

        let next_start = end.saturating_sub(overlap);
        if next_start <= start {
            start = end;
        } else {
            start = next_start;
        }
    }

    chunks
}

fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .collect()
}
