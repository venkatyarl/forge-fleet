//! Typed query helpers for common database operations.
//!
//! Provides a clean Rust API over raw SQL. All functions take a `&Connection`
//! to work with both pooled and standalone connections.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

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
