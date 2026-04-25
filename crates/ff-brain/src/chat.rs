//! Channel-agnostic chat service.
//!
//! Handles incoming messages from any channel (Discord, CLI, web, etc.),
//! resolves users and threads, and persists messages to Postgres.

use crate::context::BrainMessage;
use sqlx::PgPool;
use uuid::Uuid;

/// Summary of a brain thread for listing.
pub struct ThreadSummary {
    pub id: Uuid,
    pub slug: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub last_message_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: String,
}

/// Process an incoming message from any channel.
///
/// 1. Resolve user from channel identity
/// 2. Get or create attached thread
/// 3. Insert message
/// 4. Update thread timestamp
/// 5. Return the persisted message
pub async fn receive_message(
    pool: &PgPool,
    channel: &str,
    external_id: &str,
    content: &str,
) -> Result<BrainMessage, String> {
    let user_id = resolve_user(pool, channel, external_id).await?;
    let thread_id = match get_attached_thread(pool, channel, external_id).await? {
        Some(tid) => tid,
        None => {
            let tid = create_thread(pool, user_id, "inbox", None).await?;
            attach_thread(pool, channel, external_id, tid).await?;
            tid
        }
    };

    let now = chrono::Utc::now();

    // Insert message
    sqlx::query(
        r#"
        INSERT INTO brain_messages (id, thread_id, user_id, role, content, channel, created_at)
        VALUES ($1, $2, $3, 'user', $4, $5, $6)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(thread_id)
    .bind(user_id)
    .bind(content)
    .bind(channel)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error inserting message: {e}"))?;

    // Update thread timestamp
    sqlx::query("UPDATE brain_threads SET last_message_at = $1 WHERE id = $2")
        .bind(now)
        .bind(thread_id)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error updating thread: {e}"))?;

    Ok(BrainMessage {
        role: "user".to_string(),
        content: content.to_string(),
        channel: channel.to_string(),
        created_at: now,
    })
}

/// Look up or auto-create a user from a channel identity.
pub async fn resolve_user(pool: &PgPool, channel: &str, external_id: &str) -> Result<Uuid, String> {
    // Try to find existing identity
    let existing: Option<(Uuid,)> = sqlx::query_as(
        "SELECT user_id FROM brain_channel_identities WHERE channel = $1 AND external_id = $2",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("DB error resolving user: {e}"))?;

    if let Some((user_id,)) = existing {
        return Ok(user_id);
    }

    // Auto-create user + identity
    let user_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO brain_users (id, display_name, created_at)
        VALUES ($1, $2, NOW())
        "#,
    )
    .bind(user_id)
    .bind(format!("{channel}:{external_id}"))
    .execute(pool)
    .await
    .map_err(|e| format!("DB error creating user: {e}"))?;

    sqlx::query(
        r#"
        INSERT INTO brain_channel_identities (channel, external_id, user_id, created_at)
        VALUES ($1, $2, $3, NOW())
        "#,
    )
    .bind(channel)
    .bind(external_id)
    .bind(user_id)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error creating identity: {e}"))?;

    Ok(user_id)
}

/// Look up the thread currently attached to a channel identity.
pub async fn get_attached_thread(
    pool: &PgPool,
    channel: &str,
    external_id: &str,
) -> Result<Option<Uuid>, String> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT thread_id FROM brain_thread_attachments
        WHERE channel = $1 AND external_id = $2
        "#,
    )
    .bind(channel)
    .bind(external_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("DB error fetching thread attachment: {e}"))?;

    Ok(row.map(|(tid,)| tid))
}

/// Attach a thread to a channel identity (upsert).
pub async fn attach_thread(
    pool: &PgPool,
    channel: &str,
    external_id: &str,
    thread_id: Uuid,
) -> Result<(), String> {
    sqlx::query(
        r#"
        INSERT INTO brain_thread_attachments (channel, external_id, thread_id, attached_at)
        VALUES ($1, $2, $3, NOW())
        ON CONFLICT (channel, external_id) DO UPDATE SET thread_id = $3, attached_at = NOW()
        "#,
    )
    .bind(channel)
    .bind(external_id)
    .bind(thread_id)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error attaching thread: {e}"))?;

    Ok(())
}

/// Create a new brain thread.
pub async fn create_thread(
    pool: &PgPool,
    user_id: Uuid,
    slug: &str,
    project: Option<&str>,
) -> Result<Uuid, String> {
    let thread_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO brain_threads (id, user_id, slug, project, status, created_at)
        VALUES ($1, $2, $3, $4, 'active', NOW())
        "#,
    )
    .bind(thread_id)
    .bind(user_id)
    .bind(slug)
    .bind(project)
    .execute(pool)
    .await
    .map_err(|e| format!("DB error creating thread: {e}"))?;

    Ok(thread_id)
}

/// List threads for a user.
pub async fn list_threads(pool: &PgPool, user_id: Uuid) -> Result<Vec<ThreadSummary>, String> {
    let rows: Vec<(
        Uuid,
        String,
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        String,
    )> = sqlx::query_as(
        r#"
        SELECT id, slug, title, project, last_message_at, status
        FROM brain_threads
        WHERE user_id = $1
        ORDER BY last_message_at DESC NULLS LAST
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("DB error listing threads: {e}"))?;

    Ok(rows
        .into_iter()
        .map(
            |(id, slug, title, project, last_message_at, status)| ThreadSummary {
                id,
                slug,
                title,
                project,
                last_message_at,
                status,
            },
        )
        .collect())
}
