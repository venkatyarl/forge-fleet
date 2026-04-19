//! NATS-backed fleet-event bus.
//!
//! Complements [`crate::fleet_events`] (Redis pub/sub) by mirroring the
//! same lifecycle events to NATS so dashboards, log aggregators, and
//! external integrations can subscribe with NATS clients in any
//! language. NATS is optional — every method here silently no-ops if the
//! global NATS client wasn't initialized at startup.
//!
//! ## Subjects
//!
//! - `fleet.events.leader_changed`            — JSON `{old, new, epoch}`
//! - `fleet.events.member.{name}.online`      — JSON `{name, ts}`
//! - `fleet.events.member.{name}.offline`     — JSON `{name, reason, ts}`
//! - `fleet.events.deployment.{computer}.{status}` — JSON `{computer, deployment_id, status, model_id, ts}`
//! - `fleet.events.work_item.{status}`        — JSON `{work_item_id, status, assigned_to, ts}`
//!
//! Subscribe to `fleet.events.>` to receive everything.

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::nats_client::publish_json;

/// Namespace prefix for all fleet event subjects.
pub const FLEET_EVENTS_PREFIX: &str = "fleet.events";

pub struct FleetEventBus;

impl FleetEventBus {
    /// Publish a leader-change event.
    pub async fn publish_leader_change(old: Option<&str>, new: &str, epoch: u64) {
        let subject = format!("{FLEET_EVENTS_PREFIX}.leader_changed");
        let payload = json!({
            "old": old,
            "new": new,
            "epoch": epoch,
            "ts": Utc::now().to_rfc3339(),
        });
        publish_json(subject, &payload).await;
    }

    /// Publish that a member came online (or rejoined).
    pub async fn publish_member_online(name: &str) {
        let subject = format!("{FLEET_EVENTS_PREFIX}.member.{name}.online");
        let payload = json!({
            "name": name,
            "ts": Utc::now().to_rfc3339(),
        });
        publish_json(subject, &payload).await;
    }

    /// Publish that a member went offline.
    pub async fn publish_member_offline(name: &str, reason: &str) {
        let subject = format!("{FLEET_EVENTS_PREFIX}.member.{name}.offline");
        let payload = json!({
            "name": name,
            "reason": reason,
            "ts": Utc::now().to_rfc3339(),
        });
        publish_json(subject, &payload).await;
    }

    /// Publish a deployment lifecycle transition (started/stopped/failed/etc).
    pub async fn publish_deployment_change(
        computer: &str,
        deployment_id: Uuid,
        status: &str,
        model_id: &str,
    ) {
        let subject = format!("{FLEET_EVENTS_PREFIX}.deployment.{computer}.{status}");
        let payload = json!({
            "computer": computer,
            "deployment_id": deployment_id,
            "status": status,
            "model_id": model_id,
            "ts": Utc::now().to_rfc3339(),
        });
        publish_json(subject, &payload).await;
    }

    /// Publish a work-item status change (dispatched / started / done / failed / etc.).
    pub async fn publish_work_item_change(
        work_item_id: Uuid,
        new_status: &str,
        assigned_to: Option<&str>,
    ) {
        let subject = format!("{FLEET_EVENTS_PREFIX}.work_item.{new_status}");
        let payload = json!({
            "work_item_id": work_item_id,
            "status": new_status,
            "assigned_to": assigned_to,
            "ts": Utc::now().to_rfc3339(),
        });
        publish_json(subject, &payload).await;
    }
}
