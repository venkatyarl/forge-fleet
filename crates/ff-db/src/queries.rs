//! Typed query helpers for common database operations.
//!
//! Provides a clean Rust API over raw SQL. All functions take a `&Connection`
//! to work with both pooled and standalone connections.

use std::collections::HashMap;

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
pub struct WorkerRow {
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
    pub worker_name: Option<String>,
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
    pub worker_name: Option<String>,
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
pub fn upsert_node(conn: &Connection, node: &WorkerRow) -> Result<()> {
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
pub fn get_node_by_name(conn: &Connection, name: &str) -> Result<Option<WorkerRow>> {
    let row = conn
        .query_row(
            "SELECT id, name, host, port, role, election_priority, status,
                    hardware_json, models_json, last_heartbeat, registered_at
             FROM nodes WHERE name = ?1",
            [name],
            |row| {
                Ok(WorkerRow {
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
pub fn list_nodes(conn: &Connection) -> Result<Vec<WorkerRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, host, port, role, election_priority, status,
                hardware_json, models_json, last_heartbeat, registered_at
         FROM nodes ORDER BY election_priority, name
         LIMIT 100",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(WorkerRow {
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

    let heartbeat = parse_utc_timestamp(last_heartbeat).unwrap_or(*now);
    let age_secs = now.signed_duration_since(heartbeat).num_seconds().max(0);

    if age_secs >= offline_threshold {
        return ("offline".to_string(), age_secs);
    }

    if age_secs >= degraded_threshold {
        return ("degraded".to_string(), age_secs);
    }

    (normalize_runtime_status(reported_status), age_secs)
}

/// Insert or update a fleet runtime node heartbeat/state snapshot.
pub fn upsert_fleet_worker_runtime(
    conn: &Connection,
    row: &FleetNodeRuntimeHeartbeatRow,
) -> Result<()> {
    let degraded_threshold = row.stale_degraded_after_secs.max(1);
    let offline_threshold = row.stale_offline_after_secs.max(degraded_threshold + 1);

    conn.execute(
        "INSERT INTO fleet_worker_runtime (
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
pub fn list_fleet_worker_runtime(conn: &Connection) -> Result<Vec<FleetNodeRuntimeRow>> {
    list_fleet_worker_runtime_at(conn, Utc::now())
}

/// List live fleet runtime nodes with status derived relative to `now`.
pub fn list_fleet_worker_runtime_at(
    conn: &Connection,
    now: DateTime<Utc>,
) -> Result<Vec<FleetNodeRuntimeRow>> {
    let mut stmt = conn.prepare(
        "SELECT node_id, hostname, ips_json, role, reported_status, last_heartbeat,
                resources_json, services_json, models_json, capabilities_json,
                stale_degraded_after_secs, stale_offline_after_secs, updated_at
         FROM fleet_worker_runtime
         ORDER BY hostname, node_id
         LIMIT 100",
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
pub fn fleet_worker_runtime_exists(conn: &Connection, node_id: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fleet_worker_runtime WHERE node_id = ?1",
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
         FROM tasks WHERE status = ?1 ORDER BY priority DESC, created_at LIMIT 100",
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
pub fn claim_next_task(conn: &mut Connection, worker_name: &str) -> Result<Option<TaskRow>> {
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
        params![worker_name, now, task.id],
    )?;

    if changed == 1 {
        task.status = "claimed".to_string();
        task.assigned_node = Some(worker_name.to_string());
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
pub fn assign_task(conn: &Connection, task_id: &str, worker_name: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let changed = conn.execute(
        "UPDATE tasks SET status = 'running', assigned_node = ?1, started_at = ?2
         WHERE id = ?3 AND status = 'pending'",
        params![worker_name, now, task_id],
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
         ORDER BY task_id
         LIMIT 100",
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
        "INSERT INTO sessions (id, channel, user_id, worker_name, status, metadata_json, created_at, last_activity, closed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            session.id, session.channel, session.user_id, session.worker_name,
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
            "SELECT id, channel, user_id, worker_name, status, metadata_json,
                    created_at, last_activity, closed_at
             FROM sessions WHERE id = ?1",
            [id],
            |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    user_id: row.get(2)?,
                    worker_name: row.get(3)?,
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
        "SELECT id, channel, user_id, worker_name, status, metadata_json,
                created_at, last_activity, closed_at
         FROM sessions
         WHERE channel = ?1 AND user_id = ?2 AND status = 'active'
         ORDER BY last_activity DESC
         LIMIT 100",
    )?;

    let rows = stmt.query_map(params![channel, user_id], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            channel: row.get(1)?,
            user_id: row.get(2)?,
            worker_name: row.get(3)?,
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
    worker_name: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO audit_log (event_type, actor, target, details_json, worker_name)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![event_type, actor, target, details_json, worker_name],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Get recent audit log entries.
pub fn recent_audit_log(conn: &Connection, limit: u32) -> Result<Vec<AuditLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, event_type, actor, target, details_json, worker_name
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
            worker_name: row.get(6)?,
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
    let mut stmt = conn.prepare("SELECT key, value FROM config_kv ORDER BY key LIMIT 100")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Postgres fleet config — row types, query helpers, seed function
// ═══════════════════════════════════════════════════════════════════════════════

/// A fleet node row from the Postgres `fleet_workers` table.
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
    /// "venkatyarl"). NULL for existing nodes still on Taylor's PAT. V12.
    #[serde(default)]
    pub gh_account: Option<String>,
    /// Map of installed-tool versions:
    ///   {"os":{"current":"Ubuntu 24.04.4","latest":"Ubuntu 24.04.5","checked_at":"..."}}
    /// Populated every 6h by the daemon's version_check tick. V12.
    #[serde(default = "default_tooling")]
    pub tooling: JsonValue,
    // ─── Hardware/GPU (joined from the `computers` table) ──────────────────
    // fleet_workers carries the worker *role*; physical hardware (GPU vendor,
    // VRAM, true RAM) lives on `computers`. These are LEFT-JOINed in so a
    // single `ff nodes` / `fleet_nodes_db` call can answer "which boxes are
    // AMD/NVIDIA/Apple and how much VRAM" without SSH-probing. None when the
    // worker has no matching computers row.
    #[serde(default)]
    pub gpu_kind: Option<String>,
    #[serde(default)]
    pub gpu_model: Option<String>,
    #[serde(default)]
    pub gpu_vram_gb: Option<f64>,
    /// Total GPU VRAM (GB). For unified-memory boxes (Apple Silicon, GB10
    /// Grace+Blackwell) per-GPU `gpu_vram_gb` is NULL by design, so this is
    /// the correct source for "how much VRAM"; prefer it when present.
    #[serde(default)]
    pub gpu_total_vram_gb: Option<f64>,
    #[serde(default)]
    pub has_gpu: Option<bool>,
    /// True RAM (GB) from the `computers` hardware row. `ram_gb` above is the
    /// often-stale worker-registry value; prefer this when present.
    #[serde(default)]
    pub computer_ram_gb: Option<i32>,
    /// True CPU cores from the `computers` hardware row; prefer over the
    /// often-stale `cpu_cores` worker-registry value when present.
    #[serde(default)]
    pub computer_cpu_cores: Option<i32>,
}

fn default_runtime() -> String {
    "unknown".to_string()
}
fn default_models_dir() -> String {
    "~/models".to_string()
}
fn default_disk_quota_pct() -> i32 {
    80
}
fn default_sub_agent_count() -> i32 {
    1
}
fn default_tooling() -> JsonValue {
    serde_json::json!({})
}

/// A fleet model row from the Postgres `fleet_models` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetModelRow {
    pub id: String,
    pub worker_name: String,
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
        "SELECT fw.name, fw.ip, fw.ssh_user, fw.ram_gb, fw.cpu_cores, fw.os, fw.role,
                fw.election_priority, fw.hardware, fw.alt_ips, fw.capabilities,
                fw.preferences, fw.resources, fw.status,
                COALESCE(fw.runtime, 'unknown') AS runtime,
                COALESCE(fw.models_dir, '~/models') AS models_dir,
                COALESCE(fw.disk_quota_pct, 80) AS disk_quota_pct,
                COALESCE(fw.sub_agent_count, 1) AS sub_agent_count,
                fw.gh_account,
                COALESCE(fw.tooling, '{}'::jsonb) AS tooling,
                c.gpu_kind, c.gpu_model, c.gpu_vram_gb, c.gpu_total_vram_gb, c.has_gpu,
                c.total_ram_gb AS computer_ram_gb,
                c.cpu_cores AS computer_cpu_cores
         FROM fleet_workers fw
         LEFT JOIN computers c ON c.name = fw.name
         ORDER BY fw.election_priority, fw.name
         LIMIT 100",
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
            gpu_kind: r.get("gpu_kind"),
            gpu_model: r.get("gpu_model"),
            gpu_vram_gb: r.get("gpu_vram_gb"),
            gpu_total_vram_gb: r.get("gpu_total_vram_gb"),
            has_gpu: r.get("has_gpu"),
            computer_ram_gb: r.get("computer_ram_gb"),
            computer_cpu_cores: r.get("computer_cpu_cores"),
        })
        .collect())
}

/// Get a single fleet node by name from Postgres.
pub async fn pg_get_node(pool: &PgPool, name: &str) -> Result<Option<FleetNodeRow>> {
    let row = sqlx::query(
        "SELECT fw.name, fw.ip, fw.ssh_user, fw.ram_gb, fw.cpu_cores, fw.os, fw.role,
                fw.election_priority, fw.hardware, fw.alt_ips, fw.capabilities,
                fw.preferences, fw.resources, fw.status,
                COALESCE(fw.runtime, 'unknown') AS runtime,
                COALESCE(fw.models_dir, '~/models') AS models_dir,
                COALESCE(fw.disk_quota_pct, 80) AS disk_quota_pct,
                COALESCE(fw.sub_agent_count, 1) AS sub_agent_count,
                fw.gh_account,
                COALESCE(fw.tooling, '{}'::jsonb) AS tooling,
                c.gpu_kind, c.gpu_model, c.gpu_vram_gb, c.gpu_total_vram_gb, c.has_gpu,
                c.total_ram_gb AS computer_ram_gb,
                c.cpu_cores AS computer_cpu_cores
         FROM fleet_workers fw
         LEFT JOIN computers c ON c.name = fw.name
         WHERE fw.name = $1",
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
        gpu_kind: r.get("gpu_kind"),
        gpu_model: r.get("gpu_model"),
        gpu_vram_gb: r.get("gpu_vram_gb"),
        gpu_total_vram_gb: r.get("gpu_total_vram_gb"),
        has_gpu: r.get("has_gpu"),
        computer_ram_gb: r.get("computer_ram_gb"),
        computer_cpu_cores: r.get("computer_cpu_cores"),
    }))
}

/// Upsert a fleet node in Postgres.
pub async fn pg_upsert_node(pool: &PgPool, node: &FleetNodeRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_workers (name, ip, ssh_user, ram_gb, cpu_cores, os, role,
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
            runtime = COALESCE(NULLIF(EXCLUDED.runtime, ''), fleet_workers.runtime),
            models_dir = COALESCE(NULLIF(EXCLUDED.models_dir, ''), fleet_workers.models_dir),
            disk_quota_pct = COALESCE(NULLIF(EXCLUDED.disk_quota_pct, 0), fleet_workers.disk_quota_pct),
            sub_agent_count = COALESCE(NULLIF(EXCLUDED.sub_agent_count, 0), fleet_workers.sub_agent_count),
            gh_account = COALESCE(EXCLUDED.gh_account, fleet_workers.gh_account),
            tooling = CASE
                WHEN EXCLUDED.tooling = '{}'::jsonb THEN fleet_workers.tooling
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
        "SELECT id, worker_name, slug, name, family, port, tier,
                local_model, lifecycle, mode, preferred_workloads
         FROM fleet_models ORDER BY worker_name, slug
         LIMIT 100",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| FleetModelRow {
            id: r.get("id"),
            worker_name: r.get("worker_name"),
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
        "SELECT id, worker_name, slug, name, family, port, tier,
                local_model, lifecycle, mode, preferred_workloads
         FROM fleet_models WHERE worker_name = $1 ORDER BY slug
         LIMIT 100",
    )
    .bind(node)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| FleetModelRow {
            id: r.get("id"),
            worker_name: r.get("worker_name"),
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

/// List fleet models whose `preferred_workloads` JSONB array contains *all*
/// of the given workload tags.  Joins with `fleet_workers` so the caller also
/// gets the node's primary IP and current health status.
pub async fn pg_list_models_by_workload(
    pool: &PgPool,
    workloads: &[String],
) -> Result<Vec<(FleetModelRow, String, String)>> {
    let workloads_json = serde_json::to_value(workloads)?;
    let rows = sqlx::query(
        "SELECT
            m.id, m.worker_name, m.slug, m.name, m.family, m.port, m.tier,
            m.local_model, m.lifecycle, m.mode, m.preferred_workloads,
            n.primary_ip, n.status
         FROM fleet_models m
         JOIN fleet_workers n ON n.name = m.worker_name
         WHERE m.preferred_workloads @> $1::jsonb
           AND n.status IN ('online', 'degraded')
         ORDER BY m.tier ASC, m.worker_name, m.slug
         LIMIT 100",
    )
    .bind(&workloads_json)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let model = FleetModelRow {
                id: r.get("id"),
                worker_name: r.get("worker_name"),
                slug: r.get("slug"),
                name: r.get("name"),
                family: r.get("family"),
                port: r.get("port"),
                tier: r.get("tier"),
                local_model: r.get("local_model"),
                lifecycle: r.get("lifecycle"),
                mode: r.get("mode"),
                preferred_workloads: r.get("preferred_workloads"),
            };
            let primary_ip: String = r.get("primary_ip");
            let node_status: String = r.get("status");
            (model, primary_ip, node_status)
        })
        .collect())
}

/// Upsert a fleet model in Postgres.
pub async fn pg_upsert_model(pool: &PgPool, model: &FleetModelRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_models (id, worker_name, slug, name, family, port, tier,
                local_model, lifecycle, mode, preferred_workloads, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW())
         ON CONFLICT (id) DO UPDATE SET
            worker_name = EXCLUDED.worker_name,
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
    .bind(&model.worker_name)
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

/// Read a self-expiring safety gate stored in `fleet_secrets`.
///
/// Returns the parsed boolean from the row, with two TTL-aware exceptions:
///   - Missing row                                  → `default_when_missing`
///   - Row with falsy value and `expires_at < NOW()` → `restore_when_expired`
///
/// Permanent-off rows (no `expires_at`) are honored as-is. The TTL path
/// converts an operator's temporary disable into "extend or auto-restore"
/// — the kill-switch can't outlive its purpose by accident.
///
/// Falsy parses: `false | 0 | no | off | disabled` (case-insensitive).
/// Anything else parses as `true`.
///
/// For `auto_upgrade_enabled`: pass `default_when_missing = false`
/// (preserves pre-V58 behavior on a fleet with no row) and
/// `restore_when_expired = true` (the safe "feature ON" default that the
/// expired kill-switch should auto-restore to).
pub async fn pg_read_safety_gate(
    pool: &PgPool,
    key: &str,
    default_when_missing: bool,
    restore_when_expired: bool,
) -> Result<bool> {
    let row = sqlx::query(
        "SELECT value, expires_at, disabled_reason
           FROM fleet_secrets WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(default_when_missing);
    };
    let value: String = row.get("value");
    let expires_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("expires_at").ok();

    let parsed = matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on" | "enabled"
    );

    // Falsy + expired TTL → auto-restore to the safe "on" default. Log a
    // warning so the auto-restore is visible in journalctl.
    if !parsed
        && let Some(exp) = expires_at
        && exp < chrono::Utc::now()
    {
        let reason: Option<String> = row.try_get("disabled_reason").ok();
        tracing::warn!(
            key = %key,
            expired_at = %exp,
            reason = ?reason,
            "safety gate auto-restoring: kill-switch TTL expired"
        );
        return Ok(restore_when_expired);
    }

    Ok(parsed)
}

/// Set a safety gate to `false` (disabled) with a TTL and a required
/// reason. Used by `ff secrets disable-gate` so operators can never
/// leave a permanent off-state without explicit context.
pub async fn pg_disable_safety_gate(
    pool: &PgPool,
    key: &str,
    reason: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
    updated_by: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_secrets
            (key, value, expires_at, disabled_reason, updated_at, updated_by)
         VALUES ($1, 'false', $2, $3, NOW(), $4)
         ON CONFLICT (key) DO UPDATE SET
            value = 'false',
            expires_at = EXCLUDED.expires_at,
            disabled_reason = EXCLUDED.disabled_reason,
            updated_at = NOW(),
            updated_by = EXCLUDED.updated_by",
    )
    .bind(key)
    .bind(expires_at)
    .bind(reason)
    .bind(updated_by)
    .execute(pool)
    .await?;
    Ok(())
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
    /// V111: first-class tool-calling capability. The agent router filters on
    /// this. On upsert it's auto-derived from preferred_workloads containing
    /// "tool_calling" so the TOML→DB sync keeps it correct without a separate
    /// TOML field.
    pub tool_calling: bool,
}

/// True if a `preferred_workloads` JSONB array contains the "tool_calling" tag.
fn workloads_have_tool_calling(workloads: &JsonValue) -> bool {
    workloads
        .as_array()
        .map(|arr| arr.iter().any(|v| v.as_str() == Some("tool_calling")))
        .unwrap_or(false)
}

/// Upsert a catalog entry. Returns the id.
pub async fn pg_upsert_catalog(pool: &PgPool, row: &ModelCatalogRow) -> Result<String> {
    // Derive tool_calling from the workloads tag OR an explicitly-set field, so
    // the TOML→DB sync (which only carries preferred_workloads) stays correct.
    let tool_calling = row.tool_calling || workloads_have_tool_calling(&row.preferred_workloads);
    sqlx::query(
        "INSERT INTO fleet_model_catalog
            (id, name, family, parameters, tier, description, gated, preferred_workloads, variants, tool_calling, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW())
         ON CONFLICT (id) DO UPDATE SET
            name = EXCLUDED.name,
            family = EXCLUDED.family,
            parameters = EXCLUDED.parameters,
            tier = EXCLUDED.tier,
            description = EXCLUDED.description,
            gated = EXCLUDED.gated,
            preferred_workloads = EXCLUDED.preferred_workloads,
            variants = EXCLUDED.variants,
            tool_calling = EXCLUDED.tool_calling,
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
    .bind(tool_calling)
    .execute(pool)
    .await?;
    Ok(row.id.clone())
}

/// List catalog entries sorted by tier (desc) then name (asc).
pub async fn pg_list_catalog(pool: &PgPool) -> Result<Vec<ModelCatalogRow>> {
    let rows = sqlx::query(
        "SELECT id, name, family, parameters, tier, description, gated, preferred_workloads, variants, tool_calling
           FROM fleet_model_catalog
          ORDER BY tier DESC, name ASC
         LIMIT 100",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ModelCatalogRow {
            id: r.get("id"),
            name: r.get("name"),
            family: r.get("family"),
            parameters: r.get("parameters"),
            tier: r.get("tier"),
            description: r.get("description"),
            gated: r.get("gated"),
            preferred_workloads: r.get("preferred_workloads"),
            variants: r.get("variants"),
            tool_calling: r.get("tool_calling"),
        })
        .collect())
}

/// Search catalog by substring on name/family/id (case-insensitive).
pub async fn pg_search_catalog(pool: &PgPool, query: &str) -> Result<Vec<ModelCatalogRow>> {
    let pattern = format!("%{}%", query.to_lowercase());
    let rows = sqlx::query(
        "SELECT id, name, family, parameters, tier, description, gated, preferred_workloads, variants, tool_calling
           FROM fleet_model_catalog
          WHERE LOWER(id) LIKE $1 OR LOWER(name) LIKE $1 OR LOWER(family) LIKE $1
          ORDER BY tier DESC, name ASC
         LIMIT 100",
    )
    .bind(&pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ModelCatalogRow {
            id: r.get("id"),
            name: r.get("name"),
            family: r.get("family"),
            parameters: r.get("parameters"),
            tier: r.get("tier"),
            description: r.get("description"),
            gated: r.get("gated"),
            preferred_workloads: r.get("preferred_workloads"),
            variants: r.get("variants"),
            tool_calling: r.get("tool_calling"),
        })
        .collect())
}

/// Fetch one catalog entry by id.
pub async fn pg_get_catalog(pool: &PgPool, id: &str) -> Result<Option<ModelCatalogRow>> {
    let row = sqlx::query(
        "SELECT id, name, family, parameters, tier, description, gated, preferred_workloads, variants, tool_calling
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
        tool_calling: r.get("tool_calling"),
    }))
}

/// V118: the set of catalog ids that have been RETIRED via the lifecycle table
/// (`model_catalog.lifecycle_status = 'retired'`). The disk-reconcile classifier
/// treats a retired model as a DELETE candidate even if it's the only copy —
/// nobody wants it back. Best-effort: returns an empty set if the lifecycle
/// table is missing (older DB) rather than erroring.
pub async fn pg_retired_catalog_ids(pool: &PgPool) -> Result<std::collections::HashSet<String>> {
    let rows = match sqlx::query("SELECT id FROM model_catalog WHERE lifecycle_status = 'retired'")
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(_) => return Ok(std::collections::HashSet::new()),
    };
    Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
}

/// Library entry — a model file on disk on a specific node.
#[derive(Debug, Clone)]
pub struct ModelLibraryRow {
    pub id: String,
    pub worker_name: String,
    pub catalog_id: String,
    pub runtime: String,
    pub quant: Option<String>,
    pub file_path: String,
    pub size_bytes: i64,
    pub sha256: Option<String>,
    pub downloaded_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub source_url: Option<String>,
    /// V118: a pinned library row is never auto-evicted (delete or move-then-
    /// delete) by the disk-reconcile tick, regardless of age/peer-copies.
    pub pinned: bool,
}

/// Upsert a library entry keyed by (worker_name, file_path). Returns library id.
#[allow(clippy::too_many_arguments)]
pub async fn pg_upsert_library(
    pool: &PgPool,
    worker_name: &str,
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
            (worker_name, catalog_id, runtime, quant, file_path, size_bytes, sha256, source_url)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (worker_name, file_path) DO UPDATE SET
            catalog_id = EXCLUDED.catalog_id,
            runtime = EXCLUDED.runtime,
            quant = COALESCE(EXCLUDED.quant, fleet_model_library.quant),
            size_bytes = EXCLUDED.size_bytes,
            sha256 = COALESCE(EXCLUDED.sha256, fleet_model_library.sha256),
            source_url = COALESCE(EXCLUDED.source_url, fleet_model_library.source_url)
         RETURNING id",
    )
    .bind(worker_name)
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
pub async fn pg_list_library(
    pool: &PgPool,
    worker_name: Option<&str>,
) -> Result<Vec<ModelLibraryRow>> {
    let rows = if let Some(n) = worker_name {
        sqlx::query(
            "SELECT * FROM fleet_model_library WHERE worker_name = $1 ORDER BY worker_name, catalog_id LIMIT 100",
        )
        .bind(n)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query("SELECT * FROM fleet_model_library ORDER BY worker_name, catalog_id LIMIT 100")
            .fetch_all(pool)
            .await?
    };
    Ok(rows
        .iter()
        .map(|r| {
            let id: sqlx::types::Uuid = r.get("id");
            ModelLibraryRow {
                id: id.to_string(),
                worker_name: r.get("worker_name"),
                catalog_id: r.get("catalog_id"),
                runtime: r.get("runtime"),
                quant: r.get("quant"),
                file_path: r.get("file_path"),
                size_bytes: r.get("size_bytes"),
                sha256: r.get("sha256"),
                downloaded_at: r.get("downloaded_at"),
                last_used_at: r.get("last_used_at"),
                source_url: r.get("source_url"),
                // V118: present after the disk_management migration; default
                // false on older rows via the column default.
                pinned: r.try_get("pinned").unwrap_or(false),
            }
        })
        .collect())
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
    pub worker_name: String,
    pub library_id: Option<String>,
    pub catalog_id: Option<String>,
    pub runtime: String,
    pub port: i32,
    pub pid: Option<i32>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_health_at: Option<chrono::DateTime<chrono::Utc>>,
    pub health_status: String,
    pub context_window: Option<i32>,
    /// Launched `--parallel` slot count (llama.cpp). `None` for older rows /
    /// runtimes that don't split context across slots.
    pub parallel_slots: Option<i32>,
    /// Effective context per parallel slot (= context_window / parallel_slots,
    /// == context_window when parallel_slots = 1). This is the ctx an agent
    /// actually gets; the router filters on it so a tool-schema system prompt
    /// can't overflow a 4K-per-slot endpoint. `None` until V111 backfill / the
    /// next load records it.
    pub usable_agent_ctx: Option<i32>,
    pub tokens_used: i64,
    pub request_count: i64,
}

/// List deployments optionally filtered by node.
pub async fn pg_list_deployments(
    pool: &PgPool,
    worker_name: Option<&str>,
) -> Result<Vec<ModelDeploymentRow>> {
    let rows = if let Some(n) = worker_name {
        sqlx::query(
            "SELECT * FROM fleet_model_deployments WHERE worker_name = $1 ORDER BY worker_name, port LIMIT 100",
        )
        .bind(n)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query("SELECT * FROM fleet_model_deployments ORDER BY worker_name, port LIMIT 100")
            .fetch_all(pool)
            .await?
    };
    Ok(rows
        .iter()
        .map(|r| {
            let id: sqlx::types::Uuid = r.get("id");
            let lib_id: Option<sqlx::types::Uuid> = r.get("library_id");
            ModelDeploymentRow {
                id: id.to_string(),
                worker_name: r.get("worker_name"),
                library_id: lib_id.map(|u| u.to_string()),
                catalog_id: r.get("catalog_id"),
                runtime: r.get("runtime"),
                port: r.get("port"),
                pid: r.get("pid"),
                started_at: r.get("started_at"),
                last_health_at: r.get("last_health_at"),
                health_status: r.get("health_status"),
                context_window: r.get("context_window"),
                parallel_slots: r.get("parallel_slots"),
                usable_agent_ctx: r.get("usable_agent_ctx"),
                tokens_used: r.get("tokens_used"),
                request_count: r.get("request_count"),
            }
        })
        .collect())
}

/// Upsert a deployment (node + port is unique).
///
/// `parallel_slots` is the launched `--parallel` (llama.cpp slot count); pass
/// `None` for runtimes that don't split ctx across slots. When both
/// `context_window` and `parallel_slots` are known, `usable_agent_ctx` (the
/// effective per-slot ctx the router filters on) is derived in SQL as
/// `context_window / GREATEST(1, parallel_slots)` so the agent router can tell
/// a 32K-per-slot endpoint from a 4K-per-slot one.
#[allow(clippy::too_many_arguments)]
pub async fn pg_upsert_deployment(
    pool: &PgPool,
    worker_name: &str,
    library_id: Option<&str>,
    catalog_id: Option<&str>,
    runtime: &str,
    port: i32,
    pid: Option<i32>,
    health_status: &str,
    context_window: Option<i32>,
    parallel_slots: Option<i32>,
) -> Result<String> {
    let lib_uuid = library_id
        .map(|s| {
            sqlx::types::Uuid::parse_str(s)
                .map_err(|e| crate::error::DbError::NotFound(format!("bad library uuid {s}: {e}")))
        })
        .transpose()?;
    let row = sqlx::query(
        "INSERT INTO fleet_model_deployments
            (worker_name, library_id, catalog_id, runtime, port, pid, health_status,
             context_window, parallel_slots, usable_agent_ctx, last_health_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                 CASE WHEN $8 IS NOT NULL AND $9 IS NOT NULL
                      THEN $8 / GREATEST(1, $9) END,
                 NOW())
         ON CONFLICT (worker_name, port) DO UPDATE SET
            library_id = EXCLUDED.library_id,
            catalog_id = EXCLUDED.catalog_id,
            runtime = EXCLUDED.runtime,
            pid = EXCLUDED.pid,
            health_status = EXCLUDED.health_status,
            context_window = COALESCE(EXCLUDED.context_window, fleet_model_deployments.context_window),
            parallel_slots = COALESCE(EXCLUDED.parallel_slots, fleet_model_deployments.parallel_slots),
            usable_agent_ctx = COALESCE(EXCLUDED.usable_agent_ctx, fleet_model_deployments.usable_agent_ctx),
            last_health_at = NOW()
         RETURNING id",
    )
    .bind(worker_name)
    .bind(lib_uuid)
    .bind(catalog_id)
    .bind(runtime)
    .bind(port)
    .bind(pid)
    .bind(health_status)
    .bind(context_window)
    .bind(parallel_slots)
    .fetch_one(pool)
    .await?;
    let id: sqlx::types::Uuid = row.get("id");
    Ok(id.to_string())
}

/// Filters for [`pg_route_deployments`] — the single scored selector that
/// backs both the `fleet_route` MCP tool and the agent-capable router.
///
/// All filters are AND-ed. Leaving a field at its default disables that
/// filter, so the historical `fleet_route` behaviour (workload-only) is just
/// `RouteFilter { workload: Some("code"), ..Default::default() }`.
#[derive(Debug, Clone, Default)]
pub struct RouteFilter {
    /// Match `fleet_model_catalog.preferred_workloads @> [workload]`
    /// (singular/plural tolerant). `None` = any workload.
    pub workload: Option<String>,
    /// Require `fleet_model_catalog.tool_calling = TRUE`. Used by the agent
    /// router so dispatch never lands on a non-tool model (e.g. gemma).
    pub require_tool_calling: bool,
    /// Require `fleet_model_deployments.usable_agent_ctx >= min_ctx` — the
    /// effective per-slot ctx must fit the agent's tool-schema system prompt.
    /// `None` = no ctx floor.
    pub min_ctx: Option<i32>,
    /// Hosts to exclude by worker_name (case-insensitive), e.g. ["taylor"] to
    /// keep agent load off the leader.
    pub exclude_hosts: Vec<String>,
    /// Max candidates to return (scored best-first). Defaults to 3 if 0.
    pub limit: i64,
}

/// One scored routing candidate. Ordering in the returned Vec is best-first
/// (smaller tier wins, then most-recently-healthy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteCandidate {
    pub worker_name: String,
    /// `http://<host_or_ip>:<port>` ready to use as an OpenAI base URL.
    pub endpoint: String,
    pub port: i32,
    pub catalog_id: Option<String>,
    pub catalog_name: Option<String>,
    pub family: Option<String>,
    pub tier: i32,
    pub tool_calling: bool,
    pub context_window: Option<i32>,
    pub usable_agent_ctx: Option<i32>,
    pub parallel_slots: Option<i32>,
    pub health_status: String,
    pub health_age_sec: Option<i32>,
    pub os_family: Option<String>,
    pub has_gpu: Option<bool>,
    pub is_unified_memory: Option<bool>,
    pub total_ram_gb: Option<i32>,
}

/// The shared scored selector over `fleet_model_deployments JOIN
/// fleet_model_catalog`. This is the one place the routing scorer lives — the
/// `fleet_route` MCP tool and the agent-capable router both call it so there's
/// no parallel scorer to drift.
///
/// Scoring (unchanged from the original `fleet_route`): only `health_status =
/// 'healthy'` rows, ordered by `tier ASC` (T1 small/fast beats T4 huge) then
/// `last_health_at DESC` (freshest first → natural load spreading as health
/// pings rotate). The new `require_tool_calling` / `min_ctx` / `exclude_hosts`
/// filters are additive WHERE clauses.
pub async fn pg_route_deployments(
    pool: &PgPool,
    filter: &RouteFilter,
) -> Result<Vec<RouteCandidate>> {
    // Singular-plural tolerance — V39 seed uses "embeddings"/"reranking",
    // V91 uses "embedding". `@>` is exact-element match so we OR both forms.
    let workload = filter.workload.as_deref();
    let plural = match workload {
        Some("embedding") => Some("embeddings"),
        Some("rerank") => Some("reranking"),
        Some("embeddings") => Some("embedding"),
        Some("reranking") => Some("rerank"),
        _ => None,
    };
    // When no workload filter is requested, pass a tag that can't match any
    // real workload and rely on the `$3` "workload filter disabled" guard.
    let arr_a = JsonValue::Array(vec![JsonValue::String(
        workload.unwrap_or("__any__").to_string(),
    )]);
    let arr_b = JsonValue::Array(vec![JsonValue::String(
        plural.unwrap_or(workload.unwrap_or("__any__")).to_string(),
    )]);

    // Lower-cased exclude list for case-insensitive worker_name match.
    let excludes: Vec<String> = filter
        .exclude_hosts
        .iter()
        .map(|h| h.to_lowercase())
        .collect();

    let limit = if filter.limit > 0 { filter.limit } else { 3 };

    let rows = sqlx::query(
        r#"
        SELECT d.worker_name,
               d.port,
               d.catalog_id,
               cat.name        AS catalog_name,
               cat.family      AS catalog_family,
               cat.tier        AS tier,
               cat.tool_calling AS tool_calling,
               d.context_window AS context_window,
               d.usable_agent_ctx AS usable_agent_ctx,
               d.parallel_slots AS parallel_slots,
               d.health_status AS health_status,
               COALESCE(c.primary_ip, w.name) AS host_or_name,
               c.os_family     AS os_family,
               c.has_gpu       AS has_gpu,
               (c.gpu_kind IN ('apple_silicon', 'gb10')) AS is_unified_memory,
               c.total_ram_gb  AS total_ram_gb,
               EXTRACT(EPOCH FROM (NOW() - d.last_health_at))::int AS health_age_sec
          FROM fleet_model_deployments d
          JOIN fleet_model_catalog cat ON cat.id = d.catalog_id
          LEFT JOIN fleet_workers w     ON w.name = d.worker_name
          LEFT JOIN computers c         ON LOWER(c.name) = LOWER(d.worker_name)
         WHERE d.health_status = 'healthy'
           -- workload filter ($3 = true disables it)
           AND ($3 OR cat.preferred_workloads @> $1::jsonb
                   OR cat.preferred_workloads @> $2::jsonb)
           -- tool_calling filter ($4 = false disables it)
           AND (NOT $4 OR cat.tool_calling = TRUE)
           -- min usable per-slot ctx ($5 IS NULL disables it). NULL ctx rows
           -- are excluded when a floor is requested (can't prove they fit).
           AND ($5::int IS NULL OR d.usable_agent_ctx >= $5)
           -- exclude hosts (case-insensitive; $6 empty disables it)
           AND (LOWER(d.worker_name) <> ALL($6))
         ORDER BY cat.tier ASC,
                  d.last_health_at DESC NULLS LAST
         LIMIT $7
        "#,
    )
    .bind(&arr_a)
    .bind(&arr_b)
    .bind(workload.is_none())
    .bind(filter.require_tool_calling)
    .bind(filter.min_ctx)
    .bind(&excludes)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let worker_name: String = r.try_get("worker_name").unwrap_or_default();
            let port: i32 = r.try_get("port").unwrap_or(0);
            let host: String = r.try_get("host_or_name").unwrap_or_default();
            RouteCandidate {
                endpoint: format!("http://{host}:{port}"),
                worker_name,
                port,
                catalog_id: r.try_get("catalog_id").ok(),
                catalog_name: r.try_get("catalog_name").ok(),
                family: r.try_get("catalog_family").ok(),
                tier: r.try_get("tier").unwrap_or(2),
                tool_calling: r.try_get("tool_calling").unwrap_or(false),
                context_window: r.try_get("context_window").ok(),
                usable_agent_ctx: r.try_get("usable_agent_ctx").ok(),
                parallel_slots: r.try_get("parallel_slots").ok(),
                health_status: r.try_get("health_status").unwrap_or_default(),
                health_age_sec: r.try_get("health_age_sec").ok(),
                os_family: r.try_get("os_family").ok(),
                has_gpu: r.try_get("has_gpu").ok(),
                is_unified_memory: r.try_get("is_unified_memory").ok(),
                total_ram_gb: r.try_get("total_ram_gb").ok(),
            }
        })
        .collect())
}

/// Pick the best agent-capable deployment: tool-calling model + enough per-slot
/// ctx for the tool-schema system prompt, excluding `exclude_hosts`. Returns the
/// top-scored candidate, or `None` if nothing in the fleet qualifies (callers
/// should surface a clear "no agent-capable endpoint" error rather than falling
/// back to a non-tool model).
pub async fn pg_pick_agent_endpoint(
    pool: &PgPool,
    min_ctx: i32,
    exclude_hosts: &[String],
) -> Result<Option<RouteCandidate>> {
    let filter = RouteFilter {
        workload: None,
        require_tool_calling: true,
        min_ctx: Some(min_ctx),
        exclude_hosts: exclude_hosts.to_vec(),
        limit: 1,
    };
    Ok(pg_route_deployments(pool, &filter)
        .await?
        .into_iter()
        .next())
}

/// Active-deployment count per worker — a cheap host-load proxy for offload's
/// least-loaded tiebreak (a host serving 5 model servers is busier than one
/// serving 1; this is what stalled an offload on a 5-server host for 8 min).
/// P3 (the adaptive autoscaler) will replace this with a real demand/load signal.
pub async fn pg_active_deployment_counts(
    pool: &PgPool,
) -> Result<std::collections::HashMap<String, i64>> {
    let rows = sqlx::query(
        "SELECT worker_name, COUNT(*)::bigint AS n
           FROM fleet_model_deployments
          WHERE desired_state = 'active'
          GROUP BY worker_name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let w: String = r.get("worker_name");
            let n: i64 = r.get("n");
            (w, n)
        })
        .collect())
}

/// Map an offload task `kind` to a preferred catalog workload tag. Code-shaped
/// work prefers a coder-family model (`code-gen`); every other kind has no
/// preference and routes to the cheapest warm tool-capable model.
fn offload_workload_for_kind(kind: Option<&str>) -> Option<&'static str> {
    match kind.map(|k| k.to_ascii_lowercase()).as_deref() {
        Some("codegen") | Some("edits") | Some("tests") | Some("code") => Some("code-gen"),
        _ => None,
    }
}

/// Offload-specific endpoint selection: capability + kind-aware, with a
/// least-loaded-host tiebreak. Kept DISTINCT from [`pg_pick_agent_endpoint`]
/// (which agents/crew use) so their default ranking is untouched — but built on
/// the SAME [`pg_route_deployments`] scorer so there's still no parallel router.
///
/// Selection order: (1) prefer a warm tool-capable model whose workload matches
/// the task `kind` (codegen/edits/tests/code -> a `code-gen` coder); (2) fall
/// back to any warm tool-capable model; (3) rank by smaller tier first (cheapest
/// appropriate model), then least-loaded host as a tiebreak among equal tiers.
/// Both `ff offload` and the `fleet_offload` MCP tool call this.
pub async fn pg_pick_offload_endpoint(
    pool: &PgPool,
    min_ctx: i32,
    kind: Option<&str>,
    exclude_hosts: &[String],
) -> Result<Option<RouteCandidate>> {
    let mk = |workload: Option<&str>| RouteFilter {
        workload: workload.map(str::to_string),
        require_tool_calling: true,
        min_ctx: Some(min_ctx),
        exclude_hosts: exclude_hosts.to_vec(),
        limit: 8,
    };

    // 1) workload-matching candidates (e.g. coders for code kinds).
    let mut cands = match offload_workload_for_kind(kind) {
        Some(w) => pg_route_deployments(pool, &mk(Some(w))).await?,
        None => Vec::new(),
    };
    // 2) fall back to any warm tool-capable endpoint.
    if cands.is_empty() {
        cands = pg_route_deployments(pool, &mk(None)).await?;
    }
    if cands.is_empty() {
        return Ok(None);
    }
    // 3) cheapest appropriate model first (smaller tier), then least-loaded host
    //    as a tiebreak among equal tiers — so two same-tier endpoints spread load
    //    instead of always hammering the same one. (Hard saturation avoidance —
    //    skipping an overloaded host outright — is P3's job, not P1's.)
    let load = pg_active_deployment_counts(pool).await.unwrap_or_default();
    cands.sort_by_key(|c| {
        (
            c.tier as i64,
            load.get(&c.worker_name).copied().unwrap_or(0),
        )
    });
    Ok(cands.into_iter().next())
}

// ─── Orchestrator P2: per-session demand sensing ─────────────────────────────
//
// Captures the work-kind signal that already flows through the offload path
// and session_runner dispatch into a cheap bucketed table, then rolls the last
// N minutes into a fair-shared fleet-wide demand vector that P3's adaptive
// serving-mix autoscaler consumes. See schema V116.

/// The fleet-wide demand vector: how many code-vs-general inference slots the
/// currently-active sessions want, fair-shared so one loud session can't starve
/// another. Produced live by [`pg_current_demand_vector`] and persisted (one
/// row per leader tick) into `fleet_demand_snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemandVector {
    /// Slots of code-capable (`code-gen` workload) model wanted across sessions.
    pub code_slots_wanted: f64,
    /// Slots of general-purpose model wanted across sessions.
    pub general_slots_wanted: f64,
    /// Distinct sessions that emitted any signal inside the window.
    pub active_sessions: i32,
    /// Window the vector was computed over, in seconds.
    pub window_secs: i64,
    /// Per-session fairness breakdown `[{session_id, code, general}]`.
    pub per_session: JsonValue,
    /// When this vector was computed (UTC).
    pub captured_at: DateTime<Utc>,
}

/// Record a single per-session work-kind signal (fire-and-forget telemetry).
///
/// `raw_kind` is mapped through the SAME [`offload_workload_for_kind`] taxonomy
/// used by the router, so "code-shaped" is defined in exactly one place:
/// `Some("code-gen")` → `work_kind='code'`, `None` → `work_kind='general'`
/// (research/chat/synthesis all collapse to general for slot-shape purposes —
/// the raw kind is preserved in the `raw_kind` column for observability).
///
/// `session_id` falls back to a synthetic `adhoc:<source>` bucket when absent
/// (e.g. a session-less `ff offload`). UPSERTs into a per-minute bucket so a
/// chatty session writes one row per minute, not thousands. Callers should
/// treat this as best-effort — a failed write must NEVER fail the offload/step.
pub async fn record_session_work_signal(
    pool: &PgPool,
    session_id: Option<&str>,
    raw_kind: &str,
    source: &str,
) -> Result<()> {
    let work_kind = match offload_workload_for_kind(Some(raw_kind)) {
        Some("code-gen") => "code",
        _ => "general",
    };
    let session = session_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("adhoc:{source}"));

    sqlx::query(
        r#"
        INSERT INTO session_work_signal
            (session_id, work_kind, raw_kind, source, bucket_minute, hits)
        VALUES ($1, $2, $3, $4, date_trunc('minute', NOW()), 1)
        ON CONFLICT (session_id, work_kind, source, bucket_minute)
        DO UPDATE SET hits = session_work_signal.hits + 1,
                      raw_kind = EXCLUDED.raw_kind
        "#,
    )
    .bind(&session)
    .bind(work_kind)
    .bind(raw_kind)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Compute the live fleet-wide demand vector over the last `window_secs`.
///
/// FAIR-SHARE aggregate: each active session contributes ONE unit of total
/// demand, split by that session's own code/general hit ratio. So a session
/// that fired 1000 code hits and a session that fired 2 chat hits each weigh
/// exactly 1 — no session starves another regardless of raw call volume.
/// `code_slots_wanted = Σ_session (1 * session_code_fraction)`, likewise
/// general. Pure SQL aggregate over the indexed `bucket_minute` time-window
/// slice — no per-row Rust. NUMERIC sums are cast to float8 (the ff-db sqlx
/// build has no decimal feature).
pub async fn pg_current_demand_vector(pool: &PgPool, window_secs: i64) -> Result<DemandVector> {
    let window = window_secs.max(1);
    let row = sqlx::query(
        r#"
        WITH win AS (
            SELECT session_id, work_kind, SUM(hits) AS hits
              FROM session_work_signal
             WHERE bucket_minute > NOW() - make_interval(secs => $1)
             GROUP BY session_id, work_kind
        ),
        per_session AS (
            SELECT
                session_id,
                COALESCE(SUM(hits) FILTER (WHERE work_kind = 'code'), 0)::float8    AS code_hits,
                COALESCE(SUM(hits) FILTER (WHERE work_kind = 'general'), 0)::float8 AS general_hits,
                COALESCE(SUM(hits), 0)::float8                                      AS total_hits
              FROM win
             GROUP BY session_id
        ),
        shares AS (
            -- Each session = 1 unit, split by its own code/general fraction.
            SELECT
                session_id,
                CASE WHEN total_hits > 0 THEN code_hits    / total_hits ELSE 0 END AS code_share,
                CASE WHEN total_hits > 0 THEN general_hits / total_hits ELSE 0 END AS general_share
              FROM per_session
        )
        SELECT
            COALESCE(SUM(code_share), 0)::float8    AS code_slots_wanted,
            COALESCE(SUM(general_share), 0)::float8 AS general_slots_wanted,
            COUNT(*)::int                           AS active_sessions,
            COALESCE(
                jsonb_agg(
                    jsonb_build_object(
                        'session_id', session_id,
                        'code', round(code_share::numeric, 4),
                        'general', round(general_share::numeric, 4)
                    )
                    ORDER BY session_id
                ) FILTER (WHERE session_id IS NOT NULL),
                '[]'::jsonb
            ) AS per_session
          FROM shares
        "#,
    )
    .bind(window)
    .fetch_one(pool)
    .await?;

    Ok(DemandVector {
        code_slots_wanted: row.get::<f64, _>("code_slots_wanted"),
        general_slots_wanted: row.get::<f64, _>("general_slots_wanted"),
        active_sessions: row.get::<i32, _>("active_sessions"),
        window_secs: window,
        per_session: row.get::<JsonValue, _>("per_session"),
        captured_at: Utc::now(),
    })
}

/// Return the most recent `fleet_demand_snapshot` row as a [`DemandVector`], or
/// `None` if the sensor tick hasn't snapshotted anything yet. One indexed
/// lookup — the read side P3 + the dashboard use (vs. the live recompute the
/// tick itself runs).
pub async fn pg_latest_demand_snapshot(pool: &PgPool) -> Result<Option<DemandVector>> {
    let row = sqlx::query(
        r#"
        SELECT
            captured_at,
            window_secs,
            active_sessions,
            code_slots_wanted::float8    AS code_slots_wanted,
            general_slots_wanted::float8 AS general_slots_wanted,
            per_session
          FROM fleet_demand_snapshot
         ORDER BY captured_at DESC
         LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| DemandVector {
        code_slots_wanted: r.get::<f64, _>("code_slots_wanted"),
        general_slots_wanted: r.get::<f64, _>("general_slots_wanted"),
        active_sessions: r.get::<i32, _>("active_sessions"),
        window_secs: r.get::<i32, _>("window_secs") as i64,
        per_session: r.get::<JsonValue, _>("per_session"),
        captured_at: r.get::<DateTime<Utc>, _>("captured_at"),
    }))
}

// ─── Orchestrator P3: adaptive serving-mix autoscaler ───────────────────────
//
// The autoscaler compares the P2 demand vector (`pg_latest_demand_snapshot`)
// against live SUPPLY (healthy agent-capable deployments bucketed code/general)
// and, on a gap, picks a target host to LOAD a model on (or an idle endpoint to
// UNLOAD). These helpers are pure consumers of existing tables — no new schema.
// See `ff_agent::autoscaler` for the control loop + safety gate.

/// How many healthy, agent-capable inference endpoints currently serve each
/// "kind". A row counts toward `code` when its catalog
/// `preferred_workloads @> '["code-gen"]'`, else toward `general`. Only
/// agent-capable rows count: `cat.tool_calling = TRUE` AND the deployment's
/// `usable_agent_ctx >= min_ctx` (the same floor the agent router enforces, so
/// the autoscaler's "supply" matches what dispatch can actually use).
///
/// `code_endpoints` / `general_endpoints` carry the per-host detail the
/// placement + unload steps need (worker_name, deployment id, port, idle proxy).
#[derive(Debug, Clone, Default)]
pub struct ServingSupply {
    pub code_count: i64,
    pub general_count: i64,
    pub code_endpoints: Vec<ServingEndpoint>,
    pub general_endpoints: Vec<ServingEndpoint>,
}

/// One healthy agent-capable endpoint, with the fields the unload step uses to
/// pick the least-valuable (idle) one to retire.
#[derive(Debug, Clone)]
pub struct ServingEndpoint {
    pub deployment_id: String,
    pub worker_name: String,
    pub port: i32,
    pub catalog_id: Option<String>,
    pub request_count: i64,
    /// Seconds since the last health ping (NULL → very old / unknown).
    pub health_age_sec: Option<i32>,
}

/// Bucket the healthy agent-capable deployments into code vs general supply.
pub async fn pg_supplied_slots_by_kind(pool: &PgPool, min_ctx: i32) -> Result<ServingSupply> {
    let rows = sqlx::query(
        r#"
        SELECT d.id              AS id,
               d.worker_name     AS worker_name,
               d.port            AS port,
               d.catalog_id      AS catalog_id,
               d.request_count   AS request_count,
               EXTRACT(EPOCH FROM (NOW() - d.last_health_at))::int AS health_age_sec,
               (cat.preferred_workloads @> '["code-gen"]'::jsonb)  AS is_code
          FROM fleet_model_deployments d
          JOIN fleet_model_catalog cat ON cat.id = d.catalog_id
         WHERE d.health_status = 'healthy'
           AND d.desired_state = 'active'
           AND cat.tool_calling = TRUE
           AND d.usable_agent_ctx IS NOT NULL
           AND d.usable_agent_ctx >= $1
        "#,
    )
    .bind(min_ctx)
    .fetch_all(pool)
    .await?;

    let mut supply = ServingSupply::default();
    for r in &rows {
        let id: sqlx::types::Uuid = r.get("id");
        let ep = ServingEndpoint {
            deployment_id: id.to_string(),
            worker_name: r.get("worker_name"),
            port: r.try_get("port").unwrap_or(0),
            catalog_id: r.try_get("catalog_id").ok(),
            request_count: r.try_get("request_count").unwrap_or(0),
            health_age_sec: r.try_get("health_age_sec").ok(),
        };
        if r.try_get::<bool, _>("is_code").unwrap_or(false) {
            supply.code_count += 1;
            supply.code_endpoints.push(ep);
        } else {
            supply.general_count += 1;
            supply.general_endpoints.push(ep);
        }
    }
    Ok(supply)
}

/// A candidate host the autoscaler can place a new model on. One row per online
/// computer, JOINed with its V114 reservation state, hardware facts, and current
/// deployment count (the least-loaded tiebreak). `free_ram_gb` is a conservative
/// estimate: `total_ram_gb` minus the summed size of resident active models on
/// that host (from `fleet_model_library` via the deployment join) — no reliance
/// on a possibly-stale metrics row.
#[derive(Debug, Clone)]
pub struct PlacementCandidate {
    pub worker_name: String,
    pub primary_ip: String,
    pub os_family: String,
    pub gpu_kind: Option<String>,
    pub has_gpu: bool,
    pub gpu_total_vram_gb: Option<f64>,
    pub total_ram_gb: Option<i32>,
    pub reservation_state: String,
    pub status: String,
    /// Active deployments currently on this host (load proxy).
    pub active_deployments: i64,
    /// GB of resident model weights already loaded on this host.
    pub resident_model_gb: f64,
    /// Conservative free RAM after accounting for resident models.
    pub free_ram_gb: f64,
}

/// All online computers as placement candidates, with reservation state, current
/// deployment counts, and resident-model RAM. Caller (the placement scorer in
/// `ff_agent::autoscaler`) applies hard gates + scoring.
pub async fn pg_placement_candidates(pool: &PgPool) -> Result<Vec<PlacementCandidate>> {
    let rows = sqlx::query(
        r#"
        WITH resident AS (
            SELECT d.worker_name,
                   COUNT(*)                                    AS n_active,
                   COALESCE(SUM(lib.size_bytes), 0)::float8    AS resident_bytes
              FROM fleet_model_deployments d
              LEFT JOIN fleet_model_library lib ON lib.id = d.library_id
             WHERE d.desired_state = 'active'
             GROUP BY d.worker_name
        )
        SELECT c.name              AS name,
               c.primary_ip        AS primary_ip,
               c.os_family         AS os_family,
               c.gpu_kind          AS gpu_kind,
               c.has_gpu           AS has_gpu,
               c.gpu_total_vram_gb AS gpu_total_vram_gb,
               c.total_ram_gb      AS total_ram_gb,
               c.reservation_state AS reservation_state,
               c.status            AS status,
               COALESCE(r.n_active, 0)        AS active_deployments,
               COALESCE(r.resident_bytes, 0)  AS resident_bytes
          FROM computers c
          LEFT JOIN resident r ON LOWER(r.worker_name) = LOWER(c.name)
         WHERE c.status = 'online'
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let total_ram_gb: Option<i32> = r.try_get("total_ram_gb").ok();
            let resident_bytes: f64 = r.try_get("resident_bytes").unwrap_or(0.0);
            let resident_model_gb = resident_bytes / 1e9;
            let free_ram_gb = (total_ram_gb.unwrap_or(0) as f64) - resident_model_gb;
            PlacementCandidate {
                worker_name: r.get("name"),
                primary_ip: r.try_get("primary_ip").unwrap_or_default(),
                os_family: r.try_get("os_family").unwrap_or_default(),
                gpu_kind: r.try_get("gpu_kind").ok(),
                has_gpu: r.try_get("has_gpu").unwrap_or(false),
                gpu_total_vram_gb: r.try_get("gpu_total_vram_gb").ok(),
                total_ram_gb,
                reservation_state: r
                    .try_get("reservation_state")
                    .unwrap_or_else(|_| "available".to_string()),
                status: r.try_get("status").unwrap_or_default(),
                active_deployments: r.try_get("active_deployments").unwrap_or(0),
                resident_model_gb,
                free_ram_gb,
            }
        })
        .collect())
}

/// The best library row on a host for a given kind that is NOT already deployed
/// there — what the autoscaler would `load`. `want_code = true` requires the
/// catalog `preferred_workloads @> '["code-gen"]'`; either way the model must be
/// tool-calling. Cheapest tier first (smallest appropriate model), then smallest
/// on-disk size. Returns `(library_id, catalog_id, runtime, size_gb)`.
pub async fn pg_loadable_library_for_kind(
    pool: &PgPool,
    worker_name: &str,
    want_code: bool,
) -> Result<Option<(String, String, String, f64)>> {
    let row = sqlx::query(
        r#"
        SELECT lib.id          AS lib_id,
               lib.catalog_id  AS catalog_id,
               lib.runtime     AS runtime,
               (lib.size_bytes::float8 / 1e9) AS size_gb
          FROM fleet_model_library lib
          JOIN fleet_model_catalog cat ON cat.id = lib.catalog_id
         WHERE lib.worker_name = $1
           AND cat.tool_calling = TRUE
           AND ($2 = (cat.preferred_workloads @> '["code-gen"]'::jsonb))
           AND NOT EXISTS (
               SELECT 1 FROM fleet_model_deployments d
                WHERE d.library_id = lib.id
                  AND d.desired_state = 'active'
           )
         ORDER BY cat.tier ASC, lib.size_bytes ASC
         LIMIT 1
        "#,
    )
    .bind(worker_name)
    .bind(want_code)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| {
        let lib_id: sqlx::types::Uuid = r.get("lib_id");
        (
            lib_id.to_string(),
            r.get::<String, _>("catalog_id"),
            r.get::<String, _>("runtime"),
            r.try_get::<f64, _>("size_gb").unwrap_or(0.0),
        )
    }))
}

/// Atomically reserve a host for the autoscaler: flip `reservation_state`
/// `available` → `reserved` via a conditional UPDATE (CAS). Returns `true` only
/// if THIS call won the reservation. A host already `reserved`/`drained` by
/// someone else returns `false` and the caller skips it — this is the hard fence
/// that stops the deploy wave + reconciler from fighting a model swap (V114).
pub async fn pg_reserve_host(pool: &PgPool, worker_name: &str, reason: &str) -> Result<bool> {
    let res = sqlx::query(
        r#"
        UPDATE computers
           SET reservation_state = 'reserved',
               reserved_reason   = $2,
               reserved_at       = NOW()
         WHERE LOWER(name) = LOWER($1)
           AND reservation_state = 'available'
        "#,
    )
    .bind(worker_name)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Release a host reservation: flip back to `available` (idempotent). Always
/// safe to call even if the host wasn't reserved by us — used by the
/// scope-guard unreserve so a crashed pass can't leave a host stuck.
pub async fn pg_unreserve_host(pool: &PgPool, worker_name: &str) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE computers
           SET reservation_state = 'available',
               reserved_reason   = NULL,
               reserved_at       = NULL
         WHERE LOWER(name) = LOWER($1)
        "#,
    )
    .bind(worker_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stale-reservation reaper: release any host the autoscaler reserved more than
/// `ttl_secs` ago and whose `reserved_reason` matches our owner tag. Guards
/// against a leader crash/failover mid-pass leaving a host stuck `reserved`.
/// Returns the number of reservations cleared.
pub async fn pg_reap_stale_reservations(
    pool: &PgPool,
    owner_reason: &str,
    ttl_secs: i64,
) -> Result<u64> {
    let res = sqlx::query(
        r#"
        UPDATE computers
           SET reservation_state = 'available',
               reserved_reason   = NULL,
               reserved_at       = NULL
         WHERE reservation_state = 'reserved'
           AND reserved_reason   = $1
           AND reserved_at       < NOW() - make_interval(secs => $2)
        "#,
    )
    .bind(owner_reason)
    .bind(ttl_secs.max(1))
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ─── Fleet agents catalog (V112) ─────────────────────────────────────────────
//
// The `fleet_agents` table is the canonical catalog of specialized agents the
// crew / orchestrator can instantiate by `name`. Mirrors the V105 skills shape:
// a DB row carries the system_prompt + allowed_tools + the capability the
// agent's endpoint must satisfy (require_tool_calling + min_ctx), which the
// crew feeds straight into [`pg_pick_agent_endpoint`].

/// One row from the `fleet_agents` catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetAgentRow {
    pub id: String,
    /// Stable handle used by the crew + CLI (e.g. "code-writer").
    pub name: String,
    pub role: String,
    pub description: Option<String>,
    pub system_prompt: String,
    /// jsonb array of tool names. Empty = inherit the session default tools.
    pub allowed_tools: JsonValue,
    /// jsonb array of "when to use" trigger strings.
    pub triggers: JsonValue,
    pub require_tool_calling: bool,
    pub min_ctx: i32,
    pub source: String,
    pub source_url: Option<String>,
    pub enabled: bool,
}

fn agent_row_from(r: &sqlx::postgres::PgRow) -> FleetAgentRow {
    let id: sqlx::types::Uuid = r.get("id");
    FleetAgentRow {
        id: id.to_string(),
        name: r.get("name"),
        role: r.get("role"),
        description: r.try_get("description").ok(),
        system_prompt: r.get("system_prompt"),
        allowed_tools: r.try_get("allowed_tools").unwrap_or(JsonValue::Null),
        triggers: r.try_get("triggers").unwrap_or(JsonValue::Null),
        require_tool_calling: r.try_get("require_tool_calling").unwrap_or(true),
        min_ctx: r.try_get("min_ctx").unwrap_or(16384),
        source: r.try_get("source").unwrap_or_default(),
        source_url: r.try_get("source_url").ok(),
        enabled: r.try_get("enabled").unwrap_or(true),
    }
}

const AGENT_COLS: &str = "id, name, role, description, system_prompt, allowed_tools, \
     triggers, require_tool_calling, min_ctx, source, source_url, enabled";

/// List agents from the catalog, ordered by name. `enabled_only = true`
/// returns only enabled rows (the set the crew/router should consider).
pub async fn pg_list_agents(pool: &PgPool, enabled_only: bool) -> Result<Vec<FleetAgentRow>> {
    let sql = format!(
        "SELECT {AGENT_COLS} FROM fleet_agents {} ORDER BY name",
        if enabled_only {
            "WHERE enabled = true"
        } else {
            ""
        }
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    Ok(rows.iter().map(agent_row_from).collect())
}

/// Fetch a single agent by its stable `name` handle. Returns `None` if no row
/// matches (the caller decides whether that's a hard error or a fallback).
pub async fn pg_get_agent(pool: &PgPool, name: &str) -> Result<Option<FleetAgentRow>> {
    let sql = format!("SELECT {AGENT_COLS} FROM fleet_agents WHERE name = $1");
    let row = sqlx::query(&sql).bind(name).fetch_optional(pool).await?;
    Ok(row.as_ref().map(agent_row_from))
}

/// Upsert an agent by `name` (used by the AGENT.md importer / `ff agents add`).
/// Returns the row id.
#[allow(clippy::too_many_arguments)]
pub async fn pg_upsert_agent(
    pool: &PgPool,
    name: &str,
    role: &str,
    description: Option<&str>,
    system_prompt: &str,
    allowed_tools: &JsonValue,
    triggers: &JsonValue,
    require_tool_calling: bool,
    min_ctx: i32,
    source: &str,
    source_url: Option<&str>,
) -> Result<String> {
    let row = sqlx::query(
        r#"
        INSERT INTO fleet_agents
            (name, role, description, system_prompt, allowed_tools, triggers,
             require_tool_calling, min_ctx, source, source_url, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
        ON CONFLICT (name) DO UPDATE
            SET role                 = EXCLUDED.role,
                description          = EXCLUDED.description,
                system_prompt        = EXCLUDED.system_prompt,
                allowed_tools        = EXCLUDED.allowed_tools,
                triggers             = EXCLUDED.triggers,
                require_tool_calling = EXCLUDED.require_tool_calling,
                min_ctx              = EXCLUDED.min_ctx,
                source               = EXCLUDED.source,
                source_url           = EXCLUDED.source_url,
                updated_at           = now()
        RETURNING id
        "#,
    )
    .bind(name)
    .bind(role)
    .bind(description)
    .bind(system_prompt)
    .bind(allowed_tools)
    .bind(triggers)
    .bind(require_tool_calling)
    .bind(min_ctx)
    .bind(source)
    .bind(source_url)
    .fetch_one(pool)
    .await?;
    let id: sqlx::types::Uuid = row.get("id");
    Ok(id.to_string())
}

/// Enable / disable an agent without deleting it. Returns true if a row changed.
pub async fn pg_set_agent_enabled(pool: &PgPool, name: &str, enabled: bool) -> Result<bool> {
    let r = sqlx::query("UPDATE fleet_agents SET enabled = $2, updated_at = now() WHERE name = $1")
        .bind(name)
        .bind(enabled)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
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
    worker_name: &str,
    models_dir: &str,
    total_bytes: i64,
    used_bytes: i64,
    free_bytes: i64,
    models_bytes: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_disk_usage (worker_name, models_dir, total_bytes, used_bytes, free_bytes, models_bytes)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(worker_name)
    .bind(models_dir)
    .bind(total_bytes)
    .bind(used_bytes)
    .bind(free_bytes)
    .bind(models_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── V118: disk-management resource + observability helpers ─────────────────

/// Latest free/total bytes for ONE node from `fleet_disk_usage`, plus how stale
/// the sample is. `(free_bytes, total_bytes, sample_age_secs)`. `None` when the
/// node has never been sampled.
///
/// This is the small "free-disk as a resource" read (#6) the future arbiter and
/// the transfer/placement pre-checks consume — a single point of truth so we
/// never re-implement `df` parsing in three places.
pub async fn pg_node_free_disk(
    pool: &PgPool,
    worker_name: &str,
) -> Result<Option<(i64, i64, i64)>> {
    let row = sqlx::query(
        "SELECT free_bytes, total_bytes,
                EXTRACT(EPOCH FROM (NOW() - sampled_at))::BIGINT AS age_secs
           FROM fleet_disk_usage
          WHERE worker_name = $1
          ORDER BY sampled_at DESC
          LIMIT 1",
    )
    .bind(worker_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        (
            r.get::<i64, _>("free_bytes"),
            r.get::<i64, _>("total_bytes"),
            r.get::<i64, _>("age_secs"),
        )
    }))
}

/// Set the `pinned` flag on a library row. A pinned row is never auto-evicted.
pub async fn pg_set_library_pinned(pool: &PgPool, id: &str, pinned: bool) -> Result<bool> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let r = sqlx::query("UPDATE fleet_model_library SET pinned = $2 WHERE id = $1")
        .bind(uuid)
        .bind(pinned)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Insert one observability row for a leader disk-reconcile pass. Returns the
/// run id.
#[allow(clippy::too_many_arguments)]
pub async fn pg_insert_disk_policy_run(
    pool: &PgPool,
    mode: &str,
    nodes_over_quota: i32,
    planned_deletes: i32,
    planned_moves: i32,
    actuated_deletes: i32,
    actuated_moves: i32,
    bytes_planned: i64,
    bytes_freed: i64,
    detail: &serde_json::Value,
) -> Result<i64> {
    let row = sqlx::query(
        "INSERT INTO disk_policy_runs
            (mode, nodes_over_quota, planned_deletes, planned_moves,
             actuated_deletes, actuated_moves, bytes_planned, bytes_freed, detail)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         RETURNING id",
    )
    .bind(mode)
    .bind(nodes_over_quota)
    .bind(planned_deletes)
    .bind(planned_moves)
    .bind(actuated_deletes)
    .bind(actuated_moves)
    .bind(bytes_planned)
    .bind(bytes_freed)
    .bind(detail)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<i64, _>("id"))
}

/// Open a move-log row when the active tick starts a relocation. Returns id.
#[allow(clippy::too_many_arguments)]
pub async fn pg_open_disk_move(
    pool: &PgPool,
    source_node: &str,
    target_node: &str,
    catalog_id: &str,
    runtime: &str,
    src_library_id: &str,
    size_bytes: i64,
) -> Result<i64> {
    let row = sqlx::query(
        "INSERT INTO disk_move_log
            (source_node, target_node, catalog_id, runtime, src_library_id, size_bytes)
         VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
    )
    .bind(source_node)
    .bind(target_node)
    .bind(catalog_id)
    .bind(runtime)
    .bind(src_library_id)
    .bind(size_bytes)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<i64, _>("id"))
}

/// Advance a move-log row's status (verified / source_deleted / failed) and
/// optionally record the new target library id and/or an error message.
pub async fn pg_update_disk_move(
    pool: &PgPool,
    id: i64,
    status: &str,
    dst_library_id: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    let finished = matches!(status, "source_deleted" | "failed");
    sqlx::query(
        "UPDATE disk_move_log
            SET status = $2,
                dst_library_id = COALESCE($3, dst_library_id),
                error = COALESCE($4, error),
                finished_at = CASE WHEN $5 THEN NOW() ELSE finished_at END
          WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(dst_library_id)
    .bind(error)
    .bind(finished)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get the latest disk usage sample per node.
pub async fn pg_latest_disk_usage(
    pool: &PgPool,
) -> Result<
    Vec<(
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        chrono::DateTime<chrono::Utc>,
    )>,
> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (worker_name)
                worker_name, models_dir, total_bytes, used_bytes, free_bytes, models_bytes, sampled_at
           FROM fleet_disk_usage
          ORDER BY worker_name, sampled_at DESC
          LIMIT 100",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("worker_name"),
                r.get::<String, _>("models_dir"),
                r.get::<i64, _>("total_bytes"),
                r.get::<i64, _>("used_bytes"),
                r.get::<i64, _>("free_bytes"),
                r.get::<i64, _>("models_bytes"),
                r.get::<chrono::DateTime<chrono::Utc>, _>("sampled_at"),
            )
        })
        .collect())
}

/// A model lifecycle job (download, delete, load, etc.) — tracks progress.
#[derive(Debug, Clone)]
pub struct ModelJobRow {
    pub id: String,
    pub worker_name: String,
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
    worker_name: &str,
    kind: &str,
    target_catalog_id: Option<&str>,
    target_library_id: Option<&str>,
    params: &JsonValue,
) -> Result<String> {
    let lib_uuid = target_library_id
        .map(|s| {
            sqlx::types::Uuid::parse_str(s)
                .map_err(|e| crate::error::DbError::NotFound(format!("bad library uuid {s}: {e}")))
        })
        .transpose()?;
    let row = sqlx::query(
        "INSERT INTO fleet_model_jobs (worker_name, kind, target_catalog_id, target_library_id, params)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(worker_name)
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
#[allow(clippy::too_many_arguments)]
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
pub async fn pg_list_jobs(
    pool: &PgPool,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<ModelJobRow>> {
    let rows = if let Some(s) = status {
        sqlx::query(
            "SELECT * FROM fleet_model_jobs WHERE status = $1 ORDER BY created_at DESC LIMIT $2",
        )
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
    Ok(rows
        .iter()
        .map(|r| {
            let id: sqlx::types::Uuid = r.get("id");
            let lib_id: Option<sqlx::types::Uuid> = r.get("target_library_id");
            ModelJobRow {
                id: id.to_string(),
                worker_name: r.get("worker_name"),
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
        })
        .collect())
}

// ─── Onboarding: SSH keys + mesh status (schema V12) ──────────────────────

/// One SSH key row for a fleet worker.
#[derive(Debug, Clone)]
pub struct WorkerSshKeyRow {
    pub worker_name: String,
    pub key_purpose: String, // 'user' | 'host'
    pub public_key: String,
    pub key_type: String, // 'ed25519' | 'rsa' | 'ecdsa'
    pub fingerprint: String,
    pub added_at: chrono::DateTime<chrono::Utc>,
}

/// Upsert a public key for a worker. Idempotent on (worker_name, fingerprint).
pub async fn pg_insert_worker_ssh_key(
    pool: &PgPool,
    worker_name: &str,
    key_purpose: &str,
    public_key: &str,
    key_type: &str,
    fingerprint: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fleet_workers_ssh_keys (worker_name, key_purpose, public_key, key_type, fingerprint)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (worker_name, fingerprint) DO UPDATE SET
            public_key = EXCLUDED.public_key,
            key_type = EXCLUDED.key_type,
            key_purpose = EXCLUDED.key_purpose",
    )
    .bind(worker_name)
    .bind(key_purpose)
    .bind(public_key)
    .bind(key_type)
    .bind(fingerprint)
    .execute(pool)
    .await?;
    Ok(())
}

/// Legacy alias retained during the rename window. Calls
/// [`pg_insert_worker_ssh_key`] unchanged.
pub async fn pg_insert_node_ssh_key(
    pool: &PgPool,
    worker_name: &str,
    key_purpose: &str,
    public_key: &str,
    key_type: &str,
    fingerprint: &str,
) -> Result<()> {
    pg_insert_worker_ssh_key(
        pool,
        worker_name,
        key_purpose,
        public_key,
        key_type,
        fingerprint,
    )
    .await
}

/// List SSH keys for a worker (optionally filtered by purpose: 'user' or 'host').
pub async fn pg_list_worker_ssh_keys(
    pool: &PgPool,
    worker_name: &str,
    purpose: Option<&str>,
) -> Result<Vec<WorkerSshKeyRow>> {
    let rows = if let Some(p) = purpose {
        sqlx::query(
            "SELECT * FROM fleet_workers_ssh_keys
              WHERE worker_name = $1 AND key_purpose = $2
              ORDER BY added_at
              LIMIT 100",
        )
        .bind(worker_name)
        .bind(p)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT * FROM fleet_workers_ssh_keys WHERE worker_name = $1 ORDER BY added_at LIMIT 100",
        )
        .bind(worker_name)
        .fetch_all(pool)
        .await?
    };
    Ok(rows
        .iter()
        .map(|r| WorkerSshKeyRow {
            worker_name: r.get("worker_name"),
            key_purpose: r.get("key_purpose"),
            public_key: r.get("public_key"),
            key_type: r.get("key_type"),
            fingerprint: r.get("fingerprint"),
            added_at: r.get("added_at"),
        })
        .collect())
}

/// Legacy alias retained during the rename window.
pub async fn pg_list_node_ssh_keys(
    pool: &PgPool,
    worker_name: &str,
    purpose: Option<&str>,
) -> Result<Vec<WorkerSshKeyRow>> {
    pg_list_worker_ssh_keys(pool, worker_name, purpose).await
}

/// Delete all SSH keys for a worker (used during `ff onboard revoke`).
pub async fn pg_delete_worker_ssh_keys(pool: &PgPool, worker_name: &str) -> Result<u64> {
    let r = sqlx::query("DELETE FROM fleet_workers_ssh_keys WHERE worker_name = $1")
        .bind(worker_name)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// Legacy alias retained during the rename window.
pub async fn pg_delete_node_ssh_keys(pool: &PgPool, worker_name: &str) -> Result<u64> {
    pg_delete_worker_ssh_keys(pool, worker_name).await
}

/// One row in the mesh-reachability matrix.
#[derive(Debug, Clone)]
pub struct MeshStatusRow {
    pub src_node: String,
    pub dst_node: String,
    pub status: String, // 'ok' | 'failed' | 'pending'
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
pub async fn pg_list_mesh_status(pool: &PgPool, node: Option<&str>) -> Result<Vec<MeshStatusRow>> {
    let rows = if let Some(n) = node {
        sqlx::query(
            "SELECT * FROM fleet_mesh_status
              WHERE src_node = $1 OR dst_node = $1
              ORDER BY src_node, dst_node
              LIMIT 100",
        )
        .bind(n)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query("SELECT * FROM fleet_mesh_status ORDER BY src_node, dst_node LIMIT 100")
            .fetch_all(pool)
            .await?
    };
    Ok(rows
        .iter()
        .map(|r| MeshStatusRow {
            src_node: r.get("src_node"),
            dst_node: r.get("dst_node"),
            status: r.get("status"),
            last_checked: r.get("last_checked"),
            last_error: r.get("last_error"),
            attempts: r.get("attempts"),
        })
        .collect())
}

/// Remove all mesh-status rows involving a given node (used during revoke).
pub async fn pg_delete_mesh_status_for_node(pool: &PgPool, node: &str) -> Result<u64> {
    let r = sqlx::query("DELETE FROM fleet_mesh_status WHERE src_node = $1 OR dst_node = $1")
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
#[allow(clippy::too_many_arguments)]
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

/// Force-cancel a deferred task by id, INCLUDING a stuck `running` task.
/// `pg_cancel_deferred` refuses `running` (a live worker may own it); this is
/// the operator escape hatch (`ff defer cancel --force`) for tasks orphaned by
/// a worker that died/restarted mid-run. Returns true if a row was updated.
pub async fn pg_force_cancel_deferred(pool: &PgPool, id: &str) -> Result<bool> {
    let uuid = sqlx::types::Uuid::parse_str(id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad uuid {id}: {e}")))?;
    let r = sqlx::query(
        "UPDATE deferred_tasks
            SET status = 'cancelled', completed_at = NOW(),
                last_error = LEFT(COALESCE(last_error, '') || ' [force-cancelled by operator]', 500)
          WHERE id = $1
            AND status IN ('pending', 'dispatchable', 'failed', 'running')",
    )
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Reap deferred tasks stuck in `running` longer than `max_age_secs` — i.e.
/// orphaned because the worker that claimed them died or restarted mid-run
/// (common during upgrade waves) with no one to mark them done. Without this
/// they sit `running` forever, accumulating (613 found 2026-06-01) and
/// blocking the per-family upgrade singleton.
///
/// Reclaim semantics mirror a normal failed attempt: increment `attempts`,
/// clear the claim, and re-queue as `pending` for another worker — UNLESS
/// `max_attempts` is now exhausted, in which case the task goes terminal
/// `failed`. Returns the number of tasks reaped.
pub async fn pg_reap_stale_running(pool: &PgPool, max_age_secs: i64) -> Result<u64> {
    let r = sqlx::query(
        "UPDATE deferred_tasks
            SET attempts = attempts + 1,
                status = CASE WHEN attempts + 1 >= max_attempts THEN 'failed' ELSE 'pending' END,
                claimed_by = NULL,
                claimed_at = NULL,
                next_attempt_at = NOW(),
                completed_at = CASE WHEN attempts + 1 >= max_attempts THEN NOW() ELSE completed_at END,
                last_error = LEFT(COALESCE(last_error, '') ||
                    ' [reaped: orphaned in running > ' || $1::text || 's, worker presumed dead]', 500)
          WHERE status = 'running'
            AND claimed_at IS NOT NULL
            AND claimed_at < NOW() - ($1 * INTERVAL '1 second')",
    )
    .bind(max_age_secs)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
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
                last_error = NULL,
                -- Clear the prior failure's captured output too. Since
                -- pg_finish_deferred now persists result on failure, a retry
                -- that only cleared last_error would leave a stale stderr that
                -- `ff defer get` renders as the (now full) output stream.
                result = NULL
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

    // manual / now / operator: promote immediately. `manual` is for retry
    // loops; `now` is fire-and-forget; `operator` is a human (or `ff fleet
    // upgrade`, version_check, mesh_check) enqueue that should dispatch
    // as soon as the queue is pumped. Before adding `operator` here, those
    // enqueues sat at status='pending' forever — discovered 2026-05-16
    // with 38 operator-triggered tasks accumulated indefinitely.
    let immediate_promoted = sqlx::query(
        "UPDATE deferred_tasks
            SET status = 'dispatchable',
                next_attempt_at = NOW()
          WHERE status = 'pending'
            AND trigger_type IN ('manual', 'now', 'operator')
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
        // Note: attempts is NOT bumped here. It moved to the failure
        // finalize path (pg_finish_deferred) so `--max-attempts N`
        // means "max N failures", not "max N claims" — a worker crash
        // mid-task no longer burns a retry slot.
        sqlx::query(
            "UPDATE deferred_tasks
                SET status = 'running',
                    claimed_by = $1,
                    claimed_at = NOW()
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
        // Bump attempts here (not in pg_claim_deferred) so the counter
        // tracks actual failures, not claim/restart noise. Retry while
        // (attempts + 1) < max_attempts; else terminal fail.
        sqlx::query(
            "UPDATE deferred_tasks
                SET attempts = attempts + 1,
                    status = CASE
                        WHEN attempts + 1 >= max_attempts THEN 'failed'
                        ELSE 'pending'
                    END,
                    last_error = $1,
                    -- Keep the full execution output (stdout/stderr) on failure
                    -- too, so `ff defer get` can surface the complete stderr
                    -- instead of only the truncated last_error summary.
                    result = COALESCE($3, result),
                    claimed_by = NULL,
                    claimed_at = NULL,
                    -- Exponential backoff capped at 4h: 1m, 5m, 30m, 1h, 4h
                    next_attempt_at = NOW() + (LEAST(240, GREATEST(1, POWER(5, attempts + 1)::int)) * INTERVAL '1 minute')
              WHERE id = $2",
        )
        .bind(error)
        .bind(uuid)
        .bind(result)
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
pub async fn pg_list_secrets(
    pool: &PgPool,
) -> Result<
    Vec<(
        String,
        Option<String>,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    )>,
> {
    let rows = sqlx::query(
        "SELECT key, description, updated_by, updated_at
         FROM fleet_secrets ORDER BY key
         LIMIT 100",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("key"),
                r.get::<Option<String>, _>("description"),
                r.get::<Option<String>, _>("updated_by"),
                r.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
            )
        })
        .collect())
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
            gpu_kind: None,
            gpu_model: None,
            gpu_vram_gb: None,
            gpu_total_vram_gb: None,
            has_gpu: None,
            computer_ram_gb: None,
            computer_cpu_cores: None,
        };

        pg_upsert_node(pool, &node_row).await?;
        node_count += 1;

        // Insert models for this node.
        for (slug, model_cfg) in &node_cfg.models {
            let model_id = format!("{name}:{slug}");
            let model_row = FleetModelRow {
                id: model_id,
                worker_name: name.clone(),
                slug: slug.clone(),
                name: model_cfg.name.clone(),
                family: model_cfg.family.clone().unwrap_or_default(),
                port: model_cfg.port.unwrap_or(0) as i32,
                tier: model_cfg.tier as i32,
                local_model: model_cfg.local.unwrap_or(true),
                lifecycle: model_cfg
                    .lifecycle
                    .clone()
                    .unwrap_or_else(|| "production".into()),
                mode: model_cfg.mode.clone().unwrap_or_else(|| "always_on".into()),
                preferred_workloads: serde_json::to_value(&model_cfg.preferred_workloads)
                    .unwrap_or_default(),
            };

            pg_upsert_model(pool, &model_row).await?;
            model_count += 1;
        }
    }

    // Seed settings from various config sections.
    pg_set_setting(
        pool,
        "scheduling",
        &serde_json::to_value(&config.scheduling)?,
    )
    .await?;
    pg_set_setting(pool, "ports", &serde_json::to_value(&config.ports)?).await?;
    pg_set_setting(pool, "llm", &serde_json::to_value(&config.llm)?).await?;
    pg_set_setting(
        pool,
        "enrollment",
        &serde_json::to_value(&config.enrollment)?,
    )
    .await?;
    pg_set_setting(pool, "fleet", &serde_json::to_value(&config.fleet)?).await?;

    info!(
        nodes = node_count,
        models = model_count,
        "seeded postgres fleet tables from fleet.toml"
    );

    Ok(())
}

// ─── Load FleetConfig FROM Postgres ─────────────────────────────────────────

/// Load a `FleetConfig` from Postgres tables.
///
/// This is the inverse of `seed_from_fleet_toml`.  Nodes that already have a
/// minimal `fleet.toml` with a `[database]` section can call this on startup
/// to pull the rest of the fleet state from the shared Postgres.
///
/// The returned `FleetConfig` contains:
/// - `nodes`  → from `fleet_workers` + `fleet_models`
/// - `fleet`, `scheduling`, `ports`, `llm`, `enrollment`  → from `fleet_settings`
/// - `database` and `redis` are **NOT** overwritten (keep local bootstrap values)
pub async fn load_fleet_config_from_postgres(
    pool: &PgPool,
    bootstrap: &ff_core::config::FleetConfig,
) -> Result<ff_core::config::FleetConfig> {
    use ff_core::config::*;
    use ff_core::types::Role;
    use tracing::{info, warn};

    let mut config = bootstrap.clone();

    // ── Nodes + Models ──
    let pg_nodes = pg_list_nodes(pool).await?;
    let pg_models = pg_list_models(pool).await?;

    let mut nodes: HashMap<String, NodeConfig> = HashMap::new();
    for n in pg_nodes {
        let role = serde_json::from_value::<Role>(serde_json::Value::String(n.role.clone()))
            .unwrap_or(Role::Worker);
        let mut node = NodeConfig {
            ip: n.ip,
            ssh_user: Some(n.ssh_user).filter(|s| !s.is_empty()),
            ram_gb: Some(n.ram_gb as u64),
            cpu_cores: Some(n.cpu_cores as u32),
            os: Some(n.os).filter(|s| !s.is_empty()),
            role,
            election_priority: Some(n.election_priority as u32),
            alt_ips: serde_json::from_value(n.alt_ips).unwrap_or_default(),
            resources: serde_json::from_value(n.resources).ok(),
            capabilities: serde_json::from_value(n.capabilities).ok(),
            preferences: serde_json::from_value(n.preferences).ok(),
            ..Default::default()
        };

        // Attach models that belong to this node.
        for m in pg_models.iter().filter(|m| m.worker_name == n.name) {
            let model_cfg = NodeModelConfig {
                name: m.name.clone(),
                family: Some(m.family.clone()).filter(|s| !s.is_empty()),
                port: Some(m.port as u16),
                tier: m.tier as u32,
                local: Some(m.local_model),
                lifecycle: Some(m.lifecycle.clone()),
                mode: Some(m.mode.clone()),
                preferred_workloads: serde_json::from_value(m.preferred_workloads.clone())
                    .unwrap_or_default(),
            };
            node.models.insert(m.slug.clone(), model_cfg);
        }

        nodes.insert(n.name, node);
    }
    config.nodes = nodes;

    // ── Settings ──
    macro_rules! load_setting {
        ($key:expr, $ty:ty, $field:ident) => {
            match pg_get_setting(pool, $key).await? {
                Some(v) => match serde_json::from_value::<$ty>(v) {
                    Ok(parsed) => {
                        config.$field = parsed;
                    }
                    Err(e) => {
                        warn!(key = $key, error = %e, "failed to parse fleet_setting");
                    }
                },
                None => {
                    warn!(key = $key, "fleet_setting missing in postgres");
                }
            }
        };
    }

    load_setting!("fleet", FleetSettings, fleet);
    load_setting!("scheduling", SchedulingConfig, scheduling);
    load_setting!("ports", PortsConfig, ports);
    load_setting!("llm", LlmConfig, llm);
    load_setting!("enrollment", EnrollmentConfig, enrollment);
    load_setting!("notifications", NotificationsConfig, notifications);
    load_setting!("transport", TransportConfig, transport);
    load_setting!("agent", AgentSettings, agent);
    load_setting!("loops", LoopSettings, loops);

    // ── Services ──
    if let Some(services_val) = pg_get_setting(pool, "services").await?
        && let Ok(map) = serde_json::from_value::<HashMap<String, ServiceConfig>>(services_val)
    {
        config.services = map;
    }

    // ── MCP configs ──
    if let Some(mcp_val) = pg_get_setting(pool, "mcp").await?
        && let Ok(map) = serde_json::from_value::<HashMap<String, McpConfig>>(mcp_val)
    {
        config.mcp = map;
    }

    info!(
        nodes = config.nodes.len(),
        "loaded fleet config from postgres"
    );

    Ok(config)
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
        let node = WorkerRow {
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
        let node = WorkerRow {
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
            &WorkerRow {
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
            worker_name: Some("taylor".into()),
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

        upsert_fleet_worker_runtime(
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
                models_json: r#"["qwen3-coder-30b","llama-3.1-8b"]"#.to_string(),
                capabilities_json: r#"{"tool_exec":true,"voice":false}"#.to_string(),
                stale_degraded_after_secs: 45,
                stale_offline_after_secs: 120,
            },
        )
        .unwrap();

        let rows = list_fleet_worker_runtime(&conn).unwrap();
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

        upsert_fleet_worker_runtime(
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
        let degraded_rows = list_fleet_worker_runtime_at(&conn, degraded_now).unwrap();
        assert_eq!(degraded_rows.len(), 1);
        assert_eq!(degraded_rows[0].derived_status, "degraded");
        assert!(degraded_rows[0].heartbeat_age_secs >= 12);

        let offline_now = Utc::now() + Duration::seconds(25);
        let offline_rows = list_fleet_worker_runtime_at(&conn, offline_now).unwrap();
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

    // ── offload_workload_for_kind: pure kind→workload-tag mapping (no DB) ──

    #[test]
    fn offload_workload_maps_code_kinds_to_code_gen() {
        // Exactly the four kinds the impl matches (post-lowercase) → "code-gen".
        for kind in ["codegen", "edits", "tests", "code"] {
            assert_eq!(
                offload_workload_for_kind(Some(kind)),
                Some("code-gen"),
                "kind {kind:?} should prefer a code-gen coder model"
            );
        }
    }

    #[test]
    fn offload_workload_is_case_insensitive_for_code_kinds() {
        // Impl lowercases input before matching, so upper/mixed case still maps.
        assert_eq!(offload_workload_for_kind(Some("CODEGEN")), Some("code-gen"));
        assert_eq!(offload_workload_for_kind(Some("Code")), Some("code-gen"));
        assert_eq!(offload_workload_for_kind(Some("Tests")), Some("code-gen"));
    }

    #[test]
    fn offload_workload_unmapped_kinds_are_none() {
        // Any non-code kind has no workload preference → None (routes to the
        // cheapest warm tool-capable model).
        assert_eq!(offload_workload_for_kind(Some("chat")), None);
        assert_eq!(offload_workload_for_kind(Some("summarize")), None);
        // Empty string is just another unmapped kind.
        assert_eq!(offload_workload_for_kind(Some("")), None);
    }

    #[test]
    fn offload_workload_none_input_is_none() {
        assert_eq!(offload_workload_for_kind(None), None);
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
         WHERE task_id = $1 ORDER BY id LIMIT 100",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let hops_json: Vec<serde_json::Value> = hops
        .iter()
        .map(|r| {
            serde_json::json!({
                "from": r.try_get::<String, _>("from_node").unwrap_or_default(),
                "to": r.try_get::<String, _>("to_node").unwrap_or_default(),
                "reason": r.try_get::<String, _>("reason").unwrap_or_default(),
                "at": r.try_get::<String, _>("routed_at").unwrap_or_default(),
            })
        })
        .collect();

    // Ownership events
    let events = sqlx::query(
        "SELECT event_type, from_owner, to_owner, reason, created_at FROM ownership_events
         WHERE task_id = $1 ORDER BY id LIMIT 100",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let events_json: Vec<serde_json::Value> = events
        .iter()
        .map(|r| {
            serde_json::json!({
                "event": r.try_get::<String, _>("event_type").unwrap_or_default(),
                "from": r.try_get::<Option<String>, _>("from_owner").unwrap_or_default(),
                "to": r.try_get::<Option<String>, _>("to_owner").unwrap_or_default(),
                "reason": r.try_get::<Option<String>, _>("reason").unwrap_or_default(),
                "at": r.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "task_id": task_id,
        "routing": hops_json,
        "ownership_events": events_json,
    }))
}

// ═══════════════════════════════════════════════════════════════════════════
// ─── Virtual Brain (Schema V13) ───────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════

// ── brain_users ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BrainUserRow {
    pub id: uuid::Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn pg_create_brain_user(
    pool: &PgPool,
    name: &str,
    display_name: Option<&str>,
) -> Result<uuid::Uuid> {
    let row = sqlx::query(
        "INSERT INTO brain_users (name, display_name) VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE SET display_name = COALESCE(EXCLUDED.display_name, brain_users.display_name)
         RETURNING id",
    )
    .bind(name)
    .bind(display_name)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_get_brain_user(pool: &PgPool, name: &str) -> Result<Option<BrainUserRow>> {
    let row = sqlx::query("SELECT * FROM brain_users WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| BrainUserRow {
        id: r.get("id"),
        name: r.get("name"),
        display_name: r.get("display_name"),
        created_at: r.get("created_at"),
    }))
}

pub async fn pg_get_brain_user_by_id(
    pool: &PgPool,
    id: uuid::Uuid,
) -> Result<Option<BrainUserRow>> {
    let row = sqlx::query("SELECT * FROM brain_users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| BrainUserRow {
        id: r.get("id"),
        name: r.get("name"),
        display_name: r.get("display_name"),
        created_at: r.get("created_at"),
    }))
}

// ── brain_channel_identities ─────────────────────────────────────────────

pub async fn pg_upsert_channel_identity(
    pool: &PgPool,
    channel: &str,
    external_id: &str,
    user_id: uuid::Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO brain_channel_identities (channel, external_id, user_id) VALUES ($1, $2, $3)
         ON CONFLICT (channel, external_id) DO UPDATE SET user_id = $3",
    )
    .bind(channel)
    .bind(external_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_resolve_channel_user(
    pool: &PgPool,
    channel: &str,
    external_id: &str,
) -> Result<Option<uuid::Uuid>> {
    let row = sqlx::query(
        "SELECT user_id FROM brain_channel_identities WHERE channel = $1 AND external_id = $2",
    )
    .bind(channel)
    .bind(external_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get("user_id")))
}

// ── brain_threads ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrainThreadRow {
    pub id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub slug: String,
    pub title: Option<String>,
    pub icon: Option<String>,
    pub project: Option<String>,
    pub status: String,
    pub last_message_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn row_to_brain_thread(r: &sqlx::postgres::PgRow) -> BrainThreadRow {
    BrainThreadRow {
        id: r.get("id"),
        user_id: r.get("user_id"),
        slug: r.get("slug"),
        title: r.get("title"),
        icon: r.get("icon"),
        project: r.get("project"),
        status: r.get("status"),
        last_message_at: r.get("last_message_at"),
        created_at: r.get("created_at"),
    }
}

pub async fn pg_create_brain_thread(
    pool: &PgPool,
    user_id: uuid::Uuid,
    slug: &str,
    title: Option<&str>,
    project: Option<&str>,
) -> Result<uuid::Uuid> {
    let row = sqlx::query(
        "INSERT INTO brain_threads (user_id, slug, title, project) VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, slug) DO UPDATE SET
            title = COALESCE(EXCLUDED.title, brain_threads.title),
            project = COALESCE(EXCLUDED.project, brain_threads.project)
         RETURNING id",
    )
    .bind(user_id)
    .bind(slug)
    .bind(title)
    .bind(project)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_get_brain_thread(
    pool: &PgPool,
    user_id: uuid::Uuid,
    slug: &str,
) -> Result<Option<BrainThreadRow>> {
    let row = sqlx::query("SELECT * FROM brain_threads WHERE user_id = $1 AND slug = $2")
        .bind(user_id)
        .bind(slug)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_brain_thread))
}

pub async fn pg_get_brain_thread_by_id(
    pool: &PgPool,
    id: uuid::Uuid,
) -> Result<Option<BrainThreadRow>> {
    let row = sqlx::query("SELECT * FROM brain_threads WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_brain_thread))
}

pub async fn pg_list_brain_threads(
    pool: &PgPool,
    user_id: uuid::Uuid,
) -> Result<Vec<BrainThreadRow>> {
    let rows = sqlx::query(
        "SELECT * FROM brain_threads WHERE user_id = $1 AND status = 'active'
         ORDER BY last_message_at DESC NULLS LAST, created_at DESC
         LIMIT 100",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_brain_thread).collect())
}

pub async fn pg_archive_brain_thread(pool: &PgPool, id: uuid::Uuid) -> Result<()> {
    sqlx::query("UPDATE brain_threads SET status = 'archived' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn pg_touch_brain_thread(pool: &PgPool, id: uuid::Uuid) -> Result<()> {
    sqlx::query("UPDATE brain_threads SET last_message_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── brain_thread_attachments ─────────────────────────────────────────────

pub async fn pg_attach_thread(
    pool: &PgPool,
    channel: &str,
    external_id: &str,
    user_id: uuid::Uuid,
    thread_id: uuid::Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO brain_thread_attachments (channel, external_id, user_id, thread_id, attached_at)
         VALUES ($1, $2, $3, $4, NOW())
         ON CONFLICT (channel, external_id) DO UPDATE SET thread_id = $4, attached_at = NOW()",
    )
    .bind(channel)
    .bind(external_id)
    .bind(user_id)
    .bind(thread_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_get_attached_thread(
    pool: &PgPool,
    channel: &str,
    external_id: &str,
) -> Result<Option<uuid::Uuid>> {
    let row = sqlx::query(
        "SELECT thread_id FROM brain_thread_attachments WHERE channel = $1 AND external_id = $2",
    )
    .bind(channel)
    .bind(external_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get("thread_id")))
}

// ── brain_messages ───────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrainMessageRow {
    pub id: uuid::Uuid,
    pub thread_id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub channel: String,
    pub external_id: String,
    pub role: String,
    pub content: String,
    pub metadata: JsonValue,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn row_to_brain_message(r: &sqlx::postgres::PgRow) -> BrainMessageRow {
    BrainMessageRow {
        id: r.get("id"),
        thread_id: r.get("thread_id"),
        user_id: r.get("user_id"),
        channel: r.get("channel"),
        external_id: r.get("external_id"),
        role: r.get("role"),
        content: r.get("content"),
        metadata: r.get("metadata"),
        created_at: r.get("created_at"),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn pg_insert_brain_message(
    pool: &PgPool,
    thread_id: uuid::Uuid,
    user_id: uuid::Uuid,
    channel: &str,
    external_id: &str,
    role: &str,
    content: &str,
    metadata: Option<&JsonValue>,
) -> Result<uuid::Uuid> {
    let row = sqlx::query(
        "INSERT INTO brain_messages (thread_id, user_id, channel, external_id, role, content, metadata)
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, '{}'))
         RETURNING id",
    )
    .bind(thread_id)
    .bind(user_id)
    .bind(channel)
    .bind(external_id)
    .bind(role)
    .bind(content)
    .bind(metadata)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_list_brain_messages(
    pool: &PgPool,
    thread_id: uuid::Uuid,
    limit: i64,
) -> Result<Vec<BrainMessageRow>> {
    let rows = sqlx::query(
        "SELECT * FROM brain_messages WHERE thread_id = $1
         ORDER BY created_at DESC LIMIT $2",
    )
    .bind(thread_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_brain_message).collect())
}

// ── brain_vault_nodes ────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrainVaultNodeRow {
    pub id: uuid::Uuid,
    pub path: String,
    pub title: String,
    pub node_type: Option<String>,
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub extends_path: Option<String>,
    pub applies_to: Vec<String>,
    pub from_thread: Option<String>,
    pub confidence: Option<f32>,
    pub content_hash: String,
    pub valid_from: chrono::DateTime<chrono::Utc>,
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
    pub superseded_by: Option<uuid::Uuid>,
    pub hits: i32,
    pub references_: i32,
    pub last_accessed: chrono::DateTime<chrono::Utc>,
    pub community_id: Option<i32>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

fn row_to_brain_vault_node(r: &sqlx::postgres::PgRow) -> BrainVaultNodeRow {
    BrainVaultNodeRow {
        id: r.get("id"),
        path: r.get("path"),
        title: r.get("title"),
        node_type: r.get("node_type"),
        project: r.get("project"),
        tags: r.get("tags"),
        extends_path: r.get("extends_path"),
        applies_to: r.get("applies_to"),
        from_thread: r.get("from_thread"),
        confidence: r.get("confidence"),
        content_hash: r.get("content_hash"),
        valid_from: r.get("valid_from"),
        valid_until: r.get("valid_until"),
        superseded_by: r.get("superseded_by"),
        hits: r.get("hits"),
        references_: r.get("references_"),
        last_accessed: r.get("last_accessed"),
        community_id: r.get("community_id"),
        updated_at: r.get("updated_at"),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn pg_upsert_brain_vault_node(
    pool: &PgPool,
    path: &str,
    title: &str,
    node_type: Option<&str>,
    project: Option<&str>,
    tags: &[String],
    extends_path: Option<&str>,
    applies_to: &[String],
    from_thread: Option<&str>,
    confidence: Option<f32>,
    content_hash: &str,
) -> Result<uuid::Uuid> {
    let row = sqlx::query(
        "INSERT INTO brain_vault_nodes
            (path, title, node_type, project, tags, extends_path, applies_to,
             from_thread, confidence, content_hash, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW())
         ON CONFLICT (path) DO UPDATE SET
            title = EXCLUDED.title,
            node_type = COALESCE(EXCLUDED.node_type, brain_vault_nodes.node_type),
            project = COALESCE(EXCLUDED.project, brain_vault_nodes.project),
            tags = EXCLUDED.tags,
            extends_path = EXCLUDED.extends_path,
            applies_to = EXCLUDED.applies_to,
            from_thread = COALESCE(EXCLUDED.from_thread, brain_vault_nodes.from_thread),
            confidence = COALESCE(EXCLUDED.confidence, brain_vault_nodes.confidence),
            content_hash = EXCLUDED.content_hash,
            updated_at = NOW()
         RETURNING id",
    )
    .bind(path)
    .bind(title)
    .bind(node_type)
    .bind(project)
    .bind(tags)
    .bind(extends_path)
    .bind(applies_to)
    .bind(from_thread)
    .bind(confidence)
    .bind(content_hash)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_get_brain_vault_node(
    pool: &PgPool,
    path: &str,
) -> Result<Option<BrainVaultNodeRow>> {
    let row = sqlx::query("SELECT * FROM brain_vault_nodes WHERE path = $1")
        .bind(path)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_brain_vault_node))
}

pub async fn pg_list_brain_vault_nodes_current(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Vec<BrainVaultNodeRow>> {
    let rows = if let Some(p) = project {
        sqlx::query("SELECT * FROM brain_vault_nodes WHERE valid_until IS NULL AND project = $1 ORDER BY updated_at DESC LIMIT 100")
            .bind(p)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query(
            "SELECT * FROM brain_vault_nodes WHERE valid_until IS NULL ORDER BY updated_at DESC LIMIT 100",
        )
        .fetch_all(pool)
        .await?
    };
    Ok(rows.iter().map(row_to_brain_vault_node).collect())
}

/// Count current (non-superseded) brain_vault_nodes. Unlike
/// `pg_list_brain_vault_nodes_current` (which is capped at LIMIT 100 for display),
/// this returns the true total so `ff brain stats` can report >100.
pub async fn pg_count_brain_vault_nodes_current(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<i64> {
    let count: i64 = if let Some(p) = project {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM brain_vault_nodes WHERE valid_until IS NULL AND project = $1",
        )
        .bind(p)
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM brain_vault_nodes WHERE valid_until IS NULL")
            .fetch_one(pool)
            .await?
    };
    Ok(count)
}

/// Count current Cortex code symbols (node_type LIKE 'code:%') for a corpus slug.
/// `corpus::list_corpora` only counts `content:%` nodes, so this fills the gap for
/// `ff cortex status` which needs the code-symbol total.
pub async fn pg_count_corpus_code_symbols(pool: &PgPool, slug: &str) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM brain_vault_nodes
         WHERE project = $1 AND valid_until IS NULL AND node_type LIKE 'code:%'",
    )
    .bind(slug)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn pg_search_brain_vault_nodes(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<BrainVaultNodeRow>> {
    let pattern = format!("%{}%", query);
    let rows = sqlx::query(
        "SELECT * FROM brain_vault_nodes
         WHERE valid_until IS NULL
           AND (title ILIKE $1 OR path ILIKE $1 OR $1 = ANY(tags))
         ORDER BY hits DESC, updated_at DESC
         LIMIT $2",
    )
    .bind(&pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_brain_vault_node).collect())
}

pub async fn pg_bump_vault_node_hits(pool: &PgPool, id: uuid::Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE brain_vault_nodes SET hits = hits + 1, last_accessed = NOW() WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_supersede_vault_node(
    pool: &PgPool,
    old_path: &str,
    new_id: uuid::Uuid,
) -> Result<()> {
    sqlx::query(
        "UPDATE brain_vault_nodes SET valid_until = NOW(), superseded_by = $2 WHERE path = $1 AND valid_until IS NULL",
    )
    .bind(old_path)
    .bind(new_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ── brain_vault_edges ────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrainVaultEdgeRow {
    pub src_id: uuid::Uuid,
    pub dst_id: uuid::Uuid,
    pub edge_type: String,
    pub confidence: f32,
    pub provenance: String,
}

pub async fn pg_upsert_brain_vault_edge(
    pool: &PgPool,
    src_id: uuid::Uuid,
    dst_id: uuid::Uuid,
    edge_type: &str,
    confidence: f32,
    provenance: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO brain_vault_edges (src_id, dst_id, edge_type, confidence, provenance)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE SET
            confidence = EXCLUDED.confidence, provenance = EXCLUDED.provenance",
    )
    .bind(src_id)
    .bind(dst_id)
    .bind(edge_type)
    .bind(confidence)
    .bind(provenance)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_list_brain_vault_edges_for_node(
    pool: &PgPool,
    node_id: uuid::Uuid,
) -> Result<Vec<BrainVaultEdgeRow>> {
    let rows =
        sqlx::query("SELECT * FROM brain_vault_edges WHERE src_id = $1 OR dst_id = $1 LIMIT 100")
            .bind(node_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .iter()
        .map(|r| BrainVaultEdgeRow {
            src_id: r.get("src_id"),
            dst_id: r.get("dst_id"),
            edge_type: r.get("edge_type"),
            confidence: r.get("confidence"),
            provenance: r.get("provenance"),
        })
        .collect())
}

// ── brain_knowledge_candidates ───────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrainCandidateRow {
    pub id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub thread_id: Option<uuid::Uuid>,
    pub action: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Vec<String>,
    pub project: Option<String>,
    pub target_path: Option<String>,
    pub from_thread: Option<String>,
    pub confidence: Option<f32>,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[allow(clippy::too_many_arguments)]
pub async fn pg_insert_brain_candidate(
    pool: &PgPool,
    user_id: uuid::Uuid,
    thread_id: Option<uuid::Uuid>,
    action: &str,
    kind: Option<&str>,
    title: Option<&str>,
    body: Option<&str>,
    tags: &[String],
    project: Option<&str>,
    target_path: Option<&str>,
    from_thread: Option<&str>,
    confidence: Option<f32>,
) -> Result<uuid::Uuid> {
    let row = sqlx::query(
        "INSERT INTO brain_knowledge_candidates
            (user_id, thread_id, action, kind, title, body, tags, project,
             target_path, from_thread, confidence)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         RETURNING id",
    )
    .bind(user_id)
    .bind(thread_id)
    .bind(action)
    .bind(kind)
    .bind(title)
    .bind(body)
    .bind(tags)
    .bind(project)
    .bind(target_path)
    .bind(from_thread)
    .bind(confidence)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_list_brain_candidates_pending(
    pool: &PgPool,
    user_id: uuid::Uuid,
) -> Result<Vec<BrainCandidateRow>> {
    let rows = sqlx::query(
        "SELECT * FROM brain_knowledge_candidates WHERE user_id = $1 AND status = 'pending'
         ORDER BY created_at DESC
         LIMIT 100",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| BrainCandidateRow {
            id: r.get("id"),
            user_id: r.get("user_id"),
            thread_id: r.get("thread_id"),
            action: r.get("action"),
            kind: r.get("kind"),
            title: r.get("title"),
            body: r.get("body"),
            tags: r.get("tags"),
            project: r.get("project"),
            target_path: r.get("target_path"),
            from_thread: r.get("from_thread"),
            confidence: r.get("confidence"),
            status: r.get("status"),
            created_at: r.get("created_at"),
        })
        .collect())
}

pub async fn pg_update_brain_candidate_status(
    pool: &PgPool,
    id: uuid::Uuid,
    status: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE brain_knowledge_candidates SET status = $2, reviewed_at = NOW() WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

// ── brain_reminders ──────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrainReminderRow {
    pub id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub thread_id: Option<uuid::Uuid>,
    pub content: String,
    pub remind_at: chrono::DateTime<chrono::Utc>,
    pub channel_pref: Option<String>,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn pg_insert_brain_reminder(
    pool: &PgPool,
    user_id: uuid::Uuid,
    thread_id: Option<uuid::Uuid>,
    content: &str,
    remind_at: chrono::DateTime<chrono::Utc>,
    channel_pref: Option<&str>,
) -> Result<uuid::Uuid> {
    let row = sqlx::query(
        "INSERT INTO brain_reminders (user_id, thread_id, content, remind_at, channel_pref)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(user_id)
    .bind(thread_id)
    .bind(content)
    .bind(remind_at)
    .bind(channel_pref)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_list_due_reminders(pool: &PgPool) -> Result<Vec<BrainReminderRow>> {
    let rows = sqlx::query(
        "SELECT * FROM brain_reminders WHERE status = 'pending' AND remind_at <= NOW()
         ORDER BY remind_at ASC LIMIT 50",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| BrainReminderRow {
            id: r.get("id"),
            user_id: r.get("user_id"),
            thread_id: r.get("thread_id"),
            content: r.get("content"),
            remind_at: r.get("remind_at"),
            channel_pref: r.get("channel_pref"),
            status: r.get("status"),
            created_at: r.get("created_at"),
        })
        .collect())
}

pub async fn pg_fire_brain_reminder(pool: &PgPool, id: uuid::Uuid) -> Result<()> {
    sqlx::query("UPDATE brain_reminders SET status = 'fired', fired_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn pg_snooze_brain_reminder(
    pool: &PgPool,
    id: uuid::Uuid,
    until: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE brain_reminders SET status = 'pending', remind_at = $2, snoozed_until = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(until)
    .execute(pool)
    .await?;
    Ok(())
}

// ── brain_communities ────────────────────────────────────────────────────

pub async fn pg_upsert_brain_community(
    pool: &PgPool,
    label: Option<&str>,
    god_node_id: Option<uuid::Uuid>,
    member_count: i32,
    color: Option<&str>,
) -> Result<i32> {
    let row = sqlx::query(
        "INSERT INTO brain_communities (label, god_node_id, member_count, color, updated_at)
         VALUES ($1, $2, $3, $4, NOW()) RETURNING id",
    )
    .bind(label)
    .bind(god_node_id)
    .bind(member_count)
    .bind(color)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_set_vault_node_community(
    pool: &PgPool,
    node_id: uuid::Uuid,
    community_id: i32,
) -> Result<()> {
    sqlx::query("UPDATE brain_vault_nodes SET community_id = $2 WHERE id = $1")
        .bind(node_id)
        .bind(community_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrainCommunityRow {
    pub id: i32,
    pub label: Option<String>,
    pub god_node_id: Option<uuid::Uuid>,
    pub member_count: i32,
    pub color: Option<String>,
}

pub async fn pg_list_brain_communities(pool: &PgPool) -> Result<Vec<BrainCommunityRow>> {
    let rows = sqlx::query(
        "SELECT * FROM brain_communities ORDER BY id
         LIMIT 100",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| BrainCommunityRow {
            id: r.get("id"),
            label: r.get("label"),
            god_node_id: r.get("god_node_id"),
            member_count: r.get("member_count"),
            color: r.get("color"),
        })
        .collect())
}

// ─── V19: shared volumes / computer schedules / training jobs ──────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct SharedVolumeRow {
    pub id: sqlx::types::Uuid,
    pub name: String,
    pub host_computer_id: sqlx::types::Uuid,
    pub host_name: Option<String>,
    pub export_path: String,
    pub mount_path: String,
    pub nfs_version: String,
    pub read_only: bool,
    pub size_gb: Option<f64>,
    pub used_gb: Option<f64>,
    pub purpose: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub metadata: JsonValue,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SharedVolumeMountRow {
    pub volume_id: sqlx::types::Uuid,
    pub volume_name: Option<String>,
    pub computer_id: sqlx::types::Uuid,
    pub computer_name: Option<String>,
    pub mount_path: Option<String>,
    pub mounted_at: chrono::DateTime<chrono::Utc>,
    pub status: String,
    pub last_check_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
}

/// Insert a new shared volume row. Returns the generated UUID.
pub async fn pg_create_shared_volume(
    pool: &PgPool,
    name: &str,
    host_computer_id: sqlx::types::Uuid,
    export_path: &str,
    mount_path: &str,
    purpose: Option<&str>,
    read_only: bool,
) -> Result<sqlx::types::Uuid> {
    let row = sqlx::query(
        "INSERT INTO shared_volumes
            (name, host_computer_id, export_path, mount_path, purpose, read_only)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(name)
    .bind(host_computer_id)
    .bind(export_path)
    .bind(mount_path)
    .bind(purpose)
    .bind(read_only)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_get_shared_volume(pool: &PgPool, name: &str) -> Result<Option<SharedVolumeRow>> {
    let row = sqlx::query(
        "SELECT v.*, c.name as host_name
         FROM shared_volumes v
         LEFT JOIN computers c ON c.id = v.host_computer_id
         WHERE v.name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| SharedVolumeRow {
        id: r.get("id"),
        name: r.get("name"),
        host_computer_id: r.get("host_computer_id"),
        host_name: r.try_get("host_name").ok(),
        export_path: r.get("export_path"),
        mount_path: r.get("mount_path"),
        nfs_version: r.get("nfs_version"),
        read_only: r.get("read_only"),
        size_gb: r.try_get("size_gb").ok(),
        used_gb: r.try_get("used_gb").ok(),
        purpose: r.try_get("purpose").ok(),
        created_at: r.get("created_at"),
        metadata: r.get("metadata"),
    }))
}

pub async fn pg_list_shared_volumes(pool: &PgPool) -> Result<Vec<SharedVolumeRow>> {
    let rows = sqlx::query(
        "SELECT v.*, c.name as host_name
         FROM shared_volumes v
         LEFT JOIN computers c ON c.id = v.host_computer_id
         ORDER BY v.created_at DESC
         LIMIT 100",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| SharedVolumeRow {
            id: r.get("id"),
            name: r.get("name"),
            host_computer_id: r.get("host_computer_id"),
            host_name: r.try_get("host_name").ok(),
            export_path: r.get("export_path"),
            mount_path: r.get("mount_path"),
            nfs_version: r.get("nfs_version"),
            read_only: r.get("read_only"),
            size_gb: r.try_get("size_gb").ok(),
            used_gb: r.try_get("used_gb").ok(),
            purpose: r.try_get("purpose").ok(),
            created_at: r.get("created_at"),
            metadata: r.get("metadata"),
        })
        .collect())
}

pub async fn pg_upsert_shared_volume_mount(
    pool: &PgPool,
    volume_id: sqlx::types::Uuid,
    computer_id: sqlx::types::Uuid,
    mount_path: Option<&str>,
    status: &str,
    last_error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO shared_volume_mounts
            (volume_id, computer_id, mount_path, status, last_check_at, last_error)
         VALUES ($1, $2, $3, $4, NOW(), $5)
         ON CONFLICT (volume_id, computer_id) DO UPDATE SET
             mount_path = EXCLUDED.mount_path,
             status = EXCLUDED.status,
             last_check_at = NOW(),
             last_error = EXCLUDED.last_error",
    )
    .bind(volume_id)
    .bind(computer_id)
    .bind(mount_path)
    .bind(status)
    .bind(last_error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_delete_shared_volume_mount(
    pool: &PgPool,
    volume_id: sqlx::types::Uuid,
    computer_id: sqlx::types::Uuid,
) -> Result<bool> {
    let res =
        sqlx::query("DELETE FROM shared_volume_mounts WHERE volume_id = $1 AND computer_id = $2")
            .bind(volume_id)
            .bind(computer_id)
            .execute(pool)
            .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn pg_list_shared_volume_mounts(
    pool: &PgPool,
    volume_id: Option<sqlx::types::Uuid>,
) -> Result<Vec<SharedVolumeMountRow>> {
    let rows = if let Some(vid) = volume_id {
        sqlx::query(
            "SELECT m.*, v.name as volume_name, c.name as computer_name
             FROM shared_volume_mounts m
             LEFT JOIN shared_volumes v ON v.id = m.volume_id
             LEFT JOIN computers c       ON c.id = m.computer_id
             WHERE m.volume_id = $1
             ORDER BY m.mounted_at DESC
             LIMIT 100",
        )
        .bind(vid)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT m.*, v.name as volume_name, c.name as computer_name
             FROM shared_volume_mounts m
             LEFT JOIN shared_volumes v ON v.id = m.volume_id
             LEFT JOIN computers c       ON c.id = m.computer_id
             ORDER BY m.mounted_at DESC
             LIMIT 100",
        )
        .fetch_all(pool)
        .await?
    };
    Ok(rows
        .iter()
        .map(|r| SharedVolumeMountRow {
            volume_id: r.get("volume_id"),
            volume_name: r.try_get("volume_name").ok(),
            computer_id: r.get("computer_id"),
            computer_name: r.try_get("computer_name").ok(),
            mount_path: r.try_get("mount_path").ok(),
            mounted_at: r.get("mounted_at"),
            status: r.get("status"),
            last_check_at: r.try_get("last_check_at").ok(),
            last_error: r.try_get("last_error").ok(),
        })
        .collect())
}

// ─── computer_schedules ────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct ComputerScheduleRow {
    pub id: sqlx::types::Uuid,
    pub computer_id: sqlx::types::Uuid,
    pub computer_name: Option<String>,
    pub kind: String,
    pub cron_expr: String,
    pub condition: Option<String>,
    pub enabled: bool,
    pub last_fired_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_result: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub created_by: Option<String>,
}

pub async fn pg_create_schedule(
    pool: &PgPool,
    computer_id: sqlx::types::Uuid,
    kind: &str,
    cron_expr: &str,
    condition: Option<&str>,
    created_by: Option<&str>,
) -> Result<sqlx::types::Uuid> {
    let row = sqlx::query(
        "INSERT INTO computer_schedules
            (computer_id, kind, cron_expr, condition, created_by)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(computer_id)
    .bind(kind)
    .bind(cron_expr)
    .bind(condition)
    .bind(created_by)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_list_schedules(
    pool: &PgPool,
    computer_id: Option<sqlx::types::Uuid>,
    only_enabled: bool,
) -> Result<Vec<ComputerScheduleRow>> {
    let rows = match (computer_id, only_enabled) {
        (Some(cid), true) => {
            sqlx::query(
                "SELECT s.*, c.name as computer_name
             FROM computer_schedules s
             LEFT JOIN computers c ON c.id = s.computer_id
             WHERE s.computer_id = $1 AND s.enabled = true
             ORDER BY s.created_at DESC
             LIMIT 100",
            )
            .bind(cid)
            .fetch_all(pool)
            .await?
        }
        (Some(cid), false) => {
            sqlx::query(
                "SELECT s.*, c.name as computer_name
             FROM computer_schedules s
             LEFT JOIN computers c ON c.id = s.computer_id
             WHERE s.computer_id = $1
             ORDER BY s.created_at DESC
             LIMIT 100",
            )
            .bind(cid)
            .fetch_all(pool)
            .await?
        }
        (None, true) => {
            sqlx::query(
                "SELECT s.*, c.name as computer_name
             FROM computer_schedules s
             LEFT JOIN computers c ON c.id = s.computer_id
             WHERE s.enabled = true
             ORDER BY s.created_at DESC
             LIMIT 100",
            )
            .fetch_all(pool)
            .await?
        }
        (None, false) => {
            sqlx::query(
                "SELECT s.*, c.name as computer_name
             FROM computer_schedules s
             LEFT JOIN computers c ON c.id = s.computer_id
             ORDER BY s.created_at DESC
             LIMIT 100",
            )
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows
        .iter()
        .map(|r| ComputerScheduleRow {
            id: r.get("id"),
            computer_id: r.get("computer_id"),
            computer_name: r.try_get("computer_name").ok(),
            kind: r.get("kind"),
            cron_expr: r.get("cron_expr"),
            condition: r.try_get("condition").ok(),
            enabled: r.get("enabled"),
            last_fired_at: r.try_get("last_fired_at").ok(),
            last_result: r.try_get("last_result").ok(),
            created_at: r.get("created_at"),
            created_by: r.try_get("created_by").ok(),
        })
        .collect())
}

pub async fn pg_mark_schedule_fired(
    pool: &PgPool,
    id: sqlx::types::Uuid,
    result: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE computer_schedules
         SET last_fired_at = NOW(), last_result = $2
         WHERE id = $1",
    )
    .bind(id)
    .bind(result)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_delete_schedule(pool: &PgPool, id: sqlx::types::Uuid) -> Result<bool> {
    let res = sqlx::query("DELETE FROM computer_schedules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ─── training_jobs ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrainingJobRow {
    pub id: sqlx::types::Uuid,
    pub name: String,
    pub base_model_id: Option<String>,
    pub training_data_path: String,
    pub adapter_output_path: Option<String>,
    pub training_type: String,
    pub computer_id: Option<sqlx::types::Uuid>,
    pub computer_name: Option<String>,
    pub status: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub loss_curve: JsonValue,
    pub params: JsonValue,
    pub result_model_id: Option<String>,
    pub deferred_task_id: Option<sqlx::types::Uuid>,
    pub error_message: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub created_by: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn pg_create_training_job(
    pool: &PgPool,
    name: &str,
    base_model_id: Option<&str>,
    training_data_path: &str,
    adapter_output_path: Option<&str>,
    training_type: &str,
    computer_id: Option<sqlx::types::Uuid>,
    params: &JsonValue,
    created_by: Option<&str>,
) -> Result<sqlx::types::Uuid> {
    let row = sqlx::query(
        "INSERT INTO training_jobs
            (name, base_model_id, training_data_path, adapter_output_path,
             training_type, computer_id, params, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id",
    )
    .bind(name)
    .bind(base_model_id)
    .bind(training_data_path)
    .bind(adapter_output_path)
    .bind(training_type)
    .bind(computer_id)
    .bind(params)
    .bind(created_by)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn pg_get_training_job(
    pool: &PgPool,
    id: sqlx::types::Uuid,
) -> Result<Option<TrainingJobRow>> {
    let row = sqlx::query(
        "SELECT t.*, c.name as computer_name
         FROM training_jobs t
         LEFT JOIN computers c ON c.id = t.computer_id
         WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| TrainingJobRow {
        id: r.get("id"),
        name: r.get("name"),
        base_model_id: r.try_get("base_model_id").ok(),
        training_data_path: r.get("training_data_path"),
        adapter_output_path: r.try_get("adapter_output_path").ok(),
        training_type: r.get("training_type"),
        computer_id: r.try_get("computer_id").ok(),
        computer_name: r.try_get("computer_name").ok(),
        status: r.get("status"),
        started_at: r.try_get("started_at").ok(),
        completed_at: r.try_get("completed_at").ok(),
        loss_curve: r.get("loss_curve"),
        params: r.get("params"),
        result_model_id: r.try_get("result_model_id").ok(),
        deferred_task_id: r.try_get("deferred_task_id").ok(),
        error_message: r.try_get("error_message").ok(),
        created_at: r.get("created_at"),
        created_by: r.try_get("created_by").ok(),
    }))
}

pub async fn pg_list_training_jobs(
    pool: &PgPool,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<TrainingJobRow>> {
    let rows = if let Some(s) = status {
        sqlx::query(
            "SELECT t.*, c.name as computer_name
             FROM training_jobs t
             LEFT JOIN computers c ON c.id = t.computer_id
             WHERE t.status = $1
             ORDER BY t.created_at DESC
             LIMIT $2",
        )
        .bind(s)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT t.*, c.name as computer_name
             FROM training_jobs t
             LEFT JOIN computers c ON c.id = t.computer_id
             ORDER BY t.created_at DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?
    };
    Ok(rows
        .iter()
        .map(|r| TrainingJobRow {
            id: r.get("id"),
            name: r.get("name"),
            base_model_id: r.try_get("base_model_id").ok(),
            training_data_path: r.get("training_data_path"),
            adapter_output_path: r.try_get("adapter_output_path").ok(),
            training_type: r.get("training_type"),
            computer_id: r.try_get("computer_id").ok(),
            computer_name: r.try_get("computer_name").ok(),
            status: r.get("status"),
            started_at: r.try_get("started_at").ok(),
            completed_at: r.try_get("completed_at").ok(),
            loss_curve: r.get("loss_curve"),
            params: r.get("params"),
            result_model_id: r.try_get("result_model_id").ok(),
            deferred_task_id: r.try_get("deferred_task_id").ok(),
            error_message: r.try_get("error_message").ok(),
            created_at: r.get("created_at"),
            created_by: r.try_get("created_by").ok(),
        })
        .collect())
}

pub async fn pg_update_training_job_status(
    pool: &PgPool,
    id: sqlx::types::Uuid,
    status: &str,
    error_message: Option<&str>,
) -> Result<()> {
    // Transition timestamps based on status.
    let set_started = status == "running";
    let set_completed = matches!(status, "completed" | "failed" | "cancelled");
    sqlx::query(
        "UPDATE training_jobs
         SET status = $2,
             started_at   = CASE WHEN $3 AND started_at   IS NULL THEN NOW() ELSE started_at   END,
             completed_at = CASE WHEN $4 AND completed_at IS NULL THEN NOW() ELSE completed_at END,
             error_message = COALESCE($5, error_message)
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(set_started)
    .bind(set_completed)
    .bind(error_message)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_attach_training_deferred_task(
    pool: &PgPool,
    id: sqlx::types::Uuid,
    deferred_task_id: sqlx::types::Uuid,
) -> Result<()> {
    sqlx::query("UPDATE training_jobs SET deferred_task_id = $2 WHERE id = $1")
        .bind(id)
        .bind(deferred_task_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn pg_append_training_loss_sample(
    pool: &PgPool,
    id: sqlx::types::Uuid,
    step: i64,
    loss: f64,
) -> Result<()> {
    let sample = serde_json::json!({
        "step": step,
        "loss": loss,
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    sqlx::query(
        "UPDATE training_jobs
         SET loss_curve = loss_curve || $2::jsonb
         WHERE id = $1",
    )
    .bind(id)
    .bind(sample)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Benchmark results (helpers around model_catalog.benchmark_results) ────

/// Append one benchmark result to `model_catalog.benchmark_results`. The
/// column is a JSON object keyed by `"<computer>:<iso-timestamp>"`; new runs
/// merge in without overwriting history.
pub async fn pg_append_benchmark_result(
    pool: &PgPool,
    catalog_id: &str,
    computer_name: &str,
    result: &JsonValue,
) -> Result<()> {
    let ts = chrono::Utc::now().to_rfc3339();
    let key = format!("{computer_name}:{ts}");
    let merge = serde_json::json!({ key: result });
    sqlx::query(
        "UPDATE model_catalog
         SET benchmark_results = benchmark_results || $2::jsonb
         WHERE id = $1",
    )
    .bind(catalog_id)
    .bind(merge)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_get_benchmark_results(
    pool: &PgPool,
    catalog_id: &str,
) -> Result<Option<JsonValue>> {
    let row = sqlx::query("SELECT benchmark_results FROM model_catalog WHERE id = $1")
        .bind(catalog_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get("benchmark_results")))
}

// ─── V119 Resource arbiter — work_intents registry + set-atomic lease ────────
//
// Backlog #7. EXPLICIT-declaration arbiter. These helpers wire the existing
// per-host CAS (`pg_reserve_host`, queries.rs:2988) into a set-atomic,
// deadlock-free, TTL-leased grant keyed by a `work_intents` row.

/// One row from the `work_intents` registry (the intent IS the FIFO queue).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkIntentRow {
    pub id: String,
    pub requester: String,
    pub project: Option<String>,
    pub target_host_set: JsonValue,
    pub requires_capability: JsonValue,
    pub exclusive: bool,
    pub requested_secs: i64,
    pub priority: i64,
    pub state: String,
    pub task_desc: Option<String>,
    pub prework_plan: JsonValue,
    pub restore_plan: JsonValue,
    pub prework_cursor: i64,
    pub restore_cursor: i64,
    pub denied_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub granted_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub released_at: Option<DateTime<Utc>>,
}

fn work_intent_from_row(r: &sqlx::postgres::PgRow) -> WorkIntentRow {
    let id: uuid::Uuid = r.get("id");
    WorkIntentRow {
        id: id.to_string(),
        requester: r.get("requester"),
        project: r.get("project"),
        target_host_set: r.get("target_host_set"),
        requires_capability: r.get("requires_capability"),
        exclusive: r.get("exclusive"),
        requested_secs: r.get::<i32, _>("requested_secs") as i64,
        priority: r.get::<i32, _>("priority") as i64,
        state: r.get("state"),
        task_desc: r.get("task_desc"),
        prework_plan: r.get("prework_plan"),
        restore_plan: r.get("restore_plan"),
        prework_cursor: r.get::<i32, _>("prework_cursor") as i64,
        restore_cursor: r.get::<i32, _>("restore_cursor") as i64,
        denied_reason: r.get("denied_reason"),
        created_at: r.get("created_at"),
        granted_at: r.get("granted_at"),
        expires_at: r.get("expires_at"),
        released_at: r.get("released_at"),
    }
}

const WORK_INTENT_COLS: &str = "id, requester, project, target_host_set, \
    requires_capability, exclusive, requested_secs, priority, state, task_desc, \
    prework_plan, restore_plan, prework_cursor, restore_cursor, denied_reason, \
    created_at, granted_at, expires_at, released_at";

/// Insert a new work intent in `pending` state. Returns the new intent id.
/// This is what `ff reserve` calls — explicit declaration, no inference.
#[allow(clippy::too_many_arguments)]
pub async fn pg_insert_work_intent(
    pool: &PgPool,
    requester: &str,
    project: Option<&str>,
    target_host_set: &JsonValue,
    requires_capability: &JsonValue,
    exclusive: bool,
    requested_secs: i64,
    priority: i64,
    task_desc: Option<&str>,
    prework_plan: &JsonValue,
    restore_plan: &JsonValue,
) -> Result<String> {
    let id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO work_intents
            (requester, project, target_host_set, requires_capability, exclusive,
             requested_secs, priority, task_desc, prework_plan, restore_plan)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING id
        "#,
    )
    .bind(requester)
    .bind(project)
    .bind(target_host_set)
    .bind(requires_capability)
    .bind(exclusive)
    .bind(requested_secs.max(1) as i32)
    .bind(priority as i32)
    .bind(task_desc)
    .bind(prework_plan)
    .bind(restore_plan)
    .fetch_one(pool)
    .await?;
    Ok(id.to_string())
}

/// Fetch a single intent by id.
pub async fn pg_get_work_intent(pool: &PgPool, intent_id: &str) -> Result<Option<WorkIntentRow>> {
    let uid = uuid::Uuid::parse_str(intent_id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad intent id {intent_id}: {e}")))?;
    let row = sqlx::query(&format!(
        "SELECT {WORK_INTENT_COLS} FROM work_intents WHERE id = $1"
    ))
    .bind(uid)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(work_intent_from_row))
}

/// List intents in deterministic order (priority DESC, created_at ASC), most
/// recent terminal ones excluded when `active_only`.
pub async fn pg_list_work_intents(pool: &PgPool, active_only: bool) -> Result<Vec<WorkIntentRow>> {
    let sql = if active_only {
        format!(
            "SELECT {WORK_INTENT_COLS} FROM work_intents \
             WHERE state NOT IN ('done','denied') \
             ORDER BY priority DESC, created_at ASC"
        )
    } else {
        format!(
            "SELECT {WORK_INTENT_COLS} FROM work_intents \
             ORDER BY priority DESC, created_at ASC"
        )
    };
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    Ok(rows.iter().map(work_intent_from_row).collect())
}

/// The pending FIFO queue: state='pending', deterministic (priority DESC,
/// created_at ASC). This IS the grant queue — no separate queue table.
pub async fn pg_pending_work_intents(pool: &PgPool) -> Result<Vec<WorkIntentRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {WORK_INTENT_COLS} FROM work_intents \
         WHERE state = 'pending' ORDER BY priority DESC, created_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(work_intent_from_row).collect())
}

/// Transition an intent's state. `granted` stamps granted_at/expires_at;
/// `done`/`releasing` stamp released_at. Idempotent on already-terminal states
/// via the optional `from` guard.
pub async fn pg_set_work_intent_state(
    pool: &PgPool,
    intent_id: &str,
    new_state: &str,
    denied_reason: Option<&str>,
) -> Result<()> {
    let uid = uuid::Uuid::parse_str(intent_id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad intent id {intent_id}: {e}")))?;
    sqlx::query(
        r#"
        UPDATE work_intents
           SET state         = $2,
               denied_reason = COALESCE($3, denied_reason),
               released_at   = CASE WHEN $2 IN ('done','releasing')
                                    THEN COALESCE(released_at, NOW()) ELSE released_at END
         WHERE id = $1
        "#,
    )
    .bind(uid)
    .bind(new_state)
    .bind(denied_reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Advance a crash-resumable executor cursor (prework or restore). The arbiter
/// persists this AFTER each successful step so a leader crash resumes mid-plan.
pub async fn pg_advance_intent_cursor(
    pool: &PgPool,
    intent_id: &str,
    restore: bool,
    new_cursor: i64,
) -> Result<()> {
    let uid = uuid::Uuid::parse_str(intent_id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad intent id {intent_id}: {e}")))?;
    let col = if restore {
        "restore_cursor"
    } else {
        "prework_cursor"
    };
    sqlx::query(&format!("UPDATE work_intents SET {col} = $2 WHERE id = $1"))
        .bind(uid)
        .bind(new_cursor as i32)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set-atomic, deadlock-free reservation of a host SET for one intent. Built on
/// the V114 per-host CAS. `hosts` MUST be pre-sorted by the caller (lowercased
/// name ASC) — the global resource-ordering is what makes overlapping concurrent
/// grants deadlock-free (Coffman hold-and-wait broken). All-or-nothing: if ANY
/// host is not `available`, the whole transaction rolls back and NO host is left
/// stranded. Returns `true` only if the ENTIRE set was won + committed.
///
/// On success each host carries `reservation_state='reserved'`,
/// `reserved_reason='arbiter:<intent_id>'`, `reservation_owner=<intent_id>`,
/// and `reservation_expires_at=NOW()+requested_secs`.
pub async fn pg_arbiter_grant_set(
    pool: &PgPool,
    intent_id: &str,
    hosts: &[String],
    requested_secs: i64,
) -> Result<bool> {
    let uid = uuid::Uuid::parse_str(intent_id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad intent id {intent_id}: {e}")))?;
    if hosts.is_empty() {
        return Ok(false);
    }
    let reason = format!("arbiter:{intent_id}");
    let mut tx = pool.begin().await?;
    for host in hosts {
        let res = sqlx::query(
            r#"
            UPDATE computers
               SET reservation_state      = 'reserved',
                   reserved_reason        = $2,
                   reserved_at            = NOW(),
                   reservation_owner      = $3,
                   reservation_expires_at = NOW() + make_interval(secs => $4)
             WHERE LOWER(name) = LOWER($1)
               AND reservation_state = 'available'
            "#,
        )
        .bind(host)
        .bind(&reason)
        .bind(uid)
        .bind(requested_secs.max(1))
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            // Partial set unattainable — roll back everything flipped this tx.
            tx.rollback().await?;
            return Ok(false);
        }
    }
    tx.commit().await?;
    Ok(true)
}

/// Arbiter lease reaper: find hosts whose lease has expired and clear them.
/// Returns the list of (intent_id) owning the expired leases so the caller can
/// run each intent's restore_plan BEFORE the host is handed to the next queued
/// intent. The host's reservation is flipped back to `available` here, but the
/// arbiter only marks the host truly free after restore completes (it re-checks
/// the owning intent reaches `done`). Mirrors RESERVATION_TTL_SECS but
/// per-intent + data-driven.
pub async fn pg_reap_expired_leases(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT reservation_owner AS owner
          FROM computers
         WHERE reservation_owner IS NOT NULL
           AND reservation_expires_at IS NOT NULL
           AND reservation_expires_at < NOW()
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| r.get::<Option<uuid::Uuid>, _>("owner"))
        .map(|u| u.to_string())
        .collect())
}

/// Free every host held by one intent: flip `reserved` → `available`, NULL the
/// owner/lease/reason. Idempotent — safe on an already-free set. Called AFTER
/// the intent's restore_plan finishes (lease expiry, `ff arbiter release`, or
/// the reaper). Returns the number of hosts freed.
pub async fn pg_arbiter_free_set(pool: &PgPool, intent_id: &str) -> Result<u64> {
    let uid = uuid::Uuid::parse_str(intent_id)
        .map_err(|e| crate::error::DbError::NotFound(format!("bad intent id {intent_id}: {e}")))?;
    let res = sqlx::query(
        r#"
        UPDATE computers
           SET reservation_state      = 'available',
               reserved_reason        = NULL,
               reserved_at            = NULL,
               reservation_owner      = NULL,
               reservation_expires_at = NULL
         WHERE reservation_owner = $1
        "#,
    )
    .bind(uid)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Snapshot of an arbiter-reserved host for `ff arbiter status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbiterReservedHost {
    pub name: String,
    pub reservation_state: String,
    pub reserved_reason: Option<String>,
    pub reservation_owner: Option<String>,
    pub reservation_expires_at: Option<DateTime<Utc>>,
}

/// All hosts currently reserved/drained (deterministic ORDER BY name ASC).
pub async fn pg_list_reserved_hosts(pool: &PgPool) -> Result<Vec<ArbiterReservedHost>> {
    let rows = sqlx::query(
        r#"
        SELECT name, reservation_state, reserved_reason,
               reservation_owner, reservation_expires_at
          FROM computers
         WHERE reservation_state <> 'available'
         ORDER BY LOWER(name) ASC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ArbiterReservedHost {
            name: r.get("name"),
            reservation_state: r.get("reservation_state"),
            reserved_reason: r.get("reserved_reason"),
            reservation_owner: r
                .get::<Option<uuid::Uuid>, _>("reservation_owner")
                .map(|u| u.to_string()),
            reservation_expires_at: r.get("reservation_expires_at"),
        })
        .collect())
}

// ─── Interaction Log (V121 ff_interactions) ─────────────────────────────────

/// One row of the unified interaction log — a single ff "turn" across any
/// channel. Maps 1:1 to the `ff_interactions` table (V121). Columns with DB
/// defaults (`id`, `ts`) are not part of the record.
#[derive(Debug, Clone)]
pub struct InteractionRecord {
    pub session_id: Option<uuid::Uuid>,
    pub channel: String,
    pub user_id: Option<uuid::Uuid>,
    pub request_text: String,
    pub request_meta: serde_json::Value,
    pub route_decision: serde_json::Value,
    pub engine: Option<String>,
    pub steps: serde_json::Value,
    pub response_text: String,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub cost_usd: f64,
    pub latency_ms: Option<i32>,
    pub outcome: String,
    pub error_text: Option<String>,
    pub error_signature: Option<String>,
    pub ff_build_sha: Option<String>,
    pub model_versions: serde_json::Value,
}

impl Default for InteractionRecord {
    fn default() -> Self {
        Self {
            session_id: None,
            channel: "unknown".to_string(),
            user_id: None,
            request_text: String::new(),
            request_meta: serde_json::json!({}),
            route_decision: serde_json::json!({}),
            engine: None,
            steps: serde_json::json!([]),
            response_text: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            latency_ms: None,
            outcome: "ok".to_string(),
            error_text: None,
            error_signature: None,
            ff_build_sha: None,
            model_versions: serde_json::json!({}),
        }
    }
}

/// Insert one interaction-log row and return its generated UUID.
pub async fn pg_record_interaction(pool: &PgPool, r: &InteractionRecord) -> Result<uuid::Uuid> {
    let row = sqlx::query(
        "INSERT INTO ff_interactions
            (session_id, channel, user_id, request_text, request_meta,
             route_decision, engine, steps, response_text, tokens_in,
             tokens_out, cost_usd, latency_ms, outcome, error_text,
             error_signature, ff_build_sha, model_versions)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)
         RETURNING id",
    )
    .bind(r.session_id)
    .bind(&r.channel)
    .bind(r.user_id)
    .bind(&r.request_text)
    .bind(&r.request_meta)
    .bind(&r.route_decision)
    .bind(&r.engine)
    .bind(&r.steps)
    .bind(&r.response_text)
    .bind(r.tokens_in)
    .bind(r.tokens_out)
    .bind(r.cost_usd)
    .bind(r.latency_ms)
    .bind(&r.outcome)
    .bind(&r.error_text)
    .bind(&r.error_signature)
    .bind(&r.ff_build_sha)
    .bind(&r.model_versions)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}
