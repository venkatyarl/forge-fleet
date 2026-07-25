//! Obsidian vault parser + indexer.
//!
//! Parses markdown files with YAML frontmatter, extracts wikilinks,
//! performs hierarchical chunking, and upserts into Postgres.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Configuration for an Obsidian vault to index.
pub struct VaultConfig {
    /// Root path of the vault on disk, e.g. ~/projects/Yarli_KnowledgeBase
    pub vault_path: PathBuf,
    /// Subfolder within the vault to scope indexing, e.g. "Virtual Brain"
    pub brain_subfolder: String,
}

impl VaultConfig {
    /// Returns the full path to the brain subfolder.
    pub fn brain_root(&self) -> PathBuf {
        self.vault_path.join(&self.brain_subfolder)
    }
}

/// A parsed Obsidian markdown node (one .md file).
pub struct ParsedNode {
    pub path: String,
    pub title: String,
    pub node_type: Option<String>,
    pub tags: Vec<String>,
    pub extends_path: Option<String>,
    pub applies_to: Vec<String>,
    pub from_thread: Option<String>,
    pub confidence: Option<f32>,
    pub body: String,
    pub wikilinks: Vec<String>,
    pub content_hash: String,
}

/// A chunk of markdown text with its heading breadcrumb.
pub struct VaultChunk {
    /// e.g. "Projects/ForgeFleet/UI Design.md > Overrides > Color palette"
    pub breadcrumb: String,
    pub text: String,
    pub char_offset: usize,
    pub token_estimate: usize,
}

/// Summary of an indexing run.
pub struct IndexReport {
    pub files_scanned: usize,
    pub nodes_upserted: usize,
    pub edges_created: usize,
    pub chunks_written: usize,
    pub unchanged_skipped: usize,
}

/// Spawn the vault re-index loop for the production daemon.
///
/// Leader-gated: spawned unconditionally on every node, but each tick checks
/// the live `fleet_leader_state` and skips unless this node is the current
/// leader (so only the leader's checkout drives the canonical vault corpus).
pub fn spawn_vault_index_tick(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE member_name = $1
                              AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                        )
                        "#,
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);
                    if !is_leader {
                        continue;
                    }

                    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/venkat".to_string());
                    let vault_path = PathBuf::from(home).join("projects").join("Yarli_KnowledgeBase");
                    if !vault_path.exists() {
                        continue;
                    }

                    let config = VaultConfig {
                        vault_path,
                        brain_subfolder: String::new(),
                    };
                    match index_vault(&pg, &config).await {
                        Ok(report) => {
                            tracing::info!(
                                nodes_upserted = report.nodes_upserted,
                                skipped = report.unchanged_skipped,
                                "vault re-index tick complete"
                            );
                        }
                        Err(err) => tracing::warn!(error = %err, "vault re-index tick failed"),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

/// Parse YAML frontmatter from a markdown file.
/// Returns (frontmatter as JSON Value, body without frontmatter).
pub fn parse_frontmatter(content: &str) -> (serde_json::Value, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (serde_json::Value::Null, content.to_string());
    }

    // Find the closing ---
    let after_first = &trimmed[3..];
    let close_pos = after_first.find("\n---");
    match close_pos {
        Some(pos) => {
            let yaml_str = &after_first[..pos];
            let body_start = 3 + pos + 4; // skip "---" + "\n---"
            let body = trimmed[body_start..].trim_start_matches('\n').to_string();

            let fm: serde_json::Value = match serde_yaml::from_str(yaml_str) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to parse YAML frontmatter: {e}");
                    serde_json::Value::Null
                }
            };
            (fm, body)
        }
        None => (serde_json::Value::Null, content.to_string()),
    }
}

/// Extract `[[wikilinks]]` from markdown body. Returns list of target page names.
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    let re = regex::Regex::new(r"\[\[([^\]]+)\]\]").expect("valid regex");
    re.captures_iter(body)
        .map(|cap| {
            let target = cap[1].to_string();
            // Handle [[target|alias]] — return only the target part
            if let Some(pipe_pos) = target.find('|') {
                target[..pipe_pos].to_string()
            } else {
                target
            }
        })
        .collect()
}

const MAX_FILE_SIZE: u64 = 500_000; // 500KB — skip huge generated/API-dump files

/// Parse a single .md file into a ParsedNode.
pub fn parse_vault_file(path: &Path, vault_root: &Path) -> Result<ParsedNode, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("metadata {}: {e}", path.display()))?;
    if meta.len() > MAX_FILE_SIZE {
        return Err(format!(
            "skipping oversized file ({} bytes): {}",
            meta.len(),
            path.display()
        ));
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

    let relative = path
        .strip_prefix(vault_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();

    let title = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let (fm, body) = parse_frontmatter(&content);
    let wikilinks = extract_wikilinks(&body);

    // Hash the full file content for change detection
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let content_hash = format!("{:x}", hasher.finalize());

    // Extract fields from frontmatter
    let node_type = fm.get("type").and_then(|v| v.as_str()).map(String::from);
    let tags: Vec<String> = match fm.get("tags") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::String(s)) => s.split(',').map(|t| t.trim().to_string()).collect(),
        _ => Vec::new(),
    };
    let extends_path = fm.get("extends").and_then(|v| v.as_str()).map(String::from);
    let applies_to: Vec<String> = match fm.get("applies_to") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    };
    let from_thread = fm
        .get("from_thread")
        .and_then(|v| v.as_str())
        .map(String::from);
    let confidence = fm
        .get("confidence")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);

    Ok(ParsedNode {
        path: relative,
        title,
        node_type,
        tags,
        extends_path,
        applies_to,
        from_thread,
        confidence,
        body,
        wikilinks,
        content_hash,
    })
}

/// Hierarchical chunking: split by headings first, then recursive 512-token
/// with ~20% overlap. Each chunk carries its heading breadcrumb.
pub fn chunk_markdown(body: &str, file_path: &str) -> Vec<VaultChunk> {
    let mut chunks = Vec::new();
    let mut heading_stack: Vec<(usize, String)> = Vec::new(); // (level, text)
    let mut current_text = String::new();
    let mut section_start: usize = 0;

    let lines: Vec<&str> = body.lines().collect();
    let max_chunk_chars = 512 * 4; // ~512 tokens at 4 chars/token
    let _overlap_chars = max_chunk_chars / 5; // ~20% overlap

    let build_breadcrumb = |stack: &[(usize, String)]| -> String {
        let mut parts: Vec<&str> = vec![file_path];
        for (_, h) in stack {
            parts.push(h.as_str());
        }
        parts.join(" > ")
    };

    let flush_section = |text: &str, offset: usize, breadcrumb: &str, out: &mut Vec<VaultChunk>| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }

        if trimmed.len() <= max_chunk_chars {
            out.push(VaultChunk {
                breadcrumb: breadcrumb.to_string(),
                text: trimmed.to_string(),
                char_offset: offset,
                token_estimate: trimmed.len() / 4,
            });
        } else {
            // Split large sections with overlap — char-boundary safe.
            let chars: Vec<char> = trimmed.chars().collect();
            let total = chars.len();
            let chunk_chars = max_chunk_chars / 4; // work in char count not byte count
            let overlap = chunk_chars / 5;
            let mut pos = 0;
            let mut chunk_idx = 0;
            while pos < total {
                let end = (pos + chunk_chars).min(total);
                let actual_end = if end < total {
                    // Find space near end
                    let window: String = chars[pos..end].iter().collect();
                    match window.rfind(' ') {
                        Some(sp) => pos + sp,
                        None => end,
                    }
                } else {
                    end
                };
                let slice: String = chars[pos..actual_end].iter().collect();
                let byte_offset = trimmed
                    .chars()
                    .take(pos)
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                out.push(VaultChunk {
                    breadcrumb: if chunk_idx == 0 {
                        breadcrumb.to_string()
                    } else {
                        format!("{breadcrumb} (cont.)")
                    },
                    text: slice.clone(),
                    char_offset: offset + byte_offset,
                    token_estimate: slice.len() / 4,
                });
                chunk_idx += 1;

                if actual_end >= total {
                    break;
                }
                let advance = if actual_end > pos + overlap {
                    actual_end - pos - overlap
                } else {
                    actual_end - pos
                };
                pos += advance;
            }
        }
    };

    let mut offset = 0;
    for line in &lines {
        let line_len = line.len() + 1; // +1 for newline

        // Check if this is a heading
        if let Some(level) = heading_level(line) {
            let heading_text = line.trim_start_matches('#').trim().to_string();

            // Flush current section
            let breadcrumb = build_breadcrumb(&heading_stack);
            flush_section(&current_text, section_start, &breadcrumb, &mut chunks);
            current_text.clear();
            section_start = offset;

            // Update heading stack: pop headings at same or deeper level
            while heading_stack.last().is_some_and(|(l, _)| *l >= level) {
                heading_stack.pop();
            }
            heading_stack.push((level, heading_text));
        } else {
            current_text.push_str(line);
            current_text.push('\n');
        }

        offset += line_len;
    }

    // Flush final section
    let breadcrumb = build_breadcrumb(&heading_stack);
    flush_section(&current_text, section_start, &breadcrumb, &mut chunks);

    // If no chunks were created (no headings in the doc), make one from the whole body
    if chunks.is_empty() && !body.trim().is_empty() {
        flush_section(body, 0, file_path, &mut chunks);
    }

    chunks
}

fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    // Must be followed by a space
    if trimmed.len() > level && trimmed.as_bytes()[level] == b' ' {
        Some(level)
    } else {
        None
    }
}

// ── Council decision records ────────────────────────────────────────────

/// A member's revised position after the adversarial critique round —
/// recorded ALONGSIDE its round-1 [`CouncilMemberEntry::answer`], never in
/// place of it, so a persisted transcript still shows what the member
/// originally said even when a counter-argument superseded it downstream.
pub struct CouncilCounterArgument {
    pub answer: String,
    pub confidence: f32,
    pub evidence: Vec<String>,
}

/// One council member's round-1 answer, as captured for a persisted decision
/// record, plus its adversarial-round counter-argument when one was given.
pub struct CouncilMemberEntry {
    pub member: String,
    pub answer: String,
    pub confidence: f32,
    pub evidence: Vec<String>,
    pub counter_argument: Option<CouncilCounterArgument>,
}

/// One critique raised during a council's adversarial round, attributed to
/// the critiquing member and the (blinded) label it targeted.
pub struct CouncilCritiqueEntry {
    pub critic: String,
    pub target_label: String,
    pub critique: String,
}

/// The chairman's structured synthesis of a council's answers.
pub struct CouncilSynthesisEntry {
    pub chairman: String,
    pub consensus: String,
    pub disagreements: Vec<String>,
    pub unique_findings: Vec<String>,
    pub rationale: String,
}

/// Full record of one `ff council` deliberation, ready to persist as a
/// decision record. `members` always carries every round-1 answer — an
/// adversarial counter-argument is attached to its member's entry, not
/// substituted in — so the transcript stays complete regardless of how the
/// council concluded (including when [`CouncilDecision::synthesis`] is
/// `None`, e.g. the chairman produced no usable output).
pub struct CouncilDecision {
    pub question: String,
    pub members: Vec<CouncilMemberEntry>,
    pub critiques: Vec<CouncilCritiqueEntry>,
    pub synthesis: Option<CouncilSynthesisEntry>,
    pub final_output: String,
    pub session_id: Option<uuid::Uuid>,
}

/// Where a persisted council decision landed.
pub struct CouncilDecisionRecord {
    pub vault_path: String,
    pub vault_node_id: uuid::Uuid,
    pub interaction_id: uuid::Uuid,
}

/// Persist a completed `ff council` deliberation as a decision record: the
/// full transcript (every member's round-1 answer plus any adversarial
/// counter-argument), the chairman's synthesis (when there is one), and the
/// final output are rendered into the six-section operator template
/// (Question / Council Transcript / Adversarial Critique Round / Chairman
/// Synthesis / Final Decision / Metadata), upserted into the vault graph
/// (`brain_vault_nodes` + `rag_chunks`, so it's searchable via
/// `brain_search`), and appended to `ff_interactions` so the deliberation
/// shows up in the same audit trail as every other ff dispatch.
///
/// Callers should always call this once the council has run, even when the
/// chairman produced no synthesis at all — the transcript is worth
/// persisting on its own, and skipping the call would silently drop the
/// deliberation from the audit trail. Treat a returned `Err` as non-fatal
/// (mirrors the `log_council` best-effort pattern in `ff-terminal`) — a DB
/// hiccup here should never fail the council itself.
pub async fn save_council_decision(
    pool: &PgPool,
    decision: &CouncilDecision,
) -> Result<CouncilDecisionRecord, String> {
    let now = chrono::Utc::now();
    let path = format!(
        "decisions/council/{}-{}.md",
        now.format("%Y%m%d%H%M%S"),
        slugify_question(&decision.question)
    );
    let title = format!("Council decision: {}", truncate_title(&decision.question));
    let body = render_decision_record(decision, now);

    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    let content_hash = format!("{:x}", hasher.finalize());

    let confidence = decision.synthesis.as_ref().map(|_| 1.0);
    let tags = vec!["council".to_string(), "decision".to_string()];
    let vault_node_id = ff_db::pg_upsert_brain_vault_node(
        pool,
        &path,
        &title,
        Some("decision:council"),
        None,
        &tags,
        None,
        &[],
        None,
        confidence,
        &content_hash,
    )
    .await
    .map_err(|e| format!("DB error upserting decision node '{path}': {e}"))?;

    let chunks = chunk_markdown(&body, &path);
    write_chunks(pool, &path, &chunks).await?;

    let members: Vec<&str> = decision.members.iter().map(|m| m.member.as_str()).collect();
    let rec = ff_db::InteractionRecord {
        session_id: decision.session_id,
        channel: "council_decision".to_string(),
        purpose: Some("council".to_string()),
        request_text: decision.question.chars().take(16000).collect(),
        request_meta: serde_json::json!({
            "members": members,
            "vault_path": path,
            "adversarial": !decision.critiques.is_empty(),
        }),
        engine: decision.synthesis.as_ref().map(|s| s.chairman.clone()),
        response_text: body.chars().take(16000).collect(),
        outcome: "success".to_string(),
        ..Default::default()
    };
    let interaction_id = ff_db::pg_record_interaction(pool, &rec)
        .await
        .map_err(|e| format!("DB error logging council decision interaction: {e}"))?;

    Ok(CouncilDecisionRecord {
        vault_path: path,
        vault_node_id,
        interaction_id,
    })
}

/// Render a [`CouncilDecision`] into the six-section operator output
/// template used for persisted decision records.
fn render_decision_record(decision: &CouncilDecision, ts: chrono::DateTime<chrono::Utc>) -> String {
    let mut out = String::new();

    out.push_str("## 1. Question\n\n");
    out.push_str(decision.question.trim());
    out.push_str("\n\n");

    out.push_str("## 2. Council Transcript\n\n");
    if decision.members.is_empty() {
        out.push_str("(no member answered)\n\n");
    } else {
        for m in &decision.members {
            out.push_str(&format!(
                "### {} (confidence: {:.2})\n\n{}\n\n",
                m.member,
                m.confidence,
                m.answer.trim()
            ));
            if !m.evidence.is_empty() {
                out.push_str("Evidence:\n");
                for e in &m.evidence {
                    out.push_str(&format!("- {e}\n"));
                }
                out.push('\n');
            }
            if let Some(counter) = &m.counter_argument {
                out.push_str(&format!(
                    "**Revised after adversarial critique** (confidence: {:.2}):\n\n{}\n\n",
                    counter.confidence,
                    counter.answer.trim()
                ));
                if !counter.evidence.is_empty() {
                    out.push_str("Evidence:\n");
                    for e in &counter.evidence {
                        out.push_str(&format!("- {e}\n"));
                    }
                    out.push('\n');
                }
            }
        }
    }

    out.push_str("## 3. Adversarial Critique Round\n\n");
    if decision.critiques.is_empty() {
        out.push_str("(adversarial round not run, or no critiques were raised)\n\n");
    } else {
        for c in &decision.critiques {
            out.push_str(&format!(
                "- **{}** on {}: {}\n",
                c.critic, c.target_label, c.critique
            ));
        }
        out.push('\n');
    }

    out.push_str("## 4. Chairman Synthesis\n\n");
    match &decision.synthesis {
        Some(s) => {
            out.push_str(&format!("Chairman: {}\n\n", s.chairman));
            out.push_str(&format!("**Consensus:** {}\n\n", s.consensus.trim()));
            if !s.disagreements.is_empty() {
                out.push_str("**Disagreements:**\n");
                for d in &s.disagreements {
                    out.push_str(&format!("- {d}\n"));
                }
                out.push('\n');
            }
            if !s.unique_findings.is_empty() {
                out.push_str("**Unique findings:**\n");
                for f in &s.unique_findings {
                    out.push_str(&format!("- {f}\n"));
                }
                out.push('\n');
            }
            if !s.rationale.trim().is_empty() {
                out.push_str(&format!("**Rationale:** {}\n\n", s.rationale.trim()));
            }
        }
        None => out.push_str(
            "(no chairman synthesis — sole answer, deliberation ended without \
            one, or the chairman produced no usable output)\n\n",
        ),
    }

    out.push_str("## 5. Final Decision\n\n");
    out.push_str(decision.final_output.trim());
    out.push_str("\n\n");

    out.push_str("## 6. Metadata\n\n");
    out.push_str(&format!("- Timestamp: {}\n", ts.to_rfc3339()));
    out.push_str(&format!(
        "- Members: {}\n",
        decision
            .members
            .iter()
            .map(|m| m.member.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if let Some(s) = &decision.synthesis {
        out.push_str(&format!("- Chairman: {}\n", s.chairman));
    }
    out.push_str(&format!(
        "- Adversarial round: {}\n",
        if decision.critiques.is_empty() {
            "no"
        } else {
            "yes"
        }
    ));

    out
}

/// Slugify a council question into a filesystem-safe vault path component.
fn slugify_question(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(60));
    for ch in s.chars().take(60) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

/// Truncate a question to a readable note title.
fn truncate_title(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() > 80 {
        format!("{}…", t.chars().take(80).collect::<String>())
    } else {
        t.to_string()
    }
}

/// Full index pass: walk the vault, parse every .md file, upsert brain_vault_nodes
/// + brain_vault_edges, chunk and write to rag_chunks. Incremental: only processes
///   files whose content_hash changed since last run.
pub async fn index_vault(pool: &PgPool, config: &VaultConfig) -> Result<IndexReport, String> {
    let brain_root = config.brain_root();
    if !brain_root.exists() {
        return Err(format!(
            "Brain root does not exist: {}",
            brain_root.display()
        ));
    }

    // Collect all .md files on a blocking thread — vault may be large.
    let md_files = tokio::task::spawn_blocking({
        let root = brain_root.clone();
        move || collect_md_files(&root)
    })
    .await
    .map_err(|e| format!("spawn error: {e}"))??;
    info!("Found {} .md files in vault", md_files.len());

    // Fetch existing hashes from DB for incremental indexing
    let existing_hashes = fetch_existing_hashes(pool).await?;

    let mut report = IndexReport {
        files_scanned: md_files.len(),
        nodes_upserted: 0,
        edges_created: 0,
        chunks_written: 0,
        unchanged_skipped: 0,
    };

    for file_path in &md_files {
        let file_path = file_path.clone();
        let brain_root = brain_root.clone();
        let node =
            match tokio::task::spawn_blocking(move || parse_vault_file(&file_path, &brain_root))
                .await
                .map_err(|e| format!("spawn error: {e}"))
                .and_then(|r| r)
            {
                Ok(n) => n,
                Err(e) => {
                    if e.contains("skipping oversized") {
                        debug!("{e}");
                    } else {
                        warn!("parse error: {e}");
                    }
                    report.unchanged_skipped += 1;
                    continue;
                }
            };

        // Check if content changed
        if let Some(old_hash) = existing_hashes.get(&node.path)
            && *old_hash == node.content_hash
        {
            report.unchanged_skipped += 1;
            continue;
        }

        upsert_node(pool, &node).await?;
        report.nodes_upserted += 1;

        // Upsert edges from wikilinks
        let edge_count = upsert_edges(pool, &node).await?;
        report.edges_created += edge_count;

        // Chunk and write
        let chunks = chunk_markdown(&node.body, &node.path);
        write_chunks(pool, &node.path, &chunks).await?;
        report.chunks_written += chunks.len();
    }

    info!(
        "Index complete: {} scanned, {} upserted, {} skipped, {} edges, {} chunks",
        report.files_scanned,
        report.nodes_upserted,
        report.unchanged_skipped,
        report.edges_created,
        report.chunks_written
    );

    Ok(report)
}

/// Incremental index: only re-process files in the given list (from git diff).
pub async fn index_changed_files(
    pool: &PgPool,
    config: &VaultConfig,
    paths: &[String],
) -> Result<IndexReport, String> {
    let brain_root = config.brain_root();
    let mut report = IndexReport {
        files_scanned: paths.len(),
        nodes_upserted: 0,
        edges_created: 0,
        chunks_written: 0,
        unchanged_skipped: 0,
    };

    for rel_path in paths {
        let full_path = brain_root.join(rel_path);
        if !full_path.exists() {
            debug!("Skipping deleted file: {rel_path}");
            // Mark node as invalid
            let _ = sqlx::query(
                "UPDATE brain_vault_nodes SET valid_until = NOW() WHERE path = $1 AND valid_until IS NULL",
            )
            .bind(rel_path)
            .execute(pool)
            .await
            .map_err(|e| format!("DB error invalidating node: {e}"))?;
            continue;
        }

        let fp = full_path.clone();
        let br = brain_root.clone();
        let node = tokio::task::spawn_blocking(move || parse_vault_file(&fp, &br))
            .await
            .map_err(|e| format!("spawn error: {e}"))??;
        upsert_node(pool, &node).await?;
        report.nodes_upserted += 1;

        let edge_count = upsert_edges(pool, &node).await?;
        report.edges_created += edge_count;

        let chunks = chunk_markdown(&node.body, &node.path);
        write_chunks(pool, &node.path, &chunks).await?;
        report.chunks_written += chunks.len();
    }

    Ok(report)
}

// ── Internal helpers ──────────────────────────────────────────────────────

fn collect_md_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_md_recursive(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_md_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("Failed to read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Dir entry error: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            collect_md_recursive(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

async fn fetch_existing_hashes(pool: &PgPool) -> Result<HashMap<String, String>, String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT path, content_hash FROM brain_vault_nodes WHERE valid_until IS NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("DB error fetching hashes: {e}"))?;

    Ok(rows.into_iter().collect())
}

async fn upsert_node(pool: &PgPool, node: &ParsedNode) -> Result<(), String> {
    // Use the pg_upsert_brain_vault_node helper from ff-db (matches V13 schema).
    ff_db::pg_upsert_brain_vault_node(
        pool,
        &node.path,
        &node.title,
        node.node_type.as_deref(),
        None, // project — derived from folder path later
        &node.tags,
        node.extends_path.as_deref(),
        &node.applies_to,
        node.from_thread.as_deref(),
        node.confidence,
        &node.content_hash,
    )
    .await
    .map_err(|e| format!("DB error upserting node '{}': {e}", node.path))?;
    Ok(())
}

async fn upsert_edges(pool: &PgPool, node: &ParsedNode) -> Result<usize, String> {
    // Resolve source node UUID.
    let src = ff_db::pg_get_brain_vault_node(pool, &node.path)
        .await
        .map_err(|e| format!("get src node: {e}"))?;
    let src_id = match src {
        Some(n) => n.id,
        None => return Ok(0),
    };

    let mut count = 0;

    // Wikilink edges — resolve target by matching the wikilink text to an
    // existing node path (basename match, Obsidian-style shortest path).
    for target in &node.wikilinks {
        if let Some(dst) = resolve_wikilink_target(pool, target).await {
            let _ = ff_db::pg_upsert_brain_vault_edge(pool, src_id, dst, "link", 1.0, "extracted")
                .await;
            count += 1;
        }
    }

    // Extends edge
    if let Some(extends) = &node.extends_path {
        let clean = extends.trim_start_matches("[[").trim_end_matches("]]");
        if let Some(dst) = resolve_wikilink_target(pool, clean).await {
            let _ =
                ff_db::pg_upsert_brain_vault_edge(pool, src_id, dst, "extends", 1.0, "extracted")
                    .await;
            count += 1;
        }
    }

    // Applies-to edges
    for target in &node.applies_to {
        let clean = target.trim_start_matches("[[").trim_end_matches("]]");
        if let Some(dst) = resolve_wikilink_target(pool, clean).await {
            let _ = ff_db::pg_upsert_brain_vault_edge(
                pool,
                src_id,
                dst,
                "applies_to",
                1.0,
                "extracted",
            )
            .await;
            count += 1;
        }
    }

    Ok(count)
}

/// Resolve a wikilink target text (e.g. "UI Design" or "Projects/ForgeFleet/UI Design")
/// to an existing brain_vault_nodes.id. Uses Obsidian's shortest-path semantics:
/// first try exact path match, then basename match.
async fn resolve_wikilink_target(pool: &PgPool, target: &str) -> Option<uuid::Uuid> {
    // Try exact path match (e.g. "Projects/ForgeFleet/UI Design.md")
    let with_md = if target.ends_with(".md") {
        target.to_string()
    } else {
        format!("{target}.md")
    };
    if let Ok(Some(node)) = ff_db::pg_get_brain_vault_node(pool, &with_md).await {
        return Some(node.id);
    }
    // Try basename match (e.g. "UI Design" matches "any/path/UI Design.md")
    let basename = target.rsplit('/').next().unwrap_or(target);
    let pattern = format!("%/{basename}.md");
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT id FROM brain_vault_nodes WHERE path LIKE $1 AND valid_until IS NULL LIMIT 1",
    )
    .bind(&pattern)
    .fetch_optional(pool)
    .await
    .ok()?;
    row.map(|r| r.0)
}

async fn write_chunks(pool: &PgPool, node_path: &str, chunks: &[VaultChunk]) -> Result<(), String> {
    // The rag_chunks table is created by ff-memory's RAG engine, not by our
    // V13 migration. If it doesn't exist, skip chunk writing silently — nodes
    // + edges still index fine without chunks. Chunks enable semantic search
    // which only kicks in once pgvector + embeddings are deployed (Phase 4b).
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_name = 'rag_chunks')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false);
    if !table_exists {
        return Ok(());
    }

    let _ = sqlx::query(
        "DELETE FROM rag_chunks WHERE workspace_id = 'brain_vault' AND source_path = $1",
    )
    .bind(node_path)
    .execute(pool)
    .await;

    // Deterministic document_id from path (simple hash-based).
    let mut hasher = Sha256::new();
    hasher.update(b"brain_vault_doc:");
    hasher.update(node_path.as_bytes());
    let hash = hasher.finalize();
    let doc_id = uuid::Uuid::from_slice(&hash[..16]).unwrap_or_else(|_| uuid::Uuid::new_v4());

    for (i, chunk) in chunks.iter().enumerate() {
        let chunk_id = uuid::Uuid::new_v4();
        let metadata = serde_json::json!({
            "breadcrumb": chunk.breadcrumb,
            "char_offset": chunk.char_offset,
            "token_estimate": chunk.token_estimate,
        });
        let _ = sqlx::query(
            "INSERT INTO rag_chunks (id, workspace_id, document_id, source_path, chunk_index, content, metadata)
             VALUES ($1, 'brain_vault', $2, $3, $4, $5, $6)",
        )
        .bind(chunk_id)
        .bind(doc_id)
        .bind(node_path)
        .bind(i as i32)
        .bind(&chunk.text)
        .bind(metadata.to_string())
        .execute(pool)
        .await
        .map_err(|e| format!("DB error inserting chunk: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod council_decision_tests {
    use super::*;

    fn sample_decision() -> CouncilDecision {
        CouncilDecision {
            question: "Should we retire the legacy ff daemon?".to_string(),
            members: vec![
                CouncilMemberEntry {
                    member: "codex".to_string(),
                    answer: "Yes, phase it out.".to_string(),
                    confidence: 0.9,
                    evidence: vec!["reaper already SIGTERMs stale procs".to_string()],
                    counter_argument: Some(CouncilCounterArgument {
                        answer: "Yes, but migrate the legacy-only ticks first.".to_string(),
                        confidence: 0.85,
                        evidence: vec!["kimi flagged the SSH mesh-repair tick".to_string()],
                    }),
                },
                CouncilMemberEntry {
                    member: "kimi".to_string(),
                    answer: "Yes, but move the legacy-only ticks first.".to_string(),
                    confidence: 0.8,
                    evidence: vec![],
                    counter_argument: None,
                },
            ],
            critiques: vec![CouncilCritiqueEntry {
                critic: "kimi".to_string(),
                target_label: "Member A".to_string(),
                critique: "ignores ticks that only exist in the legacy daemon".to_string(),
            }],
            synthesis: Some(CouncilSynthesisEntry {
                chairman: "codex".to_string(),
                consensus: "Retire in phases, migrate ticks first.".to_string(),
                disagreements: vec!["codex vs kimi on rollout speed".to_string()],
                unique_findings: vec!["kimi flagged the SSH mesh-repair tick".to_string()],
                rationale: "phased rollout avoids stranding legacy-only ticks".to_string(),
            }),
            final_output: "Retire in phases, migrate ticks first.".to_string(),
            session_id: None,
        }
    }

    #[test]
    fn render_decision_record_has_all_six_sections_in_order() {
        let decision = sample_decision();
        let body = render_decision_record(&decision, chrono::Utc::now());
        let sections = [
            "## 1. Question",
            "## 2. Council Transcript",
            "## 3. Adversarial Critique Round",
            "## 4. Chairman Synthesis",
            "## 5. Final Decision",
            "## 6. Metadata",
        ];
        let mut last_pos = 0;
        for section in sections {
            let pos = body
                .find(section)
                .unwrap_or_else(|| panic!("missing section: {section}"));
            assert!(pos >= last_pos, "sections out of order at {section}");
            last_pos = pos;
        }
    }

    #[test]
    fn render_decision_record_includes_member_and_synthesis_content() {
        let decision = sample_decision();
        let body = render_decision_record(&decision, chrono::Utc::now());
        assert!(body.contains("Should we retire the legacy ff daemon?"));
        assert!(body.contains("codex"));
        assert!(body.contains("Yes, but move the legacy-only ticks first."));
        assert!(body.contains("ignores ticks that only exist in the legacy daemon"));
        assert!(body.contains("Retire in phases, migrate ticks first."));
        assert!(body.contains("codex vs kimi on rollout speed"));
        assert!(body.contains("kimi flagged the SSH mesh-repair tick"));
    }

    /// The root-cause regression test: an adversarial counter-argument must
    /// be recorded ALONGSIDE the member's round-1 answer, never in place of
    /// it — a persisted transcript that shows only the revised position has
    /// silently lost what the member originally said.
    #[test]
    fn render_decision_record_preserves_round1_answer_next_to_counter_argument() {
        let decision = sample_decision();
        let body = render_decision_record(&decision, chrono::Utc::now());
        assert!(
            body.contains("Yes, phase it out."),
            "round-1 answer must survive even though codex also gave a counter-argument"
        );
        assert!(
            body.contains("Yes, but migrate the legacy-only ticks first."),
            "counter-argument must also be present"
        );
        let round1_pos = body.find("Yes, phase it out.").unwrap();
        let counter_pos = body.find("Revised after adversarial critique").unwrap();
        assert!(
            round1_pos < counter_pos,
            "round-1 answer should be rendered before the counter-argument that revised it"
        );
    }

    #[test]
    fn render_decision_record_notes_missing_synthesis_and_critiques() {
        let mut decision = sample_decision();
        decision.synthesis = None;
        decision.critiques.clear();
        for m in &mut decision.members {
            m.counter_argument = None;
        }
        let body = render_decision_record(&decision, chrono::Utc::now());
        assert!(body.contains("no chairman synthesis"));
        assert!(body.contains("adversarial round not run"));
        assert!(body.contains("- Adversarial round: no\n"));
    }

    /// A council whose chairman produced neither structured nor raw output
    /// still has a synthesis of `None`, but the transcript and final_output
    /// fallback must still render — this is what a caller persists so the
    /// deliberation isn't silently dropped from the audit trail.
    #[test]
    fn render_decision_record_still_renders_transcript_when_chairman_failed_entirely() {
        let mut decision = sample_decision();
        decision.synthesis = None;
        decision.final_output = "Yes, phase it out.".to_string();
        let body = render_decision_record(&decision, chrono::Utc::now());
        assert!(body.contains("chairman produced no usable output"));
        assert!(body.contains("Yes, phase it out."));
        assert!(body.contains("codex"));
        assert!(body.contains("kimi"));
    }

    #[test]
    fn slugify_question_produces_filesystem_safe_slug() {
        assert_eq!(
            slugify_question("Should we retire `ff daemon`?!"),
            "should-we-retire-ff-daemon"
        );
        assert_eq!(slugify_question(""), "untitled");
    }

    #[test]
    fn truncate_title_shortens_long_questions() {
        let long = "x".repeat(200);
        let title = truncate_title(&long);
        assert!(title.chars().count() <= 81);
        assert!(title.ends_with('…'));
    }
}
