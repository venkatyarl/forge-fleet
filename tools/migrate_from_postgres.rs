//! Standalone migration tool: Postgres (Python ForgeFleet) → SQLite (Rust ForgeFleet).
//!
//! Usage:
//!   migrate_from_postgres --pg "postgresql://user:pass@host/forgefleet" --out fleet.db
//!   DATABASE_URL=postgresql://... migrate_from_postgres --out fleet.db
//!
//! Reads nodes, models, tasks, task_results, and memories from the old Postgres
//! database and writes them into a fresh SQLite file via `ff-db`.
//!
//! Extra or missing columns in Postgres are handled gracefully — the script
//! skips unknown columns and fills defaults for expected ones that are absent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process;

use clap::Parser;

use tokio_postgres::{NoTls, Row};

use ff_db::queries::{self, MemoryRow, NodeRow, TaskRow};
use ff_db::{DbPool, DbPoolConfig, run_migrations};
use ff_mc::McDb;

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "migrate_from_postgres",
    about = "Migrate data from Python ForgeFleet Postgres to Rust ForgeFleet SQLite"
)]
struct Cli {
    /// Postgres connection string.
    #[arg(long)]
    pg: Option<String>,

    /// Path to the output SQLite database file.
    #[arg(long, default_value = "forgefleet.db")]
    out: PathBuf,

    /// Optional path to Mission Control SQLite database output.
    ///
    /// When set, core MC domains are migrated: epics, work_items, review_items,
    /// work_item_dependencies, and task_groups.
    #[arg(long)]
    mc_out: Option<PathBuf>,

    /// Wipe the SQLite file(s) before migrating (useful for re-runs).
    #[arg(long, default_value_t = false)]
    clean: bool,

    /// Only report what would be migrated without writing.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Try to read a column by name; return a default if the column doesn't exist.
fn col_or_str(row: &Row, name: &str) -> String {
    row.try_get::<_, String>(name).unwrap_or_default()
}

/// Read a string from primary column name, fallback to alternate.
fn col_or_str_fallback(row: &Row, primary: &str, fallback: &str) -> String {
    row.try_get::<_, String>(primary)
        .or_else(|_| row.try_get::<_, String>(fallback))
        .unwrap_or_default()
}

/// Read an optional string column.
fn col_opt_str(row: &Row, name: &str) -> Option<String> {
    row.try_get::<_, Option<String>>(name).ok().flatten()
}

/// Read an i64 column with fallback.
fn col_i64(row: &Row, name: &str) -> i64 {
    // Postgres might store as i32 or i64, try both
    row.try_get::<_, i64>(name)
        .or_else(|_| row.try_get::<_, i32>(name).map(|v| v as i64))
        .unwrap_or(0)
}

/// Read an i32 column with fallback.
fn col_i32(row: &Row, name: &str) -> i32 {
    row.try_get::<_, i32>(name).unwrap_or(0)
}

/// Read an f64 column with fallback.
fn col_f64(row: &Row, name: &str) -> f64 {
    row.try_get::<_, f64>(name)
        .or_else(|_| row.try_get::<_, f32>(name).map(|v| v as f64))
        .unwrap_or(0.0)
}

/// Read a bool column with fallback.
fn col_bool(row: &Row, name: &str) -> bool {
    row.try_get::<_, bool>(name).unwrap_or(false)
}

// ─── Table Checks ────────────────────────────────────────────────────────────

/// Check which of the expected tables exist in Postgres.
async fn discover_tables(client: &tokio_postgres::Client) -> HashMap<String, bool> {
    let expected = [
        "nodes",
        "models",
        "tasks",
        "task_results",
        "memories",
        "epics",
        "work_items",
        "review_items",
        "work_item_dependencies",
        "task_groups",
    ];
    let mut found = HashMap::new();

    for table in expected {
        let exists = client
            .query_opt(
                "SELECT 1 FROM information_schema.tables WHERE table_name = $1",
                &[&table],
            )
            .await
            .map(|r| r.is_some())
            .unwrap_or(false);
        found.insert(table.to_string(), exists);
    }

    found
}

// ─── Migrate Nodes ───────────────────────────────────────────────────────────

async fn migrate_nodes(
    pg: &tokio_postgres::Client,
    pool: &DbPool,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM nodes", &[]).await?;
    let count = rows.len();
    println!("  nodes: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    for row in &rows {
        let node = NodeRow {
            id: col_or_str(row, "id"),
            name: col_or_str(row, "name"),
            host: col_or_str(row, "host"),
            port: col_i64(row, "port"),
            role: col_or_str(row, "role"),
            election_priority: col_i64(row, "election_priority"),
            status: col_or_str(row, "status"),
            hardware_json: col_or_str(row, "hardware_json"),
            models_json: col_or_str(row, "models_json"),
            last_heartbeat: col_opt_str(row, "last_heartbeat"),
            registered_at: col_or_str(row, "registered_at"),
        };

        pool.with_conn(move |conn| {
            queries::upsert_node(conn, &node)?;
            Ok(())
        })
        .await?;
    }

    Ok(count)
}

// ─── Migrate Models ──────────────────────────────────────────────────────────

async fn migrate_models(
    pg: &tokio_postgres::Client,
    pool: &DbPool,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM models", &[]).await?;
    let count = rows.len();
    println!("  models: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    for row in &rows {
        let id = col_or_str(row, "id");
        let name = col_or_str(row, "name");
        let tier: i64 = col_i64(row, "tier");
        let params_b: f64 = col_f64(row, "params_b");
        let quant = col_or_str(row, "quant");
        let path = col_or_str(row, "path");
        let ctx_size: i64 = col_i64(row, "ctx_size");
        let runtime = col_or_str(row, "runtime");
        let nodes_json = col_or_str(row, "nodes_json");
        let created_at = col_or_str(row, "created_at");
        let updated_at = col_or_str(row, "updated_at");

        pool.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO models (id, name, tier, params_b, quant, path, ctx_size, runtime, nodes_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    tier = excluded.tier,
                    params_b = excluded.params_b,
                    quant = excluded.quant,
                    path = excluded.path,
                    ctx_size = excluded.ctx_size,
                    runtime = excluded.runtime,
                    nodes_json = excluded.nodes_json,
                    updated_at = excluded.updated_at",
                rusqlite::params![id, name, tier, params_b, quant, path, ctx_size, runtime, nodes_json, created_at, updated_at],
            )?;
            Ok(())
        })
        .await?;
    }

    Ok(count)
}

// ─── Migrate Tasks ───────────────────────────────────────────────────────────

async fn migrate_tasks(
    pg: &tokio_postgres::Client,
    pool: &DbPool,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM tasks", &[]).await?;
    let count = rows.len();
    println!("  tasks: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    for row in &rows {
        let task = TaskRow {
            id: col_or_str(row, "id"),
            kind: col_or_str(row, "kind"),
            payload_json: col_or_str(row, "payload_json"),
            status: col_or_str(row, "status"),
            assigned_node: col_opt_str(row, "assigned_node"),
            priority: col_i64(row, "priority"),
            created_at: col_or_str(row, "created_at"),
            started_at: col_opt_str(row, "started_at"),
            completed_at: col_opt_str(row, "completed_at"),
        };

        pool.with_conn(move |conn| {
            queries::insert_task(conn, &task)?;
            Ok(())
        })
        .await?;
    }

    Ok(count)
}

// ─── Migrate Task Results ────────────────────────────────────────────────────

async fn migrate_task_results(
    pg: &tokio_postgres::Client,
    pool: &DbPool,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM task_results", &[]).await?;
    let count = rows.len();
    println!("  task_results: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    for row in &rows {
        let task_id = col_or_str(row, "task_id");
        let success = col_bool(row, "success");
        let output = col_or_str(row, "output");
        let duration_ms = col_i32(row, "duration_ms");

        pool.with_conn(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO task_results (task_id, success, output, duration_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![task_id, success as i32, output, duration_ms],
            )?;
            Ok(())
        })
        .await?;
    }

    Ok(count)
}

// ─── Migrate Memories ────────────────────────────────────────────────────────

async fn migrate_memories(
    pg: &tokio_postgres::Client,
    pool: &DbPool,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM memories", &[]).await?;
    let count = rows.len();
    println!("  memories: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    for row in &rows {
        let mem = MemoryRow {
            id: col_or_str(row, "id"),
            namespace: col_or_str(row, "namespace"),
            key: col_or_str(row, "key"),
            content: col_or_str(row, "content"),
            embedding_json: col_opt_str(row, "embedding_json"),
            metadata_json: col_or_str(row, "metadata_json"),
            importance: col_f64(row, "importance"),
            created_at: col_or_str(row, "created_at"),
            updated_at: col_or_str(row, "updated_at"),
            expires_at: col_opt_str(row, "expires_at"),
        };

        pool.with_conn(move |conn| {
            queries::upsert_memory(conn, &mem)?;
            Ok(())
        })
        .await?;
    }

    Ok(count)
}

// ─── Mission Control domain migration ────────────────────────────────────────

async fn migrate_mc_epics(
    pg: &tokio_postgres::Client,
    mc_db: &McDb,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM epics", &[]).await?;
    let count = rows.len();
    println!("  mc.epics: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    let conn = mc_db.conn();
    for row in rows {
        let id = col_or_str(&row, "id");
        let title = col_or_str(&row, "title");
        let description = col_or_str(&row, "description");
        let status = col_or_str(&row, "status");
        let created_at = col_or_str_fallback(&row, "created_at", "created");
        let updated_at = col_or_str_fallback(&row, "updated_at", "updated");

        conn.execute(
            "INSERT INTO epics (id, title, description, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               title=excluded.title,
               description=excluded.description,
               status=excluded.status,
               updated_at=excluded.updated_at",
            rusqlite::params![id, title, description, status, created_at, updated_at],
        )?;
    }

    Ok(count)
}

async fn migrate_mc_work_items(
    pg: &tokio_postgres::Client,
    mc_db: &McDb,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM work_items", &[]).await?;
    let count = rows.len();
    println!("  mc.work_items: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    let conn = mc_db.conn();
    for row in rows {
        let id = col_or_str(&row, "id");
        let title = col_or_str(&row, "title");
        let description = col_or_str(&row, "description");
        let status = col_or_str(&row, "status");
        let priority = col_i64(&row, "priority");
        let assignee = col_or_str(&row, "assignee");
        let epic_id = col_opt_str(&row, "epic_id");
        let sprint_id = col_opt_str(&row, "sprint_id");
        let task_group_id =
            col_opt_str(&row, "task_group_id").or_else(|| col_opt_str(&row, "task_group"));
        let sequence_order = row
            .try_get::<_, Option<i32>>("sequence_order")
            .ok()
            .flatten();

        let labels = if let Ok(v) = row.try_get::<_, String>("labels") {
            v
        } else if let Ok(v) = row.try_get::<_, String>("labels_json") {
            v
        } else {
            "[]".to_string()
        };

        let created_at = col_or_str_fallback(&row, "created_at", "created");
        let updated_at = col_or_str_fallback(&row, "updated_at", "updated");

        conn.execute(
            "INSERT INTO work_items (id, title, description, status, priority, assignee, epic_id, sprint_id, task_group_id, sequence_order, labels, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(id) DO UPDATE SET
               title=excluded.title,
               description=excluded.description,
               status=excluded.status,
               priority=excluded.priority,
               assignee=excluded.assignee,
               epic_id=excluded.epic_id,
               sprint_id=excluded.sprint_id,
               task_group_id=excluded.task_group_id,
               sequence_order=excluded.sequence_order,
               labels=excluded.labels,
               updated_at=excluded.updated_at",
            rusqlite::params![
                id,
                title,
                description,
                status,
                priority,
                assignee,
                epic_id,
                sprint_id,
                task_group_id,
                sequence_order,
                labels,
                created_at,
                updated_at
            ],
        )?;
    }

    Ok(count)
}

async fn migrate_mc_review_items(
    pg: &tokio_postgres::Client,
    mc_db: &McDb,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM review_items", &[]).await?;
    let count = rows.len();
    println!("  mc.review_items: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    let conn = mc_db.conn();
    for row in rows {
        let id = col_or_str(&row, "id");
        let work_item_id = col_or_str(&row, "work_item_id");
        let title = col_or_str_fallback(&row, "title", "check_item");
        let status = col_or_str(&row, "status");
        let reviewer = col_opt_str(&row, "reviewer");
        let notes = col_opt_str(&row, "notes");
        let created_at = col_or_str_fallback(&row, "created_at", "created");
        let updated_at = col_or_str_fallback(&row, "updated_at", "updated");

        conn.execute(
            "INSERT INTO review_items (id, work_item_id, title, status, reviewer, notes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
               title=excluded.title,
               status=excluded.status,
               reviewer=excluded.reviewer,
               notes=excluded.notes,
               updated_at=excluded.updated_at",
            rusqlite::params![id, work_item_id, title, status, reviewer, notes, created_at, updated_at],
        )?;
    }

    Ok(count)
}

async fn migrate_mc_dependencies(
    pg: &tokio_postgres::Client,
    mc_db: &McDb,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg
        .query("SELECT * FROM work_item_dependencies", &[])
        .await?;
    let count = rows.len();
    println!("  mc.work_item_dependencies: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    let conn = mc_db.conn();
    for row in rows {
        let work_item_id = col_or_str_fallback(&row, "work_item_id", "item_id");
        let depends_on_id = col_or_str_fallback(&row, "depends_on_id", "dependency_id");
        let created_at = col_or_str_fallback(&row, "created_at", "created");

        conn.execute(
            "INSERT OR IGNORE INTO work_item_dependencies (work_item_id, depends_on_id, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![work_item_id, depends_on_id, created_at],
        )?;
    }

    Ok(count)
}

async fn migrate_mc_task_groups(
    pg: &tokio_postgres::Client,
    mc_db: &McDb,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let rows = pg.query("SELECT * FROM task_groups", &[]).await?;
    let count = rows.len();
    println!("  mc.task_groups: found {count} rows");

    if dry_run {
        return Ok(count);
    }

    let conn = mc_db.conn();
    for row in rows {
        let id = col_or_str(&row, "id");
        let name = col_or_str(&row, "name");
        let description = col_or_str(&row, "description");
        let created_at = col_or_str_fallback(&row, "created_at", "created");
        let updated_at = col_or_str_fallback(&row, "updated_at", "updated");

        conn.execute(
            "INSERT INTO task_groups (id, name, description, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name,
               description=excluded.description,
               updated_at=excluded.updated_at",
            rusqlite::params![id, name, description, created_at, updated_at],
        )?;
    }

    Ok(count)
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Resolve Postgres connection string: --pg flag or DATABASE_URL env
    let pg_url = cli
        .pg
        .clone()
        .or_else(|| std::env::var("DATABASE_URL").ok());
    let pg_url = match pg_url {
        Some(url) => url,
        None => {
            eprintln!("ERROR: provide --pg <url> or set DATABASE_URL environment variable");
            process::exit(1);
        }
    };

    println!("ForgeFleet Postgres → SQLite Migration Tool");
    println!("============================================");
    println!("  Source: {}", pg_url.split('@').last().unwrap_or(&pg_url));
    println!("  Target (core): {}", cli.out.display());
    if let Some(mc_out) = &cli.mc_out {
        println!("  Target (mc):   {}", mc_out.display());
    }
    if cli.dry_run {
        println!("  Mode: DRY RUN (no writes)");
    }
    println!();

    // ── Connect to Postgres ──────────────────────────────────────────────────
    let (pg_client, pg_conn) = match tokio_postgres::connect(&pg_url, NoTls).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("ERROR: failed to connect to Postgres: {e}");
            process::exit(1);
        }
    };

    // Spawn the connection driver as a background task.
    tokio::spawn(async move {
        if let Err(e) = pg_conn.await {
            eprintln!("Postgres connection error: {e}");
        }
    });

    println!("✓ Connected to Postgres");

    // ── Discover available tables ────────────────────────────────────────────
    let tables = discover_tables(&pg_client).await;
    println!("\nAvailable tables:");
    for (table, exists) in &tables {
        let mark = if *exists { "✓" } else { "✗" };
        println!("  {mark} {table}");
    }
    println!();

    // ── Open/create SQLite ───────────────────────────────────────────────────
    if cli.clean && cli.out.exists() {
        std::fs::remove_file(&cli.out).ok();
        println!("Removed existing core SQLite file");
    }
    if cli.clean
        && let Some(mc_out) = &cli.mc_out
        && mc_out.exists()
    {
        std::fs::remove_file(mc_out).ok();
        println!("Removed existing MC SQLite file");
    }

    let pool = match DbPool::open(DbPoolConfig::with_path(&cli.out)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: failed to open SQLite: {e}");
            process::exit(1);
        }
    };

    // Run migrations to create all tables.
    let conn = pool.open_raw_connection().expect("raw connection");
    let applied = run_migrations(&conn).expect("migrations");
    println!("✓ SQLite ready (applied {applied} migration(s))\n");
    drop(conn);

    // Optional Mission Control SQLite output.
    let mc_db = if let Some(mc_out) = &cli.mc_out {
        match McDb::open(mc_out) {
            Ok(db) => Some(db),
            Err(e) => {
                eprintln!(
                    "ERROR: failed to open MC SQLite ({}): {e}",
                    mc_out.display()
                );
                process::exit(1);
            }
        }
    } else {
        None
    };

    // ── Migrate each table ───────────────────────────────────────────────────
    let mut total = 0usize;
    let mut total_mc = 0usize;

    if *tables.get("nodes").unwrap_or(&false) {
        match migrate_nodes(&pg_client, &pool, cli.dry_run).await {
            Ok(n) => {
                println!("    → migrated {n} nodes");
                total += n;
            }
            Err(e) => eprintln!("    ✗ nodes migration error: {e}"),
        }
    }

    if *tables.get("models").unwrap_or(&false) {
        match migrate_models(&pg_client, &pool, cli.dry_run).await {
            Ok(n) => {
                println!("    → migrated {n} models");
                total += n;
            }
            Err(e) => eprintln!("    ✗ models migration error: {e}"),
        }
    }

    if *tables.get("tasks").unwrap_or(&false) {
        match migrate_tasks(&pg_client, &pool, cli.dry_run).await {
            Ok(n) => {
                println!("    → migrated {n} tasks");
                total += n;
            }
            Err(e) => eprintln!("    ✗ tasks migration error: {e}"),
        }
    }

    if *tables.get("task_results").unwrap_or(&false) {
        match migrate_task_results(&pg_client, &pool, cli.dry_run).await {
            Ok(n) => {
                println!("    → migrated {n} task results");
                total += n;
            }
            Err(e) => eprintln!("    ✗ task_results migration error: {e}"),
        }
    }

    if *tables.get("memories").unwrap_or(&false) {
        match migrate_memories(&pg_client, &pool, cli.dry_run).await {
            Ok(n) => {
                println!("    → migrated {n} memories");
                total += n;
            }
            Err(e) => eprintln!("    ✗ memories migration error: {e}"),
        }
    }

    if let Some(mc_db) = &mc_db {
        println!("\nMission Control domain migration:");

        if *tables.get("epics").unwrap_or(&false) {
            match migrate_mc_epics(&pg_client, mc_db, cli.dry_run).await {
                Ok(n) => {
                    println!("    → migrated {n} mc epics");
                    total_mc += n;
                }
                Err(e) => eprintln!("    ✗ mc epics migration error: {e}"),
            }
        }

        if *tables.get("work_items").unwrap_or(&false) {
            match migrate_mc_work_items(&pg_client, mc_db, cli.dry_run).await {
                Ok(n) => {
                    println!("    → migrated {n} mc work_items");
                    total_mc += n;
                }
                Err(e) => eprintln!("    ✗ mc work_items migration error: {e}"),
            }
        }

        if *tables.get("review_items").unwrap_or(&false) {
            match migrate_mc_review_items(&pg_client, mc_db, cli.dry_run).await {
                Ok(n) => {
                    println!("    → migrated {n} mc review_items");
                    total_mc += n;
                }
                Err(e) => eprintln!("    ✗ mc review_items migration error: {e}"),
            }
        }

        if *tables.get("work_item_dependencies").unwrap_or(&false) {
            match migrate_mc_dependencies(&pg_client, mc_db, cli.dry_run).await {
                Ok(n) => {
                    println!("    → migrated {n} mc dependencies");
                    total_mc += n;
                }
                Err(e) => eprintln!("    ✗ mc dependencies migration error: {e}"),
            }
        }

        if *tables.get("task_groups").unwrap_or(&false) {
            match migrate_mc_task_groups(&pg_client, mc_db, cli.dry_run).await {
                Ok(n) => {
                    println!("    → migrated {n} mc task_groups");
                    total_mc += n;
                }
                Err(e) => eprintln!("    ✗ mc task_groups migration error: {e}"),
            }
        }
    }

    // ── Summary ──────────────────────────────────────────────────────────────
    println!("\n============================================");
    if cli.dry_run {
        if let Some(mc_out) = &cli.mc_out {
            println!(
                "DRY RUN complete: {total} core rows + {total_mc} MC rows would be migrated (MC target: {})",
                mc_out.display()
            );
        } else {
            println!("DRY RUN complete: {total} rows would be migrated");
        }
    } else if let Some(mc_out) = &cli.mc_out {
        println!(
            "Migration complete: {total} core rows → {}, {total_mc} MC rows → {}",
            cli.out.display(),
            mc_out.display()
        );
    } else {
        println!(
            "Migration complete: {total} rows migrated to {}",
            cli.out.display()
        );
    }
}
