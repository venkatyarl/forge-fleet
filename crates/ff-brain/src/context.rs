//! Smart context selector with graph-aware retrieval.
//!
//! Selects relevant vault nodes, messages, and project context for a given
//! query, fitting everything within a token budget.

use sqlx::PgPool;

/// A bundle of context to inject into a prompt.
pub struct ContextBundle {
    pub recent_messages: Vec<BrainMessage>,
    pub resolved_nodes: Vec<ResolvedNode>,
    pub active_stack: Vec<serde_json::Value>,
    pub top_backlog: Vec<serde_json::Value>,
    pub token_estimate: usize,
}

/// A vault node resolved with inheritance and scoring.
pub struct ResolvedNode {
    pub path: String,
    pub title: String,
    pub effective_body: String,
    pub source_chain: Vec<String>,
    pub node_type: Option<String>,
    pub community_id: Option<i32>,
    pub score: f32,
}

/// A single message in a brain thread.
pub struct BrainMessage {
    pub role: String,
    pub content: String,
    pub channel: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Select context for a query within a token budget.
///
/// 1. Fetch last 20 messages from brain_messages for the thread
/// 2. Text-match vault nodes against query (ILIKE proxy for BM25)
/// 3. Walk 1-hop edges for top results
/// 4. Resolve inheritance (extends)
/// 5. Project boost
/// 6. Pack within budget
pub async fn select_context(
    pool: &PgPool,
    _user_id: uuid::Uuid,
    thread_id: uuid::Uuid,
    query: &str,
    budget_tokens: usize,
    active_project: Option<&str>,
) -> Result<ContextBundle, String> {
    // 1. Recent messages
    let msg_rows: Vec<(String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        r#"
        SELECT role, content, channel, created_at
        FROM brain_messages
        WHERE thread_id = $1
        ORDER BY created_at DESC
        LIMIT 20
        "#,
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("DB error fetching messages: {e}"))?;

    let recent_messages: Vec<BrainMessage> = msg_rows
        .into_iter()
        .rev() // oldest first
        .map(|(role, content, channel, created_at)| BrainMessage {
            role,
            content,
            channel,
            created_at,
        })
        .collect();

    // 2. Text search on vault nodes (ILIKE proxy for BM25)
    let search_pattern = format!("%{query}%");
    let node_rows: Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<i32>,
        Option<String>,
    )> = sqlx::query_as(
        r#"
            SELECT path, title, body, node_type, community_id, extends_path
            FROM brain_vault_nodes
            WHERE valid_until IS NULL
              AND (title ILIKE $1 OR body ILIKE $1)
            LIMIT 20
            "#,
    )
    .bind(&search_pattern)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("DB error searching nodes: {e}"))?;

    // Score and collect candidates
    let query_lower = query.to_lowercase();
    let mut candidates: Vec<ResolvedNode> = Vec::new();

    for (path, title, body, node_type, community_id, extends_path) in &node_rows {
        let title_lower = title.to_lowercase();
        let body_lower = body.to_lowercase();

        // Simple scoring: title match is stronger
        let mut score: f32 = 0.0;
        if title_lower.contains(&query_lower) {
            score += 0.6;
        }
        if body_lower.contains(&query_lower) {
            score += 0.3;
        }

        // 5. Project boost
        if let Some(proj) = active_project {
            let proj_lower = proj.to_lowercase();
            if title_lower.contains(&proj_lower) || body_lower.contains(&proj_lower) {
                score += 0.2;
            }
        }

        // 4. Resolve inheritance
        let mut effective_body = body.clone();
        let mut source_chain = vec![path.clone()];

        if let Some(parent_path) = extends_path {
            let parent: Option<(String, String)> = sqlx::query_as(
                "SELECT path, body FROM brain_vault_nodes WHERE path = $1 AND valid_until IS NULL",
            )
            .bind(parent_path)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("DB error resolving parent: {e}"))?;

            if let Some((p_path, p_body)) = parent {
                source_chain.push(p_path);
                // Prepend parent body
                effective_body = format!("{p_body}\n\n---\n\n{effective_body}");
            }
        }

        candidates.push(ResolvedNode {
            path: path.clone(),
            title: title.clone(),
            effective_body,
            source_chain,
            node_type: node_type.clone(),
            community_id: *community_id,
            score,
        });
    }

    // 3. Walk 1-hop edges for top-5 results
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top_paths: Vec<String> = candidates.iter().take(5).map(|n| n.path.clone()).collect();

    for source_path in &top_paths {
        let neighbor_rows: Vec<(String, String, String, Option<String>, Option<i32>)> =
            sqlx::query_as(
                r#"
                SELECT n.path, n.title, n.body, n.node_type, n.community_id
                FROM brain_vault_edges e
                JOIN brain_vault_nodes n ON n.path = e.target_path AND n.valid_until IS NULL
                WHERE e.source_path = $1
                LIMIT 5
                "#,
            )
            .fetch_all(pool)
            .await
            .map_err(|e| format!("DB error walking edges: {e}"))?;

        for (n_path, n_title, n_body, n_type, n_community) in neighbor_rows {
            // Skip if already in candidates
            if candidates.iter().any(|c| c.path == n_path) {
                continue;
            }
            candidates.push(ResolvedNode {
                path: n_path.clone(),
                title: n_title,
                effective_body: n_body,
                source_chain: vec![source_path.clone(), n_path],
                node_type: n_type,
                community_id: n_community,
                score: 0.1, // neighbor bonus
            });
        }
    }

    // Re-sort after adding neighbors
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 6. Pack within budget (estimate 4 chars per token)
    let chars_per_token = 4;
    let mut used_tokens: usize = 0;

    // Messages take priority
    for msg in &recent_messages {
        used_tokens += msg.content.len() / chars_per_token;
    }

    let mut resolved_nodes: Vec<ResolvedNode> = Vec::new();
    for node in candidates {
        let node_tokens = node.effective_body.len() / chars_per_token;
        if used_tokens + node_tokens > budget_tokens {
            break;
        }
        used_tokens += node_tokens;
        resolved_nodes.push(node);
    }

    Ok(ContextBundle {
        recent_messages,
        resolved_nodes,
        active_stack: Vec::new(),
        top_backlog: Vec::new(),
        token_estimate: used_tokens,
    })
}
