//! Cortex context pack for Pillar-4 dispatch.
//!
//! Before handing a work_item to a coding agent (codex/claude/kimi), pull the
//! EXACT existing symbols it will need to touch from the Cortex code graph and
//! prepend them to the prompt. Without this the agent grep-storms the whole repo
//! cold to orient itself — burning context + wall-clock, and (on Rust) dragging
//! in the cold-compile explore phase. The graph is shared + indexed once, so this
//! is the "many computers, many models" lever: every LLM on every node starts
//! from the same precise, token-cheap context instead of re-deriving it.
//!
//! v2 prefers loading the precomputed context (`brain_node_ids` and `touched_paths`)
//! stored on the canonical `work_items` row. Those fields are populated by the
//! `work_item_context` extractor during Cortex reindex, so dispatch no longer
//! recomputes the symbol set on every build. If the row has no stored context,
//! the legacy SUBSTRING path over `ff cortex find` is used as a fail-open fallback.

use sqlx::{PgPool, Row};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Node types eligible for the "known decisions and gotchas" pack.
const BRAIN_NODE_TYPES: &[&str] = &["decision", "distilled_fact", "gotcha"];

/// Byte cap for the decisions/gotchas pack (separate from the symbol pack's
/// `max_symbols` cap — this one is measured in bytes since node titles vary
/// widely in length).
const BRAIN_PACK_MAX_BYTES: usize = 1200;

/// Max nodes surfaced in the decisions/gotchas pack, regardless of byte budget.
const BRAIN_PACK_MAX_NODES: usize = 5;

/// Tokens too generic to be worth a graph lookup even if they look like idents.
const STOPWORDS: &[&str] = &[
    "String", "Result", "Option", "Vec", "Self", "None", "Some", "true", "false", "add", "the",
    "and", "for", "with", "when", "else", "must", "compile", "under", "print", "test", "tests",
    "handler", "value", "count", "code", "rows", "run",
];

/// Extract candidate code identifiers from a task's title+description: CamelCase
/// types (`FleetCommand`) and snake_case names (`sub_agent_count`, `fleet_workers`).
/// Deduped, order-preserving, filtered by length + a small stopword set.
pub fn extract_task_identifiers(title: &str, description: &str) -> Vec<String> {
    let text = format!("{title}\n{description}");
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut token = String::new();
    let flush = |tok: &mut String, out: &mut Vec<String>, seen: &mut BTreeSet<String>| {
        let t = std::mem::take(tok);
        if t.len() < 4 || STOPWORDS.contains(&t.as_str()) {
            return;
        }
        let has_underscore = t.contains('_');
        let has_inner_upper = t.chars().skip(1).any(|c| c.is_ascii_uppercase());
        // Only identifier-shaped tokens: CamelCase or snake_case, not plain words.
        if (has_underscore || has_inner_upper) && seen.insert(t.clone()) {
            out.push(t);
        }
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            flush(&mut token, &mut out, &mut seen);
        }
    }
    flush(&mut token, &mut out, &mut seen);
    out
}

/// One `ff cortex` invocation in `repo_path`, returning parsed JSON or None.
fn cortex_json(repo_path: &Path, args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new("ff")
        .arg("cortex")
        .args(args)
        .arg("--format")
        .arg("json")
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Make a corpus file path node-independent: the graph stores the LEADER's
/// absolute paths (`/Users/venkat/projects/forge-fleet/crates/...`), which don't
/// exist on a follower. Strip to a repo-relative form so the pointer is valid on
/// any node. Falls back to the basename for non-`crates/` layouts.
fn relativize(file: &str) -> &str {
    if let Some(i) = file.find("/crates/") {
        return &file[i + 1..];
    }
    if let Some(i) = file.find("/src/") {
        return &file[i + 1..];
    }
    file.rsplit('/').next().unwrap_or(file)
}

/// Build the context pack: `--all` substring-find each task identifier across
/// EVERY indexed corpus (cwd-independent — a fresh worktree has no corpus of its
/// own), rank unique hits by fan-in, and emit them as SYMBOL POINTERS
/// (`qualified_name — kind at file:line`). Pointers alone kill the grep-storm:
/// the agent opens the exact symbol directly instead of hunting for it. Returns
/// empty when Cortex has nothing (or is unavailable) — caller prepends it, so
/// empty == unchanged behaviour. Bounded + best-effort.
pub fn build_cortex_context_pack(
    title: &str,
    description: &str,
    repo_path: &Path,
    max_symbols: usize,
) -> String {
    let idents = extract_task_identifiers(title, description);
    if idents.is_empty() {
        return String::new();
    }

    // Unique hits: (qualified_name, kind, relfile, line, fan_in).
    let mut ranked: Vec<(String, String, String, i64, i64)> = Vec::new();
    let mut seen = BTreeSet::new();
    for id in idents.iter().take(8) {
        let Some(serde_json::Value::Array(hits)) =
            cortex_json(repo_path, &["find", id, "--all-corpora"])
        else {
            continue;
        };
        for h in hits.iter().take(3) {
            let Some(qn) = h.get("qualified_name").and_then(|v| v.as_str()) else {
                continue;
            };
            if !seen.insert(qn.to_string()) {
                continue;
            }
            let kind = h
                .get("node_type")
                .and_then(|v| v.as_str())
                .unwrap_or("symbol")
                .trim_start_matches("code:")
                .to_string();
            let file = h.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            let line = h.get("start_line").and_then(|v| v.as_i64()).unwrap_or(0);
            let fan = h.get("fan_in").and_then(|v| v.as_i64()).unwrap_or(0);
            ranked.push((
                qn.to_string(),
                kind,
                relativize(file).to_string(),
                line,
                fan,
            ));
        }
    }
    if ranked.is_empty() {
        return String::new();
    }
    ranked.sort_by(|a, b| b.4.cmp(&a.4)); // highest fan-in first

    let mut pack = String::from(
        "## Relevant existing code (from the Cortex code graph)\n\
         These are the exact symbols this task touches — open them directly \
         instead of grepping the repo to find them:\n\n",
    );
    for (qn, kind, file, line, _) in ranked.into_iter().take(max_symbols) {
        pack.push_str(&format!("- `{qn}` — {kind} at {file}:{line}\n"));
    }
    pack.push('\n');
    pack
}

/// Async wrapper: runs the (subprocess-heavy) pack build off the runtime with a
/// hard cap so a slow/hung Cortex can never stall a dispatch. Fail-open.
pub async fn cortex_context_pack_async(
    title: String,
    description: String,
    repo_path: std::path::PathBuf,
    max_symbols: usize,
) -> String {
    let fut = tokio::task::spawn_blocking(move || {
        build_cortex_context_pack(&title, &description, &repo_path, max_symbols)
    });
    match tokio::time::timeout(Duration::from_secs(20), fut).await {
        Ok(Ok(pack)) => pack,
        _ => String::new(),
    }
}

/// Build a context pack from the precomputed `brain_node_ids` and `touched_paths`
/// stored on the `work_items` row. Deduplicates across the two sources and emits
/// the same "symbol pointer" style as [`build_cortex_context_pack`] so the agent
/// can open the relevant files/symbols directly.
pub fn build_context_pack_from_store(
    brain_node_ids: &[String],
    touched_paths: &[String],
    max_symbols: usize,
) -> String {
    #[derive(Debug)]
    struct Entry {
        name: String,
        kind: String,
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen = BTreeSet::new();

    for path in brain_node_ids.iter().filter(|p| !p.trim().is_empty()) {
        if !seen.insert(path.clone()) {
            continue;
        }
        let entry = if let Some(rest) = path.strip_prefix("code://") {
            if let Some((file, symbol)) = rest.rsplit_once('/') {
                Entry {
                    name: symbol.to_string(),
                    kind: format!("symbol at {file}"),
                }
            } else {
                Entry {
                    name: path.clone(),
                    kind: "symbol".to_string(),
                }
            }
        } else {
            Entry {
                name: path.clone(),
                kind: "brain node".to_string(),
            }
        };
        entries.push(entry);
        if entries.len() >= max_symbols {
            break;
        }
    }

    for path in touched_paths.iter().filter(|p| !p.trim().is_empty()) {
        if !seen.insert(path.clone()) {
            continue;
        }
        entries.push(Entry {
            name: path.clone(),
            kind: "file".to_string(),
        });
        if entries.len() >= max_symbols {
            break;
        }
    }

    if entries.is_empty() {
        return String::new();
    }

    let mut pack = String::from(
        "## Relevant existing code (from the Cortex code graph)\n\
         These are the exact symbols this task touches — open them directly \
         instead of grepping the repo to find them:\n\n",
    );
    for entry in entries {
        pack.push_str(&format!("- `{}` — {}\n", entry.name, entry.kind));
    }
    pack.push('\n');
    pack
}

/// Query `brain_vault_nodes` for decisions/distilled-facts/gotchas relevant to
/// this task, ranked by `recency + degree` (plain SQL; PageRank-over-Falkor is a
/// future upgrade). Relevance is a substring match against each task identifier
/// (same tokens as [`extract_task_identifiers`]) over the node's `title`/`path` —
/// there's no dedicated body column on `brain_vault_nodes` today, and title/path
/// already carry the meaningful slug (see `ff-brain::vault::parse_vault_file`).
/// Degree is in+out edge count from `brain_vault_edges`. Fail-open: any DB error
/// (including a not-yet-migrated table) yields an empty pack, never an error.
pub async fn build_brain_decisions_pack(pool: &PgPool, title: &str, description: &str) -> String {
    let idents = extract_task_identifiers(title, description);
    if idents.is_empty() {
        return String::new();
    }
    let patterns: Vec<String> = idents.iter().map(|id| format!("%{id}%")).collect();

    let rows = match sqlx::query(
        r#"
        WITH edge_degree AS (
            SELECT node_id, COUNT(*) AS degree
            FROM (
                SELECT src_id AS node_id FROM brain_vault_edges
                UNION ALL
                SELECT dst_id AS node_id FROM brain_vault_edges
            ) e
            GROUP BY node_id
        )
        SELECT n.title, n.path, n.node_type,
               COALESCE(d.degree, 0) AS degree
          FROM brain_vault_nodes n
          LEFT JOIN edge_degree d ON d.node_id = n.id
         WHERE n.node_type = ANY($1)
           AND n.valid_until IS NULL
           AND EXISTS (
               SELECT 1 FROM unnest($2::text[]) AS pat
                WHERE n.title ILIKE pat OR n.path ILIKE pat
           )
         ORDER BY (
             COALESCE(d.degree, 0)::double precision
             + 1.0 / (1.0 + EXTRACT(EPOCH FROM (NOW() - n.updated_at)) / 86400.0)
         ) DESC
         LIMIT $3
        "#,
    )
    .bind(BRAIN_NODE_TYPES)
    .bind(&patterns)
    .bind(BRAIN_PACK_MAX_NODES as i64)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(error = %err, "build_brain_decisions_pack: query failed, skipping");
            return String::new();
        }
    };
    if rows.is_empty() {
        return String::new();
    }

    let mut pack = String::from(
        "## Known decisions and gotchas relevant to this task\n\
         These are prior decisions/facts from Brain that bear on this task:\n\n",
    );
    let mut node_count = 0usize;
    for row in &rows {
        let node_title: String = row.get("title");
        let path: String = row.get("path");
        let node_type: String = row.get("node_type");
        let line = format!("- **{node_type}**: {node_title} ({path})\n");
        if pack.len() + line.len() > BRAIN_PACK_MAX_BYTES {
            break;
        }
        pack.push_str(&line);
        node_count += 1;
    }
    pack.push('\n');
    tracing::info!(
        pack_bytes = pack.len(),
        node_count,
        "build_brain_decisions_pack: built decisions/gotchas pack"
    );
    pack
}

/// Build a context pack for dispatch, preferring the precomputed DB context and
/// falling back to a live Cortex lookup only when nothing is stored. This keeps
/// dispatch fast and consistent across the fleet while remaining compatible with
/// work items that have not yet been indexed. After the symbol pack, appends
/// relevant Brain decisions/gotchas (see [`build_brain_decisions_pack`]).
pub async fn context_pack_for_dispatch(
    pool: &PgPool,
    brain_node_ids: Vec<String>,
    touched_paths: Vec<String>,
    title: String,
    description: String,
    repo_path: std::path::PathBuf,
    max_symbols: usize,
) -> String {
    let store_pack = build_context_pack_from_store(&brain_node_ids, &touched_paths, max_symbols);
    let symbol_pack = if !store_pack.is_empty() {
        store_pack
    } else {
        cortex_context_pack_async(title.clone(), description.clone(), repo_path, max_symbols).await
    };

    let decisions_pack = build_brain_decisions_pack(pool, &title, &description).await;

    format!("{symbol_pack}{decisions_pack}")
}

#[cfg(test)]
mod tests {
    use super::{
        build_brain_decisions_pack, build_context_pack_from_store, extract_task_identifiers,
        relativize,
    };

    #[test]
    fn extracts_camel_and_snake_idents_skips_plain_words() {
        let ids = extract_task_identifiers(
            "Add ff fleet set-slots verb",
            "Add a subcommand under the FleetCommand enum. Run \
             UPDATE fleet_workers SET sub_agent_count = $1 WHERE worker_name = $2. \
             Print how many rows changed.",
        );
        assert!(ids.contains(&"FleetCommand".to_string()));
        assert!(ids.contains(&"fleet_workers".to_string()));
        assert!(ids.contains(&"sub_agent_count".to_string()));
        assert!(ids.contains(&"worker_name".to_string()));
        // Plain english words and stopwords are not identifiers.
        assert!(!ids.iter().any(|i| i == "subcommand" || i == "changed"));
        assert!(!ids.contains(&"count".to_string())); // stopword
        // Deduped.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }

    #[test]
    fn relativize_strips_absolute_paths_to_repo_relative() {
        // 1. /crates/ found -> strips to repo-relative
        assert_eq!(
            relativize("/Users/venkat/projects/forge-fleet/crates/ff-agent/src/foo.rs"),
            "crates/ff-agent/src/foo.rs"
        );
        // 2. /src/ found (but not /crates/) -> strips to src-relative
        assert_eq!(relativize("/home/x/repo/src/bar.rs"), "src/bar.rs");
        // 3. neither found -> returns basename
        assert_eq!(relativize("/var/log/system/thing.log"), "thing.log");
    }

    #[test]
    fn build_context_pack_from_store_renders_symbols_and_files() {
        let pack = build_context_pack_from_store(
            &[
                "code://crates/ff-agent/src/work_item_dispatch.rs/run_git".to_string(),
                "pm://work_item/82cd7aa9-9942-4774-bdd1-5ac1b3d65c62".to_string(),
            ],
            &["crates/ff-agent/src/dispatch_context.rs".to_string()],
            8,
        );
        assert!(pack.contains("run_git"));
        assert!(pack.contains("crates/ff-agent/src/work_item_dispatch.rs"));
        assert!(pack.contains("crates/ff-agent/src/dispatch_context.rs"));
        assert!(pack.contains("pm://work_item/82cd7aa9-9942-4774-bdd1-5ac1b3d65c62"));
    }

    #[test]
    fn build_context_pack_from_store_deduplicates_and_caps() {
        let pack = build_context_pack_from_store(
            &[
                "code://crates/a.rs/foo".to_string(),
                "code://crates/a.rs/foo".to_string(),
            ],
            &[],
            1,
        );
        // Deduplication keeps one symbol; cap keeps only the first entry.
        assert_eq!(pack.matches("`foo`").count(), 1);
        assert!(!pack.contains("crates/b.rs"));
    }

    #[test]
    fn build_context_pack_from_store_empty_when_no_context() {
        assert!(build_context_pack_from_store(&[], &[], 8).is_empty());
        assert!(
            build_context_pack_from_store(&["".to_string()], &["  ".to_string()], 8).is_empty()
        );
    }

    // -- DB tests: early-return (skip) when no Postgres is configured; CI's
    //    `cargo test --lib` has no database and must never panic here.

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_dispatch_context_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn create_temp_db() -> Option<(sqlx::PgPool, sqlx::PgPool, String)> {
        let (admin_url, db_url, db_name) = temp_db_urls()?;
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&db_url)
            .await
            .expect("connect temp db");
        // Minimal slice of the live brain_vault schema: only the columns
        // build_brain_decisions_pack's query touches.
        sqlx::raw_sql(
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE brain_vault_nodes (
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                 path TEXT NOT NULL,
                 title TEXT NOT NULL,
                 node_type TEXT NOT NULL,
                 valid_until TIMESTAMPTZ,
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
             );
             CREATE TABLE brain_vault_edges (
                 src_id UUID NOT NULL,
                 dst_id UUID NOT NULL,
                 edge_type TEXT NOT NULL DEFAULT 'link'
             );",
        )
        .execute(&pool)
        .await
        .expect("create minimal brain_vault schema");
        Some((admin, pool, db_name))
    }

    async fn drop_temp_db(admin: sqlx::PgPool, pool: sqlx::PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1
                AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .ok();
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .ok();
        admin.close().await;
    }

    #[tokio::test]
    async fn build_brain_decisions_pack_ranks_by_recency_and_degree_and_excludes_stale() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!("skipping: FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL not set");
            return;
        };

        // High-degree decision: matches, 5 edges, 30 days old.
        let high_degree: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO brain_vault_nodes (path, title, node_type, updated_at)
             VALUES ('decisions/wave_dispatcher.md', 'decision_wave_dispatcher_owns_slots',
                     'decision', NOW() - INTERVAL '30 days')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        for _ in 0..5 {
            let other: uuid::Uuid = sqlx::query_scalar(
                "INSERT INTO brain_vault_nodes (path, title, node_type, updated_at)
                 VALUES ('code://x', 'x', 'code:function', NOW()) RETURNING id",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO brain_vault_edges (src_id, dst_id, edge_type) VALUES ($1, $2, 'link')",
            )
            .bind(high_degree)
            .bind(other)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Zero-degree gotcha: matches, no edges, fresh.
        sqlx::query(
            "INSERT INTO brain_vault_nodes (path, title, node_type, updated_at)
             VALUES ('gotchas/wave_dispatcher_timing.md', 'gotcha_wave_dispatcher_timing',
                     'gotcha', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Matches on title but superseded (valid_until set) -- must be excluded.
        sqlx::query(
            "INSERT INTO brain_vault_nodes (path, title, node_type, updated_at, valid_until)
             VALUES ('decisions/wave_dispatcher_old.md', 'decision_wave_dispatcher_old',
                     'decision', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        // No identifier match -- must be excluded.
        sqlx::query(
            "INSERT INTO brain_vault_nodes (path, title, node_type, updated_at)
             VALUES ('decisions/unrelated.md', 'decision_unrelated_topic', 'decision', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let pack = build_brain_decisions_pack(
            &pool,
            "Fix wave_dispatcher self-kill race",
            "The wave_dispatcher must not kill its own worker.",
        )
        .await;

        assert!(pack.contains("Known decisions and gotchas relevant to this task"));
        assert!(pack.contains("decision_wave_dispatcher_owns_slots"));
        assert!(pack.contains("gotcha_wave_dispatcher_timing"));
        assert!(!pack.contains("decision_wave_dispatcher_old"));
        assert!(!pack.contains("decision_unrelated_topic"));
        let high_degree_pos = pack.find("decision_wave_dispatcher_owns_slots").unwrap();
        let gotcha_pos = pack.find("gotcha_wave_dispatcher_timing").unwrap();
        assert!(
            high_degree_pos < gotcha_pos,
            "higher-degree node should rank first: {pack}"
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn build_brain_decisions_pack_empty_when_no_identifiers() {
        let Some((admin, pool, db_name)) = create_temp_db().await else {
            eprintln!("skipping: FORGEFLEET_POSTGRES_URL/FORGEFLEET_DATABASE_URL not set");
            return;
        };
        let pack = build_brain_decisions_pack(&pool, "fix the bug", "please fix it").await;
        assert!(pack.is_empty());
        drop_temp_db(admin, pool, &db_name).await;
    }
}
