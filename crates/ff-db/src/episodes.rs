//! Canonical writes for the fleet-wide episodic stream.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct FleetEpisode {
    pub id: Uuid,
    pub source_kind: String,
    pub node: String,
    pub model: Option<String>,
    pub session_id: String,
    pub work_item_id: Option<Uuid>,
    pub workstream_id: Option<Uuid>,
    pub operator_intent: Option<String>,
    pub seq: i32,
    pub ts: DateTime<Utc>,
    pub role: String,
    pub content: String,
    pub tokens: Option<i32>,
    pub redacted: bool,
}

impl FleetEpisode {
    pub fn new(
        source_kind: impl Into<String>,
        node: impl Into<String>,
        session_id: impl Into<String>,
        seq: i32,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_kind: source_kind.into(),
            node: node.into(),
            model: None,
            session_id: session_id.into(),
            work_item_id: None,
            workstream_id: None,
            operator_intent: None,
            seq,
            ts: Utc::now(),
            role: role.into(),
            content: content.into(),
            tokens: None,
            redacted: true,
        }
    }
}

async fn insert_episode(
    tx: &mut Transaction<'_, Postgres>,
    episode: &FleetEpisode,
) -> Result<bool> {
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fleet_episodes
            (id, source_kind, node, model, session_id, work_item_id,
             workstream_id, operator_intent, seq, ts, role, content, tokens, redacted)
         VALUES
            ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT (source_kind, node, session_id, seq) DO NOTHING
         RETURNING id",
    )
    .bind(episode.id)
    .bind(&episode.source_kind)
    .bind(&episode.node)
    .bind(&episode.model)
    .bind(&episode.session_id)
    .bind(episode.work_item_id)
    .bind(episode.workstream_id)
    .bind(&episode.operator_intent)
    .bind(episode.seq)
    .bind(episode.ts)
    .bind(&episode.role)
    .bind(&episode.content)
    .bind(episode.tokens)
    .bind(episode.redacted)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(inserted.is_some())
}

/// First-party push path used by attached clients, including ff-TUI.
pub async fn pg_append_episode(pool: &PgPool, episode: &FleetEpisode) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let inserted = insert_episode(&mut tx, episode).await?;
    tx.commit().await?;
    Ok(inserted)
}

/// Workstream-2 first-party push entrypoint. Attached clients send normalized,
/// redacted turns here in real time; no vendor transcript parsing is involved.
pub async fn ff_session_sync(pool: &PgPool, episodes: &[FleetEpisode]) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let mut inserted = 0;
    for episode in episodes {
        inserted += usize::from(insert_episode(&mut tx, episode).await?);
    }
    tx.commit().await?;
    Ok(inserted)
}

/// Fallback adapter path. Episode inserts and the source high-watermark commit
/// atomically, so a crash cannot create a watermark gap.
pub async fn pg_append_episode_batch(
    pool: &PgPool,
    source_kind: &str,
    node: &str,
    stream_id: &str,
    session_id: &str,
    episodes: &[FleetEpisode],
) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let mut inserted = 0;
    for episode in episodes {
        inserted += usize::from(insert_episode(&mut tx, episode).await?);
    }
    if let Some(seq) = episodes.iter().map(|episode| episode.seq).max() {
        sqlx::query(
            "INSERT INTO fleet_episode_watermarks
                (source_kind, node, stream_id, session_id, seq)
             VALUES ($1,$2,$3,$4,$5)
             ON CONFLICT (source_kind, node, stream_id) DO UPDATE
                SET session_id = EXCLUDED.session_id,
                    seq = GREATEST(fleet_episode_watermarks.seq, EXCLUDED.seq),
                    updated_at = NOW()",
        )
        .bind(source_kind)
        .bind(node)
        .bind(stream_id)
        .bind(session_id)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(inserted)
}
