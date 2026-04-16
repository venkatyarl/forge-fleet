//! Typed query helpers for common database operations.
//!
//! Provides a clean Rust API over raw SQL. All functions take a `&Connection`
//! to work with both pooled and standalone connections.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};

use crate::error::Result;

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn lease_expiry_iso(now: chrono::DateTime<Utc>, lease_secs: i64) -> String {
    let clamped = lease_secs.max(0);
    (now + Duration::seconds(clamped)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

// ─── Row Types ─────────────────────────────────────────────────────────────

/// A node row from the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: i64,
    pub role: String,
    pub election_priority: i64,
    pub status: String,
    pub hardware_json: String,
    pub models_json: String,
    pub last_heartbeat: Option<String>,
    pub registered_at: String,
}

/// A task row from the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub kind: String,
    pub payload_json: String,
    pub status: String,
    pub assigned_node: Option<String>,
    pub priority: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

/// A memory row from the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRow {
    pub id: String,
    pub namespace: String,
    pub key: String,
    pub content: String,
    pub embedding_json: Option<String>,
    pub metadata_json: String,
    pub importance: f64,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
}

/// A session row from the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub channel: String,
    pub user_id: Option<String>,
    pub node_name: Option<String>,
    pub status: String,
    pub metadata_json: String,
    pub created_at: String,
    pub last_activity: String,
    pub closed_at: Option<String>,
}

/// An audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogRow {
    pub id: i64,
    pub timestamp: String,
    pub event_type: String,
    pub actor: String,
    pub target: Option<String>,
    pub details_json: String,
    pub node_name: Option<String>,
}

/// An autonomy decision event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyEventRow {
    pub id: i64,
    pub event_type: String,
    pub action_type: String,
    pub decision: String,
    pub reason: String,
    pub created_at: String,
}

/// A persisted Telegram media ingest metadata row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramMediaIngestRow {
    pub id: i64,
    pub chat_id: String,
    pub message_id: String,
    pub media_kind: String,
    pub local_path: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub created_at: String,
}

/// Runtime heartbeat payload row used to upsert live fleet node state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetNodeRuntimeHeartbeatRow {
    pub node_id: String,
    pub hostname: String,
    pub ips_json: String,
    pub role: String,
    pub reported_status: String,
    pub last_heartbeat: String,
    pub resources_json: String,
    pub services_json: String,
    pub models_json: String,
    pub capabilities_json: String,
    pub stale_degraded_after_secs: i64,
    pub stale_offline_after_secs: i64,
}

/// Runtime fleet node row with derived staleness status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetNodeRuntimeRow {
    pub node_id: String,
    pub hostname: String,
    pub ips_json: String,
    pub role: String,
    pub reported_status: String,
    pub derived_status: String,
    pub last_heartbeat: String,
    pub heartbeat_age_secs: i64,
    pub resources_json: String,
    pub services_json: String,
    pub models_json: String,
    pub capabilities_json: String,
    pub stale_degraded_after_secs: i64,
    pub stale_offline_after_secs: i64,
    pub updated_at: String,
}

/// Payload used to record fleet enrollment outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetEnrollmentEventInsert {
    pub node_id: Option<String>,
    pub hostname: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
    pub role: Option<String>,
    pub service_version: Option<String>,
    pub addresses_json: String,
    pub capabilities_json: String,
    pub metadata_json: String,
}

/// Fleet enrollment event row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetEnrollmentEventRow {
    pub id: i64,
    pub node_id: Option<String>,
    pub hostname: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
    pub role: Option<String>,
    pub service_version: Option<String>,
    pub addresses_json: String,
    pub capabilities_json: String,
    pub metadata_json: String,
    pub created_at: String,
}

// ─── Node Queries ──────────────────────────────────────────────────────────

/// Insert or replace a node.
pub fn upsert_node(conn: &Connection, node: &NodeRow) -> Result<()> {
    conn.execute(
        "INSERT INTO nodes (id, name, host, port, role, election_priority, status, hardware_json, models_json, last_heartbeat, registered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            host = excluded.host,
            port = excluded.port,
            role = excluded.role,
            election_priority = excluded.election_priority,
            status = excluded.status,
            hardware_json = excluded.hardware_json,
            models_json = excluded.models_json,
            last_heartbeat = excluded.last_heartbeat",
        params![
            node.id, node.name, node.host, node.port,
            node.role, node.election_priority, node.status,
            node.hardware_json, node.models_json,
            node.last_heartbeat, node.registered_at
        ],
    )?;
    Ok(())
}

/// Get a node by name.
pub fn get_node_by_name(conn: &Connection, name: &str) -> Result<Option<NodeRow>> {
    let row = conn
        .query_row(
            "SELECT id, name, host, port, role, election_priority, status,
                    hardware_json, models_json, last_heartbeat, registered_at
             FROM nodes WHERE name = ?1",
            [name],
            |row| {
                Ok(NodeRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    host: row.get(2)?,
                    port: row.get(3)?,
                    role: row.get(4)?,
                    election_priority: row.get(5)?,
                    status: row.get(6)?,
                    hardware_json: row.get(7)?,
                    models_json: row.get(8)?,
                    last_heartbeat: row.get(9)?,
                    registered_at: row.get(10)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Get all nodes.
pub fn list_nodes(conn: &Connection) -> Result<Vec<NodeRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, host, port, role, election_priority, status,
                hardware_json, models_json, last_heartbeat, registered_at
         FROM nodes ORDER BY election_priority, name",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(NodeRow {
            id: row.get(0)?,
            name: row.get(1)?,
            host: row.get(2)?,
            port: row.get(3)?,
            role: row.get(4)?,
            election_priority: row.get(5)?,
            status: row.get(6)?,
            hardware_json: row.get(7)?,
            models_json: row.get(8)?,
            last_heartbeat: row.get(9)?,
            registered_at: row.get(10)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Update a node's heartbeat timestamp.
pub fn update_node_heartbeat(conn: &Connection, name: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let changed = conn.execute(
        "UPDATE nodes SET last_heartbeat = ?1 WHERE name = ?2",
        params![now, name],
    )?;
    Ok(changed > 0)
}

/// Update a node's status.
pub fn update_node_status(conn: &Connection, name: &str, status: &str) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE nodes SET status = ?1 WHERE name = ?2",
        params![status, name],
    )?;
    Ok(changed > 0)
}

/// Delete a node by name.
pub fn delete_node(conn: &Connection, name: &str) -> Result<bool> {
    let changed = conn.execute("DELETE FROM nodes WHERE name = ?1", [name])?;
    Ok(changed > 0)
}

// ─── Fleet Runtime Registry Queries ────────────────────────────────────────

fn parse_utc_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn normalize_runtime_status(status: &str) -> String {
    match status.trim().to_ascii_lowercase().as_str() {
        "online" | "healthy" | "ok" => "online".to_string(),
        "degraded" | "starting" | "maintenance" | "busy" => "degraded".to_string(),
        "offline" | "unreachable" | "down" => "offline".to_string(),
        "unknown" => "unknown".to_string(),
        _ => "unknown".to_string(),
    }
}

/// Derive runtime node status using heartbeat age and stale thresholds.
///
/// Priority:
/// 1. Heartbeat age > offline threshold => `offline`
/// 2. Heartbeat age > degraded threshold => `degraded`
/// 3. Otherwise normalized reported status
pub fn derive_runtime_node_status(
    reported_status: &str,
    last_heartbeat: &str,
    now: &DateTime<Utc>,
    degraded_after_secs: i64,
    offline_after_secs: i64,
) -> (String, i64) {
    let degraded_threshold = degraded_after_secs.max(1);
    let offline_threshold = offline_after_secs.max(degraded_threshold + 1);

    let heartbeat = parse_utc_timestamp(last_heartbeat).unwrap_or_else(|| now.clone());
    let age_secs = now
        .clone()
        .signed_duration_since(heartbeat)
        .num_seconds()
        .max(0);

    if age_secs >= offline_threshold {
        return ("offline".to_string(), age_secs);
    }

    if age_secs >= degraded_threshold {
        return ("degraded".to_string(), age_secs);
    }

    (normalize_runtime_status(reported_status), age_secs)
}

/// Insert or update a fleet runtime node heartbeat/state snapshot.
pub fn upsert_fleet_node_runtime(
    conn: &Connection,
    row: &FleetNodeRuntimeHeartbeatRow,
) -> Result<()> {
    let degraded_threshold = row.stale_degraded_after_secs.max(1);
    let offline_threshold = row.stale_offline_after_secs.max(degraded_threshold + 1);

    conn.execute(
        "INSERT INTO fleet_node_runtime (
            node_id, hostname, ips_json, role, reported_status, last_heartbeat,
            resources_json, services_json, models_json, capabilities_json,
            stale_degraded_after_secs, stale_offline_after_secs, updated_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(node_id) DO UPDATE SET
            hostname = excluded.hostname,
            ips_json = excluded.ips_json,
            role = excluded.role,
            reported_status = excluded.reported_status,
            last_heartbeat = excluded.last_heartbeat,
            resources_json = excluded.resources_json,
            services_json = excluded.services_json,
            models_json = excluded.models_json,
            capabilities_json = excluded.capabilities_json,
            stale_degraded_after_secs = excluded.stale_degraded_after_secs,
            stale_offline_after_secs = excluded.stale_offline_after_secs,
            updated_at = excluded.updated_at",
        params![
            row.node_id,
            row.hostname,
            row.ips_json,
            row.role,
            row.reported_status,
            row.last_heartbeat,
            row.resources_json,
            row.services_json,
            row.models_json,
            row.capabilities_json,
            degraded_threshold,
            offline_threshold,
            now_iso(),
        ],
    )?;

    Ok(())
}

/// List live fleet runtime nodes, deriving staleness at query time.
pub fn list_fleet_node_runtime(conn: &Connection) -> Result<Vec<FleetNodeRuntimeRow>> {
    list_fleet_node_runtime_at(conn, Utc::now())
}

/// List live fleet runtime nodes with status derived relative to `now`.
pub fn list_fleet_node_runtime_at(
    conn: &Connection,
    now: DateTime<Utc>,
) -> Result<Vec<FleetNodeRuntimeRow>> {
    let mut stmt = conn.prepare(
        "SELECT node_id, hostname, ips_json, role, reported_status, last_heartbeat,
                resources_json, services_json, models_json, capabilities_json,
                stale_degraded_after_secs, stale_offline_after_secs, updated_at
         FROM fleet_node_runtime
         ORDER BY hostname, node_id",
    )?;

    let rows = stmt.query_map([], |row| {
        let reported_status: String = row.get(4)?;
        let last_heartbeat: String = row.get(5)?;
        let degraded_after_secs: i64 = row.get(10)?;
        let offline_after_secs: i64 = row.get(11)?;

        let (derived_status, heartbeat_age_secs) = derive_runtime_node_status(
            &reported_status,
            &last_heartbeat,
            &now,
            degraded_after_secs,
            offline_after_secs,
        );

        Ok(FleetNodeRuntimeRow {
            node_id: row.get(0)?,
            hostname: row.get(1)?,
            ips_json: row.get(2)?,
            role: row.get(3)?,
            reported_status,
            derived_status,
            last_heartbeat,
            heartbeat_age_secs,
            resources_json: row.get(6)?,
            services_json: row.get(7)?,
            models_json: row.get(8)?,
            capabilities_json: row.get(9)?,
            stale_degraded_after_secs: degraded_after_secs,
            stale_offline_after_secs: offline_after_secs,
            updated_at: row.get(12)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Check whether a runtime fleet node row exists for `node_id`.
pub fn fleet_node_runtime_exists(conn: &Connection, node_id: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fleet_node_runtime WHERE node_id = ?1",
        [node_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Record a fleet enrollment event (accepted or rejected).
pub fn insert_fleet_enrollment_event(
    conn: &Connection,
    event: &FleetEnrollmentEventInsert,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO fleet_enrollment_events (
            node_id, hostname, outcome, reason, role, service_version,
            addresses_json, capabilities_json, metadata_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            event.node_id,
            event.hostname,
            event.outcome,
            event.reason,
            event.role,
            event.service_version,
            event.addresses_json,
            event.capabilities_json,
            event.metadata_json,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// List recent fleet enrollment events (newest first).
pub fn list_fleet_enrollment_events(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<FleetEnrollmentEventRow>> {
    let clamped = (limit as i64).clamp(1, 500);
    let mut stmt = conn.prepare(
        "SELECT id, node_id, hostname, outcome, reason, role, service_version,
                addresses_json, capabilities_json, metadata_json, created_at
         FROM fleet_enrollment_events
         ORDER BY id DESC
         LIMIT ?1",
    )?;

    let rows = stmt.query_map([clamped], |row| {
        Ok(FleetEnrollmentEventRow {
            id: row.get(0)?,
            node_id: row.get(1)?,
            hostname: row.get(2)?,
            outcome: row.get(3)?,
            reason: row.get(4)?,
            role: row.get(5)?,
            service_version: row.get(6)?,
            addresses_json: row.get(7)?,
            capabilities_json: row.get(8)?,
            metadata_json: row.get(9)?,
            created_at: row.get(10)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// ─── Task Queries ──────────────────────────────────────────────────────────

/// Insert a new task.
pub fn insert_task(conn: &Connection, task: &TaskRow) -> Result<()> {
    conn.execute(
        "INSERT INTO tasks (id, kind, payload_json, status, assigned_node, priority, created_at, started_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            task.id, task.kind, task.payload_json, task.status,
            task.assigned_node, task.priority, task.created_at,
            task.started_at, task.completed_at
        ],
    )?;
    Ok(())
}

/// Get a task by ID.
pub fn get_task(conn: &Connection, id: &str) -> Result<Option<TaskRow>> {
    let row = conn
        .query_row(
            "SELECT id, kind, payload_json, status, assigned_node, priority,
                    created_at, started_at, completed_at
             FROM tasks WHERE id = ?1",
            [id],
            |row| {
                Ok(TaskRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    payload_json: row.get(2)?,
                    status: row.get(3)?,
                    assigned_node: row.get(4)?,
                    priority: row.get(5)?,
                    created_at: row.get(6)?,
                    started_at: row.get(7)?,
                    completed_at: row.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// List tasks by status.
pub fn list_tasks_by_status(conn: &Connection, status: &str) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, payload_json, status, assigned_node, priority,
                created_at, started_at, completed_at
         FROM tasks WHERE status = ?1 ORDER BY priority DESC, created_at",
    )?;

    let rows = stmt.query_map([status], |row| {
        Ok(TaskRow {
            id: row.get(0)?,
            kind: row.get(1)?,
            payload_json: row.get(2)?,
            status: row.get(3)?,
            assigned_node: row.get(4)?,
            priority: row.get(5)?,
            created_at: row.get(6)?,
            started_at: row.get(7)?,
            completed_at: row.get(8)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Atomically claim the next available task for a node.
///
/// Eligible tasks are those in `pending`, `todo`, or `backlog` state with no
/// assigned node. Selection order is priority DESC, then oldest created_at.
pub fn claim_next_task(conn: &mut Connection, node_name: &str) -> Result<Option<TaskRow>> {
    let tx = conn.transaction()?;

    let mut stmt = tx.prepare(
        "SELECT id, kind, payload_json, status, assigned_node, priority,
                created_at, started_at, completed_at
         FROM tasks
         WHERE status IN ('pending', 'todo', 'backlog')
           AND (assigned_node IS NULL OR assigned_node = '')
         ORDER BY priority DESC, created_at ASC
         LIMIT 1",
    )?;

    let candidate = stmt
        .query_row([], |row| {
            Ok(TaskRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                payload_json: row.get(2)?,
                status: row.get(3)?,
                assigned_node: row.get(4)?,
                priority: row.get(5)?,
                created_at: row.get(6)?,
                started_at: row.get(7)?,
                completed_at: row.get(8)?,
            })
        })
        .optional()?;
    drop(stmt);

    let Some(mut task) = candidate else {
        tx.commit()?;
        return Ok(None);
    };

    let now = Utc::now().to_rfc3339();
    let changed = tx.execute(
        "UPDATE tasks
            SET status = 'claimed', assigned_node = ?1, started_at = COALESCE(started_at, ?2)
          WHERE id = ?3
            AND status IN ('pending', 'todo', 'backlog')
            AND (assigned_node IS NULL OR assigned_node = '')",
        params![node_name, now, task.id],
    )?;

    if changed == 1 {
        task.status = "claimed".to_string();
        task.assigned_node = Some(node_name.to_string());
        task.started_at = Some(now);
        tx.commit()?;
        Ok(Some(task))
    } else {
        tx.commit()?;
        Ok(None)
    }
}

/// Update a task status using ForgeFleet autonomous workflow states.
///
/// Supported transition states include: `claimed`, `in_progress`, `review`,
/// `done`, and `failed` (plus any existing statuses for compatibility).
pub fn set_task_status(conn: &Connection, task_id: &str, status: &str) -> Result<bool> {
    let normalized = status.trim().to_ascii_lowercase();
    let now = Utc::now().to_rfc3339();

    let changed = match normalized.as_str() {
        "claimed" | "in_progress" | "review" | "running" => conn.execute(
            "UPDATE tasks
                SET status = ?1,
                    started_at = COALESCE(started_at, ?2)
              WHERE id = ?3",
            params![normalized, now, task_id],
        )?,
        "done" | "failed" | "completed" | "cancelled" => conn.execute(
            "UPDATE tasks
                SET status = ?1,
                    completed_at = ?2,
                    started_at = COALESCE(started_at, ?2)
              WHERE id = ?3",
            params![normalized, now, task_id],
        )?,
        _ => conn.execute(
            "UPDATE tasks SET status = ?1 WHERE id = ?2",
            params![normalized, task_id],
        )?,
    };

    Ok(changed > 0)
}

/// Record task result output (insert-or-update by task_id).
pub fn record_task_result(
    conn: &Connection,
    task_id: &str,
    success: bool,
    output: &str,
    duration_ms: i64,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO task_results (task_id, success, output, duration_ms, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(task_id) DO UPDATE SET
            success = excluded.success,
            output = excluded.output,
            duration_ms = excluded.duration_ms,
            completed_at = excluded.completed_at",
        params![task_id, success as i32, output, duration_ms, now],
    )?;
    Ok(())
}

/// Assign a task to a node and mark it as running.
pub fn assign_task(conn: &Connection, task_id: &str, node_name: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let changed = conn.execute(
        "UPDATE tasks SET status = 'running', assigned_node = ?1, started_at = ?2
         WHERE id = ?3 AND status = 'pending'",
        params![node_name, now, task_id],
    )?;
    Ok(changed > 0)
}

/// Complete a task with a result.
pub fn complete_task(
    conn: &Connection,
    task_id: &str,
    success: bool,
    output: &str,
    duration_ms: i64,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE tasks SET status = ?1, completed_at = ?2 WHERE id = ?3",
        params![if success { "completed" } else { "failed" }, now, task_id],
    )?;

    conn.execute(
        "INSERT INTO task_results (task_id, success, output, duration_ms, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![task_id, success as i32, output, duration_ms, now],
    )?;

    Ok(())
}

// ─── Task Ownership Queries ────────────────────────────────────────────────

/// Claim task ownership with a lease.
///
/// Returns `true` when ownership is acquired/updated; `false` on contention.
pub fn ownership_claim(
    conn: &Connection,
    task_id: &str,
    owner_node: &str,
    lease_secs: i64,
) -> Result<bool> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let lease_expires_at = lease_expiry_iso(now, lease_secs);

    let changed = conn.execute(
        "INSERT INTO task_ownership (task_id, owner_node, lease_expires_at, status, handoff_target, updated_at)
         VALUES (?1, ?2, ?3, 'claimed', NULL, ?4)
         ON CONFLICT(task_id) DO UPDATE SET
            owner_node = excluded.owner_node,
            lease_expires_at = excluded.lease_expires_at,
            status = 'claimed',
            handoff_target = NULL,
            updated_at = excluded.updated_at
         WHERE task_ownership.owner_node = excluded.owner_node
            OR task_ownership.status = 'released'
            OR task_ownership.lease_expires_at <= excluded.updated_at",
        params![task_id, owner_node, lease_expires_at, now_iso],
    )?;

    Ok(changed > 0)
}

/// Renew an active lease. Only the current owner can renew.
pub fn ownership_renew(
    conn: &Connection,
    task_id: &str,
    owner_node: &str,
    lease_secs: i64,
) -> Result<bool> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let lease_expires_at = lease_expiry_iso(now, lease_secs);

    let changed = conn.execute(
        "UPDATE task_ownership
         SET lease_expires_at = ?1,
             status = 'claimed',
             handoff_target = NULL,
             updated_at = ?2
         WHERE task_id = ?3
           AND owner_node = ?4
           AND status != 'released'
           AND lease_expires_at > ?2",
        params![lease_expires_at, now_iso, task_id, owner_node],
    )?;

    Ok(changed > 0)
}

/// Release an active ownership lease.
pub fn ownership_release(conn: &Connection, task_id: &str, owner_node: &str) -> Result<bool> {
    let now_iso = now_iso();
    let changed = conn.execute(
        "UPDATE task_ownership
         SET status = 'released',
             handoff_target = NULL,
             lease_expires_at = ?1,
             updated_at = ?1
         WHERE task_id = ?2
           AND owner_node = ?3
           AND status != 'released'",
        params![now_iso, task_id, owner_node],
    )?;

    Ok(changed > 0)
}

/// Request handoff from one owner to another.
pub fn ownership_request_handoff(
    conn: &Connection,
    task_id: &str,
    from_owner: &str,
    to_owner: &str,
    reason: &str,
) -> Result<bool> {
    let now_iso = now_iso();

    conn.execute_batch("BEGIN IMMEDIATE;")?;

    let result = (|| -> Result<bool> {
        let changed = conn.execute(
            "UPDATE task_ownership
             SET status = 'handoff_requested',
                 handoff_target = ?1,
                 updated_at = ?2
             WHERE task_id = ?3
               AND owner_node = ?4
               AND status = 'claimed'
               AND lease_expires_at > ?2",
            params![to_owner, now_iso, task_id, from_owner],
        )?;

        if changed == 0 {
            return Ok(false);
        }

        conn.execute(
            "INSERT INTO ownership_events (task_id, event_type, from_owner, to_owner, reason, created_at)
             VALUES (?1, 'handoff_requested', ?2, ?3, ?4, ?5)",
            params![task_id, from_owner, to_owner, reason, now_iso],
        )?;

        Ok(true)
    })();

    match result {
        Ok(true) => {
            conn.execute_batch("COMMIT;")?;
            Ok(true)
        }
        Ok(false) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Ok(false)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(err)
        }
    }
}

/// Complete a requested handoff, transferring ownership.
pub fn ownership_complete_handoff(
    conn: &Connection,
    task_id: &str,
    from_owner: &str,
    to_owner: &str,
) -> Result<bool> {
    let now_iso = now_iso();

    conn.execute_batch("BEGIN IMMEDIATE;")?;

    let result = (|| -> Result<bool> {
        let changed = conn.execute(
            "UPDATE task_ownership
             SET owner_node = ?1,
                 status = 'claimed',
                 handoff_target = NULL,
                 updated_at = ?2
             WHERE task_id = ?3
               AND owner_node = ?4
               AND status = 'handoff_requested'
               AND handoff_target = ?1",
            params![to_owner, now_iso, task_id, from_owner],
        )?;

        if changed == 0 {
            return Ok(false);
        }

        conn.execute(
            "INSERT INTO ownership_events (task_id, event_type, from_owner, to_owner, reason, created_at)
             VALUES (?1, 'handoff_completed', ?2, ?3, NULL, ?4)",
            params![task_id, from_owner, to_owner, now_iso],
        )?;

        Ok(true)
    })();

    match result {
        Ok(true) => {
            conn.execute_batch("COMMIT;")?;
            Ok(true)
        }
        Ok(false) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Ok(false)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(err)
        }
    }
}

/// List task IDs with stale (expired) active ownership leases.
pub fn ownership_list_stale(conn: &Connection, now_iso: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT task_id
         FROM task_ownership
         WHERE status IN ('claimed', 'handoff_requested')
           AND lease_expires_at <= ?1
         ORDER BY task_id",
    )?;

    let rows = stmt.query_map([now_iso], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// ─── Memory Queries ────────────────────────────────────────────────────────

/// Upsert a memory entry (by namespace + key).
pub fn upsert_memory(conn: &Connection, mem: &MemoryRow) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories (id, namespace, key, content, embedding_json, metadata_json, importance, created_at, updated_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(namespace, key) DO UPDATE SET
            content = excluded.content,
            embedding_json = excluded.embedding_json,
            metadata_json = excluded.metadata_json,
            importance = excluded.importance,
            updated_at = ?9,
            expires_at = excluded.expires_at",
        params![
            mem.id, mem.namespace, mem.key, mem.content,
            mem.embedding_json, mem.metadata_json, mem.importance,
            mem.created_at, now, mem.expires_at
        ],
    )?;
    Ok(())
}

/// Search memories by keyword in content (simple LIKE search).
pub fn search_memories(
    conn: &Connection,
    namespace: Option<&str>,
    query: &str,
    limit: u32,
) -> Result<Vec<MemoryRow>> {
    let like_pattern = format!("%{query}%");

    let sql = if namespace.is_some() {
        "SELECT id, namespace, key, content, embedding_json, metadata_json,
                importance, created_at, updated_at, expires_at
         FROM memories
         WHERE namespace = ?1 AND content LIKE ?2
         ORDER BY importance DESC, updated_at DESC
         LIMIT ?3"
    } else {
        "SELECT id, namespace, key, content, embedding_json, metadata_json,
                importance, created_at, updated_at, expires_at
         FROM memories
         WHERE content LIKE ?1
         ORDER BY importance DESC, updated_at DESC
         LIMIT ?2"
    };

    let mut stmt = conn.prepare(sql)?;

    let rows = if let Some(ns) = namespace {
        stmt.query_map(params![ns, like_pattern, limit], map_memory_row)?
    } else {
        stmt.query_map(params![like_pattern, limit], map_memory_row)?
    };

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Get a specific memory by namespace and key.
pub fn get_memory(conn: &Connection, namespace: &str, key: &str) -> Result<Option<MemoryRow>> {
    let row = conn
        .query_row(
            "SELECT id, namespace, key, content, embedding_json, metadata_json,
                    importance, created_at, updated_at, expires_at
             FROM memories WHERE namespace = ?1 AND key = ?2",
            params![namespace, key],
            map_memory_row,
        )
        .optional()?;
    Ok(row)
}

/// Delete expired memories.
pub fn purge_expired_memories(conn: &Connection) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let deleted = conn.execute(
        "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
        [now],
    )?;
    Ok(deleted)
}

fn map_memory_row(row: &rusqlite::Row) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: row.get(0)?,
        namespace: row.get(1)?,
        key: row.get(2)?,
        content: row.get(3)?,
        embedding_json: row.get(4)?,
        metadata_json: row.get(5)?,
        importance: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        expires_at: row.get(9)?,
    })
}

// ─── Session Queries ───────────────────────────────────────────────────────

/// Insert a new session.
pub fn insert_session(conn: &Connection, session: &SessionRow) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (id, channel, user_id, node_name, status, metadata_json, created_at, last_activity, closed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            session.id, session.channel, session.user_id, session.node_name,
            session.status, session.metadata_json, session.created_at,
            session.last_activity, session.closed_at
        ],
    )?;
    Ok(())
}

/// Get a session by ID.
pub fn get_session(conn: &Connection, id: &str) -> Result<Option<SessionRow>> {
    let row = conn
        .query_row(
            "SELECT id, channel, user_id, node_name, status, metadata_json,
                    created_at, last_activity, closed_at
             FROM sessions WHERE id = ?1",
            [id],
            |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    user_id: row.get(2)?,
                    node_name: row.get(3)?,
                    status: row.get(4)?,
                    metadata_json: row.get(5)?,
                    created_at: row.get(6)?,
                    last_activity: row.get(7)?,
                    closed_at: row.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Find active sessions for a user on a channel.
pub fn find_active_sessions(
    conn: &Connection,
    channel: &str,
    user_id: &str,
) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, channel, user_id, node_name, status, metadata_json,
                created_at, last_activity, closed_at
         FROM sessions
         WHERE channel = ?1 AND user_id = ?2 AND status = 'active'
         ORDER BY last_activity DESC",
    )?;

    let rows = stmt.query_map(params![channel, user_id], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            channel: row.get(1)?,
            user_id: row.get(2)?,
            node_name: row.get(3)?,
            status: row.get(4)?,
            metadata_json: row.get(5)?,
            created_at: row.get(6)?,
            last_activity: row.get(7)?,
            closed_at: row.get(8)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Update session activity timestamp.
pub fn touch_session(conn: &Connection, id: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let changed = conn.execute(
        "UPDATE sessions SET last_activity = ?1 WHERE id = ?2 AND status = 'active'",
        params![now, id],
    )?;
    Ok(changed > 0)
}

/// Close a session.
pub fn close_session(conn: &Connection, id: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let changed = conn.execute(
        "UPDATE sessions SET status = 'closed', closed_at = ?1 WHERE id = ?2 AND status = 'active'",
        params![now, id],
    )?;
    Ok(changed > 0)
}

// ─── Audit Log Queries ────────────────────────────────────────────────────

/// Append an audit log entry.
pub fn audit_log(
    conn: &Connection,
    event_type: &str,
    actor: &str,
    target: Option<&str>,
    details_json: &str,
    node_name: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO audit_log (event_type, actor, target, details_json, node_name)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![event_type, actor, target, details_json, node_name],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Get recent audit log entries.
pub fn recent_audit_log(conn: &Connection, limit: u32) -> Result<Vec<AuditLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, event_type, actor, target, details_json, node_name
         FROM audit_log ORDER BY id DESC LIMIT ?1",
    )?;

    let rows = stmt.query_map([limit], |row| {
        Ok(AuditLogRow {
            id: row.get(0)?,
            timestamp: row.get(1)?,
            event_type: row.get(2)?,
            actor: row.get(3)?,
            target: row.get(4)?,
            details_json: row.get(5)?,
            node_name: row.get(6)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// ─── Autonomy Event Queries ────────────────────────────────────────────────

/// Insert an autonomy decision event.
pub fn insert_autonomy_event(
    conn: &Connection,
    event_type: &str,
    action_type: &str,
    decision: &str,
    reason: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO autonomy_events (event_type, action_type, decision, reason)
         VALUES (?1, ?2, ?3, ?4)",
        params![event_type, action_type, decision, reason],
    )?;
    Ok(conn.last_insert_rowid())
}

/// List most recent autonomy events (newest first).
pub fn list_recent_autonomy_events(conn: &Connection, limit: u32) -> Result<Vec<AutonomyEventRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, event_type, action_type, decision, reason, created_at
         FROM autonomy_events
         ORDER BY id DESC
         LIMIT ?1",
    )?;

    let rows = stmt.query_map([limit], |row| {
        Ok(AutonomyEventRow {
            id: row.get(0)?,
            event_type: row.get(1)?,
            action_type: row.get(2)?,
            decision: row.get(3)?,
            reason: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// ─── Telegram Media Ingest Queries ─────────────────────────────────────────

/// Insert a Telegram media ingest metadata record.
pub fn insert_telegram_media_ingest(
    conn: &Connection,
    chat_id: &str,
    message_id: &str,
    media_kind: &str,
    local_path: &str,
    mime_type: Option<&str>,
    size_bytes: Option<u64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO telegram_media_ingest (chat_id, message_id, media_kind, local_path, mime_type, size_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            chat_id,
            message_id,
            media_kind,
            local_path,
            mime_type,
            size_bytes.map(|value| value as i64),
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// List Telegram media ingest records, newest first.
pub fn list_telegram_media_ingest(
    conn: &Connection,
    chat_id: Option<&str>,
    limit: u32,
) -> Result<Vec<TelegramMediaIngestRow>> {
    let normalized_limit = limit.clamp(1, 1000);

    let mut stmt = if chat_id.is_some() {
        conn.prepare(
            "SELECT id, chat_id, message_id, media_kind, local_path, mime_type, size_bytes, created_at
             FROM telegram_media_ingest
             WHERE chat_id = ?1
             ORDER BY id DESC
             LIMIT ?2",
        )?
    } else {
        conn.prepare(
            "SELECT id, chat_id, message_id, media_kind, local_path, mime_type, size_bytes, created_at
             FROM telegram_media_ingest
             ORDER BY id DESC
             LIMIT ?1",
        )?
    };

    let rows = if let Some(chat) = chat_id {
        stmt.query_map(
            params![chat, normalized_limit],
            map_telegram_media_ingest_row,
        )?
    } else {
        stmt.query_map(params![normalized_limit], map_telegram_media_ingest_row)?
    };

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn map_telegram_media_ingest_row(row: &rusqlite::Row) -> rusqlite::Result<TelegramMediaIngestRow> {
    let size_bytes: Option<i64> = row.get(6)?;
    Ok(TelegramMediaIngestRow {
        id: row.get(0)?,
        chat_id: row.get(1)?,
        message_id: row.get(2)?,
        media_kind: row.get(3)?,
        local_path: row.get(4)?,
        mime_type: row.get(5)?,
        size_bytes: size_bytes.map(|value| value.max(0) as u64),
        created_at: row.get(7)?,
    })
}

// ─── Config KV Queries ─────────────────────────────────────────────────────

/// Set a config key-value pair.
pub fn config_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO config_kv (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now],
    )?;
    Ok(())
}

/// Get a config value by key.
pub fn config_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let val = conn
        .query_row("SELECT value FROM config_kv WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(val)
}

/// Delete a config key.
pub fn config_delete(conn: &Connection, key: &str) -> Result<bool> {
    let changed = conn.execute("DELETE FROM config_kv WHERE key = ?1", [key])?;
    Ok(changed > 0)
}

/// List all config key-value pairs.
pub fn config_list(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM config_kv ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Postgres fleet config — row types, query helpers, seed function
// ═══════════════════════════════════════════════════════════════════════════════

/// A fleet node row from the Postgres `fleet_nodes` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetNodeRow {
    pub name: String,
    pub ip: String,
    pub ssh_user: String,
    pub ram_gb: i32,
    pub cpu_cores: i32,
    pub os: String,
    pub role: String,
    pub election_priority: i32,
    pub hardware: String,
    pub alt_ips: JsonValue,
    pub capabilities: JsonValue,
    pub preferences: JsonValue,
    pub resources: JsonValue,
    pub status: String,
    /// Inference runtime: 'llama.cpp' | 'mlx' | 'vllm' | 'unknown'.
    /// Added in schema V11; defaults to 'unknown' for pre-existing rows.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Models directory on the node (default '~/models').
    #[serde(default = "default_models_dir")]
    pub models_dir: String,
    /// Disk quota for the models dir as a percentage of total disk (default 80).
    #[serde(default = "default_disk_quota_pct")]
    pub disk_quota_pct: i32,
    /// Concurrent defer-worker slots on this node (default 1). Scales agent-
    /// heavy workloads. Added in schema V12.
    #[serde(default = "default_sub_agent_count")]
    pub sub_agent_count: i32,
    /// GitHub owner/account this node is authenticated against (e.g.
    /// "venkat-oclaw"). NULL for existing nodes still on Taylor's PAT. V12.
    #[serde(default)]
    pub gh_account: Option<String>,
    /// Map of installed-tool versions:
    ///   {"os":{"current":"Ubuntu 24.04.4","latest":"Ubuntu 24.04.5","checked_at":"..."}}
    /// Populated every 6h by the daemon's version_check tick. V12.
    #[serde(default = "default_tooling")]
    pub tooling: JsonValue,
}

fn default_runtime() -> String { "unknown".to_string() }
fn default_models_dir() -> String { "~/models".to_string() }
fn default_disk_quota_pct() -> i32 { 80 }
fn default_sub_agent_count() -> i32 { 1 }
fn default_tooling() -> JsonValue { serde_json::json!({}) }

/// A fleet model row from the Postgres `fleet_models` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetModelRow {
    pub id: String,
    pub node_name: String,
    pub slug: String,
    pub name: String,
    pub family: String,
    pub port: i32,
    pub tier: i32,
    pub local_model: bool,
    pub lifecycle: String,
    pub mode: String,
    pub preferred_workloads: JsonValue,
}

// ─── Postgres Node Queries ───────────────────────────────────────────────────

/// List all fleet nodes from Postgres.
pub async fn pg_list_nodes(pool: &PgPool) -> Result<Vec<FleetNodeRow>> {
    let rows = sqlx::query(
        "SELECT name, ip, ssh_user, ram_gb, cpu_cores, os, role,
                election_priority, hardware, alt_ips, capabilities,
                preferences, resources, status,
                COALESCE(runtime, 'unknown') AS runtime,
                COALESCE(models_dir, '~/models') AS models_dir,
                COALESCE(disk_quota_pct, 80) AS disk_quota_pct,
                COALESCE(sub_agent_count, 1) AS sub_agent_count,
                gh_account,
                COALESCE(tooling, '{}'::jsonb) AS tooling
         FROM fleet_nodes ORDER BY election_priority, name",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| FleetNodeRow {
            name: r.get("name"),
            ip: r.get("ip"),
            ssh_user: r.get("ssh_user"),
            ram_gb: r.get("ram_gb"),
            cpu_cores: r.get("cpu_cores"),
            os: r.get("os"),
            role: r.get("role"),
            election_priority: r.get("election_priority"),
            hardware: r.get("hardware"),
            alt_ips: r.get("alt_ips"),
            capabilities: r.get("capabilities"),
            preferences: r.get("preferences"),
            resources: r.get("resources"),
            status: r.get("status"),
            runtime: r.get("runtime"),
            models_dir: r.get("models_dir"),
            disk_quota_pct: r.get("disk_quota_pct"),
            sub_agent_count: r.get("sub_agent_count"),
            gh_account: r.get("gh_account"),
            tooling: r.get("tooling"),
        })
        .collect())
}

/// Get a single fleet node by name from Postgres.
pub async fn pg_get_node(pool: &PgPool, name: &str) -> Result<Option<FleetNodeRow>> {
    let row = sqlx::query(
        "SELECT name, ip, ssh_user, ram_gb, cpu_cores, os, role,
                election_priority, hardware, alt_ips, capabilities,
                preferences, resources, status,
                COALESCE(runtime, 'unknown') AS runtime,
                COALESCE(models_dir, '~/models') AS models_dir,
                COALESCE(disk_quota_pct, 80) AS disk_quota_pct,
                COALESCE(sub_agent_count, 1) AS sub_agent_count,
                gh_account,
                COALESCE(tooling, '{}'::jsonb) AS tooling
         FROM fleet_nodes WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| FleetNodeRow {
        name: r.get("name"),
        ip: r.get("ip"),
        ssh_user: r.get("ssh_user"),
        ram_gb: r.get("ram_gb"),
        cpu_cores: r.get("cpu_cores"),
        os: r.get("os"),
        role: r.get("role"),
        election_priority: r.get("election_priority"),
        hardware: r.get("hardware"),
        alt_ips: r.get("alt_ips"),
        capabilities: r.get("capabilities"),
        preferences: r.get("preferences"),
        resources: r.get("resources"),
        status: r.get("status"),
        runtime: r.get("runtime"),
        models_dir: r.get("models_dir"),
        disk_quota_pct: r.get("disk_quota_pct"),
        sub_agent_count: r.get("sub_agent_count"),
        gh_account: r.get("gh_account"),
        tooling: r.get("tooling"),
    }))
}

/// Upsert a fleet node in Postgres.
pub async fn pg_upsert_node(pool: &PgPool, node: &FleetNodeRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_nodes (name, ip, ssh_user, ram_gb, cpu_cores, os, role,
                election_priority, hardware, alt_ips, capabilities, preferences, resources, status,
                runtime, models_dir, disk_quota_pct,
                sub_agent_count, gh_account, tooling, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                 $18, $19, $20, NOW())
         ON CONFLICT (name) DO UPDATE SET
            ip = EXCLUDED.ip,
            ssh_user = EXCLUDED.ssh_user,
            ram_gb = EXCLUDED.ram_gb,
            cpu_cores = EXCLUDED.cpu_cores,
            os = EXCLUDED.os,
            role = EXCLUDED.role,
            election_priority = EXCLUDED.election_priority,
            hardware = EXCLUDED.hardware,
            alt_ips = EXCLUDED.alt_ips,
            capabilities = EXCLUDED.capabilities,
            preferences = EXCLUDED.preferences,
            resources = EXCLUDED.resources,
            status = EXCLUDED.status,
            runtime = COALESCE(NULLIF(EXCLUDED.runtime, ''), fleet_nodes.runtime),
            models_dir = COALESCE(NULLIF(EXCLUDED.models_dir, ''), fleet_nodes.models_dir),
            disk_quota_pct = COALESCE(NULLIF(EXCLUDED.disk_quota_pct, 0), fleet_nodes.disk_quota_pct),
            sub_agent_count = COALESCE(NULLIF(EXCLUDED.sub_agent_count, 0), fleet_nodes.sub_agent_count),
            gh_account = COALESCE(EXCLUDED.gh_account, fleet_nodes.gh_account),
            tooling = CASE
                WHEN EXCLUDED.tooling = '{}'::jsonb THEN fleet_nodes.tooling
                ELSE EXCLUDED.tooling
            END,
            updated_at = NOW()",
    )
    .bind(&node.name)
    .bind(&node.ip)
    .bind(&node.ssh_user)
    .bind(node.ram_gb)
    .bind(node.cpu_cores)
    .bind(&node.os)
    .bind(&node.role)
    .bind(node.election_priority)
    .bind(&node.hardware)
    .bind(&node.alt_ips)
    .bind(&node.capabilities)
    .bind(&node.preferences)
    .bind(&node.resources)
    .bind(&node.status)
    .bind(&node.runtime)
    .bind(&node.models_dir)
    .bind(node.disk_quota_pct)
    .bind(node.sub_agent_count)
    .bind(&node.gh_account)
    .bind(&node.tooling)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Postgres Model Queries ──────────────────────────────────────────────────

/// List all fleet models from Postgres.
pub async fn pg_list_models(pool: &PgPool) -> Result<Vec<FleetModelRow>> {
    let rows = sqlx::query(
        "SELECT id, node_name, slug, name, family, port, tier,
                local_model, lifecycle, mode, preferred_workloads
         FROM fleet_models ORDER BY node_name, slug",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| FleetModelRow {
            id: r.get("id"),
            node_name: r.get("node_name"),
            slug: r.get("slug"),
            name: r.get("name"),
            family: r.get("family"),
            port: r.get("port"),
            tier: r.get("tier"),
            local_model: r.get("local_model"),
            lifecycle: r.get("lifecycle"),
            mode: r.get("mode"),
            preferred_workloads: r.get("preferred_workloads"),
        })
        .collect())
}

/// List fleet models for a specific node from Postgres.
pub async fn pg_list_models_for_node(pool: &PgPool, node: &str) -> Result<Vec<FleetModelRow>> {
    let rows = sqlx::query(
        "SELECT id, node_name, slug, name, family, port, tier,
                local_model, lifecycle, mode, preferred_workloads
         FROM fleet_models WHERE node_name = $1 ORDER BY slug",
    )
    .bind(node)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| FleetModelRow {
            id: r.get("id"),
            node_name: r.get("node_name"),
            slug: r.get("slug"),
            name: r.get("name"),
            family: r.get("family"),
            port: r.get("port"),
            tier: r.get("tier"),
            local_model: r.get("local_model"),
            lifecycle: r.get("lifecycle"),
            mode: r.get("mode"),
            preferred_workloads: r.get("preferred_workloads"),
        })
        .collect())
}

/// Upsert a fleet model in Postgres.
pub async fn pg_upsert_model(pool: &PgPool, model: &FleetModelRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_models (id, node_name, slug, name, family, port, tier,
                local_model, lifecycle, mode, preferred_workloads, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW())
         ON CONFLICT (id) DO UPDATE SET
            node_name = EXCLUDED.node_name,
            slug = EXCLUDED.slug,
            name = EXCLUDED.name,
            family = EXCLUDED.family,
            port = EXCLUDED.port,
            tier = EXCLUDED.tier,
            local_model = EXCLUDED.local_model,
            lifecycle = EXCLUDED.lifecycle,
            mode = EXCLUDED.mode,
            preferred_workloads = EXCLUDED.preferred_workloads,
            updated_at = NOW()",
    )
    .bind(&model.id)
    .bind(&model.node_name)
    .bind(&model.slug)
    .bind(&model.name)
    .bind(&model.family)
    .bind(model.port)
    .bind(model.tier)
    .bind(model.local_model)
    .bind(&model.lifecycle)
    .bind(&model.mode)
    .bind(&model.preferred_workloads)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Postgres Settings Queries ───────────────────────────────────────────────

/// Get a fleet setting from Postgres.
pub async fn pg_get_setting(pool: &PgPool, key: &str) -> Result<Option<JsonValue>> {
    let row = sqlx::query("SELECT value FROM fleet_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| r.get("value")))
}

/// Set a fleet setting in Postgres (upsert).
pub async fn pg_set_setting(pool: &PgPool, key: &str, value: &JsonValue) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_settings (key, value, updated_at)
         VALUES ($1, $2, NOW())
         ON CONFLICT (key) DO UPDATE SET
            value = EXCLUDED.value,
            updated_at = NOW()",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Fleet Secrets ─────────────────────────────────────────────────────────
// Plaintext at rest — acceptable for trusted internal fleet.
// Callers MUST NOT log secret values; prefer lengths or hashes when debugging.

/// A row from `fleet_secrets`. Contains the plaintext value — handle with care.
#[derive(Debug, Clone)]
pub struct FleetSecretRow {
    pub key: String,
    pub value: String,
    pub description: Option<String>,
    pub updated_by: Option<String>,
}

/// Fetch a single secret value by key. Returns `None` if missing.
pub async fn pg_get_secret(pool: &PgPool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM fleet_secrets WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get("value")))
}

/// Upsert a secret. `updated_by` is a free-form tag (node name, user, or tool).
pub async fn pg_set_secret(
    pool: &PgPool,
    key: &str,
    value: &str,
    description: Option<&str>,
    updated_by: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_secrets (key, value, description, updated_at, updated_by)
         VALUES ($1, $2, $3, NOW(), $4)
         ON CONFLICT (key) DO UPDATE SET
            value = EXCLUDED.value,
            description = COALESCE(EXCLUDED.description, fleet_secrets.description),
            updated_at = NOW(),
            updated_by = EXCLUDED.updated_by",
    )
    .bind(key)
    .bind(value)
    .bind(description)
    .bind(updated_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a secret by key. Returns true if a row was deleted.
pub async fn pg_delete_secret(pool: &PgPool, key: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM fleet_secrets WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ─── Model Lifecycle ───────────────────────────────────────────────────────

/// Catalog entry — what we can download.
#[derive(Debug, Clone)]
pub struct ModelCatalogRow {
    pub id: String,
    pub name: String,
    pub family: String,
    pub parameters: String,
    pub tier: i32,
    pub description: Option<String>,
    pub gated: bool,
    pub preferred_workloads: JsonValue,
    pub variants: JsonValue,
}

/// Upsert a catalog entry. Returns the id.
pub async fn pg_upsert_catalog(
    pool: &PgPool,
    row: &ModelCatalogRow,
) -> Result<String> {
    sqlx::query(
        "INSERT INTO fleet_model_catalog
            (id, name, family, parameters, tier, description, gated, preferred_workloads, variants, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
         ON CONFLICT (id) DO UPDATE SET
            name = EXCLUDED.name,
            family = EXCLUDED.family,
            parameters = EXCLUDED.parameters,
            tier = EXCLUDED.tier,
            description = EXCLUDED.description,
            gated = EXCLUDED.gated,
            preferred_workloads = EXCLUDED.preferred_workloads,
            variants = EXCLUDED.variants,
            updated_at = NOW()",
    )
    .bind(&row.id)
    .bind(&row.name)
    .bind(&row.family)
    .bind(&row.parameters)
    .bind(row.tier)
    .bind(&row.description)
    .bind(row.gated)
    .bind(&row.preferred_workloads)
    .bind(&row.variants)
    .execute(pool)
    .await?;
    Ok(row.id.clone())
}

/// List catalog entries sorted by tier (desc) then name (asc).
pub async fn pg_list_catalog(pool: &PgPool) -> Result<Vec<ModelCatalogRow>> {
    let rows = sqlx::query(
        "SELECT id, name, family, parameters, tier, description, gated, preferred_workloads, variants
           FROM fleet_model_catalog
          ORDER BY tier DESC, name ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| ModelCatalogRow {
        id: r.get("id"),
        name: r.get("name"),
        family: r.get("family"),
        parameters: r.get("parameters"),
        tier: r.get("tier"),
        description: r.get("description"),
        gated: r.get("gated"),
        preferred_workloads: r.get("preferred_workloads"),
        variants: r.get("variants"),
    }).collect())
}

/// Search catalog by substring on name/family/id (case-insensitive).
pub async fn pg_search_catalog(pool: &PgPool, query: &str) -> Result<Vec<ModelCatalogRow>> {
    let pattern = format!("%{}%", query.to_lowercase());
    let rows = sqlx::query(
        "SELECT id, name, family, parameters, tier, description, gated, preferred_workloads, variants
           FROM fleet_model_catalog
          WHERE LOWER(id) LIKE $1 OR LOWER(name) LIKE $1 OR LOWER(family) LIKE $1
          ORDER BY tier DESC, name ASC",
    )
    .bind(&pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| ModelCatalogRow {
        id: r.get("id"),
        name: r.get("name"),
        family: r.get("family"),
        parameters: r.get("parameters"),
        tier: r.get("tier"),
        description: r.get("description"),
        gated: r.get("gated"),
        preferred_workloads: r.get("preferred_workloads"),
        variants: r.get("variants"),
    }).collect())
}

/// Fetch one catalog entry by id.
pub async fn pg_get_catalog(pool: &PgPool, id: &str) -> Result<Option<ModelCatalogRow>> {
    let row = sqlx::query(
        "SELECT id, name, family, parameters, tier, description, gated, preferred_workloads, variants
           FROM fleet_model_catalog WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(|r| ModelCatalogRow {
        id: r.get("id"),
        name: r.get("name"),
        family: r.get("family"),
        parameters: r.get("parameters"),
        tier: r.get("tier"),
        description: r.get("description"),
        gated: r.get("gated"),
        preferred_workloads: r.get("preferred_workloads"),
        variants: r.get("variants"),
    }))
}

/// Library entry — a model file on disk on a specific node.
#[derive(Debug, Clone)]
pub struct ModelLibraryRow {
    pub id: String,
    pub node_name: String,
    pub catalog_id: String,
    pub runtime: String,
    pub quant: Option<String>,
    pub file_path: String,
    pub size_bytes: i64,
    pub sha256: Option<String>,
    pub downloaded_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub source_url: Option<String>,
}

/// Upsert a library entry keyed by (node_name, file_path). Returns library id.
pub async fn pg_upsert_library(
    pool: &PgPool,
    node_name: &str,
    catalog_id: &str,
    runtime: &str,
    quant: Option<&str>,
    file_path: &str,
    size_bytes: i64,
    sha256: Option<&str>,
    source_url: Option<&str>,
) -> Result<String> {
    let row = sqlx::query(
        "INSERT INTO fleet_model_library
            (node_name, catalog_id, runtime, quant, file_path, size_bytes, sha256, source_url)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (node_name, file_path) DO UPDATE SET
            catalog_id = EXCLUDED.catalog_id,
            runtime = EXCLUDED.runtime,
            quant = COALESCE(EXCLUDED.quant, fleet_model_library.quant),
            size_bytes = EXCLUDED.size_bytes,
            sha256 = COALESCE(EXCLUDED.sha256, fleet_model_library.sha256),
            source_url = COALESCE(EXCLUDED.source_url, fleet_model_library.source_url)
         RETURNING id",
    )
    .bind(node_name)
    .bind(catalog_id)
    .bind(runtime)
    .bind(quant)
    .bind(file_path)
    .bind(size_bytes)
    .bind(sha256)
    .bind(source_url)
    .fetch_one(pool)
    .await?;
    let id: sqlx::types::Uuid = row.get("id");
    Ok(id.to_string())
}

/// List all library entries, optionally filtered by node.
pub async fn pg_list_library(pool: &PgPool, node_name: Option<&str>) -> Result<Vec<ModelLibraryRow>> {
    let rows = if let Some(n) = node_name {
        sqlx::query(
            "SELECT * FROM fleet_model_library WHERE node_name = $1 ORDER BY node_name, catalog_id",
        )
        .bind(n)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query("SELECT * FROM fleet_model_library ORDER BY node_name, catalog_id")
            .fetch_all(pool)
            .await?
    };
    Ok(rows.iter().map(|r| {
        let id: sqlx::types::Uuid = r.get("id");
        ModelLibraryRow {
            id: id.to_string(),
            node_name: r.get("node_name"),
            catalog_id: r.get("catalog_id"),
            runtime: r.get("runtime"),
            quant: r.get("quant"),
            file_path: r.get("file_path"),
            size_bytes: r.get("size_bytes"),
            sha256: r.get("sha256"),
            downloaded_at: r.get("downloaded_at"),
            last_used_at: r.get("last_used_at"),
            source_url: r.get("source_url"),
        }
    }).collect())
}

/// Delete a library entry. Returns true if a row was removed.
pub async fn pg_delete_library(pool: &PgPool, id: &str) -> Result<bool> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let r = sqlx::query("DELETE FROM fleet_model_library WHERE id = $1")
        .bind(uuid)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Deployment row — a currently-running inference process.
#[derive(Debug, Clone)]
pub struct ModelDeploymentRow {
    pub id: String,
    pub node_name: String,
    pub library_id: Option<String>,
    pub catalog_id: Option<String>,
    pub runtime: String,
    pub port: i32,
    pub pid: Option<i32>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_health_at: Option<chrono::DateTime<chrono::Utc>>,
    pub health_status: String,
    pub context_window: Option<i32>,
    pub tokens_used: i64,
    pub request_count: i64,
}

/// List deployments optionally filtered by node.
pub async fn pg_list_deployments(pool: &PgPool, node_name: Option<&str>) -> Result<Vec<ModelDeploymentRow>> {
    let rows = if let Some(n) = node_name {
        sqlx::query("SELECT * FROM fleet_model_deployments WHERE node_name = $1 ORDER BY node_name, port")
            .bind(n)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query("SELECT * FROM fleet_model_deployments ORDER BY node_name, port")
            .fetch_all(pool)
            .await?
    };
    Ok(rows.iter().map(|r| {
        let id: sqlx::types::Uuid = r.get("id");
        let lib_id: Option<sqlx::types::Uuid> = r.get("library_id");
        ModelDeploymentRow {
            id: id.to_string(),
            node_name: r.get("node_name"),
            library_id: lib_id.map(|u| u.to_string()),
            catalog_id: r.get("catalog_id"),
            runtime: r.get("runtime"),
            port: r.get("port"),
            pid: r.get("pid"),
            started_at: r.get("started_at"),
            last_health_at: r.get("last_health_at"),
            health_status: r.get("health_status"),
            context_window: r.get("context_window"),
            tokens_used: r.get("tokens_used"),
            request_count: r.get("request_count"),
        }
    }).collect())
}

/// Upsert a deployment (node + port is unique).
pub async fn pg_upsert_deployment(
    pool: &PgPool,
    node_name: &str,
    library_id: Option<&str>,
    catalog_id: Option<&str>,
    runtime: &str,
    port: i32,
    pid: Option<i32>,
    health_status: &str,
    context_window: Option<i32>,
) -> Result<String> {
    let lib_uuid = library_id
        .map(|s| sqlx::types::Uuid::parse_str(s)
            .map_err(|e| crate::error::DbError::NotFound(format!("bad library uuid {s}: {e}"))))
        .transpose()?;
    let row = sqlx::query(
        "INSERT INTO fleet_model_deployments
            (node_name, library_id, catalog_id, runtime, port, pid, health_status, context_window, last_health_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
         ON CONFLICT (node_name, port) DO UPDATE SET
            library_id = EXCLUDED.library_id,
            catalog_id = EXCLUDED.catalog_id,
            runtime = EXCLUDED.runtime,
            pid = EXCLUDED.pid,
            health_status = EXCLUDED.health_status,
            context_window = COALESCE(EXCLUDED.context_window, fleet_model_deployments.context_window),
            last_health_at = NOW()
         RETURNING id",
    )
    .bind(node_name)
    .bind(lib_uuid)
    .bind(catalog_id)
    .bind(runtime)
    .bind(port)
    .bind(pid)
    .bind(health_status)
    .bind(context_window)
    .fetch_one(pool)
    .await?;
    let id: sqlx::types::Uuid = row.get("id");
    Ok(id.to_string())
}

/// Remove a deployment (when a model is unloaded).
pub async fn pg_delete_deployment(pool: &PgPool, id: &str) -> Result<bool> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let r = sqlx::query("DELETE FROM fleet_model_deployments WHERE id = $1")
        .bind(uuid)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Record a disk usage sample.
pub async fn pg_insert_disk_usage(
    pool: &PgPool,
    node_name: &str,
    models_dir: &str,
    total_bytes: i64,
    used_bytes: i64,
    free_bytes: i64,
    models_bytes: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_disk_usage (node_name, models_dir, total_bytes, used_bytes, free_bytes, models_bytes)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(node_name)
    .bind(models_dir)
    .bind(total_bytes)
    .bind(used_bytes)
    .bind(free_bytes)
    .bind(models_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get the latest disk usage sample per node.
pub async fn pg_latest_disk_usage(pool: &PgPool) -> Result<Vec<(String, String, i64, i64, i64, i64, chrono::DateTime<chrono::Utc>)>> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (node_name)
                node_name, models_dir, total_bytes, used_bytes, free_bytes, models_bytes, sampled_at
           FROM fleet_disk_usage
          ORDER BY node_name, sampled_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| (
        r.get::<String, _>("node_name"),
        r.get::<String, _>("models_dir"),
        r.get::<i64, _>("total_bytes"),
        r.get::<i64, _>("used_bytes"),
        r.get::<i64, _>("free_bytes"),
        r.get::<i64, _>("models_bytes"),
        r.get::<chrono::DateTime<chrono::Utc>, _>("sampled_at"),
    )).collect())
}

/// A model lifecycle job (download, delete, load, etc.) — tracks progress.
#[derive(Debug, Clone)]
pub struct ModelJobRow {
    pub id: String,
    pub node_name: String,
    pub kind: String,
    pub target_catalog_id: Option<String>,
    pub target_library_id: Option<String>,
    pub params: JsonValue,
    pub status: String,
    pub progress_pct: f32,
    pub bytes_done: Option<i64>,
    pub bytes_total: Option<i64>,
    pub eta_seconds: Option<i32>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub error_message: Option<String>,
}

/// Insert a new model lifecycle job. Returns job id.
pub async fn pg_create_job(
    pool: &PgPool,
    node_name: &str,
    kind: &str,
    target_catalog_id: Option<&str>,
    target_library_id: Option<&str>,
    params: &JsonValue,
) -> Result<String> {
    let lib_uuid = target_library_id
        .map(|s| sqlx::types::Uuid::parse_str(s)
            .map_err(|e| crate::error::DbError::NotFound(format!("bad library uuid {s}: {e}"))))
        .transpose()?;
    let row = sqlx::query(
        "INSERT INTO fleet_model_jobs (node_name, kind, target_catalog_id, target_library_id, params)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(node_name)
    .bind(kind)
    .bind(target_catalog_id)
    .bind(lib_uuid)
    .bind(params)
    .fetch_one(pool)
    .await?;
    let id: sqlx::types::Uuid = row.get("id");
    Ok(id.to_string())
}

/// Update job progress. Any field can be left None to keep its current value.
pub async fn pg_update_job_progress(
    pool: &PgPool,
    id: &str,
    status: Option<&str>,
    progress_pct: Option<f32>,
    bytes_done: Option<i64>,
    bytes_total: Option<i64>,
    eta_seconds: Option<i32>,
    error_message: Option<&str>,
) -> Result<()> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    sqlx::query(
        "UPDATE fleet_model_jobs SET
            status = COALESCE($2, status),
            progress_pct = COALESCE($3, progress_pct),
            bytes_done = COALESCE($4, bytes_done),
            bytes_total = COALESCE($5, bytes_total),
            eta_seconds = COALESCE($6, eta_seconds),
            error_message = COALESCE($7, error_message),
            started_at = COALESCE(started_at, CASE WHEN $2 = 'running' THEN NOW() ELSE started_at END),
            completed_at = CASE WHEN $2 IN ('completed', 'failed', 'cancelled') THEN NOW() ELSE completed_at END
          WHERE id = $1",
    )
    .bind(uuid)
    .bind(status)
    .bind(progress_pct)
    .bind(bytes_done)
    .bind(bytes_total)
    .bind(eta_seconds)
    .bind(error_message)
    .execute(pool)
    .await?;
    Ok(())
}

/// List jobs (optionally filtered by status).
pub async fn pg_list_jobs(pool: &PgPool, status: Option<&str>, limit: i64) -> Result<Vec<ModelJobRow>> {
    let rows = if let Some(s) = status {
        sqlx::query("SELECT * FROM fleet_model_jobs WHERE status = $1 ORDER BY created_at DESC LIMIT $2")
            .bind(s)
            .bind(limit)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query("SELECT * FROM fleet_model_jobs ORDER BY created_at DESC LIMIT $1")
            .bind(limit)
            .fetch_all(pool)
            .await?
    };
    Ok(rows.iter().map(|r| {
        let id: sqlx::types::Uuid = r.get("id");
        let lib_id: Option<sqlx::types::Uuid> = r.get("target_library_id");
        ModelJobRow {
            id: id.to_string(),
            node_name: r.get("node_name"),
            kind: r.get("kind"),
            target_catalog_id: r.get("target_catalog_id"),
            target_library_id: lib_id.map(|u| u.to_string()),
            params: r.get("params"),
            status: r.get("status"),
            progress_pct: r.get("progress_pct"),
            bytes_done: r.get("bytes_done"),
            bytes_total: r.get("bytes_total"),
            eta_seconds: r.get("eta_seconds"),
            started_at: r.get("started_at"),
            completed_at: r.get("completed_at"),
            created_at: r.get("created_at"),
            error_message: r.get("error_message"),
        }
    }).collect())
}

// ─── Onboarding: SSH keys + mesh status (schema V12) ──────────────────────

/// One SSH key row for a fleet node.
#[derive(Debug, Clone)]
pub struct NodeSshKeyRow {
    pub node_name: String,
    pub key_purpose: String,   // 'user' | 'host'
    pub public_key: String,
    pub key_type: String,      // 'ed25519' | 'rsa' | 'ecdsa'
    pub fingerprint: String,
    pub added_at: chrono::DateTime<chrono::Utc>,
}

/// Upsert a public key for a node. Idempotent on (node_name, fingerprint).
pub async fn pg_insert_node_ssh_key(
    pool: &PgPool,
    node_name: &str,
    key_purpose: &str,
    public_key: &str,
    key_type: &str,
    fingerprint: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_node_ssh_keys (node_name, key_purpose, public_key, key_type, fingerprint)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (node_name, fingerprint) DO UPDATE SET
            public_key = EXCLUDED.public_key,
            key_type = EXCLUDED.key_type,
            key_purpose = EXCLUDED.key_purpose",
    )
    .bind(node_name)
    .bind(key_purpose)
    .bind(public_key)
    .bind(key_type)
    .bind(fingerprint)
    .execute(pool)
    .await?;
    Ok(())
}

/// List SSH keys for a node (optionally filtered by purpose: 'user' or 'host').
pub async fn pg_list_node_ssh_keys(
    pool: &PgPool,
    node_name: &str,
    purpose: Option<&str>,
) -> Result<Vec<NodeSshKeyRow>> {
    let rows = if let Some(p) = purpose {
        sqlx::query(
            "SELECT * FROM fleet_node_ssh_keys
              WHERE node_name = $1 AND key_purpose = $2
              ORDER BY added_at",
        )
        .bind(node_name)
        .bind(p)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT * FROM fleet_node_ssh_keys WHERE node_name = $1 ORDER BY added_at",
        )
        .bind(node_name)
        .fetch_all(pool)
        .await?
    };
    Ok(rows.iter().map(|r| NodeSshKeyRow {
        node_name: r.get("node_name"),
        key_purpose: r.get("key_purpose"),
        public_key: r.get("public_key"),
        key_type: r.get("key_type"),
        fingerprint: r.get("fingerprint"),
        added_at: r.get("added_at"),
    }).collect())
}

/// Delete all SSH keys for a node (used during `ff onboard revoke`).
pub async fn pg_delete_node_ssh_keys(pool: &PgPool, node_name: &str) -> Result<u64> {
    let r = sqlx::query("DELETE FROM fleet_node_ssh_keys WHERE node_name = $1")
        .bind(node_name)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// One row in the mesh-reachability matrix.
#[derive(Debug, Clone)]
pub struct MeshStatusRow {
    pub src_node: String,
    pub dst_node: String,
    pub status: String,             // 'ok' | 'failed' | 'pending'
    pub last_checked: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub attempts: i32,
}

/// Upsert one (src, dst) mesh check result.
pub async fn pg_upsert_mesh_status(
    pool: &PgPool,
    src_node: &str,
    dst_node: &str,
    status: &str,
    last_error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_mesh_status (src_node, dst_node, status, last_checked, last_error, attempts)
         VALUES ($1, $2, $3, NOW(), $4, 1)
         ON CONFLICT (src_node, dst_node) DO UPDATE SET
            status = EXCLUDED.status,
            last_checked = NOW(),
            last_error = EXCLUDED.last_error,
            attempts = fleet_mesh_status.attempts + 1",
    )
    .bind(src_node)
    .bind(dst_node)
    .bind(status)
    .bind(last_error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch the full mesh matrix; optionally filter by node (returns rows where
/// src_node = node OR dst_node = node).
pub async fn pg_list_mesh_status(
    pool: &PgPool,
    node: Option<&str>,
) -> Result<Vec<MeshStatusRow>> {
    let rows = if let Some(n) = node {
        sqlx::query(
            "SELECT * FROM fleet_mesh_status
              WHERE src_node = $1 OR dst_node = $1
              ORDER BY src_node, dst_node",
        )
        .bind(n)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query("SELECT * FROM fleet_mesh_status ORDER BY src_node, dst_node")
            .fetch_all(pool)
            .await?
    };
    Ok(rows.iter().map(|r| MeshStatusRow {
        src_node: r.get("src_node"),
        dst_node: r.get("dst_node"),
        status: r.get("status"),
        last_checked: r.get("last_checked"),
        last_error: r.get("last_error"),
        attempts: r.get("attempts"),
    }).collect())
}

/// Remove all mesh-status rows involving a given node (used during revoke).
pub async fn pg_delete_mesh_status_for_node(pool: &PgPool, node: &str) -> Result<u64> {
    let r = sqlx::query(
        "DELETE FROM fleet_mesh_status WHERE src_node = $1 OR dst_node = $1",
    )
    .bind(node)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

// ─── Deferred Task Queue ───────────────────────────────────────────────────

/// One row of the deferred_tasks table. Payload/trigger_spec/result/required_caps are free-form JSON.
#[derive(Debug, Clone)]
pub struct DeferredTaskRow {
    pub id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub created_by: Option<String>,
    pub title: String,
    pub kind: String,
    pub payload: JsonValue,
    pub trigger_type: String,
    pub trigger_spec: JsonValue,
    pub preferred_node: Option<String>,
    pub required_caps: JsonValue,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub next_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub result: Option<JsonValue>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Insert a new deferred task. Returns the generated UUID.
pub async fn pg_enqueue_deferred(
    pool: &PgPool,
    title: &str,
    kind: &str,
    payload: &JsonValue,
    trigger_type: &str,
    trigger_spec: &JsonValue,
    preferred_node: Option<&str>,
    required_caps: &JsonValue,
    created_by: Option<&str>,
    max_attempts: Option<i32>,
) -> Result<String> {
    let row = sqlx::query(
        "INSERT INTO deferred_tasks
            (title, kind, payload, trigger_type, trigger_spec, preferred_node,
             required_caps, created_by, max_attempts)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($9, 5))
         RETURNING id",
    )
    .bind(title)
    .bind(kind)
    .bind(payload)
    .bind(trigger_type)
    .bind(trigger_spec)
    .bind(preferred_node)
    .bind(required_caps)
    .bind(created_by)
    .bind(max_attempts)
    .fetch_one(pool)
    .await?;
    let id: sqlx::types::Uuid = row.get("id");
    Ok(id.to_string())
}

/// List deferred tasks filtered by status (None = all). Newest first.
pub async fn pg_list_deferred(
    pool: &PgPool,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<DeferredTaskRow>> {
    let rows = if let Some(s) = status {
        sqlx::query(
            "SELECT * FROM deferred_tasks WHERE status = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(s)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query("SELECT * FROM deferred_tasks ORDER BY created_at DESC LIMIT $1")
            .bind(limit)
            .fetch_all(pool)
            .await?
    };
    Ok(rows.iter().map(row_to_deferred).collect())
}

/// Fetch a single deferred task by id. Returns None if missing.
pub async fn pg_get_deferred(pool: &PgPool, id: &str) -> Result<Option<DeferredTaskRow>> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let row = sqlx::query("SELECT * FROM deferred_tasks WHERE id = $1")
        .bind(uuid)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_deferred))
}

/// Cancel a deferred task by id. Only allowed from pending/dispatchable/failed states.
/// Returns true if a row was updated.
pub async fn pg_cancel_deferred(pool: &PgPool, id: &str) -> Result<bool> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let r = sqlx::query(
        "UPDATE deferred_tasks
            SET status = 'cancelled', completed_at = NOW()
          WHERE id = $1
            AND status IN ('pending', 'dispatchable', 'failed')",
    )
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Mark a failed task for retry: reset to 'pending' and clear the claim.
/// Returns true if a row was updated.
pub async fn pg_retry_deferred(pool: &PgPool, id: &str) -> Result<bool> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let r = sqlx::query(
        "UPDATE deferred_tasks
            SET status = 'pending',
                claimed_by = NULL,
                claimed_at = NULL,
                next_attempt_at = NOW(),
                last_error = NULL
          WHERE id = $1 AND status IN ('failed', 'cancelled')",
    )
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Promote pending tasks to 'dispatchable' when their trigger conditions are met.
/// `online_nodes` is the set of node names currently reachable.
/// `now` is the current UTC time (for at_time triggers).
/// Returns the number of tasks promoted.
pub async fn pg_scheduler_pass(
    pool: &PgPool,
    online_nodes: &[String],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<u64> {
    // node_online: promote if any node in trigger_spec.node matches online_nodes.
    // Using JSONB -> for key access; ANY() for set membership.
    let node_online_promoted = if online_nodes.is_empty() {
        0
    } else {
        sqlx::query(
            "UPDATE deferred_tasks
                SET status = 'dispatchable',
                    next_attempt_at = NOW()
              WHERE status = 'pending'
                AND trigger_type = 'node_online'
                AND (trigger_spec->>'node') = ANY($1)",
        )
        .bind(online_nodes)
        .execute(pool)
        .await?
        .rows_affected()
    };

    // at_time: promote if trigger_spec.at <= now.
    let at_time_promoted = sqlx::query(
        "UPDATE deferred_tasks
            SET status = 'dispatchable',
                next_attempt_at = NOW()
          WHERE status = 'pending'
            AND trigger_type = 'at_time'
            AND (trigger_spec->>'at')::timestamptz <= $1",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();

    // manual / now: promote immediately (manual is for retry loops; 'now' is fire-and-forget).
    let immediate_promoted = sqlx::query(
        "UPDATE deferred_tasks
            SET status = 'dispatchable',
                next_attempt_at = NOW()
          WHERE status = 'pending'
            AND trigger_type IN ('manual', 'now')
            AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())",
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(node_online_promoted + at_time_promoted + immediate_promoted)
}

/// Atomic worker claim: grab one dispatchable task that matches the worker's capabilities.
/// Uses FOR UPDATE SKIP LOCKED for race-free multi-worker claim semantics.
///
/// Claim precedence:
///   1. Tasks whose `preferred_node` matches this worker (the "home" worker claims)
///   2. Tasks with `preferred_node IS NULL` (any worker can handle)
///   3. Tasks whose `preferred_node` is set to a DIFFERENT node, but the task has
///      been `dispatchable` for >2 minutes — assume the preferred node has no
///      live worker and let any other worker pick it up and route via SSH.
pub async fn pg_claim_deferred(
    pool: &PgPool,
    worker_node: &str,
) -> Result<Option<DeferredTaskRow>> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT * FROM deferred_tasks
          WHERE status = 'dispatchable'
            AND (
                 preferred_node IS NULL
              OR preferred_node = $1
              OR next_attempt_at <= NOW() - INTERVAL '2 minutes'
            )
            AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())
          ORDER BY
            (preferred_node = $1) DESC NULLS LAST,
            (preferred_node IS NULL) DESC,
            created_at ASC
          FOR UPDATE SKIP LOCKED
          LIMIT 1",
    )
    .bind(worker_node)
    .fetch_optional(&mut *tx)
    .await?;

    let claimed = if let Some(r) = row {
        let id: sqlx::types::Uuid = r.get("id");
        sqlx::query(
            "UPDATE deferred_tasks
                SET status = 'running',
                    claimed_by = $1,
                    claimed_at = NOW(),
                    attempts = attempts + 1
              WHERE id = $2",
        )
        .bind(worker_node)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        Some(row_to_deferred(&r))
    } else {
        None
    };

    tx.commit().await?;
    Ok(claimed)
}

/// Operator-initiated promotion: flip a pending task (any trigger_type) to
/// dispatchable so the next worker claims it. Used by the `/versions` dashboard
/// "Apply on <node>" button and by the mesh-status "Retry" click.
pub async fn pg_promote_deferred(pool: &PgPool, id: &str) -> Result<bool> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let affected = sqlx::query(
        "UPDATE deferred_tasks
            SET status = 'dispatchable', next_attempt_at = NOW()
          WHERE id = $1 AND status = 'pending'",
    )
    .bind(uuid)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Finalize a deferred task after execution. `success = true` → completed; false → failed.
/// On failure, attempts are compared to max_attempts to decide retry vs terminal.
pub async fn pg_finish_deferred(
    pool: &PgPool,
    id: &str,
    success: bool,
    result: Option<&JsonValue>,
    error: Option<&str>,
) -> Result<()> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    if success {
        sqlx::query(
            "UPDATE deferred_tasks
                SET status = 'completed',
                    result = $1,
                    completed_at = NOW()
              WHERE id = $2",
        )
        .bind(result)
        .bind(uuid)
        .execute(pool)
        .await?;
    } else {
        // Retry if attempts < max_attempts; else terminal fail.
        sqlx::query(
            "UPDATE deferred_tasks
                SET status = CASE
                        WHEN attempts >= max_attempts THEN 'failed'
                        ELSE 'pending'
                    END,
                    last_error = $1,
                    claimed_by = NULL,
                    claimed_at = NULL,
                    -- Exponential backoff capped at 4h: 1m, 5m, 30m, 1h, 4h
                    next_attempt_at = NOW() + (LEAST(240, GREATEST(1, POWER(5, attempts)::int)) * INTERVAL '1 minute')
              WHERE id = $2",
        )
        .bind(error)
        .bind(uuid)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Map a sqlx Row into a DeferredTaskRow.
fn row_to_deferred(r: &sqlx::postgres::PgRow) -> DeferredTaskRow {
    let id: sqlx::types::Uuid = r.get("id");
    DeferredTaskRow {
        id: id.to_string(),
        created_at: r.get("created_at"),
        created_by: r.get("created_by"),
        title: r.get("title"),
        kind: r.get("kind"),
        payload: r.get("payload"),
        trigger_type: r.get("trigger_type"),
        trigger_spec: r.get("trigger_spec"),
        preferred_node: r.get("preferred_node"),
        required_caps: r.get("required_caps"),
        status: r.get("status"),
        attempts: r.get("attempts"),
        max_attempts: r.get("max_attempts"),
        next_attempt_at: r.get("next_attempt_at"),
        claimed_by: r.get("claimed_by"),
        claimed_at: r.get("claimed_at"),
        last_error: r.get("last_error"),
        result: r.get("result"),
        completed_at: r.get("completed_at"),
    }
}

/// List secrets metadata (key, description, updated_by, updated_at) — does NOT return values.
/// Use `pg_get_secret` when a specific value is needed.
pub async fn pg_list_secrets(pool: &PgPool) -> Result<Vec<(String, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>)>> {
    let rows = sqlx::query(
        "SELECT key, description, updated_by, updated_at
         FROM fleet_secrets ORDER BY key",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| (
        r.get::<String, _>("key"),
        r.get::<Option<String>, _>("description"),
        r.get::<Option<String>, _>("updated_by"),
        r.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
    )).collect())
}

// ─── Seed from FleetConfig ───────────────────────────────────────────────────

/// Seed Postgres fleet tables from a parsed `FleetConfig`.
///
/// Idempotent — uses ON CONFLICT DO UPDATE so first run populates and
/// subsequent runs refresh to match fleet.toml.
pub async fn seed_from_fleet_toml(
    pool: &PgPool,
    config: &ff_core::config::FleetConfig,
) -> Result<()> {
    use tracing::info;

    let mut node_count = 0u32;
    let mut model_count = 0u32;

    for (name, node_cfg) in &config.nodes {
        // Resolve ram/cpu from top-level fields or resources sub-struct.
        let ram_gb = node_cfg
            .ram_gb
            .or_else(|| node_cfg.resources.as_ref().and_then(|r| r.ram_gb))
            .unwrap_or(0) as i32;
        let cpu_cores = node_cfg
            .cpu_cores
            .or_else(|| node_cfg.resources.as_ref().and_then(|r| r.cpu_cores))
            .unwrap_or(0) as i32;

        let capabilities_json = node_cfg
            .capabilities
            .as_ref()
            .map(|c| serde_json::to_value(c).unwrap_or_default())
            .unwrap_or(JsonValue::Object(serde_json::Map::new()));

        let preferences_json = node_cfg
            .preferences
            .as_ref()
            .map(|p| serde_json::to_value(p).unwrap_or_default())
            .unwrap_or(JsonValue::Object(serde_json::Map::new()));

        let resources_json = node_cfg
            .resources
            .as_ref()
            .map(|r| serde_json::to_value(r).unwrap_or_default())
            .unwrap_or(JsonValue::Object(serde_json::Map::new()));

        let node_row = FleetNodeRow {
            name: name.clone(),
            ip: node_cfg.ip.clone(),
            ssh_user: node_cfg.ssh_user.clone().unwrap_or_else(|| "root".into()),
            ram_gb,
            cpu_cores,
            os: node_cfg.os.clone().unwrap_or_default(),
            role: format!("{}", node_cfg.role).to_lowercase(),
            election_priority: node_cfg.election_priority.unwrap_or(100) as i32,
            hardware: String::new(),
            alt_ips: serde_json::to_value(&node_cfg.alt_ips).unwrap_or_default(),
            capabilities: capabilities_json,
            preferences: preferences_json,
            resources: resources_json,
            status: "online".into(),
            runtime: "unknown".into(),
            models_dir: "~/models".into(),
            disk_quota_pct: 80,
            sub_agent_count: 1,
            gh_account: None,
            tooling: serde_json::json!({}),
        };

        pg_upsert_node(pool, &node_row).await?;
        node_count += 1;

        // Insert models for this node.
        for (slug, model_cfg) in &node_cfg.models {
            let model_id = format!("{name}:{slug}");
            let model_row = FleetModelRow {
                id: model_id,
                node_name: name.clone(),
                slug: slug.clone(),
                name: model_cfg.name.clone(),
                family: model_cfg.family.clone().unwrap_or_default(),
                port: model_cfg.port.unwrap_or(0) as i32,
                tier: model_cfg.tier as i32,
                local_model: model_cfg.local.unwrap_or(true),
                lifecycle: model_cfg.lifecycle.clone().unwrap_or_else(|| "production".into()),
                mode: model_cfg.mode.clone().unwrap_or_else(|| "always_on".into()),
                preferred_workloads: serde_json::to_value(&model_cfg.preferred_workloads)
                    .unwrap_or_default(),
            };

            pg_upsert_model(pool, &model_row).await?;
            model_count += 1;
        }
    }

    // Seed settings from various config sections.
    pg_set_setting(pool, "scheduling", &serde_json::to_value(&config.scheduling)?).await?;
    pg_set_setting(pool, "ports", &serde_json::to_value(&config.ports)?).await?;
    pg_set_setting(pool, "llm", &serde_json::to_value(&config.llm)?).await?;
    pg_set_setting(pool, "enrollment", &serde_json::to_value(&config.enrollment)?).await?;
    pg_set_setting(pool, "fleet", &serde_json::to_value(&config.fleet)?).await?;

    info!(
        nodes = node_count,
        models = model_count,
        "seeded postgres fleet tables from fleet.toml"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::run_migrations;
    use rusqlite::Connection;
    use uuid::Uuid;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn test_node_crud() {
        let conn = setup();
        let node = NodeRow {
            id: Uuid::new_v4().to_string(),
            name: "taylor".into(),
            host: "192.168.5.100".into(),
            port: 51800,
            role: "leader".into(),
            election_priority: 1,
            status: "online".into(),
            hardware_json: "{}".into(),
            models_json: "[]".into(),
            last_heartbeat: None,
            registered_at: Utc::now().to_rfc3339(),
        };

        upsert_node(&conn, &node).unwrap();
        let fetched = get_node_by_name(&conn, "taylor").unwrap().unwrap();
        assert_eq!(fetched.host, "192.168.5.100");

        let all = list_nodes(&conn).unwrap();
        assert_eq!(all.len(), 1);

        assert!(update_node_heartbeat(&conn, "taylor").unwrap());
        assert!(update_node_status(&conn, "taylor", "degraded").unwrap());
        assert!(delete_node(&conn, "taylor").unwrap());
        assert!(get_node_by_name(&conn, "taylor").unwrap().is_none());
    }

    #[test]
    fn test_task_lifecycle() {
        let conn = setup();

        // Insert a node first (foreign key).
        let node = NodeRow {
            id: Uuid::new_v4().to_string(),
            name: "marcus".into(),
            host: "192.168.5.200".into(),
            port: 51800,
            role: "worker".into(),
            election_priority: 10,
            status: "online".into(),
            hardware_json: "{}".into(),
            models_json: "[]".into(),
            last_heartbeat: None,
            registered_at: Utc::now().to_rfc3339(),
        };
        upsert_node(&conn, &node).unwrap();

        let task_id = Uuid::new_v4().to_string();
        let task = TaskRow {
            id: task_id.clone(),
            kind: "shell_command".into(),
            payload_json: r#"{"command":"uptime"}"#.into(),
            status: "pending".into(),
            assigned_node: None,
            priority: 5,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            completed_at: None,
        };

        insert_task(&conn, &task).unwrap();
        let pending = list_tasks_by_status(&conn, "pending").unwrap();
        assert_eq!(pending.len(), 1);

        assert!(assign_task(&conn, &task_id, "marcus").unwrap());
        let running = list_tasks_by_status(&conn, "running").unwrap();
        assert_eq!(running.len(), 1);

        complete_task(&conn, &task_id, true, "up 42 days", 150).unwrap();
        let completed = list_tasks_by_status(&conn, "completed").unwrap();
        assert_eq!(completed.len(), 1);
    }

    #[test]
    fn test_claim_next_task_orders_by_priority_and_sets_claimed() {
        let mut conn = setup();

        // Seed worker node for assignment references.
        upsert_node(
            &conn,
            &NodeRow {
                id: Uuid::new_v4().to_string(),
                name: "taylor".into(),
                host: "127.0.0.1".into(),
                port: 51800,
                role: "leader".into(),
                election_priority: 1,
                status: "online".into(),
                hardware_json: "{}".into(),
                models_json: "[]".into(),
                last_heartbeat: None,
                registered_at: Utc::now().to_rfc3339(),
            },
        )
        .unwrap();

        let low_id = Uuid::new_v4().to_string();
        insert_task(
            &conn,
            &TaskRow {
                id: low_id.clone(),
                kind: "shell_command".into(),
                payload_json: r#"{"command":"echo low"}"#.into(),
                status: "pending".into(),
                assigned_node: None,
                priority: 1,
                created_at: Utc::now().to_rfc3339(),
                started_at: None,
                completed_at: None,
            },
        )
        .unwrap();

        let high_id = Uuid::new_v4().to_string();
        insert_task(
            &conn,
            &TaskRow {
                id: high_id.clone(),
                kind: "shell_command".into(),
                payload_json: r#"{"command":"echo high"}"#.into(),
                status: "pending".into(),
                assigned_node: None,
                priority: 10,
                created_at: Utc::now().to_rfc3339(),
                started_at: None,
                completed_at: None,
            },
        )
        .unwrap();

        let claimed = claim_next_task(&mut conn, "taylor").unwrap().unwrap();
        assert_eq!(claimed.id, high_id);
        assert_eq!(claimed.status, "claimed");
        assert_eq!(claimed.assigned_node.as_deref(), Some("taylor"));

        let fetched = get_task(&conn, &high_id).unwrap().unwrap();
        assert_eq!(fetched.status, "claimed");
        assert_eq!(fetched.assigned_node.as_deref(), Some("taylor"));

        let claimed2 = claim_next_task(&mut conn, "taylor").unwrap().unwrap();
        assert_eq!(claimed2.id, low_id);

        let none = claim_next_task(&mut conn, "taylor").unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn test_set_task_status_and_record_task_result() {
        let conn = setup();

        let task_id = Uuid::new_v4().to_string();
        insert_task(
            &conn,
            &TaskRow {
                id: task_id.clone(),
                kind: "shell_command".into(),
                payload_json: r#"{"command":"echo hello"}"#.into(),
                status: "pending".into(),
                assigned_node: None,
                priority: 5,
                created_at: Utc::now().to_rfc3339(),
                started_at: None,
                completed_at: None,
            },
        )
        .unwrap();

        assert!(set_task_status(&conn, &task_id, "in_progress").unwrap());
        assert!(set_task_status(&conn, &task_id, "review").unwrap());
        assert!(set_task_status(&conn, &task_id, "done").unwrap());

        let done = get_task(&conn, &task_id).unwrap().unwrap();
        assert_eq!(done.status, "done");
        assert!(done.started_at.is_some());
        assert!(done.completed_at.is_some());

        record_task_result(&conn, &task_id, true, "ok", 42).unwrap();

        let row: (i64, String, i64) = conn
            .query_row(
                "SELECT success, output, duration_ms FROM task_results WHERE task_id = ?1",
                [task_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, 1);
        assert_eq!(row.1, "ok");
        assert_eq!(row.2, 42);
    }

    #[test]
    fn test_memory_search() {
        let conn = setup();
        let mem = MemoryRow {
            id: Uuid::new_v4().to_string(),
            namespace: "project".into(),
            key: "forgefleet-arch".into(),
            content: "ForgeFleet uses SQLite for embedded storage on every node".into(),
            embedding_json: None,
            metadata_json: "{}".into(),
            importance: 0.9,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            expires_at: None,
        };

        upsert_memory(&conn, &mem).unwrap();

        let results = search_memories(&conn, Some("project"), "SQLite", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("SQLite"));

        let results = search_memories(&conn, None, "embedded", 10).unwrap();
        assert_eq!(results.len(), 1);

        let fetched = get_memory(&conn, "project", "forgefleet-arch")
            .unwrap()
            .unwrap();
        assert_eq!(fetched.importance, 0.9);
    }

    #[test]
    fn test_config_kv() {
        let conn = setup();
        config_set(&conn, "leader.name", "taylor").unwrap();
        config_set(&conn, "fleet.version", "0.1.0").unwrap();

        assert_eq!(
            config_get(&conn, "leader.name").unwrap(),
            Some("taylor".into())
        );
        assert_eq!(config_get(&conn, "missing").unwrap(), None);

        let all = config_list(&conn).unwrap();
        assert_eq!(all.len(), 2);

        config_delete(&conn, "leader.name").unwrap();
        assert_eq!(config_get(&conn, "leader.name").unwrap(), None);
    }

    #[test]
    fn test_session_lifecycle() {
        let conn = setup();
        let session = SessionRow {
            id: Uuid::new_v4().to_string(),
            channel: "telegram".into(),
            user_id: Some("user123".into()),
            node_name: Some("taylor".into()),
            status: "active".into(),
            metadata_json: "{}".into(),
            created_at: Utc::now().to_rfc3339(),
            last_activity: Utc::now().to_rfc3339(),
            closed_at: None,
        };

        insert_session(&conn, &session).unwrap();
        let fetched = get_session(&conn, &session.id).unwrap().unwrap();
        assert_eq!(fetched.channel, "telegram");

        let active = find_active_sessions(&conn, "telegram", "user123").unwrap();
        assert_eq!(active.len(), 1);

        assert!(touch_session(&conn, &session.id).unwrap());
        assert!(close_session(&conn, &session.id).unwrap());

        let active_after = find_active_sessions(&conn, "telegram", "user123").unwrap();
        assert!(active_after.is_empty());
    }

    #[test]
    fn test_ownership_single_owner_claim_contention() {
        let conn = setup();
        let task_id = Uuid::new_v4().to_string();

        assert!(ownership_claim(&conn, &task_id, "taylor", 60).unwrap());
        assert!(!ownership_claim(&conn, &task_id, "marcus", 60).unwrap());

        let owner: String = conn
            .query_row(
                "SELECT owner_node FROM task_ownership WHERE task_id = ?1",
                [&task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, "taylor");
    }

    #[test]
    fn test_ownership_renew_only_by_owner() {
        let conn = setup();
        let task_id = Uuid::new_v4().to_string();

        assert!(ownership_claim(&conn, &task_id, "taylor", 60).unwrap());
        assert!(!ownership_renew(&conn, &task_id, "marcus", 120).unwrap());
        assert!(ownership_renew(&conn, &task_id, "taylor", 120).unwrap());
    }

    #[test]
    fn test_ownership_stale_lease_detection() {
        let conn = setup();
        let stale_task = Uuid::new_v4().to_string();
        let fresh_task = Uuid::new_v4().to_string();

        assert!(ownership_claim(&conn, &stale_task, "taylor", 1).unwrap());
        assert!(ownership_claim(&conn, &fresh_task, "marcus", 300).unwrap());

        let future_probe = (Utc::now() + Duration::seconds(120))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let stale = ownership_list_stale(&conn, &future_probe).unwrap();

        assert_eq!(stale, vec![stale_task]);
    }

    #[test]
    fn test_ownership_handoff_emits_events_and_updates_owner() {
        let conn = setup();
        let task_id = Uuid::new_v4().to_string();

        assert!(ownership_claim(&conn, &task_id, "taylor", 300).unwrap());
        assert!(
            ownership_request_handoff(&conn, &task_id, "taylor", "james", "rebalance").unwrap()
        );
        assert!(ownership_complete_handoff(&conn, &task_id, "taylor", "james").unwrap());

        let (owner, status, target): (String, String, Option<String>) = conn
            .query_row(
                "SELECT owner_node, status, handoff_target FROM task_ownership WHERE task_id = ?1",
                [&task_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(owner, "james");
        assert_eq!(status, "claimed");
        assert_eq!(target, None);

        let mut stmt = conn
            .prepare(
                "SELECT event_type, from_owner, to_owner, reason
                 FROM ownership_events
                 WHERE task_id = ?1
                 ORDER BY id",
            )
            .unwrap();
        let events = stmt
            .query_map([&task_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "handoff_requested");
        assert_eq!(events[0].1.as_deref(), Some("taylor"));
        assert_eq!(events[0].2.as_deref(), Some("james"));
        assert_eq!(events[0].3.as_deref(), Some("rebalance"));

        assert_eq!(events[1].0, "handoff_completed");
        assert_eq!(events[1].1.as_deref(), Some("taylor"));
        assert_eq!(events[1].2.as_deref(), Some("james"));
        assert_eq!(events[1].3, None);
    }

    #[test]
    fn test_autonomy_events_insert_and_list_recent() {
        let conn = setup();

        let first_id = insert_autonomy_event(
            &conn,
            "pre_execution_gate",
            "mutating_operation",
            "require_human_approval",
            "medium-risk mutation",
        )
        .unwrap();
        let second_id = insert_autonomy_event(
            &conn,
            "pre_execution_gate",
            "destructive_operation",
            "deny",
            "high-risk destructive command",
        )
        .unwrap();

        assert!(first_id > 0);
        assert!(second_id > first_id);

        let recent = list_recent_autonomy_events(&conn, 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].decision, "deny");
        assert_eq!(recent[0].action_type, "destructive_operation");
        assert_eq!(recent[1].decision, "require_human_approval");
    }

    #[test]
    fn test_telegram_media_ingest_insert_and_list() {
        let conn = setup();

        let id1 = insert_telegram_media_ingest(
            &conn,
            "8496613333",
            "101",
            "photo",
            "/tmp/forgefleet-telegram/101_0.jpg",
            Some("image/jpeg"),
            Some(12_345),
        )
        .unwrap();
        let id2 = insert_telegram_media_ingest(
            &conn,
            "8622294597",
            "202",
            "video",
            "/tmp/forgefleet-telegram/202_0.mp4",
            Some("video/mp4"),
            Some(55_000),
        )
        .unwrap();

        assert!(id2 > id1);

        let all = list_telegram_media_ingest(&conn, None, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].chat_id, "8622294597");
        assert_eq!(all[0].media_kind, "video");

        let scoped = list_telegram_media_ingest(&conn, Some("8496613333"), 10).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].message_id, "101");
        assert_eq!(scoped[0].mime_type.as_deref(), Some("image/jpeg"));
        assert_eq!(scoped[0].size_bytes, Some(12_345));
    }

    #[test]
    fn test_audit_log() {
        let conn = setup();
        let id = audit_log(
            &conn,
            "leader_elected",
            "system",
            Some("taylor"),
            r#"{"reason":"preferred"}"#,
            Some("taylor"),
        )
        .unwrap();
        assert!(id > 0);

        let recent = recent_audit_log(&conn, 10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].event_type, "leader_elected");
    }

    #[test]
    fn test_runtime_node_heartbeat_upsert_and_readback() {
        let conn = setup();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

        upsert_fleet_node_runtime(
            &conn,
            &FleetNodeRuntimeHeartbeatRow {
                node_id: "node-alpha".to_string(),
                hostname: "alpha.local".to_string(),
                ips_json: r#"["192.168.5.10"]"#.to_string(),
                role: "worker".to_string(),
                reported_status: "online".to_string(),
                last_heartbeat: now,
                resources_json: r#"{"cpu":"16 cores","ram":"64 GB","gpu":"A100"}"#.to_string(),
                services_json: r#"["gateway","runner"]"#.to_string(),
                models_json: r#"["qwen2.5-32b","llama-3.1-8b"]"#.to_string(),
                capabilities_json: r#"{"tool_exec":true,"voice":false}"#.to_string(),
                stale_degraded_after_secs: 45,
                stale_offline_after_secs: 120,
            },
        )
        .unwrap();

        let rows = list_fleet_node_runtime(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row.node_id, "node-alpha");
        assert_eq!(row.hostname, "alpha.local");
        assert_eq!(row.role, "worker");
        assert_eq!(row.derived_status, "online");
        assert!(row.heartbeat_age_secs >= 0);
        assert!(row.heartbeat_age_secs < 10);
        assert!(row.services_json.contains("gateway"));
        assert!(row.capabilities_json.contains("tool_exec"));
    }

    #[test]
    fn test_runtime_node_status_derives_degraded_and_offline_from_heartbeat_age() {
        let conn = setup();
        let heartbeat_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

        upsert_fleet_node_runtime(
            &conn,
            &FleetNodeRuntimeHeartbeatRow {
                node_id: "node-beta".to_string(),
                hostname: "beta.local".to_string(),
                ips_json: r#"["10.0.0.2"]"#.to_string(),
                role: "worker".to_string(),
                reported_status: "online".to_string(),
                last_heartbeat: heartbeat_at,
                resources_json: "{}".to_string(),
                services_json: "[]".to_string(),
                models_json: "[]".to_string(),
                capabilities_json: "{}".to_string(),
                stale_degraded_after_secs: 10,
                stale_offline_after_secs: 20,
            },
        )
        .unwrap();

        let degraded_now = Utc::now() + Duration::seconds(12);
        let degraded_rows = list_fleet_node_runtime_at(&conn, degraded_now).unwrap();
        assert_eq!(degraded_rows.len(), 1);
        assert_eq!(degraded_rows[0].derived_status, "degraded");
        assert!(degraded_rows[0].heartbeat_age_secs >= 12);

        let offline_now = Utc::now() + Duration::seconds(25);
        let offline_rows = list_fleet_node_runtime_at(&conn, offline_now).unwrap();
        assert_eq!(offline_rows.len(), 1);
        assert_eq!(offline_rows[0].derived_status, "offline");
        assert!(offline_rows[0].heartbeat_age_secs >= 25);
    }

    #[test]
    fn test_insert_and_list_fleet_enrollment_events() {
        let conn = setup();

        let inserted_id = insert_fleet_enrollment_event(
            &conn,
            &FleetEnrollmentEventInsert {
                node_id: Some("node-gamma".to_string()),
                hostname: Some("gamma.local".to_string()),
                outcome: "accepted".to_string(),
                reason: None,
                role: Some("builder".to_string()),
                service_version: Some("ff-agent/0.2.0".to_string()),
                addresses_json: r#"["10.0.0.7:51801"]"#.to_string(),
                capabilities_json: r#"{"tools":true}"#.to_string(),
                metadata_json: r#"{"source":"api.enroll"}"#.to_string(),
            },
        )
        .unwrap();
        assert!(inserted_id > 0);

        let rows = list_fleet_enrollment_events(&conn, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "accepted");
        assert_eq!(rows[0].node_id.as_deref(), Some("node-gamma"));
        assert_eq!(rows[0].hostname.as_deref(), Some("gamma.local"));
    }
}

// ─── Task Provenance ─────────────────────────────────────────────────────────

/// Append a routing hop to task_routing_log.
pub async fn pg_append_routing_log(
    pool: &PgPool,
    task_id: &str,
    from_node: &str,
    to_node: &str,
    reason: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO task_routing_log (task_id, from_node, to_node, reason)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(task_id)
    .bind(from_node)
    .bind(to_node)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// A single routing hop.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoutingHop {
    pub task_id: String,
    pub from_node: String,
    pub to_node: String,
    pub reason: String,
    pub routed_at: String,
}

/// Get full routing lineage for a task (task row + routing hops + ownership events).
pub async fn pg_get_task_lineage(pool: &PgPool, task_id: &str) -> Result<serde_json::Value> {
    // Routing hops
    let hops = sqlx::query(
        "SELECT task_id, from_node, to_node, reason, routed_at::text FROM task_routing_log
         WHERE task_id = $1 ORDER BY id",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let hops_json: Vec<serde_json::Value> = hops.iter().map(|r| {
        serde_json::json!({
            "from": r.try_get::<String, _>("from_node").unwrap_or_default(),
            "to": r.try_get::<String, _>("to_node").unwrap_or_default(),
            "reason": r.try_get::<String, _>("reason").unwrap_or_default(),
            "at": r.try_get::<String, _>("routed_at").unwrap_or_default(),
        })
    }).collect();

    // Ownership events
    let events = sqlx::query(
        "SELECT event_type, from_owner, to_owner, reason, created_at FROM ownership_events
         WHERE task_id = $1 ORDER BY id",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let events_json: Vec<serde_json::Value> = events.iter().map(|r| {
        serde_json::json!({
            "event": r.try_get::<String, _>("event_type").unwrap_or_default(),
            "from": r.try_get::<Option<String>, _>("from_owner").unwrap_or_default(),
            "to": r.try_get::<Option<String>, _>("to_owner").unwrap_or_default(),
            "reason": r.try_get::<Option<String>, _>("reason").unwrap_or_default(),
            "at": r.try_get::<String, _>("created_at").unwrap_or_default(),
        })
    }).collect();

    Ok(serde_json::json!({
        "task_id": task_id,
        "routing": hops_json,
        "ownership_events": events_json,
    }))
}
